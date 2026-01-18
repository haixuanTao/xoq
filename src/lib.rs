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

pub mod moq;

#[cfg(feature = "iroh")]
pub mod iroh;

#[cfg(feature = "serial")]
pub mod serial;

#[cfg(all(feature = "serial", feature = "iroh"))]
pub mod serial_bridge;

// Re-export commonly used types
pub use moq::{
    MoqBuilder, MoqConnection, MoqPublisher, MoqSubscriber, MoqTrackReader, MoqTrackWriter,
};

#[cfg(feature = "iroh")]
pub use iroh::{IrohClientBuilder, IrohConnection, IrohServer, IrohServerBuilder, IrohStream};

#[cfg(feature = "serial")]
pub use serial::{
    baud, list_ports, DataBits, Parity, PortType, SerialConfig, SerialPort, SerialPortInfo,
    SerialReader, SerialWriter, StopBits,
};

#[cfg(all(feature = "serial", feature = "iroh"))]
pub use serial_bridge::{Client, RemoteSerialPort, Server};

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
#[cfg(all(feature = "serial", feature = "iroh"))]
pub mod serialport {
    pub use crate::serial_bridge::{new, RemoteSerialPort, SerialPortBuilder, Transport};
}

// Re-export token generation
pub use moq_token;
