//! wser - WebTransport Serial Library
//!
//! A library for building P2P and relay-based communication using either
//! MoQ (Media over QUIC) or iroh for direct peer-to-peer connections.
//!
//! # Examples
//!
//! ## MoQ (via relay)
//!
//! ```no_run
//! use wser::moq::MoqBuilder;
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
//! use wser::iroh::{IrohServerBuilder, IrohClientBuilder};
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

// Re-export commonly used types
pub use moq::{MoqBuilder, MoqConnection, MoqPublisher, MoqSubscriber, MoqTrackReader, MoqTrackWriter};

#[cfg(feature = "iroh")]
pub use iroh::{IrohClientBuilder, IrohConnection, IrohServer, IrohServerBuilder, IrohStream};

// Re-export token generation
pub use moq_token;
