//! Python bindings for xoq
//!
//! Provides Python access to MoQ and iroh P2P communication.
//! All functions are blocking (synchronous).

// PyO3 macros generate code that triggers this lint incorrectly
#![allow(clippy::useless_conversion)]

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

// Use external crate with explicit path to avoid shadowing by our module name
use ::xoq as xoq_lib;

// Global tokio runtime for blocking calls
fn runtime() -> &'static tokio::runtime::Runtime {
    use std::sync::OnceLock;
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("Failed to create tokio runtime"))
}

// ============================================================================
// MoQ Bindings
// ============================================================================

/// A duplex MoQ connection that can publish and subscribe
#[pyclass]
struct MoqConnection {
    inner: Arc<Mutex<xoq_lib::MoqConnection>>,
}

#[pymethods]
impl MoqConnection {
    /// Connect as a duplex endpoint (can publish and subscribe)
    ///
    /// Args:
    ///     path: Path on the relay (default: "anon/xoq")
    ///     token: Optional JWT authentication token
    ///     relay: Relay URL (default: "https://cdn.moq.dev")
    #[new]
    #[pyo3(signature = (path=None, token=None, relay=None))]
    fn new(path: Option<&str>, token: Option<&str>, relay: Option<&str>) -> PyResult<Self> {
        let relay_url = relay.unwrap_or("https://cdn.moq.dev");
        let path = path.unwrap_or("anon/xoq");

        runtime().block_on(async {
            let mut builder = xoq_lib::MoqBuilder::new().relay(relay_url).path(path);
            if let Some(t) = token {
                builder = builder.token(t);
            }

            let conn = builder
                .connect_duplex()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(MoqConnection {
                inner: Arc::new(Mutex::new(conn)),
            })
        })
    }

    /// Create a track for publishing
    fn create_track(&self, name: &str) -> PyResult<MoqTrackWriter> {
        runtime().block_on(async {
            let mut conn = self.inner.lock().await;
            let track = conn.create_track(name);
            Ok(MoqTrackWriter {
                inner: Arc::new(Mutex::new(track)),
            })
        })
    }

    /// Wait for an announced broadcast and subscribe to a track
    fn subscribe_track(&self, track_name: &str) -> PyResult<Option<MoqTrackReader>> {
        runtime().block_on(async {
            let mut conn = self.inner.lock().await;
            let reader = conn
                .subscribe_track(track_name)
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(reader.map(|r| MoqTrackReader {
                inner: Arc::new(Mutex::new(r)),
            }))
        })
    }
}

/// A publish-only MoQ connection
#[pyclass]
struct MoqPublisher {
    inner: Arc<Mutex<xoq_lib::MoqPublisher>>,
}

#[pymethods]
impl MoqPublisher {
    /// Connect as publisher only
    ///
    /// Args:
    ///     path: Path on the relay (default: "anon/xoq")
    ///     token: Optional JWT authentication token
    ///     relay: Relay URL (default: "https://cdn.moq.dev")
    #[new]
    #[pyo3(signature = (path=None, token=None, relay=None))]
    fn new(path: Option<&str>, token: Option<&str>, relay: Option<&str>) -> PyResult<Self> {
        let relay_url = relay.unwrap_or("https://cdn.moq.dev");
        let path = path.unwrap_or("anon/xoq");

        runtime().block_on(async {
            let mut builder = xoq_lib::MoqBuilder::new().relay(relay_url).path(path);
            if let Some(t) = token {
                builder = builder.token(t);
            }

            let pub_conn = builder
                .connect_publisher()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(MoqPublisher {
                inner: Arc::new(Mutex::new(pub_conn)),
            })
        })
    }

    /// Create a track for publishing
    fn create_track(&self, name: &str) -> PyResult<MoqTrackWriter> {
        runtime().block_on(async {
            let mut pub_conn = self.inner.lock().await;
            let track = pub_conn.create_track(name);
            Ok(MoqTrackWriter {
                inner: Arc::new(Mutex::new(track)),
            })
        })
    }
}

/// A subscribe-only MoQ connection
#[pyclass]
struct MoqSubscriber {
    inner: Arc<Mutex<xoq_lib::MoqSubscriber>>,
}

