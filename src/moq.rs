//! MoQ transport builder
//!
//! Provides a builder API for creating MoQ clients and servers that communicate
//! via a relay server.

use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use moq_lite::Session;
use moq_native::moq_lite;
use url::Url;

/// Builder for MoQ connections
pub struct MoqBuilder {
    relay_url: String,
    token: Option<String>,
    path: String,
}

impl MoqBuilder {
    /// Create a new builder with default relay
    pub fn new() -> Self {
        Self {
            relay_url: "https://cdn.moq.dev".to_string(),
            token: None,
            path: "anon/xoq".to_string(),
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

    /// Build the full URL with optional token
    fn build_url(&self) -> Result<Url> {
        let url_str = match &self.token {
            Some(token) => format!("{}/{}?token={}", self.relay_url, self.path, token),
            None => format!("{}/{}", self.relay_url, self.path),
        };
        Ok(Url::parse(&url_str)?)
    }

    /// Connect as a duplex endpoint (can publish and subscribe)
    pub async fn connect_duplex(self) -> Result<MoqConnection> {
        let url = self.build_url()?;

        let publish_origin = moq_lite::Origin::produce();
        let subscribe_origin = moq_lite::Origin::produce();

        let mut client = moq_native::ClientConfig::default()
            .init()?
            .with_publish(publish_origin.consumer)
            .with_consume(subscribe_origin.producer);
        client.websocket.enabled = false;

        let session = Self::connect_quic_with_retry(&client, url).await?;

        Ok(MoqConnection {
            _session: session,
            publish_origin: publish_origin.producer,
            subscribe_origin: subscribe_origin.consumer,
        })
    }

    /// Connect as publisher only
    pub async fn connect_publisher(self) -> Result<MoqPublisher> {
        let url = self.build_url()?;

        let origin = moq_lite::Origin::produce();

        let mut client = moq_native::ClientConfig::default()
            .init()?
            .with_publish(origin.consumer);
        client.websocket.enabled = false;

        let session = Self::connect_quic_with_retry(&client, url).await?;

        Ok(MoqPublisher {
            _session: session,
            origin: origin.producer,
        })
    }

    /// Connect as subscriber only
    pub async fn connect_subscriber(self) -> Result<MoqSubscriber> {
        let url = self.build_url()?;
        eprintln!("[xoq] MoQ subscriber connecting to {}...", url);

        let origin = moq_lite::Origin::produce();

        let mut client = moq_native::ClientConfig::default()
            .init()?
            .with_consume(origin.producer);
        client.websocket.enabled = false;

        let session = tokio::time::timeout(
            Duration::from_secs(10),
            Self::connect_quic_with_retry(&client, url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("MoQ subscriber connection timed out after 10s"))??;

        eprintln!("[xoq] MoQ subscriber connected to relay");

        Ok(MoqSubscriber {
            origin: origin.consumer,
            _session: session,
        })
    }

    /// Connect via QUIC, retrying once if the first attempt fails (e.g. due to GSO).
    ///
    /// On Linux, the first QUIC send may fail with EIO if the NIC doesn't support
    /// UDP GSO. quinn-udp then disables GSO on the socket, so a retry succeeds.
    async fn connect_quic_with_retry(
        client: &moq_native::Client,
        url: Url,
    ) -> Result<moq_lite::Session> {
        match client.connect(url.clone()).await {
            Ok(session) => Ok(session),
            Err(first_err) => {
                eprintln!(
                    "[xoq] QUIC connect failed ({}), retrying (GSO now disabled)...",
                    first_err
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                client.connect(url).await.map_err(Into::into)
            }
        }
    }
}

impl Default for MoqBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A duplex MoQ connection that can publish and subscribe
pub struct MoqConnection {
    _session: Session,
    publish_origin: moq_lite::OriginProducer,
    subscribe_origin: moq_lite::OriginConsumer,
}

impl MoqConnection {
    /// Create a track for publishing
    pub fn create_track(&mut self, name: &str) -> MoqTrackWriter {
        let mut broadcast = moq_lite::Broadcast::produce();
        let track = broadcast.producer.create_track(moq_lite::Track {
            name: name.to_string(),
            priority: 0,
        });
        self.publish_origin
            .publish_broadcast("", broadcast.consumer);
        MoqTrackWriter {
            track,
            _broadcast: broadcast.producer,
        }
    }

    /// Wait for an announced broadcast and subscribe to a track (with timeout)
    pub async fn subscribe_track(&mut self, track_name: &str) -> Result<Option<MoqTrackReader>> {
        tracing::info!("Waiting for broadcast announcement (track: {})...", track_name);

        let announced = tokio::time::timeout(
            Duration::from_secs(10),
            self.subscribe_origin.announced(),
        )
        .await
        .map_err(|_| anyhow::anyhow!(
            "Timed out waiting for broadcast announcement after 10s. \
             Is the server publishing to this path?"
        ))?;

        if let Some((_path, Some(broadcast))) = announced {
            tracing::info!("Received broadcast announcement, subscribing to track '{}'", track_name);
            let track_info = moq_lite::Track {
                name: track_name.to_string(),
                priority: 0,
            };
            let track = broadcast.subscribe_track(&track_info);
            return Ok(Some(MoqTrackReader { track }));
        }
        tracing::warn!("Broadcast announcement returned None");
        Ok(None)
    }

    /// Get the subscribe origin for manual handling
    pub fn subscribe_origin(&mut self) -> &mut moq_lite::OriginConsumer {
        &mut self.subscribe_origin
    }

    /// Get the publish origin for manual handling
    pub fn publish_origin(&mut self) -> &mut moq_lite::OriginProducer {
        &mut self.publish_origin
    }
}

/// A publish-only MoQ connection
pub struct MoqPublisher {
    _session: Session,
    origin: moq_lite::OriginProducer,
}

impl MoqPublisher {
    /// Create a track for publishing
    pub fn create_track(&mut self, name: &str) -> MoqTrackWriter {
        let mut broadcast = moq_lite::Broadcast::produce();
        let track = broadcast.producer.create_track(moq_lite::Track {
            name: name.to_string(),
            priority: 0,
        });
        self.origin.publish_broadcast("", broadcast.consumer);
        MoqTrackWriter {
            track,
            _broadcast: broadcast.producer,
        }
    }
}

/// A subscribe-only MoQ connection
pub struct MoqSubscriber {
    _session: Session,
    origin: moq_lite::OriginConsumer,
}

impl MoqSubscriber {
    /// Wait for an announced broadcast and subscribe to a track (with timeout)
    pub async fn subscribe_track(&mut self, track_name: &str) -> Result<Option<MoqTrackReader>> {
        eprintln!("[xoq] Waiting for broadcast announcement (track: '{}')...", track_name);

        let announced = tokio::time::timeout(
            Duration::from_secs(10),
            self.origin.announced(),
        )
        .await
        .map_err(|_| anyhow::anyhow!(
            "Timed out waiting for broadcast announcement after 10s. \
             Is the server publishing to this path?"
        ))?;

        if let Some((path, Some(broadcast))) = announced {
            eprintln!("[xoq] Broadcast announced (path: '{}'), subscribing to track '{}'", path, track_name);
            let track_info = moq_lite::Track {
                name: track_name.to_string(),
                priority: 0,
            };
            let track = broadcast.subscribe_track(&track_info);
            return Ok(Some(MoqTrackReader { track }));
        }
        eprintln!("[xoq] Broadcast announcement returned None");
        Ok(None)
    }

    /// Get the origin for manual handling
    pub fn origin(&mut self) -> &mut moq_lite::OriginConsumer {
        &mut self.origin
    }
}

/// A track writer for publishing data
pub struct MoqTrackWriter {
    track: moq_lite::TrackProducer,
    // Keep the broadcast producer alive
    _broadcast: moq_lite::BroadcastProducer,
}

impl MoqTrackWriter {
    /// Write a frame of data
    pub fn write(&mut self, data: impl Into<Bytes>) {
        self.track.write_frame(data.into());
    }

    /// Write string data
    pub fn write_str(&mut self, data: &str) {
        self.write(Bytes::from(data.to_string()));
    }
}

/// A track reader for receiving data
pub struct MoqTrackReader {
    track: moq_lite::TrackConsumer,
}

impl MoqTrackReader {
    /// Read the next frame
    pub async fn read(&mut self) -> Result<Option<Bytes>> {
        if let Ok(Some(mut group)) = self.track.next_group().await {
            if let Ok(Some(frame)) = group.read_frame().await {
                return Ok(Some(frame));
            }
        }
        Ok(None)
    }

    /// Read the next frame as string
    pub async fn read_string(&mut self) -> Result<Option<String>> {
        if let Some(bytes) = self.read().await? {
            return Ok(Some(String::from_utf8_lossy(&bytes).to_string()));
        }
        Ok(None)
    }
}
