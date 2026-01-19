//! Iroh P2P Python bindings.

use crate::{runtime, xoq_lib};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

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
