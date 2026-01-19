//! serialport-compatible interface to remote serial ports over P2P.
//!
//! This module provides a `serialport` crate compatible API for connecting
//! to remote serial ports over iroh P2P or MoQ relay.
//!
//! # Example
//!
//! ```no_run
//! use xoq::serialport;
//! use std::io::{Read, Write};
//!
//! let mut port = serialport::new("server-endpoint-id").open()?;
//! port.write_all(b"AT\r\n")?;
//! let mut buf = [0u8; 100];
//! let n = port.read(&mut buf)?;
//! # Ok::<(), anyhow::Error>(())
//! ```

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::iroh::{IrohClientBuilder, IrohConnection};

/// A client that connects to a remote serial port over iroh P2P
pub struct Client {
    send: Arc<Mutex<iroh::endpoint::SendStream>>,
    recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
    _conn: IrohConnection,
}

impl Client {
    /// Connect to a remote serial bridge server
    pub async fn connect(server_id: &str) -> Result<Self> {
        tracing::debug!("Connecting to server: {}", server_id);
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

    /// Write data to the remote serial port
    pub async fn write(&self, data: &[u8]) -> Result<()> {
        let mut send = self.send.lock().await;
        send.write_all(data).await?;
        send.flush().await?;
        Ok(())
    }

    /// Write a string to the remote serial port
    pub async fn write_str(&self, data: &str) -> Result<()> {
        self.write(data.as_bytes()).await
    }

    /// Read data from the remote serial port
    pub async fn read(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        let mut recv = self.recv.lock().await;
        Ok(recv.read(buf).await?)
    }

    /// Read a string from the remote serial port
    pub async fn read_string(&self) -> Result<Option<String>> {
        let mut buf = vec![0u8; 4096];
        if let Some(n) = self.read(&mut buf).await? {
            return Ok(Some(String::from_utf8_lossy(&buf[..n]).to_string()));
        }
        Ok(None)
    }

    /// Run an interactive bridge to local stdin/stdout
    pub async fn run_interactive(&self) -> Result<()> {
        use std::io::{Read, Write};

        let recv = self.recv.clone();
        let send = self.send.clone();

        // Spawn task: network -> stdout
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1024];
            loop {
                let n = {
                    let mut r = recv.lock().await;
                    match r.read(&mut buf).await {
                        Ok(Some(n)) if n > 0 => {
                            tracing::debug!("Received {} bytes from network", n);
                            n
                        }
                        Ok(Some(_)) => {
                            tracing::debug!("Received 0 bytes (EOF)");
                            break;
                        }
                        Ok(None) => {
                            tracing::debug!("Stream closed");
                            break;
                        }
                        Err(e) => {
                            tracing::debug!("Network read error: {}", e);
                            break;
                        }
                    }
                };
                let _ = std::io::stdout().write_all(&buf[..n]);
                let _ = std::io::stdout().flush();
            }
        });

        // Main: stdin -> network
        loop {
            let result = tokio::task::spawn_blocking(|| {
                let mut buf = [0u8; 256];
                match std::io::stdin().read(&mut buf) {
                    Ok(n) if n > 0 => Some(buf[..n].to_vec()),
                    _ => None,
                }
            })
            .await?;

            match result {
                Some(data) => {
                    tracing::debug!(
                        "Sending {} bytes to network: {:?}",
                        data.len(),
                        String::from_utf8_lossy(&data)
                    );
                    let mut s = send.lock().await;
                    if let Err(e) = s.write_all(&data).await {
                        tracing::debug!("Network write error: {}", e);
                        break;
                    }
                    if let Err(e) = s.flush().await {
                        tracing::debug!("Network flush error: {}", e);
                        break;
                    }
                    tracing::debug!("Sent successfully");
                }
                None => break,
            }
        }

        Ok(())
    }
}

/// Transport type for the serial port connection.
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

/// Builder for creating a remote serial port connection.
///
/// Mimics the `serialport::new()` API for drop-in compatibility.
///
/// # Example
///
/// ```no_run
/// use xoq::serialport;
/// use std::time::Duration;
///
/// // Simple iroh P2P connection (default)
/// let port = serialport::new("server-endpoint-id").open()?;
///
/// // With timeout
/// let port = serialport::new("server-endpoint-id")
///     .timeout(Duration::from_millis(500))
///     .open()?;
///
/// // With MoQ relay
/// let port = serialport::new("my-channel")
///     .with_moq("https://relay.example.com")
///     .token("jwt-token")
///     .open()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct SerialPortBuilder {
    port_name: String,
    timeout: Duration,
    transport: Transport,
}

