//! Iroh P2P transport builder
//!
//! Provides a builder API for creating P2P connections using iroh.

use anyhow::Result;
use bytes::Bytes;
use iroh::endpoint::{AckFrequencyConfig, Connection, TransportConfig, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, SecretKey};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio_util::sync::CancellationToken;

use iroh_quinn_proto::congestion;

/// A no-op congestion controller that never limits sending.
///
/// Returns u64::MAX for the window, so poll_transmit is never blocked
/// by congestion control. Use on trusted LANs where packet loss is
/// rare and latency matters more than fairness.
struct NoopController {
    mtu: u16,
}

impl congestion::Controller for NoopController {
    fn on_congestion_event(
        &mut self,
        _now: std::time::Instant,
        _sent: std::time::Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        // Ignore loss events — don't reduce the window
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu;
    }

    fn window(&self) -> u64 {
        u64::MAX
    }

    fn clone_box(&self) -> Box<dyn congestion::Controller> {
        Box::new(NoopController { mtu: self.mtu })
    }

    fn initial_window(&self) -> u64 {
        u64::MAX
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// Factory that produces [`NoopController`] instances.
struct NoopControllerFactory;

impl congestion::ControllerFactory for NoopControllerFactory {
    fn build(
        self: Arc<Self>,
        _now: std::time::Instant,
        current_mtu: u16,
    ) -> Box<dyn congestion::Controller> {
        Box::new(NoopController { mtu: current_mtu })
    }
}

/// Create a low-latency QUIC transport config for robotics use.
///
/// Key differences from quinn defaults:
/// - initial_rtt: 10ms (vs 333ms default) — realistic for LAN/direct connections
/// - ACK frequency: immediate ACKs (threshold=0, max_delay=1ms)
/// - keep_alive: 1s
fn low_latency_transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.initial_rtt(Duration::from_millis(10));
    config.keep_alive_interval(Some(Duration::from_secs(1)));

    // Request peer to ACK immediately (every packet, max 1ms delay)
    let mut ack_freq = AckFrequencyConfig::default();
    ack_freq.ack_eliciting_threshold(VarInt::from_u32(0));
    ack_freq.max_ack_delay(Some(Duration::from_millis(1)));
    config.ack_frequency_config(Some(ack_freq));

    // Disable congestion control — never block poll_transmit due to cwnd.
    // Safe on trusted LANs; avoids 100-200ms stalls when a single packet is lost.
    config.congestion_controller_factory(Arc::new(NoopControllerFactory));

    // Disable GSO (Generic Segmentation Offload) so each QUIC packet is sent as
    // its own UDP datagram immediately, rather than coalescing up to 10 packets.
    config.enable_segmentation_offload(false);

    config
}

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
            .relay_mode(RelayMode::Default)
            .transport_config(low_latency_transport_config())
            .bind()
            .await?;

        // Wait for relay registration so remote clients can discover us via NAT traversal
        endpoint.online().await;
        tracing::info!("Iroh server: relay enabled, low-latency transport config active");

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
        let endpoint = Endpoint::builder()
            .relay_mode(RelayMode::Default)
            .transport_config(low_latency_transport_config())
            .bind()
            .await?;

        tracing::info!("Iroh client: relay enabled, low-latency transport config active");

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

    /// Send an unreliable datagram over the connection.
    ///
    /// Datagrams bypass QUIC streams, ACK-based flow control, and retransmission.
    /// Ideal for latency-sensitive data where the latest value supersedes older ones.
    pub fn send_datagram(&self, data: Bytes) -> Result<()> {
        self.conn.send_datagram(data)?;
        Ok(())
    }

    /// Receive an unreliable datagram from the connection.
    pub async fn recv_datagram(&self) -> Result<Bytes> {
        Ok(self.conn.read_datagram().await?)
    }

    /// Get the maximum datagram size supported by the peer.
    ///
    /// Returns `None` if datagrams are unsupported by the peer or disabled locally.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.conn.max_datagram_size()
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

    /// Flush the write buffer.
    ///
    /// Note: quinn's SendStream::flush() is a no-op (returns Ready immediately).
    /// For actual send behavior, yield to let the connection task run.
    pub async fn flush(&mut self) -> Result<()> {
        tokio::task::yield_now().await;
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
