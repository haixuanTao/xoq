//! Serial server Python bindings (bridges local serial port over P2P).

use crate::{runtime, xoq_lib};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::Arc;

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
