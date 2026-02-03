//! CAN bridge server - bridges local CAN interface to remote clients over iroh P2P.
//!
//! This module provides a server that exposes a local CAN interface over the network,
//! allowing remote clients to send and receive CAN frames.
//!
//! Architecture: 2 persistent OS threads (reader + writer) communicate with async
//! connection tasks via channels. No extra tokio runtimes, no Mutex held across
//! awaits. CAN-to-network writes are batched for throughput.

use anyhow::Result;
use socketcan::Socket;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::can::{CanFdFrame, CanFrame};
use crate::can_types::{wire, AnyCanFrame};
use crate::iroh::{IrohConnection, IrohServerBuilder};

/// A server that bridges a local CAN interface to remote clients over iroh P2P.
pub struct CanServer {
    server_id: String,
    /// Sender for frames to write to CAN interface (std channel — writer thread blocks on recv).
    can_write_tx: std::sync::mpsc::Sender<AnyCanFrame>,
    /// Receiver for frames read from CAN interface.
    /// Wrapped in `Mutex<Option<>>` so ownership can be transferred to/from connection tasks.
    /// The Mutex is held only for a quick `take()`/`replace()` swap, never across awaits.
    can_read_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<AnyCanFrame>>>,
    endpoint: Arc<crate::iroh::IrohServer>,
}

