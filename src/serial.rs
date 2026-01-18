//! Cross-platform serial port support.
//!
//! This module provides serial port access using the [`serialport`](https://crates.io/crates/serialport) crate,
//! which works on Linux, macOS, and Windows.
//!
//! # Features
//!
//! - Blocking I/O on dedicated threads (doesn't block tokio runtime)
//! - Configurable baud rate, data bits, parity, and stop bits
//! - Split into separate read/write halves for concurrent access
//! - Port enumeration to list available serial ports
//!
//! # Example
//!
//! ```no_run
//! use xoq::serial::{SerialPort, SerialConfig, baud};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Simple open with defaults (8N1)
//! let port = SerialPort::open_simple("/dev/ttyUSB0", baud::B115200)?;
//!
//! // Split for concurrent read/write
//! let (mut reader, mut writer) = port.split();
//!
//! // Write data (async, but uses dedicated thread internally)
//! writer.write_all(b"AT\r\n").await?;
//!
//! // Read response
//! let mut buf = [0u8; 256];
//! let n = reader.read(&mut buf).await?;
//! println!("Received: {:?}", &buf[..n]);
//! # Ok(())
//! # }
//! ```
//!
//! # Listing Available Ports
//!
//! ```no_run
//! use xoq::serial::list_ports;
//!
//! for port in list_ports()? {
//!     println!("{} - {:?}", port.name, port.port_type);
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```

use anyhow::Result;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Common baud rates as constants.
///
/// # Example
///
/// ```
/// use xoq::serial::baud;
///
/// let rate = baud::B115200; // 115200 bps
/// ```
pub mod baud {
    /// 9600 baud
    pub const B9600: u32 = 9600;
    /// 19200 baud
    pub const B19200: u32 = 19200;
    /// 38400 baud
    pub const B38400: u32 = 38400;
    /// 57600 baud
    pub const B57600: u32 = 57600;
    /// 115200 baud (most common)
    pub const B115200: u32 = 115200;
    /// 230400 baud
    pub const B230400: u32 = 230400;
    /// 460800 baud
    pub const B460800: u32 = 460800;
    /// 921600 baud
    pub const B921600: u32 = 921600;
}

/// Serial port configuration.
///
/// Specifies all parameters needed to open a serial port connection.
/// Use [`SerialConfig::new`] for common defaults (8 data bits, no parity, 1 stop bit).
///
/// # Example
///
/// ```
/// use xoq::serial::{SerialConfig, DataBits, Parity, StopBits};
///
/// // Simple config with defaults (8N1)
/// let config = SerialConfig::new("/dev/ttyUSB0", 115200);
///
/// // Full custom config
/// let config = SerialConfig {
///     port: "/dev/ttyUSB0".to_string(),
///     baud_rate: 9600,
///     data_bits: DataBits::Seven,
///     parity: Parity::Even,
///     stop_bits: StopBits::One,
/// };
/// ```
#[derive(Clone, Debug)]
pub struct SerialConfig {
    /// Port name (e.g., "/dev/ttyUSB0" on Linux, "COM3" on Windows)
    pub port: String,
    /// Baud rate (e.g., 9600, 115200)
    pub baud_rate: u32,
    /// Number of data bits per character
    pub data_bits: DataBits,
    /// Parity checking mode
    pub parity: Parity,
    /// Number of stop bits
    pub stop_bits: StopBits,
}

impl SerialConfig {
    /// Create a new config with common defaults (8N1).
    ///
    /// Uses 8 data bits, no parity, and 1 stop bit - the most common configuration.
    pub fn new(port: &str, baud_rate: u32) -> Self {
        Self {
            port: port.to_string(),
            baud_rate,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
        }
    }
}

/// Number of data bits per character.
///
/// Most devices use 8 data bits (the default).
#[derive(Clone, Copy, Debug, Default)]
pub enum DataBits {
    /// 5 data bits
    Five,
    /// 6 data bits
    Six,
    /// 7 data bits (common for ASCII text)
    Seven,
    /// 8 data bits (most common, default)
    #[default]
    Eight,
}

impl From<DataBits> for serialport::DataBits {
    fn from(db: DataBits) -> Self {
        match db {
            DataBits::Five => serialport::DataBits::Five,
            DataBits::Six => serialport::DataBits::Six,
            DataBits::Seven => serialport::DataBits::Seven,
            DataBits::Eight => serialport::DataBits::Eight,
        }
    }
}

/// Parity checking mode.
///
/// Parity is an error-detection mechanism. Most modern devices use no parity (the default).
#[derive(Clone, Copy, Debug, Default)]
pub enum Parity {
    /// No parity bit (most common, default)
    #[default]
    None,
    /// Odd parity - parity bit set so total 1-bits is odd
    Odd,
    /// Even parity - parity bit set so total 1-bits is even
    Even,
}

impl From<Parity> for serialport::Parity {
    fn from(p: Parity) -> Self {
        match p {
            Parity::None => serialport::Parity::None,
            Parity::Odd => serialport::Parity::Odd,
            Parity::Even => serialport::Parity::Even,
        }
    }
}