impl SerialPortBuilder {
    /// Create a new serial port builder.
    pub fn new(port: &str) -> Self {
        Self {
            port_name: port.to_string(),
            timeout: Duration::from_secs(1),
            transport: Transport::default(),
        }
    }

    /// Set the read/write timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
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

    /// Open the connection to the remote serial port.
    pub fn open(self) -> Result<RemoteSerialPort> {
        let runtime = tokio::runtime::Runtime::new()?;

        let client = match self.transport {
            Transport::Iroh { alpn } => runtime.block_on(async {
                let mut builder = IrohClientBuilder::new();
                if let Some(alpn) = alpn {
                    builder = builder.alpn(&alpn);
                }
                let conn = builder.connect_str(&self.port_name).await?;
                let stream = conn.open_stream().await?;
                Ok::<_, anyhow::Error>(ClientInner::Iroh {
                    stream: Arc::new(Mutex::new(stream)),
                    _conn: conn,
                })
            })?,
            Transport::Moq { relay, token } => runtime.block_on(async {
                let mut builder = crate::moq::MoqBuilder::new()
                    .relay(&relay)
                    .path(&self.port_name);
                if let Some(t) = token {
                    builder = builder.token(&t);
                }
                let conn = builder.connect_duplex().await?;
                Ok::<_, anyhow::Error>(ClientInner::Moq {
                    conn: Arc::new(tokio::sync::Mutex::new(conn)),
                })
            })?,
        };

        Ok(RemoteSerialPort {
            client,
            runtime,
            port_name: self.port_name,
            timeout: self.timeout,
            buffer: Vec::new(),
        })
    }
}

/// Internal client representation supporting multiple transports.
enum ClientInner {
    Iroh {
        stream: Arc<Mutex<crate::iroh::IrohStream>>,
        _conn: IrohConnection,
    },
    Moq {
        conn: Arc<tokio::sync::Mutex<crate::moq::MoqConnection>>,
    },
}

/// Create a new remote serial port builder.
///
/// This function mimics `serialport::new()` for drop-in compatibility.
///
/// # Example
///
/// ```no_run
/// use xoq::serialport;
///
/// // Drop-in replacement for serialport crate
/// let port = serialport::new("server-endpoint-id").open()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn new(port: &str) -> SerialPortBuilder {
    SerialPortBuilder::new(port)
}

/// A `serialport`-compatible interface to a remote serial port.
///
/// This struct provides a blocking API that mimics the `serialport` crate,
/// implementing `std::io::Read` and `std::io::Write` traits for seamless
/// integration with existing code. Supports both iroh P2P and MoQ relay transports.
///
/// # Example
///
/// ```no_run
/// use xoq::serialport;
/// use std::io::{BufRead, BufReader, Write};
///
/// // Iroh P2P (default)
/// let mut port = serialport::new("server-endpoint-id").open()?;
///
/// // Or with MoQ relay
/// let mut port = serialport::new("my-channel")
///     .with_moq("https://relay.example.com")
///     .open()?;
///
/// port.write_all(b"AT\r\n")?;
/// let mut reader = BufReader::new(port);
/// let mut line = String::new();
/// reader.read_line(&mut line)?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct RemoteSerialPort {
    client: ClientInner,
    runtime: tokio::runtime::Runtime,
    port_name: String,
    timeout: Duration,
    buffer: Vec<u8>,
}

impl RemoteSerialPort {
    /// Open a connection to a remote serial port via iroh P2P.
    ///
    /// Prefer using `xoq::serialport::new(port).open()` for more options.
    pub fn open(port: &str) -> Result<Self> {
        new(port).open()
    }

