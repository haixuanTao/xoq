//! Camera server Python bindings (streams local camera over P2P).

use crate::{runtime, xoq_lib};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

/// A server that streams camera frames to remote clients over iroh P2P
#[pyclass(unsendable)]
pub struct CameraServer {
    inner: xoq_lib::CameraServer,
}

#[pymethods]
impl CameraServer {
    /// Create a new camera server
    ///
    /// Args:
    ///     camera_index: Camera index (0 for first camera)
    ///     width: Requested frame width
    ///     height: Requested frame height
    ///     fps: Requested frames per second
    ///     identity_path: Optional path to save/load server identity
    #[new]
    #[pyo3(signature = (camera_index=0, width=640, height=480, fps=30, identity_path=None))]
    fn new(
        camera_index: u32,
        width: u32,
        height: u32,
        fps: u32,
        identity_path: Option<&str>,
    ) -> PyResult<Self> {
        runtime().block_on(async {
            let server =
                xoq_lib::CameraServer::new(camera_index, width, height, fps, identity_path)
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

            Ok(CameraServer { inner: server })
        })
    }

    /// Get the server's endpoint ID
    fn id(&self) -> String {
        self.inner.id().to_string()
    }

    /// Run the camera server (blocks forever, handling connections)
    fn run(&self) -> PyResult<()> {
        runtime().block_on(async {
            self.inner
                .run()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }

    /// Handle a single client connection
    fn run_once(&self) -> PyResult<()> {
        runtime().block_on(async {
            self.inner
                .run_once()
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })
    }
}