/// Number of stop bits.
///
/// Stop bits signal the end of a character. Most devices use 1 stop bit (the default).
#[derive(Clone, Copy, Debug, Default)]
pub enum StopBits {
    /// 1 stop bit (most common, default)
    #[default]
    One,
    /// 2 stop bits
    Two,
}

impl From<StopBits> for serialport::StopBits {
    fn from(sb: StopBits) -> Self {
        match sb {
            StopBits::One => serialport::StopBits::One,
            StopBits::Two => serialport::StopBits::Two,
        }
    }
}

/// A serial port that can be split into read/write halves.
///
/// Uses blocking I/O on dedicated threads to avoid blocking the tokio runtime.
///
/// # Example
///
/// ```no_run
/// use xoq::serial::SerialPort;
///
/// # fn example() -> anyhow::Result<()> {
/// let port = SerialPort::open_simple("/dev/ttyUSB0", 115200)?;
/// let (reader, writer) = port.split();
/// // Use reader and writer from different tasks
/// # Ok(())
/// # }
/// ```
pub struct SerialPort {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialPort {
    /// Open a serial port with the given configuration.
    pub fn open(config: &SerialConfig) -> Result<Self> {
        let port = serialport::new(&config.port, config.baud_rate)
            .data_bits(config.data_bits.into())
            .parity(config.parity.into())
            .stop_bits(config.stop_bits.into())
            .timeout(Duration::from_millis(100)) // Short timeout for responsive reads
            .open()?;

        Ok(Self { port })
    }

    /// Open a serial port with default settings (8N1)
    pub fn open_simple(port: &str, baud_rate: u32) -> Result<Self> {
        let config = SerialConfig::new(port, baud_rate);
        Self::open(&config)
    }

