//! socketcan-compatible interface to remote CAN sockets over P2P.
//!
//! This module provides a `socketcan` crate compatible API for connecting
//! to remote CAN interfaces over iroh P2P or MoQ relay.
//!
//! # Example
//!
//! ```no_run
//! use xoq::socketcan;
//!
//! let mut socket = socketcan::new("server-endpoint-id").open()?;
//! let frame = socketcan::CanFrame::new(0x123, &[1, 2, 3, 4])?;
//! socket.write_frame(&frame)?;
//! let received = socket.read_frame()?;
//! println!("Received: ID={:x} Data={:?}", received.id(), received.data());
//! # Ok::<(), anyhow::Error>(())
//! ```

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::can_types::wire;
use crate::iroh::{IrohClientBuilder, IrohConnection};

// Re-export frame types for convenience
pub use crate::can_types::{CanFdFlags, CanInterfaceInfo};

/// A client that connects to a remote CAN interface over iroh P2P
pub struct CanClient {
    send: Arc<Mutex<iroh::endpoint::SendStream>>,
    recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
    _conn: IrohConnection,
}

impl CanClient {
    /// Connect to a remote CAN bridge server
    pub async fn connect(server_id: &str) -> Result<Self> {
        tracing::debug!("Connecting to CAN server: {}", server_id);
        let conn = IrohClientBuilder::new()
            .connect_str(server_id)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to server: {}", e))?;
        tracing::debug!("Connected to server, opening stream...");
        let stream = conn
            .open_stream()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to open stream: {}", e))?;
        tracing::debug!("Stream opened successfully");

        // Split stream so reads and writes don't block each other
        let (send, recv) = stream.split();

        Ok(Self {
            send: Arc::new(Mutex::new(send)),
            recv: Arc::new(Mutex::new(recv)),
            _conn: conn,
        })
    }

    /// Write a CAN frame to the remote interface
    pub async fn write_frame(&self, frame: &CanFrame) -> Result<()> {
        let encoded = wire::encode(&AnyCanFrame::Can(frame.clone()));
        let mut send = self.send.lock().await;
        send.write_all(&encoded).await?;
        send.flush().await?;
        Ok(())
    }

    /// Write a CAN FD frame to the remote interface
    pub async fn write_fd_frame(&self, frame: &CanFdFrame) -> Result<()> {
        let encoded = wire::encode(&AnyCanFrame::CanFd(frame.clone()));
        let mut send = self.send.lock().await;
        send.write_all(&encoded).await?;
        send.flush().await?;
        Ok(())
    }

    /// Read a CAN frame from the remote interface
    pub async fn read_frame(&self) -> Result<Option<AnyCanFrame>> {
        let mut header = [0u8; 6];
        let mut recv = self.recv.lock().await;

        // Read header
        let mut read = 0;
        while read < 6 {
            match recv.read(&mut header[read..]).await? {
                Some(n) if n > 0 => read += n,
                Some(_) => continue,
                None => return Ok(None),
            }
        }

        // Get full frame size and read remaining data
        let frame_size = wire::encoded_size(&header)?;
        let mut data = vec![0u8; frame_size];
        data[..6].copy_from_slice(&header);

        let mut read = 6;
        while read < frame_size {
            match recv.read(&mut data[read..]).await? {
                Some(n) if n > 0 => read += n,
                Some(_) => continue,
                None => return Ok(None),
            }
        }

        let (frame, _) = wire::decode(&data)?;
        Ok(Some(frame))
    }
}

/// Transport type for the CAN socket connection.
#[derive(Clone)]
pub enum Transport {
    /// Iroh P2P connection (default)
    Iroh {
        /// Custom ALPN protocol
        alpn: Option<Vec<u8>>,
    },
    /// MoQ relay connection
    Moq {
        /// Relay URL
        relay: String,
        /// Authentication token
        token: Option<String>,
    },
}

impl Default for Transport {
    fn default() -> Self {
        Transport::Iroh { alpn: None }
    }
}

/// Builder for creating a remote CAN socket connection.
///
/// Mimics a builder pattern similar to `socketcan` for drop-in compatibility.
///
/// # Example
///
/// ```no_run
/// use xoq::socketcan;
/// use std::time::Duration;
///
/// // Simple iroh P2P connection (default)
/// let socket = socketcan::new("server-endpoint-id").open()?;
///
/// // With timeout
/// let socket = socketcan::new("server-endpoint-id")
///     .timeout(Duration::from_millis(500))
///     .open()?;
///
/// // With CAN FD enabled
/// let socket = socketcan::new("server-endpoint-id")
///     .enable_fd(true)
///     .open()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct CanSocketBuilder {
    server_id: String,
    timeout: Duration,
    transport: Transport,
    fd_enabled: bool,
}

