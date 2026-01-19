//! MoQ (Media over QUIC) Python bindings.

use crate::{runtime, xoq_lib};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

/// A duplex MoQ connection that can publish and subscribe
#[pyclass]
pub struct MoqConnection {
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
pub struct MoqPublisher {
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
pub struct MoqSubscriber {
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
pub struct MoqTrackWriter {
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
pub struct MoqTrackReader {
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
