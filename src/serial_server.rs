//! Serial server - bridges local serial port to remote clients over iroh P2P.

use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::iroh::{IrohConnection, IrohServerBuilder};
use crate::serial::SerialPort;

/// A server that bridges a local serial port to remote clients over iroh P2P
pub struct Server {
    server_id: String,
    /// Sender for data to write to serial port
    serial_write_tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// Receiver for data read from serial port
    serial_read_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>>>,
    endpoint: Arc<crate::iroh::IrohServer>,
}

impl Server {
    /// Create a new serial bridge server
    ///
    /// Args:
    ///     port: Serial port name (e.g., "/dev/ttyUSB0" or "COM3")
    ///     baud_rate: Baud rate (e.g., 115200)
    ///     identity_path: Optional path to save/load server identity
    pub async fn new(port: &str, baud_rate: u32, identity_path: Option<&str>) -> Result<Self> {
        // Open serial port and split
        let serial = SerialPort::open_simple(port, baud_rate)?;
        let (mut reader, mut writer) = serial.split();

        // Create channels for serial I/O
        // tokio channel for serial->network (async receiver)
        let (serial_read_tx, serial_read_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        // std channel for network->serial (blocking writer thread)
        let (serial_write_tx, serial_write_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        // Spawn dedicated reader thread that continuously polls serial
        let read_tx = serial_read_tx;
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            tracing::debug!("Serial read {} bytes", n);
                            if read_tx.send(buf[..n].to_vec()).await.is_err() {
                                break; // Channel closed
                            }
                        }
                        Ok(_) => {
                            // 0 bytes - timeout, keep polling
                        }
                        Err(e) => {
                            tracing::error!("Serial read error: {}", e);
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
                while let Ok(data) = serial_write_rx.recv() {
                    if let Err(e) = writer.write_all(&data).await {
                        tracing::error!("Serial write error: {}", e);
                        break;
                    }
                    tracing::debug!("Wrote {} bytes to serial", data.len());
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
            serial_write_tx,
            serial_read_rx: Arc::new(Mutex::new(serial_read_rx)),
            endpoint: Arc::new(server),
        })
    }

    /// Get the server's endpoint ID (share this with clients)
    pub fn id(&self) -> &str {
        &self.server_id
    }

    /// Run the bridge server (blocks forever, handling connections)
    pub async fn run(&self) -> Result<()> {
        tracing::info!("Serial bridge server running. ID: {}", self.server_id);

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
            "Serial bridge server waiting for connection. ID: {}",
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

        let serial_read_rx = self.serial_read_rx.clone();
        let serial_write_tx = self.serial_write_tx.clone();

        // Spawn task: serial -> network (event-driven via channel)
        let serial_to_net = tokio::spawn(async move {
            tracing::debug!("Serial->Network bridge task started");
            let mut rx = serial_read_rx.lock().await;
            while let Some(data) = rx.recv().await {
                tracing::debug!("Serial -> Network: {} bytes", data.len());
                if let Err(e) = send.write_all(&data).await {
                    tracing::debug!("Network write error: {}", e);
                    break;
                }
                if let Err(e) = send.flush().await {
                    tracing::debug!("Network flush error: {}", e);
                    break;
                }
            }
            tracing::debug!("Serial->Network bridge task ended");
        });

        // Main task: network -> serial
        let mut buf = vec![0u8; 1024];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    tracing::debug!(
                        "Network -> Serial: {} bytes: {:?}",
                        n,
                        String::from_utf8_lossy(&buf[..n])
                    );
                    if serial_write_tx.send(buf[..n].to_vec()).is_err() {
                        tracing::error!("Serial writer thread died");
                        break;
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

        serial_to_net.abort();
        Ok(())
    }
}