/// Reader thread: opens a socketcan socket and pushes frames into a tokio mpsc channel.
///
/// Uses `blocking_send` so no tokio runtime is needed on this thread.
fn can_reader_thread(
    interface: String,
    enable_fd: bool,
    tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    init_tx: std::sync::mpsc::SyncSender<Result<()>>,
) {
    let timeout = Duration::from_millis(100);

    if enable_fd {
        let socket = match socketcan::CanFdSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN FD reader socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        if let Err(e) = socket.set_read_timeout(timeout) {
            let _ = init_tx.send(Err(anyhow::anyhow!("Failed to set read timeout: {}", e)));
            return;
        }
        let _ = init_tx.send(Ok(()));

        loop {
            match socket.read_frame() {
                Ok(frame) => {
                    let any_frame = match frame {
                        socketcan::CanAnyFrame::Normal(f) => match CanFrame::try_from(f) {
                            Ok(cf) => AnyCanFrame::Can(cf),
                            Err(e) => {
                                tracing::warn!("CAN frame conversion error: {}", e);
                                continue;
                            }
                        },
                        socketcan::CanAnyFrame::Fd(f) => match CanFdFrame::try_from(f) {
                            Ok(cf) => AnyCanFrame::CanFd(cf),
                            Err(e) => {
                                tracing::warn!("CAN FD frame conversion error: {}", e);
                                continue;
                            }
                        },
                        socketcan::CanAnyFrame::Remote(_) | socketcan::CanAnyFrame::Error(_) => {
                            continue;
                        }
                    };
                    if tx.blocking_send(any_frame).is_err() {
                        break; // Receiver dropped
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::warn!("CAN read error (ignoring): {}", e);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    } else {
        let socket = match socketcan::CanSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN reader socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        if let Err(e) = socket.set_read_timeout(timeout) {
            let _ = init_tx.send(Err(anyhow::anyhow!("Failed to set read timeout: {}", e)));
            return;
        }
        let _ = init_tx.send(Ok(()));

        loop {
            match socket.read_frame() {
                Ok(frame) => {
                    let any_frame = match CanFrame::try_from(frame) {
                        Ok(cf) => AnyCanFrame::Can(cf),
                        Err(e) => {
                            tracing::warn!("CAN frame conversion error: {}", e);
                            continue;
                        }
                    };
                    if tx.blocking_send(any_frame).is_err() {
                        break; // Receiver dropped
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => {
                    tracing::warn!("CAN read error (ignoring): {}", e);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

/// Maximum retries on ENOBUFS before dropping a frame.
const WRITE_RETRIES: u32 = 20;
/// Delay between retries on ENOBUFS (500us). Total max backoff = 10ms.
const WRITE_RETRY_DELAY: Duration = Duration::from_micros(500);

/// Retry a write operation on ENOBUFS.
///
/// When the kernel CAN transmit queue is full, `write_frame()` returns ENOBUFS
/// immediately. Rather than dropping the frame, we sleep briefly and retry,
/// which naturally rate-limits the writer to match CAN bus throughput.
fn write_with_retry(mut write_fn: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    for attempt in 0..WRITE_RETRIES {
        match write_fn() {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(105) => {
                // ENOBUFS — kernel txqueue full, back off and retry
                if attempt == 0 {
                    tracing::trace!("CAN txqueue full, backing off");
                }
                std::thread::sleep(WRITE_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::from_raw_os_error(105))
}

/// Writer thread: receives frames from a std mpsc channel and writes them to socketcan.
fn can_writer_thread(
    interface: String,
    enable_fd: bool,
    rx: std::sync::mpsc::Receiver<AnyCanFrame>,
    init_tx: std::sync::mpsc::SyncSender<Result<()>>,
) {
    if enable_fd {
        let socket = match socketcan::CanFdSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN FD writer socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        let _ = init_tx.send(Ok(()));

        while let Ok(frame) = rx.recv() {
            let result = match &frame {
                AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                    Ok(sf) => write_with_retry(|| socket.write_frame(&sf)),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error on write: {}", e);
                        continue;
                    }
                },
                AnyCanFrame::CanFd(f) => match socketcan::CanFdFrame::try_from(f) {
                    Ok(sf) => write_with_retry(|| socket.write_frame(&sf)),
                    Err(e) => {
                        tracing::warn!("CAN FD frame conversion error on write: {}", e);
                        continue;
                    }
                },
            };
            if let Err(e) = result {
                tracing::warn!("CAN write error (dropping frame): {}", e);
            }
        }
    } else {
        let socket = match socketcan::CanSocket::open(&interface) {
            Ok(s) => s,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::anyhow!(
                    "Failed to open CAN writer socket on {}: {}",
                    interface,
                    e
                )));
                return;
            }
        };
        let _ = init_tx.send(Ok(()));

        while let Ok(frame) = rx.recv() {
            let result = match &frame {
                AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                    Ok(sf) => write_with_retry(|| socket.write_frame(&sf)),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error on write: {}", e);
                        continue;
                    }
                },
                AnyCanFrame::CanFd(_) => {
                    tracing::warn!("CAN FD frame on standard CAN socket, dropping");
                    continue;
                }
            };
            if let Err(e) = result {
                tracing::warn!("CAN write error (dropping frame): {}", e);
            }
        }
    }
}

impl CanServer {
    /// Create a new CAN bridge server.
    ///
    /// Spawns two OS threads (reader + writer) that access the CAN interface directly
    /// via socketcan. These threads communicate with async connection tasks via channels.
    ///
    /// # Arguments
    /// * `interface` - CAN interface name (e.g., "can0", "vcan0")
    /// * `enable_fd` - Enable CAN FD support
    /// * `identity_path` - Optional path to save/load server identity
    pub async fn new(
        interface: &str,
        enable_fd: bool,
        identity_path: Option<&str>,
    ) -> Result<Self> {
        // CAN→Network channel (tokio mpsc for async receiver)
        let (can_read_tx, can_read_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(256);
        // Network→CAN channel (std mpsc — writer thread blocks on recv)
        let (can_write_tx, can_write_rx) = std::sync::mpsc::channel::<AnyCanFrame>();

        // Spawn reader thread
        let (reader_init_tx, reader_init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let iface_reader = interface.to_string();
        std::thread::Builder::new()
            .name(format!("can-read-{}", interface))
            .spawn(move || {
                can_reader_thread(iface_reader, enable_fd, can_read_tx, reader_init_tx);
            })?;

        // Spawn writer thread
        let (writer_init_tx, writer_init_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let iface_writer = interface.to_string();
        std::thread::Builder::new()
            .name(format!("can-write-{}", interface))
            .spawn(move || {
                can_writer_thread(iface_writer, enable_fd, can_write_rx, writer_init_tx);
            })?;

        // Wait for both threads to initialize (propagate socket open errors)
        reader_init_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("CAN reader thread died during init"))??;
        writer_init_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("CAN writer thread died during init"))??;

        // Start iroh server
        let mut builder = IrohServerBuilder::new();
        if let Some(path) = identity_path {
            builder = builder.identity_path(path);
        }
        let server = builder.bind().await?;
        let server_id = server.id().to_string();

        Ok(Self {
            server_id,
            can_write_tx,
            can_read_rx: std::sync::Mutex::new(Some(can_read_rx)),
            endpoint: Arc::new(server),
        })
    }

    /// Get the server's endpoint ID (share this with clients).
    pub fn id(&self) -> &str {
        &self.server_id
    }

    /// Run the bridge server (blocks forever, handling connections).
    ///
    /// When a new client connects, any existing connection is automatically
    /// terminated to allow reconnection. The CAN read receiver is recovered
    /// from the old connection task before being handed to the new one.
    pub async fn run(&self) -> Result<()> {
        tracing::info!("CAN bridge server running. ID: {}", self.server_id);

        let mut current_conn: Option<(
            CancellationToken,
            tokio::task::JoinHandle<tokio::sync::mpsc::Receiver<AnyCanFrame>>,
        )> = None;

        loop {
            let conn = match self.endpoint.accept().await? {
                Some(c) => c,
                None => continue,
            };

            tracing::info!("Client connected: {}", conn.remote_id());

            // Cancel previous connection and recover the receiver
            if let Some((cancel, handle)) = current_conn.take() {
                tracing::info!("New client connected, closing previous connection");
                cancel.cancel();
                match handle.await {
                    Ok(rx) => {
                        self.can_read_rx.lock().unwrap().replace(rx);
                    }
                    Err(e) => {
                        tracing::error!("Connection task panicked: {}", e);
                    }
                }
            }

            // Take receiver ownership for the new connection
            let rx = self
                .can_read_rx
                .lock()
                .unwrap()
                .take()
                .expect("CAN read receiver should be available");

            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            let write_tx = self.can_write_tx.clone();

            let handle = tokio::spawn(async move {
                let (result, rx) = handle_connection(conn, rx, write_tx, cancel_clone).await;
                if let Err(e) = &result {
                    tracing::error!("Connection error: {}", e);
                }
                tracing::info!("Client disconnected");
                rx
            });

            current_conn = Some((cancel, handle));
        }
    }

    /// Run the bridge server for a single connection, then return.
    pub async fn run_once(&self) -> Result<()> {
        tracing::info!(
            "CAN bridge server waiting for connection. ID: {}",
            self.server_id
        );

        loop {
            let conn = match self.endpoint.accept().await? {
                Some(c) => c,
                None => continue,
            };

            tracing::info!("Client connected: {}", conn.remote_id());

            let rx = self
                .can_read_rx
                .lock()
                .unwrap()
                .take()
                .expect("CAN read receiver should be available");

            let write_tx = self.can_write_tx.clone();
            let cancel = CancellationToken::new();

            let (result, rx) = handle_connection(conn, rx, write_tx, cancel).await;

            // Put receiver back
            self.can_read_rx.lock().unwrap().replace(rx);

            tracing::info!("Client disconnected");

            if let Err(e) = result {
                tracing::error!("Connection error: {}", e);
            }

            return Ok(());
        }
    }
}

/// Core bridge logic for a single connection.
///
/// The CAN read receiver is moved in and always moved back out, ensuring no
/// Mutex is held across awaits. CAN-to-network writes are batched: after
/// receiving the first frame, up to 63 additional ready frames are collected
/// and flushed in a single write.
async fn handle_connection(
    conn: IrohConnection,
    mut can_read_rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    can_write_tx: std::sync::mpsc::Sender<AnyCanFrame>,
    cancel: CancellationToken,
) -> (Result<()>, tokio::sync::mpsc::Receiver<AnyCanFrame>) {
    let stream = match conn.accept_stream().await {
        Ok(s) => s,
        Err(e) => {
            return (
                Err(anyhow::anyhow!("Failed to accept stream: {}", e)),
                can_read_rx,
            );
        }
    };

    let (mut send, mut recv) = stream.split();

    // Drain stale frames
    let mut drained = 0;
    while can_read_rx.try_recv().is_ok() {
        drained += 1;
    }
    if drained > 0 {
        tracing::info!("Drained {} stale CAN frames from buffer", drained);
    }

    // Combine our cancellation with the connection's own token
    let conn_cancel = conn.cancellation_token();

    // CAN → Network task (with batching)
    let can_to_net_cancel = cancel.clone();
    let conn_cancel_clone = conn_cancel.clone();
    let can_to_net = tokio::spawn(async move {
        let mut batch_buf = Vec::with_capacity(1024);

        loop {
            batch_buf.clear();

            // Wait for first frame (or cancellation)
            let first = tokio::select! {
                _ = can_to_net_cancel.cancelled() => break,
                _ = conn_cancel_clone.cancelled() => break,
                frame = can_read_rx.recv() => match frame {
                    Some(f) => f,
                    None => break,
                }
            };

            // Encode first frame
            batch_buf.extend_from_slice(&wire::encode(&first));

            // Greedily collect more ready frames (up to 64 total)
            for _ in 1..64 {
                match can_read_rx.try_recv() {
                    Ok(frame) => batch_buf.extend_from_slice(&wire::encode(&frame)),
                    Err(_) => break,
                }
            }

            // Single write + flush for entire batch
            if send.write_all(&batch_buf).await.is_err() {
                break;
            }
            if send.flush().await.is_err() {
                break;
            }
        }

        can_read_rx // Return receiver ownership
    });

    // Network → CAN (inline in this task)
    let mut buf = vec![0u8; 1024];
    let mut pending = Vec::new();
    let result = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break Ok(());
            }
            _ = conn_cancel.cancelled() => {
                break Ok(());
            }
            read_result = recv.read(&mut buf) => {
                match read_result {
                    Ok(Some(n)) if n > 0 => {
                        pending.extend_from_slice(&buf[..n]);

                        while pending.len() >= 6 {
                            match wire::encoded_size(&pending) {
                                Ok(frame_size) if pending.len() >= frame_size => {
                                    match wire::decode(&pending) {
                                        Ok((frame, consumed)) => {
                                            tracing::debug!(
                                                "Network -> CAN: ID={:x}, {} bytes",
                                                frame.id(),
                                                consumed
                                            );
                                            if can_write_tx.send(frame).is_err() {
                                                tracing::error!("CAN writer thread died");
                                                break;
                                            }
                                            pending.drain(..consumed);
                                        }
                                        Err(e) => {
                                            tracing::error!("Frame decode error: {}", e);
                                            pending.clear();
                                            break;
                                        }
                                    }
                                }
                                Ok(_) => break, // Need more data
                                Err(e) => {
                                    tracing::error!("Frame size error: {}", e);
                                    pending.clear();
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        tracing::info!("Client disconnected (stream closed)");
                        break Ok(());
                    }
                    Err(e) => {
                        break Err(anyhow::anyhow!("Network read error: {}", e));
                    }
                }
            }
        }
    };

    // Cancel the CAN-to-net task and recover the receiver
    cancel.cancel();
    let can_read_rx = match can_to_net.await {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!("CAN-to-net task panicked: {}", e);
            // Receiver is lost if the task panicked — this is unrecoverable
            // but we still need to return something. The caller will see the
            // panic error from run()'s handle.await.
            return (
                Err(anyhow::anyhow!("CAN-to-net task panicked: {}", e)),
                // Create a dummy channel — run() will see the JoinError and
                // won't use this receiver anyway since the handle.await fails.
                tokio::sync::mpsc::channel(1).1,
            );
        }
    };

    (result, can_read_rx)
}
