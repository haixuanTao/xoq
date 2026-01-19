//! OpenCV-compatible VideoCapture for remote cameras over iroh P2P.

use crate::{runtime, xoq_lib};
use numpy::{PyArray1, PyArrayMethods};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

// OpenCV property constants
pub const CAP_PROP_POS_MSEC: i32 = 0;
pub const CAP_PROP_POS_FRAMES: i32 = 1;
pub const CAP_PROP_FRAME_WIDTH: i32 = 3;
pub const CAP_PROP_FRAME_HEIGHT: i32 = 4;
pub const CAP_PROP_FPS: i32 = 5;
pub const CAP_PROP_FRAME_COUNT: i32 = 7;

// Type alias for frame data (width, height, RGB data)
type FrameData = Option<(u32, u32, Vec<u8>)>;

/// OpenCV-compatible VideoCapture for remote cameras over iroh P2P.
///
/// Drop-in replacement for cv2.VideoCapture that connects to a remote
/// camera server instead of a local device.
///
/// Example:
///     # Instead of: cap = cv2.VideoCapture(0)
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
    last_frame: Arc<std::sync::Mutex<FrameData>>,
    server_id: String,
}

#[pymethods]
impl VideoCapture {
    /// Open a connection to a remote camera server.
    ///
    /// Args:
    ///     source: The server's endpoint ID (replaces device index or URL in OpenCV)
    #[new]
    fn new(source: &str) -> PyResult<Self> {
        let server_id = source.to_string();
        let client = runtime().block_on(async {
            xoq_lib::CameraClient::connect(source)
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))
        })?;

        Ok(VideoCapture {
            inner: Arc::new(Mutex::new(Some(client))),
            is_open: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            last_frame: Arc::new(std::sync::Mutex::new(None)),
            server_id,
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

                // Store frame info for get() calls
                {
                    let mut last = self.last_frame.lock().unwrap();
                    *last = Some((frame.width, frame.height, frame.data.clone()));
                }

                // Convert RGB to BGR (OpenCV format)
                let mut bgr_data = frame.data.clone();
                for chunk in bgr_data.chunks_exact_mut(3) {
                    chunk.swap(0, 2); // Swap R and B
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

    /// Grab a frame from the remote camera (without decoding).
    ///
    /// For remote cameras, this is equivalent to requesting a frame.
    fn grab(&self) -> bool {
        if !self.is_open.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }

        let frame_result = runtime().block_on(async {
            let mut guard = self.inner.lock().await;
            if let Some(client) = guard.as_mut() {
                client.read_frame().await.ok()
            } else {
                None
            }
        });

        if let Some(frame) = frame_result {
            let mut last = self.last_frame.lock().unwrap();
            *last = Some((frame.width, frame.height, frame.data));
            true
        } else {
            false
        }
    }

    /// Retrieve the grabbed frame.
    ///
    /// Returns:
    ///     Tuple of (success: bool, frame: numpy.ndarray or None)
    fn retrieve<'py>(&self, py: Python<'py>) -> PyResult<(bool, PyObject)> {
        let frame_data = {
            let last = self.last_frame.lock().unwrap();
            last.clone()
        };

        match frame_data {
            Some((width, height, data)) => {
                let height = height as usize;
                let width = width as usize;

                // Convert RGB to BGR
                let mut bgr_data = data;
                for chunk in bgr_data.chunks_exact_mut(3) {
                    chunk.swap(0, 2);
                }

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

    /// Get a camera property.
    ///
    /// Supported properties:
    ///     - cv2.CAP_PROP_FRAME_WIDTH (3)
    ///     - cv2.CAP_PROP_FRAME_HEIGHT (4)
    ///     - cv2.CAP_PROP_FPS (5) - returns 0 for remote cameras
    ///
    /// Returns:
    ///     Property value as float (0.0 if not available)
    fn get(&self, prop_id: i32) -> f64 {
        let frame_info = {
            let last = self.last_frame.lock().unwrap();
            last.clone()
        };

        match (prop_id, frame_info) {
            (CAP_PROP_FRAME_WIDTH, Some((w, _, _))) => w as f64,
            (CAP_PROP_FRAME_HEIGHT, Some((_, h, _))) => h as f64,
            (CAP_PROP_FPS, _) => 0.0, // FPS not available for remote streams
            (CAP_PROP_FRAME_COUNT, _) => -1.0, // Live stream has no frame count
            (CAP_PROP_POS_FRAMES, _) => 0.0,
            (CAP_PROP_POS_MSEC, _) => 0.0,
            _ => 0.0,
        }
    }

    /// Set a camera property (no-op for remote cameras).
    ///
    /// Note: Property setting is not supported for remote cameras.
    /// This method exists for API compatibility with cv2.VideoCapture.
    ///
    /// Returns:
    ///     Always returns False
    fn set(&self, _prop_id: i32, _value: f64) -> bool {
        // Property setting not supported for remote streams
        false
    }

    /// Get the backend name.
    #[allow(non_snake_case)]
    fn getBackendName(&self) -> &str {
        "XOQ_IROH"
    }

    /// String representation
    fn __repr__(&self) -> String {
        format!(
            "VideoCapture(server_id='{}', opened={})",
            self.server_id,
            self.isOpened()
        )
    }

    /// Context manager enter
    fn __enter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Context manager exit
    #[pyo3(signature = (exc_type=None, exc_val=None, exc_tb=None))]
    fn __exit__(
        &self,
        exc_type: Option<&pyo3::Bound<'_, pyo3::types::PyAny>>,
        exc_val: Option<&pyo3::Bound<'_, pyo3::types::PyAny>>,
        exc_tb: Option<&pyo3::Bound<'_, pyo3::types::PyAny>>,
    ) -> bool {
        let _ = (exc_type, exc_val, exc_tb); // Suppress unused warnings
        self.release();
        false
    }
}
