// PyO3 generated code triggers this lint on #[pymethods] error conversions
#![allow(clippy::useless_conversion)]

//! Drop-in replacement for python-can - remote CAN bus over P2P.
//!
//! This module provides `can.Bus` and `can.Message` compatible classes that connect
//! to remote CAN interfaces over iroh P2P.
//!
//! # Example
//!
//! ```python
//! import can
//!
//! # Connect to a remote CAN bus
//! bus = can.Bus(channel='server-endpoint-id', interface='xoq')
//! msg = can.Message(arbitration_id=0x123, data=[1, 2, 3, 4])
//! bus.send(msg)
//! received = bus.recv(timeout=1.0)
//! ```

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Run a closure in a dedicated thread.
/// This avoids "Cannot start a runtime from within a runtime" errors
/// since xoq::RemoteCanSocket::open() creates its own tokio runtime.
fn run_in_thread<F, T, E>(f: F) -> Result<T, E>
where
    F: FnOnce() -> Result<T, E> + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
{
    std::thread::spawn(f).join().expect("Thread panicked")
}

/// A CAN message compatible with python-can's Message class.
///
/// Example:
///     msg = can.Message(arbitration_id=0x123, data=[1, 2, 3, 4])
///     print(msg.arbitration_id)  # 0x123
///     print(msg.data)            # [1, 2, 3, 4]
#[pyclass]
#[derive(Clone)]
pub struct Message {
    #[pyo3(get, set)]
    arbitration_id: u32,
    #[pyo3(get, set)]
    data: Vec<u8>,
    #[pyo3(get, set)]
    is_extended_id: bool,
    #[pyo3(get, set)]
    is_fd: bool,
    #[pyo3(get, set)]
    is_remote_frame: bool,
    #[pyo3(get, set)]
    is_error_frame: bool,
    #[pyo3(get, set)]
    bitrate_switch: bool,
    #[pyo3(get, set)]
    error_state_indicator: bool,
    #[pyo3(get, set)]
    timestamp: Option<f64>,
    #[pyo3(get, set)]
    channel: Option<String>,
    #[pyo3(get, set)]
    dlc: Option<u8>,
}

#[pymethods]
impl Message {
    /// Create a new CAN message.
    ///
    /// Args:
    ///     arbitration_id: CAN identifier (11-bit or 29-bit)
    ///     data: Message data bytes (up to 8 for CAN, up to 64 for CAN FD)
    ///     is_extended_id: Whether to use extended (29-bit) ID
    ///     is_fd: Whether this is a CAN FD message
    ///     is_remote_frame: Whether this is a remote transmission request
    ///     is_error_frame: Whether this is an error frame
    ///     bitrate_switch: CAN FD bitrate switch flag
    ///     error_state_indicator: CAN FD error state indicator flag
    ///     timestamp: Message timestamp (seconds)
    ///     channel: Channel name/identifier
    ///     dlc: Data length code (optional, calculated from data if not provided)
    #[new]
    #[pyo3(signature = (
        arbitration_id = 0,
        data = None,
        is_extended_id = false,
        is_fd = false,
        is_remote_frame = false,
        is_error_frame = false,
        bitrate_switch = false,
        error_state_indicator = false,
        timestamp = None,
        channel = None,
        dlc = None,
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        arbitration_id: u32,
        data: Option<Vec<u8>>,
        is_extended_id: bool,
        is_fd: bool,
        is_remote_frame: bool,
        is_error_frame: bool,
        bitrate_switch: bool,
        error_state_indicator: bool,
        timestamp: Option<f64>,
        channel: Option<String>,
        dlc: Option<u8>,
    ) -> PyResult<Self> {
        let data = data.unwrap_or_default();

        // Validate data length
        let max_len = if is_fd { 64 } else { 8 };
        if data.len() > max_len {
            return Err(PyValueError::new_err(format!(
                "Data length {} exceeds maximum {} for {} frame",
                data.len(),
                max_len,
                if is_fd { "CAN FD" } else { "CAN" }
            )));
        }

        // Validate arbitration ID
        let max_id = if is_extended_id { 0x1FFFFFFF } else { 0x7FF };
        if arbitration_id > max_id {
            return Err(PyValueError::new_err(format!(
                "Arbitration ID 0x{:X} exceeds maximum 0x{:X} for {} ID",
                arbitration_id,
                max_id,
                if is_extended_id {
                    "extended"
                } else {
                    "standard"
                }
            )));
        }

        Ok(Message {
            arbitration_id,
            data,
            is_extended_id,
            is_fd,
            is_remote_frame,
            is_error_frame,
            bitrate_switch,
            error_state_indicator,
            timestamp,
            channel,
            dlc,
        })
    }

    fn __repr__(&self) -> String {
        let data_str: Vec<String> = self.data.iter().map(|b| format!("{:02X}", b)).collect();
        format!(
            "can.Message(arbitration_id=0x{:X}, data=[{}], is_extended_id={}, is_fd={})",
            self.arbitration_id,
            data_str.join(", "),
            self.is_extended_id,
            self.is_fd
        )
    }

    fn __str__(&self) -> String {
        self.__repr__()
    }
}

