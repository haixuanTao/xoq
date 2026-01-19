//! OpenCV-compatible camera client for remote cameras over iroh P2P.
//!
//! This module provides client functionality for receiving camera frames
//! from a remote CameraServer over iroh P2P connections.
//!
//! # Example
//!
//! ```rust,no_run
//! use xoq::opencv::CameraClient;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut client = CameraClient::connect("server-id-here").await.unwrap();
//!     loop {
//!         let frame = client.read_frame().await.unwrap();
//!         println!("Got frame: {}x{}", frame.width, frame.height);
//!     }
//! }
//! ```

use crate::camera::Frame;
use crate::iroh::{IrohClientBuilder, IrohConnection};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;

const CAMERA_ALPN: &[u8] = b"xoq/camera/0";

/// A client that receives camera frames from a remote server over iroh P2P.
pub struct CameraClient {
    send: Arc<Mutex<iroh::endpoint::SendStream>>,
    recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
    _conn: IrohConnection,
}

impl CameraClient {
    /// Connect to a remote camera server.
    pub async fn connect(server_id: &str) -> Result<Self> {
        let conn = IrohClientBuilder::new()
            .alpn(CAMERA_ALPN)
            .connect_str(server_id)
            .await?;

        let stream = conn.open_stream().await?;
        let (send, recv) = stream.split();

        Ok(CameraClient {
            send: Arc::new(Mutex::new(send)),
            recv: Arc::new(Mutex::new(recv)),
            _conn: conn,
        })
    }

    /// Request and read a single frame from the server.
    pub async fn read_frame(&mut self) -> Result<Frame> {
        // Send frame request
        {
            let mut send = self.send.lock().await;
            send.write_all(b"F").await?;
        }

        // Read frame header
        let mut header = [0u8; 20];
        {
            let mut recv = self.recv.lock().await;
            recv.read_exact(&mut header).await?;
        }

        let width = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let height = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let timestamp = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let length = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);

        // Read JPEG data
        let mut jpeg_data = vec![0u8; length as usize];
        {
            let mut recv = self.recv.lock().await;
            recv.read_exact(&mut jpeg_data).await?;
        }

        // Decode JPEG to RGB
        let mut frame = Frame::from_jpeg(&jpeg_data)?;
        frame.timestamp_us = timestamp as u64;

        // Verify dimensions match
        if frame.width != width || frame.height != height {
            tracing::warn!(
                "Frame dimension mismatch: expected {}x{}, got {}x{}",
                width,
                height,
                frame.width,
                frame.height
            );
        }

        Ok(frame)
    }

    /// Read frames continuously, calling the callback for each frame.
    pub async fn read_frames<F>(&mut self, mut callback: F) -> Result<()>
    where
        F: FnMut(Frame) -> bool,
    {
        loop {
            let frame = self.read_frame().await?;
            if !callback(frame) {
                break;
            }
        }
        Ok(())
    }
}

/// Builder for creating a remote camera connection.
pub struct RemoteCameraBuilder {
    server_id: String,
}

impl RemoteCameraBuilder {
    /// Create a new builder for connecting to a remote camera.
    pub fn new(server_id: &str) -> Self {
        RemoteCameraBuilder {
            server_id: server_id.to_string(),
        }
    }

    /// Connect to the remote camera server.
    pub async fn connect(self) -> Result<CameraClient> {
        CameraClient::connect(&self.server_id).await
    }
}

/// Create a builder for connecting to a remote camera.
pub fn remote_camera(server_id: &str) -> RemoteCameraBuilder {
    RemoteCameraBuilder::new(server_id)
}
