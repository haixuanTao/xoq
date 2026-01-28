//! CAN bridge server - bridges local CAN interface to remote clients over iroh P2P.
//!
//! This module provides a server that exposes a local CAN interface over the network,
//! allowing remote clients to send and receive CAN frames.

use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::can::{wire, AnyCanFrame, CanSocket};
use crate::iroh::{IrohConnection, IrohServerBuilder};

/// A server that bridges a local CAN interface to remote clients over iroh P2P
pub struct CanServer {
    server_id: String,
    /// Sender for frames to write to CAN interface
    can_write_tx: std::sync::mpsc::Sender<AnyCanFrame>,
    /// Receiver for frames read from CAN interface
    can_read_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<AnyCanFrame>>>,
    endpoint: Arc<crate::iroh::IrohServer>,
}

impl CanServer {
    /// Create a new CAN bridge server
    ///
    /// Args:
    ///     interface: CAN interface name (e.g., "can0", "vcan0")
    ///     enable_fd: Enable CAN FD support
    ///     identity_path: Optional path to save/load server identity
    pub async fn new(
        interface: &str,
        enable_fd: bool,
        identity_path: Option<&str>,
    ) -> Result<Self> {
        // Open CAN socket and split
        let socket = if enable_fd {
            CanSocket::open_fd(interface)?
        } else {
            CanSocket::open_simple(interface)?
        };
        let (mut reader, mut writer) = socket.split();

        // Create channels for CAN I/O
        // tokio channel for CAN->network (async receiver)
        let (can_read_tx, can_read_rx) = tokio::sync::mpsc::channel::<AnyCanFrame>(32);
        // std channel for network->CAN (blocking writer thread)
        let (can_write_tx, can_write_rx) = std::sync::mpsc::channel::<AnyCanFrame>();

        // Spawn dedicated reader thread that continuously polls CAN
        let read_tx = can_read_tx;
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                loop {
                    match reader.read_frame().await {
                        Ok(None) => {
                            // Timeout - no frame available, yield to prevent busy spin
                            tokio::task::yield_now().await;
                        }
                        Ok(Some(frame)) => {
                            tracing::debug!("CAN read frame: ID={:x}", frame.id());
                            if read_tx.send(frame).await.is_err() {
                                break; // Channel closed
                            }
                        }
                        Err(e) => {
                            tracing::error!("CAN read error: {}", e);
                            break;
                        }
                    }
                }
            });
        });

        // Spawn dedicated writer thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                while let Ok(frame) = can_write_rx.recv() {
                    if let Err(e) = writer.write_any_frame(frame).await {
                        tracing::error!("CAN write error: {}", e);
                        break;
                    }
                    tracing::debug!("Wrote frame to CAN");
                }
            });
        });

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
            can_read_rx: Arc::new(Mutex::new(can_read_rx)),
            endpoint: Arc::new(server),
        })
    }

    /// Get the server's endpoint ID (share this with clients)
    pub fn id(&self) -> &str {
        &self.server_id
    }

    /// Run the bridge server (blocks forever, handling connections)
    pub async fn run(&self) -> Result<()> {
        tracing::info!("CAN bridge server running. ID: {}", self.server_id);

        loop {
            // Accept connection
            let conn = match self.endpoint.accept().await? {
                Some(c) => c,
                None => continue,
            };

            tracing::info!("Client connected: {}", conn.remote_id());

            // Handle this connection
            if let Err(e) = self.handle_connection(conn).await {
                tracing::error!("Connection error: {}", e);
            }

            tracing::info!("Client disconnected");
        }
    }

    /// Run the bridge server for a single connection, then return
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

            if let Err(e) = self.handle_connection(conn).await {
                tracing::error!("Connection error: {}", e);
            }

            tracing::info!("Client disconnected");
            return Ok(());
        }
    }

    async fn handle_connection(&self, conn: IrohConnection) -> Result<()> {
        tracing::debug!("Waiting for client to open stream...");
        let stream = conn
            .accept_stream()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to accept stream: {}", e))?;
        tracing::debug!("Stream accepted, starting bridge");

        // Split the stream so reads and writes don't block each other
        let (mut send, mut recv) = stream.split();

        let can_read_rx = self.can_read_rx.clone();
        let can_write_tx = self.can_write_tx.clone();

        // Drain any stale frames from the channel before starting
        // This prevents old data from confusing a reconnecting client
        {
            let mut rx = can_read_rx.lock().await;
            let mut drained = 0;
            while rx.try_recv().is_ok() {
                drained += 1;
            }
            if drained > 0 {
                tracing::info!("Drained {} stale CAN frames from buffer", drained);
            }
        }

        // Use connection's cancellation token for graceful shutdown
        let cancel_token = conn.cancellation_token();

        // Spawn task: CAN -> network (event-driven via channel)
        let cancel_clone = cancel_token.clone();
        let can_to_net = tokio::spawn(async move {
            tracing::debug!("CAN->Network bridge task started");
            let mut rx = can_read_rx.lock().await;
            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => {
                        tracing::debug!("CAN->Network task cancelled");
                        break;
                    }
                    frame = rx.recv() => {
                        match frame {
                            Some(frame) => {
                                let encoded = wire::encode(&frame);
                                tracing::debug!("CAN -> Network: ID={:x}, {} bytes", frame.id(), encoded.len());
                                if let Err(e) = send.write_all(&encoded).await {
                                    tracing::debug!("Network write error: {}", e);
                                    break;
                                }
                                if let Err(e) = send.flush().await {
                                    tracing::debug!("Network flush error: {}", e);
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
            tracing::debug!("CAN->Network bridge task ended");
        });

        // Main task: network -> CAN
        let mut buf = vec![0u8; 1024];
        let mut pending = Vec::new();
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    pending.extend_from_slice(&buf[..n]);

                    // Process all complete frames in the buffer
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
                Ok(Some(_)) => {
                    // 0 bytes from network - keep waiting
                    continue;
                }
                Ok(None) => {
                    tracing::info!("Client disconnected (stream closed)");
                    break;
                }
                Err(e) => {
                    tracing::error!("Network read error: {}", e);
                    break;
                }
            }
        }

        // Signal graceful shutdown
        cancel_token.cancel();
        // Wait for task to finish and release the lock
        let _ = can_to_net.await;
        Ok(())
    }
}
