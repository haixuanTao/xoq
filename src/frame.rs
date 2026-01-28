//! Video frame type for camera streaming.
//!
//! This module provides a platform-independent Frame type that can be used
//! by both camera servers and clients without requiring camera hardware dependencies.

use anyhow::Result;

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
    /// Create a new frame with the given dimensions and data.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Self {
        Self {
            width,
            height,
            data,
            timestamp_us: 0,
        }
    }

    /// Convert frame to JPEG bytes.
    #[cfg(feature = "image")]
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
    #[cfg(feature = "image")]
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
