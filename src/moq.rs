//! MoQ transport builder
//!
//! Provides a builder API for creating MoQ clients and servers that communicate
//! via a relay server using moq-native/moq-lite.
//!
//! NOTE: cdn.moq.dev does NOT forward announcements between separate sessions.
//! For cross-session pub/sub, run your own moq-relay server.

use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use moq_native::moq_lite::{self, Broadcast, Origin, Track};
use url::Url;

/// Builder for MoQ connections
#[derive(Clone)]
pub struct MoqBuilder {
    relay_url: String,
    token: Option<String>,
    path: String,
    disable_tls_verify: bool,
}

impl MoqBuilder {
    /// Create a new builder with default relay (cdn.moq.dev)
    pub fn new() -> Self {
        Self {
            relay_url: "https://cdn.moq.dev".to_string(),
            token: None,
            path: "anon/xoq".to_string(),
            disable_tls_verify: false,
        }
    }

    /// Set the relay URL
    pub fn relay(mut self, url: &str) -> Self {
        self.relay_url = url.to_string();
        self
    }

    /// Set the path on the relay
    pub fn path(mut self, path: &str) -> Self {
        self.path = path.to_string();
        self
    }

    /// Set authentication token (JWT)
    pub fn token(mut self, token: &str) -> Self {
        self.token = Some(token.to_string());
        self
    }

    /// Disable TLS certificate verification (for self-signed certs)
    pub fn disable_tls_verify(mut self) -> Self {
        self.disable_tls_verify = true;
        self
    }

    fn build_url(&self) -> Result<Url> {
        let url_str = match &self.token {
            Some(token) => format!("{}/{}?token={}", self.relay_url, self.path, token),
            None => format!("{}/{}", self.relay_url, self.path),
        };
        Ok(Url::parse(&url_str)?)
    }

    fn create_client(&self) -> Result<moq_native::Client> {
        let mut config = moq_native::ClientConfig::default();
        if self.disable_tls_verify {
            config.tls.disable_verify = Some(true);
        }
        config.init()
    }

    /// Connect as a duplex endpoint (can publish and subscribe on the same session).
    pub async fn connect_duplex(self) -> Result<MoqConnection> {
        let url = self.build_url()?;
        let client = self.create_client()?;

        let origin = Origin::produce();
        let broadcast = Broadcast::produce();
        origin
            .producer
            .publish_broadcast("", broadcast.consumer.clone());

        let session = client
            .with_publish(origin.consumer.clone())
            .with_consume(origin.producer.clone())
            .connect(url)
            .await?;

        Ok(MoqConnection {
            broadcast: broadcast.producer,
            origin_consumer: origin.consumer,
            _session: session,
        })
    }

    /// Connect as publisher only
    pub async fn connect_publisher(self) -> Result<MoqPublisher> {
        let url = self.build_url()?;
        let client = self.create_client()?;

        let origin = Origin::produce();
        let broadcast = Broadcast::produce();
        origin
            .producer
            .publish_broadcast("", broadcast.consumer.clone());

        let session = client.with_publish(origin.consumer).connect(url).await?;

        Ok(MoqPublisher {
            broadcast: broadcast.producer,
            session,
        })
    }

    /// Connect as publisher and create a track in one step.
    ///
    /// Creates the track BEFORE connecting to avoid race conditions where
    /// a subscriber's request arrives before the track is registered.
    pub async fn connect_publisher_with_track(
        self,
        track_name: &str,
    ) -> Result<(MoqPublisher, MoqTrackWriter)> {
        let url = self.build_url()?;
        let client = self.create_client()?;

        let origin = Origin::produce();
        let mut broadcast = Broadcast::produce();

        // Create track BEFORE connecting to avoid race with incoming subscribes
        let track_producer = broadcast.producer.create_track(Track::new(track_name));
        let writer = MoqTrackWriter {
            track: track_producer,
            group: None,
        };

        origin
            .producer
            .publish_broadcast("", broadcast.consumer.clone());

        let session = client.with_publish(origin.consumer).connect(url).await?;

        Ok((
            MoqPublisher {
                broadcast: broadcast.producer,
                session,
            },
            writer,
        ))
    }

    /// Connect as subscriber only.
    ///
    /// Note: Cross-session subscription only works with relays that forward
    /// announcements (not cdn.moq.dev).
    pub async fn connect_subscriber(self) -> Result<MoqSubscriber> {
        let url = self.build_url()?;
        let client = self.create_client()?;
        let origin = Origin::produce();

        let session = client.with_consume(origin.producer).connect(url).await?;

        Ok(MoqSubscriber {
            origin_consumer: origin.consumer,
            _session: session,
        })
    }
}

impl Default for MoqBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A duplex MoQ connection that can publish and subscribe
pub struct MoqConnection {
    broadcast: moq_lite::BroadcastProducer,
    origin_consumer: moq_lite::OriginConsumer,
    _session: moq_lite::Session,
}