impl CanSocketBuilder {
    /// Create a new CAN socket builder.
    pub fn new(server_id: &str) -> Self {
        Self {
            server_id: server_id.to_string(),
            timeout: Duration::from_secs(1),
            transport: Transport::default(),
            fd_enabled: false,
        }
    }

    /// Set the read/write timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Enable CAN FD support.
    pub fn enable_fd(mut self, enabled: bool) -> Self {
        self.fd_enabled = enabled;
        self
    }

    /// Use iroh P2P transport (default).
    pub fn with_iroh(mut self) -> Self {
        self.transport = Transport::Iroh { alpn: None };
        self
    }

    /// Set custom ALPN for iroh connection.
    pub fn alpn(mut self, alpn: &[u8]) -> Self {
        if let Transport::Iroh { alpn: ref mut a } = self.transport {
            *a = Some(alpn.to_vec());
        }
        self
    }

    /// Use MoQ relay transport.
    pub fn with_moq(mut self, relay: &str) -> Self {
        self.transport = Transport::Moq {
            relay: relay.to_string(),
            token: None,
        };
        self
    }

    /// Set authentication token (for MoQ).
    pub fn token(mut self, token: &str) -> Self {
        if let Transport::Moq {
            token: ref mut t, ..
        } = self.transport
        {
            *t = Some(token.to_string());
        }
        self
    }

    /// Open the connection to the remote CAN interface.
    pub fn open(self) -> Result<RemoteCanSocket> {
        let runtime = tokio::runtime::Runtime::new()?;

        let client = match self.transport {
            Transport::Iroh { alpn } => runtime.block_on(async {
                let mut builder = IrohClientBuilder::new();
                if let Some(alpn) = alpn {
                    builder = builder.alpn(&alpn);
                }
                let conn = builder.connect_str(&self.server_id).await?;
                let stream = conn.open_stream().await?;
                // Split for true full-duplex: reads and writes don't block each other
                let (send, recv) = stream.split();
                Ok::<_, anyhow::Error>(ClientInner::Iroh {
                    send: Arc::new(Mutex::new(send)),
                    recv: Arc::new(Mutex::new(recv)),
                    _conn: conn,
                })
            })?,
            Transport::Moq { relay, token } => runtime.block_on(async {
                let mut builder = crate::moq::MoqBuilder::new()
                    .relay(&relay)
                    .path(&self.server_id);
                if let Some(t) = token {
                    builder = builder.token(&t);
                }
                let conn = builder.connect_duplex().await?;
                Ok::<_, anyhow::Error>(ClientInner::Moq {
                    conn: Arc::new(tokio::sync::Mutex::new(conn)),
                })
            })?,
        };

        Ok(RemoteCanSocket {
            client,
            runtime,
            server_id: self.server_id,
            timeout: self.timeout,
            fd_enabled: self.fd_enabled,
            read_buffer: std::sync::Mutex::new(Vec::new()),
        })
    }
}

/// Internal client representation supporting multiple transports.
enum ClientInner {
    Iroh {
        // Split stream for true full-duplex: reads and writes don't block each other
        send: Arc<Mutex<iroh::endpoint::SendStream>>,
        recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
        _conn: IrohConnection,
    },
    Moq {
        conn: Arc<tokio::sync::Mutex<crate::moq::MoqConnection>>,
    },
}

/// Create a new remote CAN socket builder.
///
/// This function provides a familiar API for CAN socket creation.
///
/// # Example
///
/// ```no_run
/// use xoq::socketcan;
///
/// // Connect to remote CAN interface
/// let socket = socketcan::new("server-endpoint-id").open()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn new(server_id: &str) -> CanSocketBuilder {
    CanSocketBuilder::new(server_id)
}

/// A remote CAN socket that provides a blocking API.
///
/// This struct mimics the `socketcan` crate's interface for drop-in compatibility.
///
/// # Example
///
/// ```no_run
/// use xoq::socketcan;
///
/// // Iroh P2P (default)
/// let mut socket = socketcan::new("server-endpoint-id").open()?;
///
/// // Write a frame
/// let frame = socketcan::CanFrame::new(0x123, &[1, 2, 3])?;
/// socket.write_frame(&frame)?;
///
/// // Read a frame
/// if let Some(frame) = socket.read_frame()? {
///     println!("ID: {:x}, Data: {:?}", frame.id(), frame.data());
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct RemoteCanSocket {
    client: ClientInner,
    runtime: tokio::runtime::Runtime,
    server_id: String,
    timeout: Duration,
    fd_enabled: bool,
    // Use Mutex for interior mutability so trait methods with &self can update buffer
    read_buffer: std::sync::Mutex<Vec<u8>>,
}

impl RemoteCanSocket {
    /// Open a connection to a remote CAN interface via iroh P2P.
    ///
    /// Prefer using `xoq::socketcan::new(server_id).open()` for more options.
    pub fn open(server_id: &str) -> Result<Self> {
        new(server_id).open()
    }

