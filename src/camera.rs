//! Camera capture functionality.
//!
//! This module provides cross-platform camera access using the nokhwa library.
//! Frames are captured and can be streamed over P2P connections.
//!
//! # Example
//!
//! ```rust,no_run
//! use xoq::camera::{Camera, list_cameras};
//!
//! // List available cameras
//! let cameras = list_cameras().unwrap();
//! for cam in &cameras {
//!     println!("Camera: {} (index {})", cam.name, cam.index);
//! }
//!
//! // Open a camera
//! let mut camera = Camera::open(0, 640, 480, 30).unwrap();
//!
//! // Capture a frame
//! let frame = camera.capture().unwrap();
//! println!("Frame: {}x{}, {} bytes", frame.width, frame.height, frame.data.len());
//! ```

use anyhow::Result;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::Camera as NokhwaCamera;

/// Information about an available camera.
#[derive(Debug, Clone)]
pub struct CameraInfo {
    /// Camera index (used to open the camera).
    pub index: u32,
    /// Human-readable camera name.
    pub name: String,
    /// Camera description/vendor info.
    pub description: String,
}

/// A captured video frame.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Raw RGB data (3 bytes per pixel, row-major).
    pub data: Vec<u8>,
    /// Frame timestamp in microseconds since capture start.
    pub timestamp_us: u64,
}

impl Frame {
    /// Convert frame to JPEG bytes.
    pub fn to_jpeg(&self, quality: u8) -> Result<Vec<u8>> {
        use image::{ImageBuffer, Rgb};

        let img: ImageBuffer<Rgb<u8>, _> =
            ImageBuffer::from_raw(self.width, self.height, self.data.clone())
                .ok_or_else(|| anyhow::anyhow!("Failed to create image buffer"))?;

        let mut jpeg_data = Vec::new();
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_data, quality);
        encoder.encode_image(&img)?;

        Ok(jpeg_data)
    }

    /// Create a frame from JPEG bytes.
    pub fn from_jpeg(jpeg_data: &[u8]) -> Result<Self> {
        use image::ImageReader;
        use std::io::Cursor;

        let img = ImageReader::new(Cursor::new(jpeg_data))
            .with_guessed_format()?
            .decode()?
            .to_rgb8();

        Ok(Frame {
            width: img.width(),
            height: img.height(),
            data: img.into_raw(),
            timestamp_us: 0,
        })
    }
}

/// A camera capture device.
pub struct Camera {
    inner: NokhwaCamera,
    start_time: std::time::Instant,
}

impl Camera {
    /// Open a camera by index with specified resolution and framerate.
    ///
    /// # Arguments
    ///
    /// * `index` - Camera index (0 for first camera)
    /// * `width` - Requested frame width
    /// * `height` - Requested frame height
    /// * `fps` - Requested frames per second
    pub fn open(index: u32, width: u32, height: u32, fps: u32) -> Result<Self> {
        let camera_index = CameraIndex::Index(index);
        let format = CameraFormat::new(Resolution::new(width, height), FrameFormat::MJPEG, fps);
        let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(format));

        let mut camera = NokhwaCamera::new(camera_index, requested)?;
        camera.open_stream()?;

        Ok(Camera {
            inner: camera,
            start_time: std::time::Instant::now(),
        })
    }

    /// Get the actual frame width.
    pub fn width(&self) -> u32 {
        self.inner.resolution().width()
    }

    /// Get the actual frame height.
    pub fn height(&self) -> u32 {
        self.inner.resolution().height()
    }

    /// Get the actual framerate.
    pub fn fps(&self) -> u32 {
        self.inner.frame_rate()
    }

    /// Capture a single frame.
    pub fn capture(&mut self) -> Result<Frame> {
        let frame = self.inner.frame()?;
        let decoded = frame.decode_image::<RgbFormat>()?;
        let timestamp_us = self.start_time.elapsed().as_micros() as u64;

        Ok(Frame {
            width: decoded.width(),
            height: decoded.height(),
            data: decoded.into_raw(),
            timestamp_us,
        })
    }

    /// Stop the camera stream.
    pub fn stop(&mut self) -> Result<()> {
        self.inner.stop_stream()?;
        Ok(())
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        let _ = self.inner.stop_stream();
    }
}

/// List all available cameras.
pub fn list_cameras() -> Result<Vec<CameraInfo>> {
    use nokhwa::utils::CameraInfo as NokhwaCameraInfo;

    let cameras = nokhwa::query(nokhwa::utils::ApiBackend::Auto)?;

    Ok(cameras
        .into_iter()
        .map(|c: NokhwaCameraInfo| CameraInfo {
            index: match c.index() {
                CameraIndex::Index(i) => *i,
                CameraIndex::String(_) => 0,
            },
            name: c.human_name().to_string(),
            description: c.description().to_string(),
        })
        .collect())
}