/// A CAN bus interface compatible with python-can's Bus class.
///
/// Example:
///     bus = can.Bus(channel='server-endpoint-id', interface='xoq')
///     bus.send(can.Message(arbitration_id=0x123, data=[1, 2, 3]))
///     msg = bus.recv(timeout=1.0)
#[pyclass]
pub struct Bus {
    // Use std::sync::Mutex since RemoteCanSocket methods are already blocking
    socket: Arc<Mutex<xoq::RemoteCanSocket>>,
    channel: String,
    is_open: Arc<std::sync::atomic::AtomicBool>,
    recv_timeout: Option<f64>,
}

#[pymethods]
impl Bus {
    /// Create a new CAN bus connection.
    ///
    /// Args:
    ///     channel: The server's endpoint ID (required)
    ///     interface: Interface type (accepted for compatibility, ignored)
    ///     bitrate: CAN bitrate (accepted for compatibility, ignored for remote)
    ///     data_bitrate: CAN FD data bitrate (accepted for compatibility, ignored for remote)
    ///     receive_own_messages: Whether to receive messages sent by this bus (ignored)
    ///     fd: Whether to enable CAN FD support
    ///     timeout: Default receive timeout in seconds (None for blocking)
    #[new]
    #[pyo3(signature = (channel=None, interface=None, bitrate=None, data_bitrate=None, receive_own_messages=false, fd=false, timeout=None, **kwargs))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        channel: Option<&str>,
        interface: Option<&str>,
        bitrate: Option<u32>,
        data_bitrate: Option<u32>,
        receive_own_messages: bool,
        fd: bool,
        timeout: Option<f64>,
        kwargs: Option<&Bound<'_, pyo3::types::PyDict>>,
    ) -> PyResult<Self> {
        // These parameters are accepted for python-can compatibility but ignored for remote connections
        let _ = (
            interface,
            bitrate,
            data_bitrate,
            receive_own_messages,
            kwargs,
        );

        let channel = channel.ok_or_else(|| PyValueError::new_err("channel is required"))?;

        let channel_str = channel.to_string();
        // Run open() in a dedicated thread since it creates its own tokio runtime
        let socket = run_in_thread(move || {
            let mut builder = xoq::socketcan::new(&channel_str);
            if fd {
                builder = builder.enable_fd(true);
            }
            if let Some(t) = timeout {
                builder = builder.timeout(Duration::from_secs_f64(t));
            }
            builder
                .open()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })?;

        Ok(Bus {
            socket: Arc::new(Mutex::new(socket)),
            channel: channel.to_string(),
            is_open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            recv_timeout: timeout,
        })
    }

    /// Send a CAN message.
    ///
    /// Args:
    ///     msg: The Message to send
    ///     timeout: Send timeout in seconds (optional)
    #[pyo3(signature = (msg, timeout=None))]
    fn send(&self, py: Python<'_>, msg: &Message, timeout: Option<f64>) -> PyResult<()> {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Bus is closed"));
        }

        let _ = timeout; // TODO: implement send timeout

        // Extract data from Python object before releasing GIL
        let is_fd = msg.is_fd;
        let arb_id = msg.arbitration_id;
        let data = msg.data.clone();
        let brs = msg.bitrate_switch;
        let esi = msg.error_state_indicator;

        let socket = self.socket.clone();
        // Release GIL during blocking network I/O
        py.allow_threads(move || {
            let mut socket = socket.lock().map_err(|e| e.to_string())?;
            if is_fd {
                let flags = xoq::CanFdFlags { brs, esi };
                let frame = xoq::CanFdFrame::new_with_flags(arb_id, &data, flags)
                    .map_err(|e| e.to_string())?;
                socket.write_fd_frame(&frame).map_err(|e| e.to_string())
            } else {
                let frame = xoq::CanFrame::new(arb_id, &data).map_err(|e| e.to_string())?;
                socket.write_frame(&frame).map_err(|e| e.to_string())
            }
        })
        .map_err(|e: String| PyRuntimeError::new_err(e))
    }

    /// Receive a CAN message.
    ///
    /// Args:
    ///     timeout: Receive timeout in seconds (None for blocking, 0 for non-blocking)
    ///
    /// Returns:
    ///     A Message object, or None if timeout occurred
    #[pyo3(signature = (timeout=None))]
    fn recv(&self, py: Python<'_>, timeout: Option<f64>) -> PyResult<Option<Message>> {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Bus is closed"));
        }

        let timeout = timeout.or(self.recv_timeout);
        let socket = self.socket.clone();
        let channel = self.channel.clone();

        // Release GIL during blocking network I/O
        let frame_result: Result<Option<(xoq::AnyCanFrame, f64)>, String> =
            py.allow_threads(move || {
                let start = Instant::now();
                let mut socket = socket.lock().map_err(|e| e.to_string())?;

                if let Some(t) = timeout {
                    let _ = socket.set_timeout(Duration::from_secs_f64(t));
                }

                loop {
                    let frame = socket.read_frame().map_err(|e| e.to_string())?;

                    match frame {
                        Some(frame) => {
                            let timestamp = start.elapsed().as_secs_f64();
                            return Ok(Some((frame, timestamp)));
                        }
                        None => {
                            if let Some(t) = timeout {
                                if start.elapsed().as_secs_f64() >= t {
                                    return Ok(None);
                                }
                            }
                            if timeout == Some(0.0) {
                                return Ok(None);
                            }
                            continue;
                        }
                    }
                }
            });

        // Convert to Python Message with GIL held
        match frame_result {
            Ok(Some((frame, timestamp))) => {
                let msg = match frame {
                    xoq::AnyCanFrame::Can(f) => Message {
                        arbitration_id: f.id(),
                        data: f.data().to_vec(),
                        is_extended_id: f.is_extended(),
                        is_fd: false,
                        is_remote_frame: f.is_remote(),
                        is_error_frame: f.is_error(),
                        bitrate_switch: false,
                        error_state_indicator: false,
                        timestamp: Some(timestamp),
                        channel: Some(channel),
                        dlc: Some(f.dlc()),
                    },
                    xoq::AnyCanFrame::CanFd(f) => Message {
                        arbitration_id: f.id(),
                        data: f.data().to_vec(),
                        is_extended_id: f.is_extended(),
                        is_fd: true,
                        is_remote_frame: false,
                        is_error_frame: false,
                        bitrate_switch: f.flags().brs,
                        error_state_indicator: f.flags().esi,
                        timestamp: Some(timestamp),
                        channel: Some(channel),
                        dlc: Some(f.len() as u8),
                    },
                };
                Ok(Some(msg))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(PyRuntimeError::new_err(e)),
        }
    }

    /// Shutdown the bus.
    fn shutdown(&self) -> PyResult<()> {
        self.is_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Get the channel name.
    #[getter]
    fn channel_info(&self) -> &str {
        &self.channel
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
        self.shutdown()?;
        Ok(false)
    }

    /// Iterator protocol - returns self.
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Iterator protocol - get next message.
    fn __next__(&self, py: Python<'_>) -> PyResult<Option<Message>> {
        self.recv(py, Some(1.0))
    }
}

// Python module
#[pymodule]
fn xoq_can(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Message>()?;
    m.add_class::<Bus>()?;

    // Create can.interface submodule (python-can compatibility)
    // LeRobot and other libraries use can.interface.Bus(...)
    let interface_mod = pyo3::types::PyModule::new_bound(m.py(), "interface")?;
    interface_mod.add_class::<Bus>()?;
    m.add_submodule(&interface_mod)?;

    // Common interface names (for compatibility)
    m.add("INTERFACE_XOQ", "xoq")?;
    m.add("INTERFACE_SOCKETCAN", "socketcan")?;

    Ok(())
}