    /// Get the server ID.
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Get the current timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Set the read/write timeout.
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.timeout = timeout;
        Ok(())
    }

    /// Check if CAN FD mode is enabled.
    pub fn is_fd_enabled(&self) -> bool {
        self.fd_enabled
    }

    /// Write a standard CAN frame to the remote interface.
    pub fn write_frame(&mut self, frame: &CanFrame) -> Result<()> {
        let encoded = wire::encode(&AnyCanFrame::Can(frame.clone()));
        self.write_raw(&encoded)
    }

    /// Write a CAN FD frame to the remote interface.
    pub fn write_fd_frame(&mut self, frame: &CanFdFrame) -> Result<()> {
        let encoded = wire::encode(&AnyCanFrame::CanFd(frame.clone()));
        self.write_raw(&encoded)
    }

    /// Write any CAN frame to the remote interface.
    pub fn write_any_frame(&mut self, frame: &AnyCanFrame) -> Result<()> {
        let encoded = wire::encode(frame);
        self.write_raw(&encoded)
    }

    fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        self.runtime.block_on(async {
            match &self.client {
                ClientInner::Iroh { send, .. } => {
                    let t0 = std::time::Instant::now();
                    let mut s = send.lock().await;
                    let lock_dur = t0.elapsed();
                    s.write_all(data).await?;
                    let write_dur = t0.elapsed();
                    s.flush().await?;
                    let total_dur = t0.elapsed();
                    if total_dur > std::time::Duration::from_millis(5) {
                        println!(
                            "[xoq] write_raw: lock={:.1}ms write={:.1}ms flush={:.1}ms total={:.1}ms",
                            lock_dur.as_secs_f64() * 1000.0,
                            (write_dur - lock_dur).as_secs_f64() * 1000.0,
                            (total_dur - write_dur).as_secs_f64() * 1000.0,
                            total_dur.as_secs_f64() * 1000.0,
                        );
                    }
                }
                ClientInner::Moq { conn } => {
                    let mut c = conn.lock().await;
                    let mut track = c.create_track("can");
                    track.write(data.to_vec());
                }
            }
            Ok::<_, anyhow::Error>(())
        })
    }

    /// Read a CAN frame from the remote interface.
    ///
    /// Returns `None` if the connection is closed or timeout occurs.
    pub fn read_frame(&mut self) -> Result<Option<AnyCanFrame>> {
        // First check if we have a complete frame in the buffer
        if let Some(frame) = self.try_decode_buffered()? {
            return Ok(Some(frame));
        }

        // Read more data with timeout
        let data = self.read_raw_with_timeout()?;
        if let Some(data) = data {
            {
                let mut buffer = self.read_buffer.lock().unwrap();
                buffer.extend_from_slice(&data);
            }
            self.try_decode_buffered()
        } else {
            Ok(None)
        }
    }

    fn read_raw_with_timeout(&mut self) -> Result<Option<Vec<u8>>> {
        let timeout = self.timeout;
        let result = self.runtime.block_on(async {
            // Flush any pending writes before reading — coalesces the entire
            // send batch (e.g. 8 motor commands) into a single QUIC flush.
            match &self.client {
                ClientInner::Iroh { send, .. } => {
                    let mut s = send.lock().await;
                    s.flush().await.ok();
                }
                ClientInner::Moq { .. } => {}
            }
            tokio::time::timeout(timeout, async {
                let mut temp_buf = vec![0u8; 128];
                match &self.client {
                    ClientInner::Iroh { recv, .. } => {
                        let mut r = recv.lock().await;
                        match r.read(&mut temp_buf).await? {
                            Some(n) => {
                                temp_buf.truncate(n);
                                Ok(Some(temp_buf))
                            }
                            None => Ok(None),
                        }
                    }
                    ClientInner::Moq { conn } => {
                        let mut c = conn.lock().await;
                        if let Some(reader) = c.subscribe_track("can").await? {
                            let mut reader = reader;
                            if let Some(data) = reader.read().await? {
                                Ok(Some(data.to_vec()))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    }
                }
            })
            .await
        });

        match result {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None), // Timeout
        }
    }

    fn try_decode_buffered(&self) -> Result<Option<AnyCanFrame>> {
        let mut buffer = self.read_buffer.lock().unwrap();

        if buffer.len() < 6 {
            return Ok(None);
        }

        // Validate data_len before proceeding - prevents buffer corruption from bad data
        let data_len = buffer[5] as usize;
        if data_len > 64 {
            // Invalid data length - corrupted packet, skip this byte and try to resync
            tracing::warn!(
                "Invalid CAN frame data_len={}, skipping byte to resync",
                data_len
            );
            buffer.drain(..1);
            return Ok(None);
        }

        match wire::encoded_size(&buffer) {
            Ok(frame_size) if buffer.len() >= frame_size => {
                match wire::decode(&buffer) {
                    Ok((frame, consumed)) => {
                        buffer.drain(..consumed);
                        Ok(Some(frame))
                    }
                    Err(e) => {
                        // Decode failed - likely corrupted data, skip one byte to try to resync
                        tracing::warn!("CAN frame decode error: {}, skipping byte to resync", e);
                        buffer.drain(..1);
                        Ok(None)
                    }
                }
            }
            Ok(_) => Ok(None), // Need more data
            Err(e) => {
                // Should not happen since we check len >= 6 above, but handle it safely
                tracing::warn!("CAN frame size error: {}, clearing buffer", e);
                buffer.clear();
                Err(e)
            }
        }
    }

    /// Read frames continuously, calling the provided callback for each frame.
    pub fn read_frames<F>(&mut self, mut callback: F) -> Result<()>
    where
        F: FnMut(AnyCanFrame) -> bool,
    {
        loop {
            match self.read_frame()? {
                Some(frame) => {
                    if !callback(frame) {
                        break;
                    }
                }
                None => continue, // Timeout, keep trying
            }
        }
        Ok(())
    }
}