impl MoqConnection {
    /// Create a track for publishing
    pub fn create_track(&mut self, name: &str) -> MoqTrackWriter {
        let track_producer = self.broadcast.create_track(Track::new(name));
        MoqTrackWriter {
            track: track_producer,
            group: None,
        }
    }

    /// Subscribe to a track by name (waits for announce).
    pub async fn subscribe_track(&mut self, track_name: &str) -> Result<Option<MoqTrackReader>> {
        let broadcast = match tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_broadcast(&mut self.origin_consumer),
        )
        .await
        {
            Ok(Some(broadcast)) => broadcast,
            Ok(None) => return Ok(None),
            Err(_) => anyhow::bail!("Timed out waiting for broadcast announcement (10s)"),
        };

        let track_consumer = broadcast.subscribe_track(&Track::new(track_name));
        Ok(Some(MoqTrackReader {
            track: track_consumer,
            group: None,
        }))
    }
}

/// A publish-only MoQ connection
pub struct MoqPublisher {
    broadcast: moq_lite::BroadcastProducer,
    session: moq_lite::Session,
}

impl MoqPublisher {
    /// Create a track for publishing
    pub fn create_track(&mut self, name: &str) -> MoqTrackWriter {
        let track_producer = self.broadcast.create_track(Track::new(name));
        MoqTrackWriter {
            track: track_producer,
            group: None,
        }
    }

    /// Returns a future that resolves when the MoQ session closes.
    pub async fn closed(&self) -> Result<(), moq_lite::Error> {
        self.session.closed().await
    }
}

/// A subscribe-only MoQ connection
pub struct MoqSubscriber {
    origin_consumer: moq_lite::OriginConsumer,
    _session: moq_lite::Session,
}

impl MoqSubscriber {
    /// Subscribe to a track by name (waits for announce from relay).
    pub async fn subscribe_track(&mut self, track_name: &str) -> Result<Option<MoqTrackReader>> {
        let broadcast = match tokio::time::timeout(
            Duration::from_secs(10),
            wait_for_broadcast(&mut self.origin_consumer),
        )
        .await
        {
            Ok(Some(broadcast)) => broadcast,
            Ok(None) => return Ok(None),
            Err(_) => anyhow::bail!("Timed out waiting for broadcast announcement (10s)"),
        };

        let track_consumer = broadcast.subscribe_track(&Track::new(track_name));
        Ok(Some(MoqTrackReader {
            track: track_consumer,
            group: None,
        }))
    }
}

async fn wait_for_broadcast(
    origin_consumer: &mut moq_lite::OriginConsumer,
) -> Option<moq_lite::BroadcastConsumer> {
    loop {
        match origin_consumer.announced().await {
            Some((_path, Some(broadcast))) => return Some(broadcast),
            Some((_path, None)) => continue,
            None => return None,
        }
    }
}

/// A track writer for publishing data
pub struct MoqTrackWriter {
    track: moq_lite::TrackProducer,
    /// Persistent group for stream-mode writes (reliable, ordered delivery)
    group: Option<moq_lite::GroupProducer>,
}

impl MoqTrackWriter {
    /// Write a frame as an independent group (suitable for state/broadcast).
    ///
    /// Each call creates a new group. Slow consumers will skip to the latest
    /// group, dropping intermediate frames. Good for state where you only
    /// care about the latest value.
    pub fn write(&mut self, data: impl Into<Bytes>) {
        self.track.write_frame(data.into());
    }

    /// Write a frame to a persistent group (suitable for commands/streams).
    ///
    /// All frames go into a single long-lived group, so the consumer reads
    /// every frame in order without skipping. Good for commands where every
    /// frame matters.
    pub fn write_stream(&mut self, data: impl Into<Bytes>) {
        let group = self.group.get_or_insert_with(|| self.track.append_group());
        group.write_frame(data.into());
    }

    /// Write string data
    pub fn write_str(&mut self, data: &str) {
        self.write(Bytes::from(data.to_string()));
    }
}

/// A track reader for receiving data
pub struct MoqTrackReader {
    track: moq_lite::TrackConsumer,
    /// Current group being read (drains all frames before moving to next)
    group: Option<moq_lite::GroupConsumer>,
}

impl MoqTrackReader {
    /// Read the next frame.
    ///
    /// Reads all frames from the current group before moving to the next.
    /// This handles both per-group writes (state) and persistent-group
    /// writes (commands) correctly.
    pub async fn read(&mut self) -> Result<Option<Bytes>> {
        loop {
            // Try to read from current group first
            if let Some(group) = &mut self.group {
                match group.read_frame().await {
                    Ok(Some(data)) => return Ok(Some(data)),
                    Ok(None) => {
                        // Group exhausted, move to next
                        self.group = None;
                    }
                    Err(e) => return Err(anyhow::anyhow!("Read error: {}", e)),
                }
            }

            // Get next group
            match self.track.next_group().await {
                Ok(Some(group)) => {
                    self.group = Some(group);
                }
                Ok(None) => return Ok(None),
                Err(e) => return Err(anyhow::anyhow!("Group error: {}", e)),
            }
        }
    }