#[pymethods]
impl MoqSubscriber {
    /// Connect as subscriber only
    ///
    /// Args:
    ///     path: Path on the relay (default: "anon/xoq")
    ///     token: Optional JWT authentication token
    ///     relay: Relay URL (default: "https://cdn.moq.dev")
    #[new]
    #[pyo3(signature = (path=None, token=None, relay=None))]
    fn new(path: Option<&str>, token: Option<&str>, relay: Option<&str>) -> PyResult<Self> {
        let relay_url = relay.unwrap_or("https://cdn.moq.dev");
        let path = path.unwrap_or("anon/xoq");

        runtime().block_on(async {
            let mut builder = xoq_lib::MoqBuilder::new().relay(relay_url).path(path);
            if let Some(t) = token {
                builder = builder.token(t);
            }

            let sub_conn = builder
                .connect_subscriber()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(MoqSubscriber {
                inner: Arc::new(Mutex::new(sub_conn)),
            })
        })
    }

    /// Wait for an announced broadcast and subscribe to a track
    fn subscribe_track(&self, track_name: &str) -> PyResult<Option<MoqTrackReader>> {
        runtime().block_on(async {
            let mut sub = self.inner.lock().await;
            let reader = sub
                .subscribe_track(track_name)
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(reader.map(|r| MoqTrackReader {
                inner: Arc::new(Mutex::new(r)),
            }))
        })
    }
}

/// A track writer for publishing data
#[pyclass]
struct MoqTrackWriter {
    inner: Arc<Mutex<xoq_lib::MoqTrackWriter>>,
}

#[pymethods]
impl MoqTrackWriter {
    /// Write bytes to the track
    fn write(&self, data: Vec<u8>) -> PyResult<()> {
        runtime().block_on(async {
            let mut writer = self.inner.lock().await;
            writer.write(data);
            Ok(())
        })
    }

    /// Write string data
    fn write_str(&self, data: &str) -> PyResult<()> {
        runtime().block_on(async {
            let mut writer = self.inner.lock().await;
            writer.write_str(data);
            Ok(())
        })
    }
}

/// A track reader for receiving data
#[pyclass]
struct MoqTrackReader {
    inner: Arc<Mutex<xoq_lib::MoqTrackReader>>,
}

#[pymethods]
impl MoqTrackReader {
    /// Read the next frame as bytes
    fn read(&self) -> PyResult<Option<Vec<u8>>> {
        runtime().block_on(async {
            let mut reader = self.inner.lock().await;
            let data = reader
                .read()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(data.map(|b| b.to_vec()))
        })
    }

