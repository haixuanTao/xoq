//! Iroh P2P transport builder
//!
//! Provides a builder API for creating P2P connections using iroh.

use anyhow::Result;
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use std::path::PathBuf;
use tokio::fs;
use tokio_util::sync::CancellationToken;

/// Default ALPN protocol for generic P2P communication.
pub const DEFAULT_ALPN: &[u8] = b"xoq/p2p/0";

/// ALPN protocol for camera streaming (legacy, JPEG).
pub const CAMERA_ALPN: &[u8] = b"xoq/camera/0";

/// ALPN protocol for camera streaming with JPEG frames.
pub const CAMERA_ALPN_JPEG: &[u8] = b"xoq/camera-jpeg/0";

/// ALPN protocol for camera streaming with H.264 encoded frames.
pub const CAMERA_ALPN_H264: &[u8] = b"xoq/camera-h264/0";

/// ALPN protocol for camera streaming with HEVC/H.265 encoded frames.
pub const CAMERA_ALPN_HEVC: &[u8] = b"xoq/camera-hevc/0";

/// ALPN protocol for camera streaming with AV1 encoded frames.
pub const CAMERA_ALPN_AV1: &[u8] = b"xoq/camera-av1/0";

/// Builder for iroh server (accepts connections)
pub struct IrohServerBuilder {
    key_path: Option<PathBuf>,
    secret_key: Option<SecretKey>,
    alpn: Vec<u8>,
}

impl IrohServerBuilder {
    /// Create a new server builder
    pub fn new() -> Self {
        Self {
            key_path: None,
            secret_key: None,
            alpn: DEFAULT_ALPN.to_vec(),
        }
    }

    /// Load or generate identity from a file path
    pub fn identity_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.key_path = Some(path.into());
        self
    }

    /// Use a specific secret key
    pub fn secret_key(mut self, key: SecretKey) -> Self {
        self.secret_key = Some(key);
        self
    }

    /// Set custom ALPN protocol
    pub fn alpn(mut self, alpn: &[u8]) -> Self {
        self.alpn = alpn.to_vec();
        self
    }

    /// Build and start the server
    pub async fn bind(self) -> Result<IrohServer> {
        let secret_key = match (self.secret_key, self.key_path) {
            (Some(key), _) => key,
            (None, Some(path)) => load_or_generate_key(&path).await?,
            (None, None) => SecretKey::generate(&mut rand::rng()),
        };

        let endpoint = Endpoint::builder()
            .alpns(vec![self.alpn])
            .secret_key(secret_key)
            .bind()
            .await?;

        endpoint.online().await;

        Ok(IrohServer { endpoint })
    }
}

impl Default for IrohServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for iroh client (initiates connections)
pub struct IrohClientBuilder {
    alpn: Vec<u8>,
}

impl IrohClientBuilder {
    /// Create a new client builder
    pub fn new() -> Self {
        Self {
            alpn: DEFAULT_ALPN.to_vec(),
        }
    }

    /// Set custom ALPN protocol
    pub fn alpn(mut self, alpn: &[u8]) -> Self {
        self.alpn = alpn.to_vec();
        self
    }

    /// Connect to a server by endpoint ID
    pub async fn connect(self, server_id: PublicKey) -> Result<IrohConnection> {
        let endpoint = Endpoint::builder().bind().await?;

        let addr = EndpointAddr::from(server_id);
        let conn = endpoint.connect(addr, &self.alpn).await?;

        Ok(IrohConnection {
            conn,
            _endpoint: endpoint,
            cancel_token: CancellationToken::new(),
        })
    }

    /// Connect to a server by endpoint ID string
    pub async fn connect_str(self, server_id: &str) -> Result<IrohConnection> {
        let id: PublicKey = server_id.parse()?;
        self.connect(id).await
    }
}

impl Default for IrohClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// An iroh server that accepts connections
pub struct IrohServer {
    endpoint: Endpoint,
}

impl IrohServer {
    /// Get the server's endpoint ID
    pub fn id(&self) -> PublicKey {
        self.endpoint.id()
    }

    /// Get the server's full address
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Accept an incoming connection
    pub async fn accept(&self) -> Result<Option<IrohConnection>> {
        if let Some(incoming) = self.endpoint.accept().await {
            let conn = incoming.await?;
            return Ok(Some(IrohConnection {
                conn,
                _endpoint: self.endpoint.clone(),
                cancel_token: CancellationToken::new(),
            }));
        }
        Ok(None)
    }

    /// Get the underlying endpoint for advanced usage
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

/// An iroh connection (either server or client side)
pub struct IrohConnection {
    conn: Connection,
    _endpoint: Endpoint,
    cancel_token: CancellationToken,
}

impl IrohConnection {
    /// Get the remote peer's ID
    pub fn remote_id(&self) -> PublicKey {
        self.conn.remote_id()
    }

    /// Open a bidirectional stream
    pub async fn open_stream(&self) -> Result<IrohStream> {
        let (send, recv) = self.conn.open_bi().await?;
        Ok(IrohStream { send, recv })
    }

    /// Accept a bidirectional stream from the remote peer
    pub async fn accept_stream(&self) -> Result<IrohStream> {
        let (send, recv) = self.conn.accept_bi().await?;
        Ok(IrohStream { send, recv })
    }

    /// Get the underlying connection for advanced usage
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Get a cancellation token that is cancelled when this connection is dropped.
    ///
    /// Use this to gracefully shut down spawned tasks when the connection ends.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }
}

impl Drop for IrohConnection {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

/// A bidirectional stream
pub struct IrohStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
}

impl IrohStream {
    /// Write data to the stream
    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.send.write_all(data).await?;
        Ok(())
    }

    /// Flush the write buffer
    pub async fn flush(&mut self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        self.send.flush().await?;
        Ok(())
    }

    /// Write a string to the stream
    pub async fn write_str(&mut self, data: &str) -> Result<()> {
        self.write(data.as_bytes()).await
    }

    /// Read data from the stream
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>> {
        Ok(self.recv.read(buf).await?)
    }

    /// Read a string from the stream (up to buffer size)
    pub async fn read_string(&mut self) -> Result<Option<String>> {
        let mut buf = vec![0u8; 4096];
        if let Some(n) = self.read(&mut buf).await? {
            return Ok(Some(String::from_utf8_lossy(&buf[..n]).to_string()));
        }
        Ok(None)
    }

    /// Split into separate send and receive halves
    pub fn split(self) -> (iroh::endpoint::SendStream, iroh::endpoint::RecvStream) {
        (self.send, self.recv)
    }
}

async fn load_or_generate_key(path: &PathBuf) -> Result<SecretKey> {
    if path.exists() {
        let bytes = fs::read(path).await?;
        let key_bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid key file"))?;
        Ok(SecretKey::from_bytes(&key_bytes))
    } else {
        let key = SecretKey::generate(&mut rand::rng());
        fs::write(path, key.to_bytes()).await?;
        Ok(key)
    }
}

/// Generate a new secret key
pub fn generate_key() -> SecretKey {
    SecretKey::generate(&mut rand::rng())
}

/// Save a secret key to a file
pub async fn save_key(key: &SecretKey, path: impl Into<PathBuf>) -> Result<()> {
    fs::write(path.into(), key.to_bytes()).await?;
    Ok(())
}

/// Load a secret key from a file
pub async fn load_key(path: impl Into<PathBuf>) -> Result<SecretKey> {
    let bytes = fs::read(path.into()).await?;
    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid key file"))?;
    Ok(SecretKey::from_bytes(&key_bytes))
}
