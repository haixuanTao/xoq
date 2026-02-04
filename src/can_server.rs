//! CAN bridge server - bridges local CAN interface to remote clients over iroh P2P.
//!
//! Architecture: 2 persistent OS threads (reader + writer) communicate with async
//! connection tasks via channels.

use anyhow::Result;
use socketcan::Socket;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::can::{CanFdFrame, CanFrame};
use crate::can_types::{wire, AnyCanFrame};
use crate::iroh::{IrohConnection, IrohServerBuilder};

/// A server that bridges a local CAN interface to remote clients over iroh P2P.
pub struct CanServer {
    server_id: String,
    can_write_tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    can_read_rx: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<AnyCanFrame>>>,
    endpoint: Arc<crate::iroh::IrohServer>,
}

/// Reader thread for CAN FD sockets.
fn can_reader_thread_fd(
    socket: Arc<socketcan::CanFdSocket>,
    tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let mut last_read = Instant::now();
    let mut would_blocks: u32 = 0;
    let mut timed_outs: u32 = 0;
    let mut writes_at_gap_start: u64 = 0;
    loop {
        let read_start = Instant::now();
        match socket.read_frame() {
            Ok(frame) => {
                let gap = last_read.elapsed();
                let writes_now = write_count.load(Ordering::Relaxed);
                let writes_during = writes_now - writes_at_gap_start;
                if gap > Duration::from_millis(50) && writes_during >= 3 {
                    tracing::warn!(
                        "CAN response delay: {:.1}ms ({} timed_out, {} would_block, {} writes during gap)",
                        gap.as_secs_f64() * 1000.0,
                        timed_outs,
                        would_blocks,
                        writes_during,
                    );
                }
                last_read = Instant::now();
                would_blocks = 0;
                timed_outs = 0;
                writes_at_gap_start = writes_now;
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
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let call_dur = read_start.elapsed();
                if call_dur > Duration::from_millis(5) {
                    tracing::warn!(
                        "CAN read_frame() WouldBlock took {:.1}ms",
                        call_dur.as_secs_f64() * 1000.0
                    );
                }
                would_blocks += 1;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                timed_outs += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!("CAN read error (ignoring): {}", e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Reader thread for standard CAN sockets.
fn can_reader_thread_std(
    socket: Arc<socketcan::CanSocket>,
    tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let mut last_read = Instant::now();
    let mut would_blocks: u32 = 0;
    let mut timed_outs: u32 = 0;
    let mut writes_at_gap_start: u64 = 0;
    loop {
        let read_start = Instant::now();
        match socket.read_frame() {
            Ok(frame) => {
                let gap = last_read.elapsed();
                let writes_now = write_count.load(Ordering::Relaxed);
                let writes_during = writes_now - writes_at_gap_start;
                if gap > Duration::from_millis(50) && writes_during >= 3 {
                    tracing::warn!(
                        "CAN response delay: {:.1}ms ({} timed_out, {} would_block, {} writes during gap)",
                        gap.as_secs_f64() * 1000.0,
                        timed_outs,
                        would_blocks,
                        writes_during,
                    );
                }
                last_read = Instant::now();
                would_blocks = 0;
                timed_outs = 0;
                writes_at_gap_start = writes_now;
                let any_frame = match CanFrame::try_from(frame) {
                    Ok(cf) => AnyCanFrame::Can(cf),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error: {}", e);
                        continue;
                    }
                };
                if tx.blocking_send(any_frame).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let call_dur = read_start.elapsed();
                if call_dur > Duration::from_millis(5) {
                    tracing::warn!(
                        "CAN read_frame() WouldBlock took {:.1}ms",
                        call_dur.as_secs_f64() * 1000.0
                    );
                }
                would_blocks += 1;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                timed_outs += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!("CAN read error (ignoring): {}", e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Writer thread for CAN FD sockets.
fn can_writer_thread_fd(
    socket: Arc<socketcan::CanFdSocket>,
    mut rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let write_count_ref = &write_count;
    let write_fn = |frame: &AnyCanFrame| {
        for attempt in 0..4u32 {
            let result = match frame {
                AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                    Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error on write: {}", e);
                        return;
                    }
                },
                AnyCanFrame::CanFd(f) => match socketcan::CanFdFrame::try_from(f) {
                    Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                    Err(e) => {
                        tracing::warn!("CAN FD frame conversion error on write: {}", e);
                        return;
                    }
                },
            };
            if result.is_ok() {
                write_count_ref.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let err = result.unwrap_err();
            if err.raw_os_error() == Some(105) && attempt < 3 {
                std::thread::sleep(Duration::from_micros(100));
                continue;
            }
            tracing::warn!("CAN write error (dropping frame): {}", err);
            return;
        }
    };

    while let Some(frame) = rx.blocking_recv() {
        write_fn(&frame);
    }
}

/// Writer thread for standard CAN sockets.
fn can_writer_thread_std(
    socket: Arc<socketcan::CanSocket>,
    mut rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    write_count: Arc<AtomicU64>,
) {
    let write_count_ref = &write_count;
    let write_fn = |frame: &AnyCanFrame| {
        for attempt in 0..4u32 {
            let result = match frame {
                AnyCanFrame::Can(f) => match socketcan::CanFrame::try_from(f) {
                    Ok(sf) => socket.write_frame(&sf).map(|_| ()),
                    Err(e) => {
                        tracing::warn!("CAN frame conversion error on write: {}", e);
                        return;
                    }
                },
                AnyCanFrame::CanFd(_) => {
                    tracing::warn!("CAN FD frame on standard CAN socket, dropping");
                    return;
                }
            };
            if result.is_ok() {
                write_count_ref.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let err = result.unwrap_err();
            if err.raw_os_error() == Some(105) && attempt < 3 {
                std::thread::sleep(Duration::from_micros(100));
                continue;
            }
            tracing::warn!("CAN write error (dropping frame): {}", err);
            return;
        }
    };

    while let Some(frame) = rx.blocking_recv() {
        write_fn(&frame);
    }
}

impl CanServer {
    /// Create a new CAN bridge server.
    pub async fn new(
        interface: &str,
        enable_fd: bool,
        identity_path: Option<&str>,
    ) -> Result<Self> {
        let (can_read_tx, can_read_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(16);
        let (can_write_tx, can_write_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(1);
        let write_count = Arc::new(AtomicU64::new(0));

        if enable_fd {
            let socket = socketcan::CanFdSocket::open(interface).map_err(|e| {
                anyhow::anyhow!("Failed to open CAN FD socket on {}: {}", interface, e)
            })?;
            socket
                .set_read_timeout(Duration::from_millis(10))
                .map_err(|e| anyhow::anyhow!("Failed to set read timeout: {}", e))?;
            tracing::info!("CAN FD socket opened on {}", interface);
            let socket = Arc::new(socket);

            let socket_reader = Arc::clone(&socket);
            let wc_reader = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-read-{}", interface))
                .spawn(move || can_reader_thread_fd(socket_reader, can_read_tx, wc_reader))?;

            let socket_writer = Arc::clone(&socket);
            let wc_writer = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-write-{}", interface))
                .spawn(move || can_writer_thread_fd(socket_writer, can_write_rx, wc_writer))?;
        } else {
            let socket = socketcan::CanSocket::open(interface).map_err(|e| {
                anyhow::anyhow!("Failed to open CAN socket on {}: {}", interface, e)
            })?;
            socket
                .set_read_timeout(Duration::from_millis(10))
                .map_err(|e| anyhow::anyhow!("Failed to set read timeout: {}", e))?;
            tracing::info!("CAN socket opened on {}", interface);
            let socket = Arc::new(socket);

            let socket_reader = Arc::clone(&socket);
            let wc_reader = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-read-{}", interface))
                .spawn(move || can_reader_thread_std(socket_reader, can_read_tx, wc_reader))?;

            let socket_writer = Arc::clone(&socket);
            let wc_writer = Arc::clone(&write_count);
            std::thread::Builder::new()
                .name(format!("can-write-{}", interface))
                .spawn(move || can_writer_thread_std(socket_writer, can_write_rx, wc_writer))?;
        }

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
async fn handle_connection(
    conn: IrohConnection,
    mut can_read_rx: tokio::sync::mpsc::Receiver<AnyCanFrame>,
    can_write_tx: tokio::sync::mpsc::Sender<AnyCanFrame>,
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

    let conn_cancel = conn.cancellation_token();

    // CAN → Network task (with batching)
    let can_to_net_cancel = cancel.clone();
    let conn_cancel_clone = conn_cancel.clone();
    let can_to_net = tokio::spawn(async move {
        let mut batch_buf = Vec::with_capacity(1024);

        loop {
            batch_buf.clear();

            let first = tokio::select! {
                _ = can_to_net_cancel.cancelled() => break,
                _ = conn_cancel_clone.cancelled() => break,
                frame = can_read_rx.recv() => match frame {
                    Some(f) => f,
                    None => break,
                }
            };

            batch_buf.extend_from_slice(&wire::encode(&first));

            // Greedily collect more ready frames (up to 8 total)
            for _ in 1..8 {
                match can_read_rx.try_recv() {
                    Ok(frame) => batch_buf.extend_from_slice(&wire::encode(&frame)),
                    Err(_) => break,
                }
            }

            if send.write_all(&batch_buf).await.is_err() {
                break;
            }
            if send.flush().await.is_err() {
                break;
            }
        }

        can_read_rx
    });

    // Network → CAN
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
                                            if can_write_tx.send(frame).await.is_err() {
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
                                Ok(_) => break,
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

    cancel.cancel();
    let can_read_rx = match can_to_net.await {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!("CAN-to-net task panicked: {}", e);
            return (
                Err(anyhow::anyhow!("CAN-to-net task panicked: {}", e)),
                tokio::sync::mpsc::channel(1).1,
            );
        }
    };

    (result, can_read_rx)
}