    /// Read the next frame as string
    fn read_string(&self) -> PyResult<Option<String>> {
        runtime().block_on(async {
            let mut reader = self.inner.lock().await;
            reader
                .read_string()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
}

// ============================================================================
// Iroh Bindings (feature-gated)
// ============================================================================

#[cfg(feature = "iroh")]
mod iroh_bindings {
    use super::*;

    /// An iroh server that accepts connections
    #[pyclass]
    pub struct IrohServer {
        inner: Arc<xoq_lib::IrohServer>,
    }

    #[pymethods]
    impl IrohServer {
        /// Start an iroh server
        ///
        /// Args:
        ///     identity_path: Path to save/load server identity key
        ///     alpn: Custom ALPN protocol bytes (default: b"xoq/p2p/0")
        #[new]
        #[pyo3(signature = (identity_path=None, alpn=None))]
        fn new(identity_path: Option<&str>, alpn: Option<Vec<u8>>) -> PyResult<Self> {
            let alpn = alpn.unwrap_or_else(|| b"xoq/p2p/0".to_vec());

            runtime().block_on(async {
                let mut builder = xoq_lib::IrohServerBuilder::new().alpn(&alpn);
                if let Some(path) = identity_path {
                    builder = builder.identity_path(path);
                }

                let server = builder
                    .bind()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(IrohServer {
                    inner: Arc::new(server),
                })
            })
        }

        /// Get the server's endpoint ID
        fn id(&self) -> String {
            self.inner.id().to_string()
        }

        /// Accept an incoming connection
        fn accept(&self) -> PyResult<Option<IrohConnection>> {
            runtime().block_on(async {
                let conn = self
                    .inner
                    .accept()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(conn.map(|c| IrohConnection { inner: Arc::new(c) }))
            })
        }
    }

    /// An iroh connection (either server or client side)
    #[pyclass]
    pub struct IrohConnection {
        inner: Arc<xoq_lib::IrohConnection>,
    }

    #[pymethods]
    impl IrohConnection {
        /// Connect to a server by endpoint ID
        ///
        /// Args:
        ///     server_id: The server's endpoint ID string
        ///     alpn: Custom ALPN protocol bytes (default: b"xoq/p2p/0")
        #[new]
        #[pyo3(signature = (server_id, alpn=None))]
        fn new(server_id: &str, alpn: Option<Vec<u8>>) -> PyResult<Self> {
            let alpn = alpn.unwrap_or_else(|| b"xoq/p2p/0".to_vec());

            runtime().block_on(async {
                let conn = xoq_lib::IrohClientBuilder::new()
                    .alpn(&alpn)
                    .connect_str(server_id)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(IrohConnection {
                    inner: Arc::new(conn),
                })
            })
        }

        /// Get the remote peer's ID
        fn remote_id(&self) -> String {
            self.inner.remote_id().to_string()
        }

        /// Open a bidirectional stream
        fn open_stream(&self) -> PyResult<IrohStream> {
            runtime().block_on(async {
                let stream = self
                    .inner
                    .open_stream()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(IrohStream {
                    inner: Arc::new(Mutex::new(stream)),
                })
            })
        }

        /// Accept a bidirectional stream from the remote peer
        fn accept_stream(&self) -> PyResult<IrohStream> {
            runtime().block_on(async {
                let stream = self
                    .inner
                    .accept_stream()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(IrohStream {
                    inner: Arc::new(Mutex::new(stream)),
                })
            })
        }
    }

    /// A bidirectional stream
    #[pyclass]
    pub struct IrohStream {
        inner: Arc<Mutex<xoq_lib::IrohStream>>,
    }

    #[pymethods]
    impl IrohStream {
        /// Write bytes to the stream
        fn write(&self, data: Vec<u8>) -> PyResult<()> {
            runtime().block_on(async {
                let mut stream = self.inner.lock().await;
                stream
                    .write(&data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
                Ok(())
            })
        }

        /// Write a string to the stream
        fn write_str(&self, data: &str) -> PyResult<()> {
            runtime().block_on(async {
                let mut stream = self.inner.lock().await;
                stream
                    .write_str(data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
                Ok(())
            })
        }

        /// Read bytes from the stream
        #[pyo3(signature = (size=4096))]
        fn read(&self, size: usize) -> PyResult<Option<Vec<u8>>> {
            runtime().block_on(async {
                let mut stream = self.inner.lock().await;
                let mut buf = vec![0u8; size];
                let n = stream
                    .read(&mut buf)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(n.map(|n| buf[..n].to_vec()))
            })
        }

        /// Read a string from the stream
        fn read_string(&self) -> PyResult<Option<String>> {
            runtime().block_on(async {
                let mut stream = self.inner.lock().await;
                stream
                    .read_string()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }
    }
}

// ============================================================================
// Serial Bindings (feature-gated)
// ============================================================================

#[cfg(feature = "serial")]
mod serial_bindings {
    use super::*;

    /// List available serial ports
    #[pyfunction]
    pub fn list_ports() -> PyResult<Vec<SerialPortInfo>> {
        let ports = xoq_lib::list_ports().map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        Ok(ports
            .into_iter()
            .map(|p| SerialPortInfo {
                name: p.name,
                port_type: format!("{:?}", p.port_type),
            })
            .collect())
    }

    /// Information about a serial port
    #[pyclass]
    #[derive(Clone)]
    pub struct SerialPortInfo {
        #[pyo3(get)]
        pub name: String,
        #[pyo3(get)]
        pub port_type: String,
    }

    /// A serial port connection
    #[pyclass]
    pub struct SerialPort {
        inner: Arc<Mutex<xoq_lib::SerialPort>>,
    }

    #[pymethods]
    impl SerialPort {
        /// Open a serial port
        ///
        /// Args:
        ///     port: Port name (e.g., "/dev/ttyUSB0" or "COM3")
        ///     baud_rate: Baud rate (default: 115200)
        #[new]
        #[pyo3(signature = (port, baud_rate=115200))]
        fn new(port: &str, baud_rate: u32) -> PyResult<Self> {
            let serial = xoq_lib::SerialPort::open_simple(port, baud_rate)
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(SerialPort {
                inner: Arc::new(Mutex::new(serial)),
            })
        }

        /// Write bytes to the serial port
        fn write(&self, data: Vec<u8>) -> PyResult<usize> {
            runtime().block_on(async {
                let mut serial = self.inner.lock().await;
                serial
                    .write(&data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }

        /// Write a string to the serial port
        fn write_str(&self, data: &str) -> PyResult<()> {
            runtime().block_on(async {
                let mut serial = self.inner.lock().await;
                serial
                    .write_str(data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }

        /// Read bytes from the serial port
        #[pyo3(signature = (size=1024))]
        fn read(&self, size: usize) -> PyResult<Vec<u8>> {
            runtime().block_on(async {
                let mut serial = self.inner.lock().await;
                let mut buf = vec![0u8; size];
                let n = serial
                    .read(&mut buf)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
                buf.truncate(n);
                Ok(buf)
            })
        }
    }
}

// ============================================================================
// Serial Bridge Bindings (requires both iroh and serial features)
// ============================================================================

#[cfg(all(feature = "serial", feature = "iroh"))]
mod bridge_bindings {
    use super::*;

    /// Find a subsequence in a slice, returns the starting position if found.
    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// A server that bridges a local serial port to remote clients over iroh P2P.
    /// All forwarding is handled internally in Rust.
    #[pyclass]
    pub struct Server {
        inner: Arc<xoq_lib::Server>,
    }

    #[pymethods]
    impl Server {
        /// Create a new serial bridge server
        ///
        /// Args:
        ///     port: Serial port name (e.g., "/dev/ttyUSB0" or "COM3")
        ///     baud_rate: Baud rate (default: 115200)
        ///     identity_path: Optional path to save/load server identity
        #[new]
        #[pyo3(signature = (port, baud_rate=115200, identity_path=None))]
        fn new(port: &str, baud_rate: u32, identity_path: Option<&str>) -> PyResult<Self> {
            runtime().block_on(async {
                let server = xoq_lib::Server::new(port, baud_rate, identity_path)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(Server {
                    inner: Arc::new(server),
                })
            })
        }

        /// Get the server's endpoint ID (share this with clients to connect)
        fn id(&self) -> String {
            self.inner.id().to_string()
        }

        /// Run the bridge server (blocks forever, handling connections)
        fn run(&self) -> PyResult<()> {
            runtime().block_on(async {
                self.inner
                    .run()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }

        /// Run the bridge server for a single connection, then return
        fn run_once(&self) -> PyResult<()> {
            runtime().block_on(async {
                self.inner
                    .run_once()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }
    }

    /// A client that connects to a remote serial port over iroh P2P.
    #[pyclass]
    pub struct Client {
        inner: Arc<xoq_lib::Client>,
    }

    #[pymethods]
    impl Client {
        /// Connect to a remote serial bridge server
        ///
        /// Args:
        ///     server_id: The server's endpoint ID
        #[new]
        fn new(server_id: &str) -> PyResult<Self> {
            runtime().block_on(async {
                let client = xoq_lib::Client::connect(server_id)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(Client {
                    inner: Arc::new(client),
                })
            })
        }

        /// Write bytes to the remote serial port
        fn write(&self, data: Vec<u8>) -> PyResult<()> {
            runtime().block_on(async {
                self.inner
                    .write(&data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }

        /// Write a string to the remote serial port
        fn write_str(&self, data: &str) -> PyResult<()> {
            runtime().block_on(async {
                self.inner
                    .write_str(data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }

        /// Read bytes from the remote serial port
        #[pyo3(signature = (size=1024))]
        fn read(&self, size: usize) -> PyResult<Option<Vec<u8>>> {
            runtime().block_on(async {
                let mut buf = vec![0u8; size];
                let n = self
                    .inner
                    .read(&mut buf)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(n.map(|n| buf[..n].to_vec()))
            })
        }

        /// Read a string from the remote serial port
        fn read_string(&self) -> PyResult<Option<String>> {
            runtime().block_on(async {
                self.inner
                    .read_string()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }

        /// Run an interactive terminal to the remote serial port (blocks)
        fn run_interactive(&self) -> PyResult<()> {
            runtime().block_on(async {
                self.inner
                    .run_interactive()
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))
            })
        }
    }

    /// A pyserial-compatible interface to a remote serial port.
    /// Drop-in replacement for serial.Serial that connects over iroh P2P.
    ///
    /// Example:
    ///     ser = xoq.Serial('abc123...')  # server endpoint id
    ///     ser.write(b'AT\r\n')
    ///     response = ser.readline()
    #[pyclass]
    pub struct Serial {
        inner: Arc<xoq_lib::Client>,
        buffer: Arc<std::sync::Mutex<Vec<u8>>>,
        is_open: Arc<std::sync::atomic::AtomicBool>,
        timeout: Option<f64>,
        port_name: String,
    }

    #[pymethods]
    impl Serial {
        /// Open a connection to a remote serial port.
        ///
        /// Args:
        ///     port: The server's endpoint ID (equivalent to port name in pyserial)
        ///     timeout: Read timeout in seconds (None for blocking)
        #[new]
        #[pyo3(signature = (port, timeout=None))]
        fn new(port: &str, timeout: Option<f64>) -> PyResult<Self> {
            let port_name = port.to_string();
            runtime().block_on(async {
                let client = xoq_lib::Client::connect(port)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok(Serial {
                    inner: Arc::new(client),
                    buffer: Arc::new(std::sync::Mutex::new(Vec::new())),
                    is_open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                    timeout,
                    port_name,
                })
            })
        }

        /// Write bytes to the serial port. Returns number of bytes written.
        fn write(&self, data: Vec<u8>) -> PyResult<usize> {
            if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(PyRuntimeError::new_err("Port is closed"));
            }
            let len = data.len();
            runtime().block_on(async {
                self.inner
                    .write(&data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
                Ok(len)
            })
        }

        /// Read up to size bytes from the serial port.
        #[pyo3(signature = (size=1))]
        fn read(&self, size: usize) -> PyResult<Vec<u8>> {
            if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(PyRuntimeError::new_err("Port is closed"));
            }

            // First check buffer
            {
                let mut buf = self.buffer.lock().unwrap();
                if !buf.is_empty() {
                    let take = std::cmp::min(size, buf.len());
                    let result: Vec<u8> = buf.drain(..take).collect();
                    return Ok(result);
                }
            }

            // Read from network
            runtime().block_on(async {
                let mut data = vec![0u8; size];
                let n = self
                    .inner
                    .read(&mut data)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                match n {
                    Some(n) => Ok(data[..n].to_vec()),
                    None => Ok(Vec::new()),
                }
            })
        }

        /// Read a line (until newline character).
        fn readline(&self) -> PyResult<Vec<u8>> {
            if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(PyRuntimeError::new_err("Port is closed"));
            }

            let mut result = Vec::new();

            // Check buffer first for existing newline
            {
                let mut buf = self.buffer.lock().unwrap();
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    result = buf.drain(..=pos).collect();
                    return Ok(result);
                }
                // Take everything from buffer
                result.append(&mut *buf);
            }

            // Keep reading until we get a newline
            runtime().block_on(async {
                let mut temp = vec![0u8; 256];
                loop {
                    let n = self
                        .inner
                        .read(&mut temp)
                        .await
                        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                    match n {
                        Some(n) => {
                            let chunk = &temp[..n];
                            if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
                                // Found newline - take up to and including it
                                result.extend_from_slice(&chunk[..=pos]);
                                // Buffer the rest
                                if pos + 1 < n {
                                    let mut buf = self.buffer.lock().unwrap();
                                    buf.extend_from_slice(&chunk[pos + 1..]);
                                }
                                return Ok(result);
                            } else {
                                result.extend_from_slice(chunk);
                            }
                        }
                        None => return Ok(result), // EOF
                    }
                }
            })
        }

        /// Number of bytes in the receive buffer.
        #[getter]
        fn in_waiting(&self) -> usize {
            self.buffer.lock().unwrap().len()
        }

        /// Whether the port is open.
        #[getter]
        fn is_open(&self) -> bool {
            self.is_open.load(std::sync::atomic::Ordering::Relaxed)
        }

        /// Read/write timeout in seconds.
        #[getter]
        fn timeout(&self) -> Option<f64> {
            self.timeout
        }

        /// The port name (server endpoint ID).
        #[getter]
        fn port(&self) -> &str {
            &self.port_name
        }

        /// Alias for port property (pyserial compatibility).
        #[getter]
        fn name(&self) -> &str {
            &self.port_name
        }

        /// Clear the receive buffer.
        fn reset_input_buffer(&self) {
            self.buffer.lock().unwrap().clear();
        }

        /// Read until a terminator sequence is found.
        #[pyo3(signature = (terminator=None))]
        fn read_until(&self, terminator: Option<Vec<u8>>) -> PyResult<Vec<u8>> {
            let terminator = terminator.unwrap_or_else(|| vec![b'\n']);
            if terminator.is_empty() {
                return Err(PyRuntimeError::new_err("Terminator cannot be empty"));
            }

            if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(PyRuntimeError::new_err("Port is closed"));
            }

            let mut result = Vec::new();

            // Check buffer first for existing terminator
            {
                let mut buf = self.buffer.lock().unwrap();
                if let Some(pos) = find_subsequence(&buf, &terminator) {
                    let end = pos + terminator.len();
                    result = buf.drain(..end).collect();
                    return Ok(result);
                }
                // Take everything from buffer
                result.append(&mut *buf);
            }

            // Keep reading until we find terminator
            runtime().block_on(async {
                let mut temp = vec![0u8; 256];
                loop {
                    let n = self
                        .inner
                        .read(&mut temp)
                        .await
                        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                    match n {
                        Some(n) => {
                            result.extend_from_slice(&temp[..n]);
                            // Check if terminator is now in result
                            if let Some(pos) = find_subsequence(&result, &terminator) {
                                let end = pos + terminator.len();
                                // Buffer anything after the terminator
                                if end < result.len() {
                                    let mut buf = self.buffer.lock().unwrap();
                                    buf.extend_from_slice(&result[end..]);
                                }
                                result.truncate(end);
                                return Ok(result);
                            }
                        }
                        None => return Ok(result), // EOF
                    }
                }
            })
        }

        /// Flush write buffer (no-op for network connection).
        fn flush(&self) -> PyResult<()> {
            Ok(())
        }

        /// Close the connection.
        fn close(&self) -> PyResult<()> {
            self.is_open
                .store(false, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        /// Context manager enter.
        fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
            slf
        }

        /// Context manager exit.
        #[pyo3(signature = (_exc_type=None, _exc_val=None, _exc_tb=None))]
        fn __exit__(
            &self,
            _exc_type: Option<&pyo3::Bound<'_, pyo3::types::PyAny>>,
            _exc_val: Option<&pyo3::Bound<'_, pyo3::types::PyAny>>,
            _exc_tb: Option<&pyo3::Bound<'_, pyo3::types::PyAny>>,
        ) -> PyResult<bool> {
            self.close()?;
            Ok(false)
        }
    }
}

#[pymodule]
fn xoq(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // MoQ classes
    m.add_class::<MoqConnection>()?;
    m.add_class::<MoqPublisher>()?;
    m.add_class::<MoqSubscriber>()?;
    m.add_class::<MoqTrackWriter>()?;
    m.add_class::<MoqTrackReader>()?;

    // Iroh classes (when feature enabled)
    #[cfg(feature = "iroh")]
    {
        m.add_class::<iroh_bindings::IrohServer>()?;
        m.add_class::<iroh_bindings::IrohConnection>()?;
        m.add_class::<iroh_bindings::IrohStream>()?;
    }

    // Serial classes (when feature enabled)
    #[cfg(feature = "serial")]
    {
        m.add_function(pyo3::wrap_pyfunction!(serial_bindings::list_ports, m)?)?;
        m.add_class::<serial_bindings::SerialPortInfo>()?;
        m.add_class::<serial_bindings::SerialPort>()?;
    }

    // Serial bridge classes (when both iroh and serial features enabled)
    #[cfg(all(feature = "serial", feature = "iroh"))]
    {
        m.add_class::<bridge_bindings::Server>()?;
        m.add_class::<bridge_bindings::Client>()?;
        m.add_class::<bridge_bindings::Serial>()?;
    }

    Ok(())
}
