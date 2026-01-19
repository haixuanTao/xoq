// Allow Arc with non-Send types since camera access is serialized through Mutex
#![allow(clippy::arc_with_non_send_sync)]

//! Camera server - streams local camera to remote clients over iroh P2P.

use crate::camera::Camera;
use crate::iroh::{IrohConnection, IrohServer, IrohServerBuilder};
use anyhow::Result;
use iroh::PublicKey;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

const CAMERA_ALPN: &[u8] = b"xoq/camera/0";

/// A server that streams camera frames to remote clients over iroh P2P.
pub struct CameraServer {
    iroh: Arc<IrohServer>,
    camera: Arc<Mutex<Camera>>,
    jpeg_quality: u8,
}

impl CameraServer {
    /// Create a new camera server.
    ///
    /// # Arguments
    ///
    /// * `camera_index` - Camera index (0 for first camera)
    /// * `width` - Requested frame width
    /// * `height` - Requested frame height
    /// * `fps` - Requested frames per second
    /// * `identity_path` - Optional path to save/load server identity
    pub async fn new(
        camera_index: u32,
        width: u32,
        height: u32,
        fps: u32,
        identity_path: Option<&str>,
    ) -> Result<Self> {
        let camera = Camera::open(camera_index, width, height, fps)?;

        let mut builder = IrohServerBuilder::new().alpn(CAMERA_ALPN);
        if let Some(path) = identity_path {
            builder = builder.identity_path(path);
        }
        let iroh = builder.bind().await?;

        Ok(CameraServer {
            iroh: Arc::new(iroh),
            camera: Arc::new(Mutex::new(camera)),
            jpeg_quality: 80,
        })
    }

    /// Set JPEG compression quality (1-100, default 80).
    pub fn set_quality(&mut self, quality: u8) {
        self.jpeg_quality = quality.clamp(1, 100);
    }

    /// Get the server's endpoint ID.
    pub fn id(&self) -> PublicKey {
        self.iroh.id()
    }

    /// Run the camera server, handling connections forever.
    pub async fn run(&self) -> Result<()> {
        loop {
            self.run_once().await?;
        }
    }

    /// Handle a single client connection.
    pub async fn run_once(&self) -> Result<()> {
        let conn = self
            .iroh
            .accept()
            .await?
            .ok_or_else(|| anyhow::anyhow!("Server closed"))?;

        tracing::info!("Client connected: {}", conn.remote_id());
        self.handle_connection(conn).await?;
        tracing::info!("Client disconnected");

        Ok(())
    }

    async fn handle_connection(&self, conn: IrohConnection) -> Result<()> {
        let stream = conn.accept_stream().await?;
        let (mut send, mut recv) = stream.split();

        let camera = self.camera.clone();
        let quality = self.jpeg_quality;

        // Read commands from client and stream frames
        let mut buf = [0u8; 32];
        loop {
            // Wait for frame request
            match recv.read(&mut buf).await {
                Ok(Some(n)) if n > 0 => {
                    // Client requested a frame
                    let frame = {
                        let mut cam = camera.lock().await;
                        cam.capture()?
                    };

                    // Compress to JPEG
                    let jpeg = frame.to_jpeg(quality)?;

                    // Send frame header: width (4) + height (4) + timestamp (8) + length (4)
                    let header = [
                        frame.width.to_le_bytes(),
                        frame.height.to_le_bytes(),
                        (frame.timestamp_us as u32).to_le_bytes(),
                        (jpeg.len() as u32).to_le_bytes(),
                    ]
                    .concat();

                    send.write_all(&header).await?;
                    send.write_all(&jpeg).await?;
                    send.flush().await?;
                }
                Ok(Some(_)) | Ok(None) => {
                    // Client disconnected
                    break;
                }
                Err(e) => {
                    tracing::debug!("Read error: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }
}