    /// Split into read and write halves.
    ///
    /// Each half runs blocking I/O on dedicated threads, communicating via channels.
    /// This allows concurrent reading and writing without blocking the tokio runtime.
    pub fn split(self) -> (SerialReader, SerialWriter) {
        let reader_port = self.port.try_clone().expect("Failed to clone serial port");
        let writer_port = self.port;

        // Create channels for async bridge
        let (read_tx, read_rx) = mpsc::channel::<ReadResult>();
        let (read_cmd_tx, read_cmd_rx) = mpsc::channel::<ReadCommand>();
        let (write_tx, write_rx) = mpsc::channel::<WriteCommand>();
        let (write_result_tx, write_result_rx) = mpsc::channel::<WriteResult>();

        // Spawn reader thread
        thread::spawn(move || {
            let mut port = reader_port;
            while let Ok(cmd) = read_cmd_rx.recv() {
                match cmd {
                    ReadCommand::Read(size) => {
                        let mut buf = vec![0u8; size];
                        match port.read(&mut buf) {
                            Ok(n) => {
                                buf.truncate(n);
                                if read_tx.send(ReadResult::Data(buf)).is_err() {
                                    break;
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                                // Timeout - no data available
                                if read_tx.send(ReadResult::Data(vec![])).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if read_tx.send(ReadResult::Error(e.to_string())).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    ReadCommand::Stop => break,
                }
            }
        });

        // Spawn writer thread
        thread::spawn(move || {
            let mut port = writer_port;
            while let Ok(cmd) = write_rx.recv() {
                match cmd {
                    WriteCommand::Write(data) => {
                        let result = port.write_all(&data).and_then(|_| port.flush());
                        let _ = write_result_tx.send(match result {
                            Ok(()) => WriteResult::Ok,
                            Err(e) => WriteResult::Error(e.to_string()),
                        });
                    }
                    WriteCommand::Stop => break,
                }
            }
        });

        (
            SerialReader {
                read_rx,
                read_cmd_tx,
            },
            SerialWriter {
                write_tx,
                write_result_rx,
            },
        )
    }
}

// Internal message types for thread communication
enum ReadCommand {
    Read(usize),
    Stop,
}

enum ReadResult {
    Data(Vec<u8>),
    Error(String),
}

enum WriteCommand {
    Write(Vec<u8>),
    Stop,
}

enum WriteResult {
    Ok,
    Error(String),
}

/// Read half of a split serial port.
///
/// Uses a dedicated thread for blocking reads, bridged to async via channels.
pub struct SerialReader {
    read_rx: mpsc::Receiver<ReadResult>,
    read_cmd_tx: mpsc::Sender<ReadCommand>,
}

impl SerialReader {
    /// Read data from the serial port.
    ///
    /// Returns the number of bytes read. Returns 0 if no data is available
    /// (timeout). This is non-blocking from tokio's perspective.
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // Send read command to dedicated thread
        self.read_cmd_tx
            .send(ReadCommand::Read(buf.len()))
            .map_err(|_| anyhow::anyhow!("Serial reader thread died"))?;

        // Wait for result (using spawn_blocking to not block tokio)
        let rx = unsafe {
            // SAFETY: We're moving the receiver to spawn_blocking and back.
            // This is safe because we wait for the result before using rx again.
            std::ptr::read(&self.read_rx)
        };

        let (result, rx) = tokio::task::spawn_blocking(move || {
            let result = rx.recv();
            (result, rx)
        })
        .await?;

        // Restore receiver
        unsafe {
            std::ptr::write(&mut self.read_rx, rx);
        }

        match result {
            Ok(ReadResult::Data(data)) => {
                let n = std::cmp::min(data.len(), buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Ok(n)
            }
            Ok(ReadResult::Error(e)) => Err(anyhow::anyhow!("Serial read error: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Serial reader thread died")),
        }
    }
}

impl Drop for SerialReader {
    fn drop(&mut self) {
        let _ = self.read_cmd_tx.send(ReadCommand::Stop);
    }
}

/// Write half of a split serial port.
///
/// Uses a dedicated thread for blocking writes, bridged to async via channels.
pub struct SerialWriter {
    write_tx: mpsc::Sender<WriteCommand>,
    write_result_rx: mpsc::Receiver<WriteResult>,
}

impl SerialWriter {
    /// Write data to the serial port.
    ///
    /// Returns the number of bytes written.
    pub async fn write(&mut self, data: &[u8]) -> Result<usize> {
        self.write_all(data).await?;
        Ok(data.len())
    }

    /// Write all data to the serial port.
    pub async fn write_all(&mut self, data: &[u8]) -> Result<()> {
        // Send write command to dedicated thread
        self.write_tx
            .send(WriteCommand::Write(data.to_vec()))
            .map_err(|_| anyhow::anyhow!("Serial writer thread died"))?;

        // Wait for result
        let rx = unsafe {
            std::ptr::read(&self.write_result_rx)
        };

        let (result, rx) = tokio::task::spawn_blocking(move || {
            let result = rx.recv();
            (result, rx)
        })
        .await?;

        unsafe {
            std::ptr::write(&mut self.write_result_rx, rx);
        }

        match result {
            Ok(WriteResult::Ok) => Ok(()),
            Ok(WriteResult::Error(e)) => Err(anyhow::anyhow!("Serial write error: {}", e)),
            Err(_) => Err(anyhow::anyhow!("Serial writer thread died")),
        }
    }

    /// Write a UTF-8 string to the serial port.
    pub async fn write_str(&mut self, data: &str) -> Result<()> {
        self.write_all(data.as_bytes()).await
    }

    /// Flush is implicit in write_all, but provided for API compatibility.
    pub async fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Drop for SerialWriter {
    fn drop(&mut self) {
        let _ = self.write_tx.send(WriteCommand::Stop);
    }
}

/// List available serial ports on the system.
///
/// Returns information about each detected serial port including its name
/// and type (USB, PCI, Bluetooth, etc.).
///
/// # Example
///
/// ```no_run
/// use xoq::serial::list_ports;
///
/// for port in list_ports()? {
///     println!("Port: {}", port.name);
///     match &port.port_type {
///         xoq::serial::PortType::Usb { vid, pid, product, .. } => {
///             println!("  USB device: VID={:04x} PID={:04x}", vid, pid);
///             if let Some(name) = product {
///                 println!("  Product: {}", name);
///             }
///         }
///         _ => println!("  Type: {:?}", port.port_type),
///     }
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn list_ports() -> Result<Vec<SerialPortInfo>> {
    let ports = serialport::available_ports()?;
    Ok(ports
        .into_iter()
        .map(|p| SerialPortInfo {
            name: p.port_name,
            port_type: match p.port_type {
                serialport::SerialPortType::UsbPort(info) => PortType::Usb {
                    vid: info.vid,
                    pid: info.pid,
                    manufacturer: info.manufacturer,
                    product: info.product,
                },
                serialport::SerialPortType::PciPort => PortType::Pci,
                serialport::SerialPortType::BluetoothPort => PortType::Bluetooth,
                serialport::SerialPortType::Unknown => PortType::Unknown,
            },
        })
        .collect())
}

/// Information about a detected serial port.
///
/// Returned by [`list_ports`].
#[derive(Clone, Debug)]
pub struct SerialPortInfo {
    /// Port name (e.g., "/dev/ttyUSB0" on Linux, "COM3" on Windows)
    pub name: String,
    /// Type of port (USB, PCI, Bluetooth, etc.)
    pub port_type: PortType,
}

/// Type of serial port hardware.
#[derive(Clone, Debug)]
pub enum PortType {
    /// USB serial adapter (most common for external devices)
    Usb {
        /// USB Vendor ID
        vid: u16,
        /// USB Product ID
        pid: u16,
        /// Manufacturer name (if available)
        manufacturer: Option<String>,
        /// Product name (if available)
        product: Option<String>,
    },
    /// PCI/PCIe serial card
    Pci,
    /// Bluetooth serial port
    Bluetooth,
    /// Unknown or unidentified port type
    Unknown,
}