    /// Read the next frame as string
    pub async fn read_string(&mut self) -> Result<Option<String>> {
        if let Some(bytes) = self.read().await? {
            return Ok(Some(String::from_utf8_lossy(&bytes).to_string()));
        }
        Ok(None)
    }
}

/// A simple bidirectional stream over MoQ.
///
/// Uses separate publisher and subscriber connections on sub-paths for each direction.
/// This enables cross-session communication through a relay.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> anyhow::Result<()> {
/// use xoq::MoqStream;
///
/// // Server side:
/// let mut stream = MoqStream::accept_at_insecure("https://relay:4443", "anon/test").await?;
///
/// // Client side:
/// let mut stream = MoqStream::connect_to_insecure("https://relay:4443", "anon/test").await?;
///
/// // Both sides:
/// stream.write("hello");
/// let data = stream.read().await?;
/// # Ok(())
/// # }
/// ```
pub struct MoqStream {
    writer: MoqTrackWriter,
    reader: MoqTrackReader,
    _publisher: MoqPublisher,
    _subscriber: MoqSubscriber,
}

impl MoqStream {
    /// Connect as client to a relay. Publishes on `path/c2s`, subscribes to `path/s2c`.
    pub async fn connect_to(relay: &str, path: &str) -> Result<Self> {
        Self::connect_internal(relay, path, false).await
    }

    /// Connect as client with TLS verification disabled (for self-signed certs).
    pub async fn connect_to_insecure(relay: &str, path: &str) -> Result<Self> {
        Self::connect_internal(relay, path, true).await
    }

    async fn connect_internal(relay: &str, path: &str, insecure: bool) -> Result<Self> {
        let mut builder = MoqBuilder::new().relay(relay);
        if insecure {
            builder = builder.disable_tls_verify();
        }

        let pub_builder = builder.clone().path(&format!("{}/c2s", path));
        let sub_builder = builder.path(&format!("{}/s2c", path));

        // Connect publisher and subscriber in parallel
        let (pub_result, sub_result) = tokio::join!(
            pub_builder.connect_publisher_with_track("data"),
            Self::connect_and_subscribe_retry(sub_builder, "data")
        );

        let (publisher, writer) = pub_result?;
        let (subscriber, reader) = sub_result?;

        Ok(Self {
            writer,
            reader,
            _publisher: publisher,
            _subscriber: subscriber,
        })
    }

    /// Connect subscriber with retry loop - keeps reconnecting until broadcast is found.
    async fn connect_and_subscribe_retry(
        builder: MoqBuilder,
        track_name: &str,
    ) -> Result<(MoqSubscriber, MoqTrackReader)> {
        let track = track_name.to_string();
        loop {
            let mut subscriber = builder.clone().connect_subscriber().await?;

            // Try to subscribe with short timeout
            match tokio::time::timeout(Duration::from_secs(2), subscriber.subscribe_track(&track))
                .await
            {
                Ok(Ok(Some(reader))) => return Ok((subscriber, reader)),
                Ok(Ok(None)) => {
                    // Broadcast ended, retry
                    tracing::debug!("Broadcast ended, reconnecting...");
                }
                Ok(Err(e)) => {
                    tracing::debug!("Subscribe error: {}, reconnecting...", e);
                }
                Err(_) => {
                    // Timeout waiting for broadcast, retry
                    tracing::debug!("No broadcast yet, reconnecting...");
                }
            }

            // Brief pause before retry
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Accept as server at a relay. Publishes on `path/s2c`, subscribes to `path/c2s`.
    pub async fn accept_at(relay: &str, path: &str) -> Result<Self> {
        Self::accept_internal(relay, path, false).await
    }

    /// Accept as server with TLS verification disabled (for self-signed certs).
    pub async fn accept_at_insecure(relay: &str, path: &str) -> Result<Self> {
        Self::accept_internal(relay, path, true).await
    }

    async fn accept_internal(relay: &str, path: &str, insecure: bool) -> Result<Self> {
        let mut builder = MoqBuilder::new().relay(relay);
        if insecure {
            builder = builder.disable_tls_verify();
        }

        let pub_builder = builder.clone().path(&format!("{}/s2c", path));
        let sub_builder = builder.path(&format!("{}/c2s", path));

        // Connect publisher and subscriber in parallel
        let (pub_result, sub_result) = tokio::join!(
            pub_builder.connect_publisher_with_track("data"),
            Self::connect_and_subscribe_retry(sub_builder, "data")
        );

        let (publisher, writer) = pub_result?;
        let (subscriber, reader) = sub_result?;

        Ok(Self {
            writer,
            reader,
            _publisher: publisher,
            _subscriber: subscriber,
        })
    }

    /// Write data.
    pub fn write(&mut self, data: impl Into<Bytes>) {
        self.writer.write(data);
    }

    /// Read the next frame.
    pub async fn read(&mut self) -> Result<Option<Bytes>> {
        self.reader.read().await
    }
}