// Re-export for public API
pub use crate::can_types::AnyCanFrame;
pub use crate::can_types::CanBusSocket;
pub use crate::can_types::CanFdFrame;
pub use crate::can_types::CanFrame;

impl CanBusSocket for RemoteCanSocket {
    fn is_open(&self) -> bool {
        // Remote socket is always considered open once connected
        true
    }

    fn write_raw(&self, can_id: u32, data: &[u8]) -> anyhow::Result<()> {
        let frame = CanFrame::new(can_id, data)?;
        // Need mutable access, but trait requires &self for Send+Sync compatibility.
        // We use interior mutability via the runtime's block_on.
        let encoded = wire::encode(&AnyCanFrame::Can(frame));
        self.runtime.block_on(async {
            match &self.client {
                ClientInner::Iroh { send, .. } => {
                    let mut s = send.lock().await;
                    s.write_all(&encoded).await?;
                    s.flush().await?; // Flush immediately to avoid buffering delays
                }
                ClientInner::Moq { conn } => {
                    let mut c = conn.lock().await;
                    let mut track = c.create_track("can");
                    track.write(encoded.to_vec());
                }
            }
            Ok::<_, anyhow::Error>(())
        })
    }

    fn read_raw(&self) -> anyhow::Result<Option<(u32, Vec<u8>)>> {
        // First check if we already have a complete frame in the buffer
        if let Some(frame) = self.try_decode_buffered()? {
            return Ok(Some((frame.id(), frame.data().to_vec())));
        }

        // Read more data with timeout
        let timeout = self.timeout;
        let result = self.runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                let mut temp_buf = vec![0u8; 128];
                match &self.client {
                    ClientInner::Iroh { recv, .. } => {
                        let mut r = recv.lock().await;
                        match r.read(&mut temp_buf).await? {
                            Some(n) => {
                                temp_buf.truncate(n);
                                Ok(Some(temp_buf))
                            }
                            None => Ok(None),
                        }
                    }
                    ClientInner::Moq { conn } => {
                        let mut c = conn.lock().await;
                        if let Some(reader) = c.subscribe_track("can").await? {
                            let mut reader = reader;
                            if let Some(data) = reader.read().await? {
                                Ok(Some(data.to_vec()))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    }
                }
            })
            .await
        });

        match result {
            Ok(Ok(Some(data))) => {
                // Add to buffer and try to decode
                {
                    let mut buffer = self.read_buffer.lock().unwrap();
                    buffer.extend_from_slice(&data);
                }
                // Try to decode a complete frame
                if let Some(frame) = self.try_decode_buffered()? {
                    Ok(Some((frame.id(), frame.data().to_vec())))
                } else {
                    Ok(None) // Partial frame, need more data
                }
            }
            Ok(Ok(None)) => Ok(None), // Stream closed
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(None), // Timeout
        }
    }

    fn is_data_available(&self, _timeout_us: u64) -> anyhow::Result<bool> {
        // Check if we have a complete frame buffered
        let buffer = self.read_buffer.lock().unwrap();
        if buffer.len() >= 6 {
            if let Ok(frame_size) = wire::encoded_size(&buffer) {
                if buffer.len() >= frame_size {
                    return Ok(true);
                }
            }
        }
        // Return false if no complete frame is buffered
        // The actual read_frame method will block/timeout as needed
        Ok(false)
    }

    fn set_recv_timeout(&mut self, timeout_us: u64) -> anyhow::Result<()> {
        self.timeout = std::time::Duration::from_micros(timeout_us);
        Ok(())
    }
}
