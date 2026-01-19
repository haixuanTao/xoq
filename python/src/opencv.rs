//! OpenCV-compatible VideoCapture for remote cameras over iroh P2P.

use crate::{runtime, xoq_lib};
use numpy::{PyArray1, PyArrayMethods};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

/// OpenCV-compatible VideoCapture for remote cameras over iroh P2P.
///
/// Drop-in replacement for cv2.VideoCapture that connects to a remote
/// camera server instead of a local device.
///
/// Example:
///     cap = xoq.VideoCapture("server-endpoint-id")
///
///     while True:
///         ret, frame = cap.read()
///         if not ret:
///             break
///         cv2.imshow('Remote Camera', frame)
///         if cv2.waitKey(1) & 0xFF == ord('q'):
///             break
///
///     cap.release()
#[pyclass]
pub struct VideoCapture {
    inner: Arc<Mutex<Option<xoq_lib::CameraClient>>>,
    is_open: Arc<std::sync::atomic::AtomicBool>,
}

#[pymethods]
impl VideoCapture {
    /// Open a connection to a remote camera server.
    ///
    /// Args:
    ///     source: The server's endpoint ID
    #[new]
    fn new(source: &str) -> PyResult<Self> {
        let client = runtime().block_on(async {
            xoq_lib::CameraClient::connect(source)
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })?;

        Ok(VideoCapture {
            inner: Arc::new(Mutex::new(Some(client))),
            is_open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    /// Read a frame from the remote camera.
    ///
    /// Returns:
    ///     Tuple of (success: bool, frame: numpy.ndarray or None)
    ///     Frame is in BGR format (OpenCV compatible), shape (height, width, 3)
    fn read<'py>(&self, py: Python<'py>) -> PyResult<(bool, PyObject)> {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok((false, py.None()));
        }

        let frame_result = runtime().block_on(async {
            let mut guard = self.inner.lock().await;
            if let Some(client) = guard.as_mut() {
                client.read_frame().await.ok()
            } else {
                None
            }
        });

        match frame_result {
            Some(frame) => {
                let height = frame.height as usize;
                let width = frame.width as usize;

                // Convert RGB to BGR (OpenCV format)
                let mut bgr_data = frame.data;
                for chunk in bgr_data.chunks_exact_mut(3) {
                    chunk.swap(0, 2);
                }

                // Create numpy array with shape (height, width, 3)
                let array = PyArray1::from_vec_bound(py, bgr_data);
                let reshaped = array
                    .reshape([height, width, 3])
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

                Ok((true, reshaped.into_any().unbind()))
            }
            None => Ok((false, py.None())),
        }
    }

    /// Check if the connection is open.
    #[allow(non_snake_case)]
    fn isOpened(&self) -> bool {
        self.is_open.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Release the connection.
    fn release(&self) {
        self.is_open
            .store(false, std::sync::atomic::Ordering::Relaxed);
        runtime().block_on(async {
            let mut guard = self.inner.lock().await;
            *guard = None;
        });
    }
}