    /// Get the port name (server endpoint ID or MoQ path).
    pub fn name(&self) -> Option<String> {
        Some(self.port_name.clone())
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

    /// Get the number of bytes available to read.
    pub fn bytes_to_read(&self) -> Result<u32> {
        Ok(self.buffer.len() as u32)
    }

    /// Get the number of bytes waiting to be written (always 0 for network).
    pub fn bytes_to_write(&self) -> Result<u32> {
        Ok(0)
    }

    /// Clear the input buffer.
    pub fn clear_input(&mut self) -> Result<()> {
        self.buffer.clear();
        Ok(())
    }

    /// Clear the output buffer (no-op for network).
    pub fn clear_output(&mut self) -> Result<()> {
        Ok(())
    }

    /// Clear all buffers.
    pub fn clear_all(&mut self) -> Result<()> {
        self.buffer.clear();
        Ok(())
    }

    /// Write bytes to the remote serial port.
    pub fn write_bytes(&mut self, data: &[u8]) -> Result<usize> {
        self.runtime.block_on(async {
            match &self.client {
                ClientInner::Iroh { stream, .. } => {
                    let mut s = stream.lock().await;
                    s.write(data).await?;
                }
                ClientInner::Moq { conn } => {
                    let mut c = conn.lock().await;
                    let mut track = c.create_track("serial");
                    track.write(data.to_vec());
                }
            }
            Ok::<_, anyhow::Error>(())
        })?;
        Ok(data.len())
    }

    /// Read bytes from the remote serial port.
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<usize> {
        // First drain from buffer
        if !self.buffer.is_empty() {
            let take = std::cmp::min(buf.len(), self.buffer.len());
            buf[..take].copy_from_slice(&self.buffer[..take]);
            self.buffer.drain(..take);
            return Ok(take);
        }

        // Read from network with timeout
        let timeout = self.timeout;
        let result = self.runtime.block_on(async {
            tokio::time::timeout(timeout, async {
                match &self.client {
                    ClientInner::Iroh { stream, .. } => {
                        let mut s = stream.lock().await;
                        s.read(buf).await
                    }
                    ClientInner::Moq { conn } => {
                        let mut c = conn.lock().await;
                        if let Some(reader) = c.subscribe_track("serial").await? {
                            let mut reader = reader;
                            if let Some(data) = reader.read().await? {
                                let n = std::cmp::min(data.len(), buf.len());
                                buf[..n].copy_from_slice(&data[..n]);
                                Ok(Some(n))
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
            Ok(Ok(Some(n))) => Ok(n),
            Ok(Ok(None)) => Ok(0),
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(0), // Timeout
        }
    }

    /// Read until a specific byte is found.
    pub fn read_until(&mut self, byte: u8) -> Result<Vec<u8>> {
        let mut result = Vec::new();

        // Check buffer first
        if let Some(pos) = self.buffer.iter().position(|&b| b == byte) {
            result.extend(self.buffer.drain(..=pos));
            return Ok(result);
        }
        result.append(&mut self.buffer);

        // Keep reading until we find the byte
        let mut temp = [0u8; 256];
        loop {
            let n = self.read_bytes(&mut temp)?;
            if n == 0 {
                break;
            }
            if let Some(pos) = temp[..n].iter().position(|&b| b == byte) {
                result.extend_from_slice(&temp[..=pos]);
                self.buffer.extend_from_slice(&temp[pos + 1..n]);
                break;
            }
            result.extend_from_slice(&temp[..n]);
        }

        Ok(result)
    }

    /// Read a line (until newline).
    pub fn read_line(&mut self) -> Result<String> {
        let bytes = self.read_until(b'\n')?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

impl std::io::Read for RemoteSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_bytes(buf).map_err(std::io::Error::other)
    }
}

impl std::io::Write for RemoteSerialPort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_bytes(buf).map_err(std::io::Error::other)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl serialport::SerialPort for RemoteSerialPort {
    fn name(&self) -> Option<String> {
        Some(self.port_name.clone())
    }

    fn baud_rate(&self) -> serialport::Result<u32> {
        // Remote port - baud rate is configured on the server side
        Ok(1_000_000)
    }

    fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
        Ok(serialport::DataBits::Eight)
    }

    fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
        Ok(serialport::FlowControl::None)
    }

    fn parity(&self) -> serialport::Result<serialport::Parity> {
        Ok(serialport::Parity::None)
    }

    fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
        Ok(serialport::StopBits::One)
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
        // Baud rate is configured on the server side
        Ok(())
    }

    fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
        Ok(())
    }

    fn set_flow_control(&mut self, _: serialport::FlowControl) -> serialport::Result<()> {
        Ok(())
    }

    fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
        Ok(())
    }

    fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
        Ok(())
    }

    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.timeout = timeout;
        Ok(())
    }

    fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
        Ok(())
    }

    fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
        Ok(())
    }

    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }

    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(true)
    }

    fn bytes_to_read(&self) -> serialport::Result<u32> {
        Ok(self.buffer.len() as u32)
    }

    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(0)
    }

    fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
        Ok(())
    }

    fn try_clone(&self) -> serialport::Result<Box<dyn serialport::SerialPort>> {
        Err(serialport::Error::new(
            serialport::ErrorKind::Io(std::io::ErrorKind::Unsupported),
            "Clone not supported for remote serial ports",
        ))
    }

    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }

    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}
