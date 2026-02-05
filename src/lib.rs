//! xoq - X-Embodiment over QUIC
//!
//! A library for building P2P and relay-based communication using either
//! MoQ (Media over QUIC) or iroh for direct peer-to-peer connections.
//!
//! # Examples
//!
//! ## MoQ (via relay)
//!
//! ```no_run
//! use xoq::moq::MoqBuilder;
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Simple anonymous connection
//! let mut conn = MoqBuilder::new()
//!     .path("anon/my-channel")
//!     .connect_duplex()
//!     .await?;
//!
//! // With authentication
//! let mut conn = MoqBuilder::new()
//!     .path("secure/my-channel")
//!     .token("your-jwt-token")
//!     .connect_duplex()
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Iroh (P2P)
//!
//! ```no_run
//! use xoq::iroh::{IrohServerBuilder, IrohClientBuilder};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Server with persistent identity
//! let server = IrohServerBuilder::new()
//!     .identity_path(".my_server_key")
//!     .bind()
//!     .await?;
//! println!("Server ID: {}", server.id());
//!
//! // Client connecting to server
//! let conn = IrohClientBuilder::new()
//!     .connect_str("server-endpoint-id-here")
//!     .await?;
//! # Ok(())
//! # }
//! ```

pub mod cmaf;
pub mod moq;

#[cfg(feature = "iroh")]
pub mod iroh;

// Frame type (available when image feature is enabled)
#[cfg(feature = "image")]
pub mod frame;

#[cfg(feature = "serial")]
pub mod serial;

#[cfg(all(feature = "serial", feature = "iroh"))]
pub mod serial_server;

// Serial client - available with serial-remote OR (serial + iroh)
#[cfg(any(feature = "serial-remote", all(feature = "serial", feature = "iroh")))]
pub mod serialport_impl;

// Sync serial client (blocking API with internal runtime)
#[cfg(any(feature = "serial-remote", all(feature = "serial", feature = "iroh")))]
pub mod serial_client;

/// `serialport`-compatible module for remote serial ports.
///
/// This module provides a drop-in compatible API with the `serialport` crate.
///
/// # Example
///
/// ```no_run
/// // Instead of: use serialport;
/// use xoq::serialport;
///
/// // Same API as serialport crate
/// let mut port = serialport::new("server-endpoint-id").open()?;
/// # Ok::<(), anyhow::Error>(())
/// ```
#[cfg(any(feature = "serial-remote", all(feature = "serial", feature = "iroh")))]
pub mod serialport {
    pub use crate::serialport_impl::{new, Client, RemoteSerialPort, SerialPortBuilder, Transport};
}

#[cfg(feature = "camera")]
pub mod camera;

#[cfg(feature = "camera-macos")]
pub mod camera_macos;

#[cfg(feature = "vtenc")]
pub mod vtenc;

#[cfg(all(feature = "camera", feature = "iroh"))]
pub mod camera_server;

// Camera client - available with camera-remote OR (camera + iroh)
#[cfg(any(feature = "camera-remote", all(feature = "camera", feature = "iroh")))]
pub mod opencv;

// Sync camera client (blocking API with internal runtime)
#[cfg(any(feature = "camera-remote", all(feature = "camera", feature = "iroh")))]
pub mod camera_client;

// Platform-independent CAN types (always available)
pub mod can_types;

// Local CAN support using socketcan (Linux only)
#[cfg(feature = "can")]
pub mod can;

// CAN server requires both local CAN and iroh
#[cfg(all(feature = "can", feature = "iroh"))]
pub mod can_server;

// Remote CAN client (cross-platform, requires iroh)
#[cfg(feature = "can-remote")]
pub mod socketcan_impl;

/// `socketcan`-compatible module for remote CAN sockets.
///
/// This module provides a drop-in compatible API similar to the `socketcan` crate.
/// Available cross-platform when the `can-remote` feature is enabled.
///
/// # Example
///
/// ```no_run
/// use xoq::socketcan;
///
/// // Connect to remote CAN interface
/// let mut socket = socketcan::new("server-endpoint-id").open()?;
///
/// // Write a frame
/// let frame = socketcan::CanFrame::new(0x123, &[1, 2, 3])?;
/// socket.write_frame(&frame)?;
///
/// // Read frames
/// if let Some(frame) = socket.read_frame()? {
///     println!("Received: ID={:x}", frame.id());
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
#[cfg(feature = "can-remote")]
pub mod socketcan {
    pub use crate::socketcan_impl::{
        new, AnyCanFrame, CanBusSocket, CanClient, CanFdFlags, CanFdFrame, CanFrame,
        CanInterfaceInfo, CanSocketBuilder, RemoteCanSocket, Transport,
    };
}

// Re-export commonly used types
pub use moq::{
    MoqBuilder, MoqConnection, MoqPublisher, MoqStream, MoqSubscriber, MoqTrackReader,
    MoqTrackWriter,
};

#[cfg(feature = "iroh")]
pub use iroh::{
    IrohClientBuilder, IrohConnection, IrohServer, IrohServerBuilder, IrohStream, CAMERA_ALPN,
    CAMERA_ALPN_AV1, CAMERA_ALPN_H264, CAMERA_ALPN_HEVC, CAMERA_ALPN_JPEG, DEFAULT_ALPN,
};

#[cfg(feature = "serial")]
pub use serial::{
    baud, list_ports, DataBits, Parity, PortType, SerialConfig, SerialPort, SerialPortInfo,
    SerialReader, SerialWriter, StopBits,
};

#[cfg(all(feature = "serial", feature = "iroh"))]
pub use serial_server::Server;

#[cfg(any(feature = "serial-remote", all(feature = "serial", feature = "iroh")))]
pub use serialport::{Client, RemoteSerialPort};

#[cfg(any(feature = "serial-remote", all(feature = "serial", feature = "iroh")))]
pub use serial_client::SyncSerialClient;

// Frame type (available with image feature or camera feature)
#[cfg(feature = "image")]
pub use frame::Frame;

#[cfg(feature = "camera")]
pub use camera::{list_cameras, Camera, CameraInfo};

#[cfg(feature = "camera-macos")]
pub use camera_macos::{
    list_cameras as list_cameras_macos, Camera as CameraMacos, CameraInfo as CameraInfoMacos,
};

// Re-export Frame from camera module when camera is enabled (for backwards compat)
#[cfg(all(feature = "camera", not(feature = "image")))]
pub use camera::Frame;

#[cfg(all(feature = "camera", feature = "iroh"))]
pub use camera_server::{CameraServer, CameraServerBuilder};

#[cfg(any(feature = "camera-remote", all(feature = "camera", feature = "iroh")))]
pub use opencv::{remote_camera, CameraClient, CameraClientBuilder};

#[cfg(feature = "videotoolbox")]
pub use opencv::VtDecoder;

#[cfg(any(feature = "camera-remote", all(feature = "camera", feature = "iroh")))]
pub use camera_client::SyncCameraClient;

// Platform-independent CAN types (always available)
pub use can_types::{
    wire as can_wire, AnyCanFrame, CanBusSocket, CanFdFlags, CanFdFrame, CanFrame, CanInterfaceInfo,
};

// Local CAN support (Linux only)
#[cfg(feature = "can")]
pub use can::{list_interfaces, CanConfig, CanReader, CanSocket, CanWriter};

#[cfg(all(feature = "can", feature = "iroh"))]
pub use can_server::CanServer;

// Remote CAN client (cross-platform)
#[cfg(feature = "can-remote")]
pub use socketcan::{CanClient, RemoteCanSocket};

// Re-export token generation
pub use moq_token;
