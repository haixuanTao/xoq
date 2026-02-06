// PyO3 generated code triggers this lint on #[pymethods] error conversions
#![allow(clippy::useless_conversion)]

//! Drop-in replacement for pyserial - remote serial ports over P2P.
//!
//! This module provides a `serial.Serial` compatible class that connects
//! to remote serial ports over iroh P2P.
//!
//! # Example
//!
//! ```python
//! import serial
//!
//! # Connect to a remote serial port
//! ser = serial.Serial('server-endpoint-id', timeout=1.0)
//! ser.write(b'Hello')
//! data = ser.readline()
//! ```

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Find a subsequence in a slice, returns the starting position if found.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A pyserial-compatible interface to a remote serial port.
/// Drop-in replacement for serial.Serial that connects over iroh P2P.
///
/// Example:
///     ser = serial.Serial('abc123...')  # server endpoint id
///     ser.write(b'AT\r\n')
///     response = ser.readline()
#[pyclass]
pub struct Serial {
    inner: Arc<Mutex<xoq::SyncSerialClient>>,
    buffer: Arc<Mutex<Vec<u8>>>,
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
        let client = xoq::SyncSerialClient::connect(port)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        Ok(Serial {
            inner: Arc::new(Mutex::new(client)),
            buffer: Arc::new(Mutex::new(Vec::new())),
            is_open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            timeout,
            port_name: port.to_string(),
        })
    }

    /// Write bytes to the serial port. Returns number of bytes written.
    fn write(&self, py: Python<'_>, data: Vec<u8>) -> PyResult<usize> {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Port is closed"));
        }
        let len = data.len();
        let inner = self.inner.clone();
        py.allow_threads(move || {
            let client = inner.lock().map_err(|e| e.to_string())?;
            client.write(&data).map_err(|e| e.to_string())
        })
        .map_err(|e: String| PyRuntimeError::new_err(e))?;
        Ok(len)
    }

    /// Read up to size bytes from the serial port.
    #[pyo3(signature = (size=1))]
    fn read(&self, py: Python<'_>, size: usize) -> PyResult<Vec<u8>> {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Port is closed"));
        }

        // First check buffer (fast, no network I/O)
        {
            let mut buf = self.buffer.lock().unwrap();
            if !buf.is_empty() {
                let take = std::cmp::min(size, buf.len());
                let result: Vec<u8> = buf.drain(..take).collect();
                return Ok(result);
            }
        }

        // Read from network without GIL
        let inner = self.inner.clone();
        let timeout = self.timeout;
        py.allow_threads(move || {
            let client = inner.lock().map_err(|e| e.to_string())?;
            let mut data = vec![0u8; size];
            let result = if let Some(t) = timeout {
                client.read_timeout(&mut data, Duration::from_secs_f64(t))
            } else {
                client.read(&mut data)
            };
            match result {
                Ok(Some(n)) => Ok(data[..n].to_vec()),
                Ok(None) => Ok(Vec::new()),
                Err(e) => Err(e.to_string()),
            }
        })
        .map_err(|e: String| PyRuntimeError::new_err(e))
    }

    /// Read a line (until newline character).
    fn readline(&self, py: Python<'_>) -> PyResult<Vec<u8>> {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Port is closed"));
        }

        let inner = self.inner.clone();
        let buffer = self.buffer.clone();
        let timeout = self.timeout;

        // Release GIL for the entire read loop
        py.allow_threads(move || {
            let mut result = Vec::new();

            // Check buffer first for existing newline
            {
                let mut buf = buffer.lock().unwrap();
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    result = buf.drain(..=pos).collect();
                    return Ok(result);
                }
                result.append(&mut *buf);
            }

            // Keep reading until we get a newline
            let client = inner.lock().map_err(|e| e.to_string())?;
            let mut temp = vec![0u8; 256];

            loop {
                let n = if let Some(t) = timeout {
                    client.read_timeout(&mut temp, Duration::from_secs_f64(t))
                } else {
                    client.read(&mut temp)
                };

                match n {
                    Ok(Some(n)) => {
                        let chunk = &temp[..n];
                        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
                            result.extend_from_slice(&chunk[..=pos]);
                            if pos + 1 < n {
                                let mut buf = buffer.lock().unwrap();
                                buf.extend_from_slice(&chunk[pos + 1..]);
                            }
                            return Ok(result);
                        } else {
                            result.extend_from_slice(chunk);
                        }
                    }
                    Ok(None) => return Ok(result),
                    Err(e) => return Err(e.to_string()),
                }
            }
        })
        .map_err(|e: String| PyRuntimeError::new_err(e))
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
    fn read_until(&self, py: Python<'_>, terminator: Option<Vec<u8>>) -> PyResult<Vec<u8>> {
        let terminator = terminator.unwrap_or_else(|| vec![b'\n']);
        if terminator.is_empty() {
            return Err(PyRuntimeError::new_err("Terminator cannot be empty"));
        }

        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(PyRuntimeError::new_err("Port is closed"));
        }

        let inner = self.inner.clone();
        let buffer = self.buffer.clone();
        let timeout = self.timeout;

        // Release GIL for the entire read loop
        py.allow_threads(move || {
            let mut result = Vec::new();

            // Check buffer first for existing terminator
            {
                let mut buf = buffer.lock().unwrap();
                if let Some(pos) = find_subsequence(&buf, &terminator) {
                    let end = pos + terminator.len();
                    result = buf.drain(..end).collect();
                    return Ok(result);
                }
                result.append(&mut *buf);
            }

            // Keep reading until we find terminator
            let client = inner.lock().map_err(|e| e.to_string())?;
            let mut temp = vec![0u8; 256];

            loop {
                let n = if let Some(t) = timeout {
                    client.read_timeout(&mut temp, Duration::from_secs_f64(t))
                } else {
                    client.read(&mut temp)
                };

                match n {
                    Ok(Some(n)) => {
                        result.extend_from_slice(&temp[..n]);
                        if let Some(pos) = find_subsequence(&result, &terminator) {
                            let end = pos + terminator.len();
                            if end < result.len() {
                                let mut buf = buffer.lock().unwrap();
                                buf.extend_from_slice(&result[end..]);
                            }
                            result.truncate(end);
                            return Ok(result);
                        }
                    }
                    Ok(None) => return Ok(result),
                    Err(e) => return Err(e.to_string()),
                }
            }
        })
        .map_err(|e: String| PyRuntimeError::new_err(e))
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

// pyserial constants
#[pymodule]
fn xoq_serial(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Serial>()?;

    // Parity constants
    m.add("PARITY_NONE", "N")?;
    m.add("PARITY_EVEN", "E")?;
    m.add("PARITY_ODD", "O")?;
    m.add("PARITY_MARK", "M")?;
    m.add("PARITY_SPACE", "S")?;

    // Stop bits constants
    m.add("STOPBITS_ONE", 1.0)?;
    m.add("STOPBITS_ONE_POINT_FIVE", 1.5)?;
    m.add("STOPBITS_TWO", 2.0)?;

    // Byte size constants
    m.add("FIVEBITS", 5)?;
    m.add("SIXBITS", 6)?;
    m.add("SEVENBITS", 7)?;
    m.add("EIGHTBITS", 8)?;

    Ok(())
}
