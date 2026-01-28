//! OpenCV-compatible camera client for remote cameras.
//!
//! Supports both iroh P2P and MoQ relay transports.
//!
//! # Example
//!
//! ```rust,no_run
//! use xoq::opencv::{CameraClient, CameraClientBuilder};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Using iroh (P2P)
//!     let mut client = CameraClient::connect("server-id-here").await.unwrap();
//!
//!     // Using MoQ (relay)
//!     let mut client = CameraClientBuilder::new()
//!         .moq("anon/my-camera")
//!         .connect()
//!         .await
//!         .unwrap();
//!
//!     loop {
//!         let frame = client.read_frame().await.unwrap();
//!         println!("Got frame: {}x{}", frame.width, frame.height);
//!     }
//! }
//! ```

use crate::frame::Frame;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::Mutex;

const CAMERA_ALPN: &[u8] = b"xoq/camera/0";

/// Transport type for camera client.
#[derive(Clone)]
pub enum Transport {
    /// Iroh P2P connection
    Iroh { server_id: String },
    /// MoQ relay connection
    Moq {
        path: String,
        relay_url: Option<String>,
    },
}

/// Builder for creating a camera client.
pub struct CameraClientBuilder {
    transport: Option<Transport>,
}

impl CameraClientBuilder {
    /// Create a new camera client builder.
    pub fn new() -> Self {
        Self { transport: None }
    }

    /// Use iroh P2P transport.
    pub fn iroh(mut self, server_id: &str) -> Self {
        self.transport = Some(Transport::Iroh {
            server_id: server_id.to_string(),
        });
        self
    }

    /// Use MoQ relay transport.
    pub fn moq(mut self, path: &str) -> Self {
        self.transport = Some(Transport::Moq {
            path: path.to_string(),
            relay_url: None,
        });
        self
    }

    /// Use MoQ relay transport with custom relay URL.
    pub fn moq_with_relay(mut self, path: &str, relay_url: &str) -> Self {
        self.transport = Some(Transport::Moq {
            path: path.to_string(),
            relay_url: Some(relay_url.to_string()),
        });
        self
    }

    /// Connect to the camera server.
    pub async fn connect(self) -> Result<CameraClient> {
        let transport = self
            .transport
            .ok_or_else(|| anyhow::anyhow!("Transport not specified"))?;

        let inner = match transport {
            Transport::Iroh { server_id } => {
                use crate::iroh::IrohClientBuilder;

                let conn = IrohClientBuilder::new()
                    .alpn(CAMERA_ALPN)
                    .connect_str(&server_id)
                    .await?;

                let stream = conn.open_stream().await?;
                let (_send, recv) = stream.split();

                CameraClientInner::Iroh {
                    recv: Arc::new(Mutex::new(recv)),
                    _conn: conn,
                }
            }
            Transport::Moq { path, relay_url } => {
                use crate::moq::MoqBuilder;

                let mut builder = MoqBuilder::new().path(&path);
                if let Some(url) = &relay_url {
                    builder = builder.relay(url);
                }
                let mut conn = builder.connect_subscriber().await?;

                // Wait for the camera track
                let track = conn
                    .subscribe_track("camera")
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Camera track not found"))?;

                CameraClientInner::Moq { track, _conn: conn }
            }
        };

        Ok(CameraClient { inner })
    }
}

impl Default for CameraClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

enum CameraClientInner {
    Iroh {
        recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
        _conn: crate::iroh::IrohConnection,
    },
    Moq {
        track: crate::moq::MoqTrackReader,
        _conn: crate::moq::MoqSubscriber,
    },
}

/// A client that receives camera frames from a remote server.
pub struct CameraClient {
    inner: CameraClientInner,
}

impl CameraClient {
    /// Connect to a remote camera server using iroh (legacy API).
    pub async fn connect(server_id: &str) -> Result<Self> {
        CameraClientBuilder::new().iroh(server_id).connect().await
    }

    /// Request and read a single frame from the server.
    pub async fn read_frame(&mut self) -> Result<Frame> {
        match &mut self.inner {
            CameraClientInner::Iroh { recv, .. } => {
                // Read frame header and data (server streams continuously)
                let (width, height, timestamp, jpeg_data) = {
                    let mut recv = recv.lock().await;

                    let mut header = [0u8; 20];
                    recv.read_exact(&mut header).await?;

                    let width = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                    let height = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
                    let timestamp = u64::from_le_bytes([
                        header[8], header[9], header[10], header[11], header[12], header[13],
                        header[14], header[15],
                    ]);
                    let length =
                        u32::from_le_bytes([header[16], header[17], header[18], header[19]]);

                    let mut jpeg_data = vec![0u8; length as usize];
                    recv.read_exact(&mut jpeg_data).await?;

                    (width, height, timestamp, jpeg_data)
                };

                // Decode JPEG to RGB
                let mut frame = Frame::from_jpeg(&jpeg_data)?;
                frame.timestamp_us = timestamp;

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
            CameraClientInner::Moq { track, .. } => {
                // Read frame from MoQ track with retry logic
                let mut retries = 0;
                let data = loop {
                    match track.read().await? {
                        Some(data) => break data,
                        None => {
                            retries += 1;
                            if retries > 200 {
                                anyhow::bail!("No frame available after retries");
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                    }
                };

                if data.len() < 12 {
                    anyhow::bail!("Invalid frame data");
                }

                let width = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let height = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let timestamp = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                let jpeg_data = &data[12..];

                let mut frame = Frame::from_jpeg(jpeg_data)?;
                frame.timestamp_us = timestamp as u64;

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
        }
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

/// Builder for creating a remote camera connection (legacy).
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

/// Create a builder for connecting to a remote camera (legacy).
pub fn remote_camera(server_id: &str) -> RemoteCameraBuilder {
    RemoteCameraBuilder::new(server_id)
}
