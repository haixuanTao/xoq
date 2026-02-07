//! sounddevice-compatible interface to remote audio over P2P.
//!
//! This module provides a `sounddevice`-like API for connecting to
//! remote audio servers over iroh P2P or MoQ relay.
//!
//! # Example
//!
//! ```no_run
//! use xoq::sounddevice;
//!
//! let mut stream = sounddevice::new("server-endpoint-id")
//!     .sample_rate(48000)
//!     .channels(1)
//!     .open()?;
//!
//! // Read audio from remote microphone
//! let frame = stream.read_chunk()?;
//!
//! // Send audio to remote speaker
//! stream.write_chunk(&frame)?;
//! # Ok::<(), anyhow::Error>(())
//! ```

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::audio::{AudioConfig, AudioFrame, SampleFormat, WIRE_HEADER_SIZE};
use crate::iroh::IrohClientBuilder;

/// Transport type for the audio connection.
#[derive(Clone)]
pub enum Transport {
    /// Iroh P2P connection (default)
    Iroh { alpn: Option<Vec<u8>> },
    /// MoQ relay connection
    Moq {
        relay: String,
        token: Option<String>,
        insecure: bool,
    },
}

impl Default for Transport {
    fn default() -> Self {
        Transport::Iroh { alpn: None }
    }
}

/// Builder for creating a remote audio stream connection.
pub struct AudioStreamBuilder {
    source: String,
    sample_rate: u32,
    channels: u16,
    sample_format: SampleFormat,
    transport: Transport,
    timeout: Duration,
}

impl AudioStreamBuilder {
    /// Create a new audio stream builder.
    pub fn new(source: &str) -> Self {
        // Auto-detect transport: "/" in source → MoQ, else → iroh
        let transport = if source.contains('/') {
            Transport::Moq {
                relay: "https://cdn.moq.dev".to_string(),
                token: None,
                insecure: false,
            }
        } else {
            Transport::default()
        };

        Self {
            source: source.to_string(),
            sample_rate: 48000,
            channels: 1,
            sample_format: SampleFormat::I16,
            transport,
            timeout: Duration::from_secs(5),
        }
    }

    /// Set sample rate (default: 48000).
    pub fn sample_rate(mut self, rate: u32) -> Self {
        self.sample_rate = rate;
        self
    }

    /// Set number of channels (default: 1).
    pub fn channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }

    /// Set sample format (default: I16).
    pub fn sample_format(mut self, format: SampleFormat) -> Self {
        self.sample_format = format;
        self
    }

    /// Use MoQ relay transport.
    pub fn with_moq(mut self, relay: &str) -> Self {
        self.transport = Transport::Moq {
            relay: relay.to_string(),
            token: None,
            insecure: false,
        };
        self
    }

    /// Disable TLS verification for MoQ (for self-signed certs).
    pub fn insecure(mut self) -> Self {
        if let Transport::Moq {
            insecure: ref mut i,
            ..
        } = self.transport
        {
            *i = true;
        }
        self
    }

    /// Set the connection timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Open the connection to the remote audio server.
    pub fn open(self) -> Result<RemoteAudioStream> {
        let runtime = tokio::runtime::Runtime::new()?;
        let config = AudioConfig {
            sample_rate: self.sample_rate,
            channels: self.channels,
            sample_format: self.sample_format,
        };

        let client = match self.transport {
            Transport::Iroh { alpn } => runtime.block_on(async {
                let mut builder = IrohClientBuilder::new();
                if let Some(alpn) = alpn {
                    builder = builder.alpn(&alpn);
                } else {
                    builder = builder.alpn(crate::audio_server::AUDIO_ALPN);
                }
                let conn = builder.connect_str(&self.source).await?;
                let mut stream = conn.open_stream().await?;
                // Write a handshake byte to trigger the QUIC STREAM frame.
                // Without this, the server's accept_bi() never returns because
                // QUIC only notifies the peer about a new stream when data is sent.
                stream.write(&[0u8]).await?;
                let (send, recv) = stream.split();
                Ok::<_, anyhow::Error>(ClientInner::Iroh {
                    send: Arc::new(Mutex::new(send)),
                    recv: Arc::new(Mutex::new(recv)),
                    _conn: conn,
                })
            })?,
            Transport::Moq {
                relay,
                token: _,
                insecure,
            } => runtime.block_on(async {
                use crate::moq::MoqBuilder;

                let mut builder = MoqBuilder::new().path(&self.source);
                if insecure {
                    builder = builder.disable_tls_verify();
                }
                builder = builder.relay(&relay);

                // Subscribe to mic track (server → client)
                let mut subscriber = builder.clone().connect_subscriber().await?;
                let mic_reader = subscriber.subscribe_track("mic").await?;

                // Publish to speaker track (client → server)
                let mut publisher = builder.connect_publisher().await?;
                let speaker_writer = publisher.create_track("speaker");

                Ok::<_, anyhow::Error>(ClientInner::Moq {
                    mic_reader,
                    speaker_writer,
                    _subscriber: subscriber,
                    _publisher: publisher,
                })
            })?,
        };

        Ok(RemoteAudioStream {
            client,
            runtime,
            config,
            timeout: self.timeout,
            source: self.source,
        })
    }
}

/// Internal client representation supporting multiple transports.
enum ClientInner {
    Iroh {
        send: Arc<Mutex<iroh::endpoint::SendStream>>,
        recv: Arc<Mutex<iroh::endpoint::RecvStream>>,
        _conn: crate::iroh::IrohConnection,
    },
    Moq {
        mic_reader: Option<crate::moq::MoqTrackReader>,
        speaker_writer: crate::moq::MoqTrackWriter,
        _subscriber: crate::moq::MoqSubscriber,
        _publisher: crate::moq::MoqPublisher,
    },
}

/// Create a new remote audio stream builder.
///
/// This function mimics `sounddevice`-style API.
pub fn new(source: &str) -> AudioStreamBuilder {
    AudioStreamBuilder::new(source)
}

/// A remote audio stream providing bidirectional audio I/O.
pub struct RemoteAudioStream {
    client: ClientInner,
    runtime: tokio::runtime::Runtime,
    config: AudioConfig,
    timeout: Duration,
    source: String,
}

impl RemoteAudioStream {
    /// Read an audio chunk from the remote microphone.
    pub fn read_chunk(&mut self) -> Result<AudioFrame> {
        let timeout = self.timeout;
        self.runtime.block_on(async {
            match &mut self.client {
                ClientInner::Iroh { recv, .. } => {
                    let mut r = recv.lock().await;
                    let mut header_buf = [0u8; WIRE_HEADER_SIZE];
                    tokio::time::timeout(timeout, r.read_exact(&mut header_buf))
                        .await
                        .map_err(|_| anyhow::anyhow!("Read timeout"))??;

                    let (config, frame_count, timestamp_us, data_length) =
                        AudioFrame::decode_header(&header_buf)?;
                    let mut data = vec![0u8; data_length as usize];
                    r.read_exact(&mut data).await?;

                    Ok(AudioFrame {
                        data,
                        frame_count,
                        timestamp_us,
                        config,
                    })
                }
                ClientInner::Moq { mic_reader, .. } => {
                    if let Some(reader) = mic_reader {
                        let data = tokio::time::timeout(timeout, async {
                            loop {
                                match reader.read().await? {
                                    Some(data) => return Ok::<_, anyhow::Error>(data),
                                    None => {
                                        tokio::time::sleep(Duration::from_millis(1)).await;
                                    }
                                }
                            }
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("Read timeout"))??;

                        AudioFrame::decode_moq(&data)
                    } else {
                        anyhow::bail!("No mic track available")
                    }
                }
            }
        })
    }

    /// Write an audio chunk to the remote speaker.
    pub fn write_chunk(&mut self, frame: &AudioFrame) -> Result<()> {
        self.runtime.block_on(async {
            match &mut self.client {
                ClientInner::Iroh { send, .. } => {
                    let mut s = send.lock().await;
                    let header = frame.encode_header();
                    s.write_all(&header).await?;
                    s.write_all(&frame.data).await?;
                    drop(s);
                    tokio::task::yield_now().await;
                    Ok(())
                }
                ClientInner::Moq { speaker_writer, .. } => {
                    let data = frame.encode_moq();
                    speaker_writer.write(data);
                    Ok(())
                }
            }
        })
    }

    /// Record audio for a given number of frames, returning f32 samples.
    pub fn record(&mut self, frames: usize) -> Result<Vec<f32>> {
        let mut samples = Vec::with_capacity(frames * self.config.channels as usize);
        let channels = self.config.channels as usize;

        while samples.len() < frames * channels {
            let frame = self.read_chunk()?;
            match frame.config.sample_format {
                SampleFormat::I16 => {
                    for chunk in frame.data.chunks_exact(2) {
                        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
                        samples.push(s as f32 / 32768.0);
                    }
                }
                SampleFormat::F32 => {
                    for chunk in frame.data.chunks_exact(4) {
                        samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                    }
                }
            }
        }

        samples.truncate(frames * channels);
        Ok(samples)
    }

    /// Play f32 audio samples to the remote speaker.
    pub fn play(&mut self, data: &[f32]) -> Result<()> {
        let channels = self.config.channels as usize;
        let chunk_frames = (self.config.sample_rate as usize * 20) / 1000; // 20ms chunks
        let chunk_samples = chunk_frames * channels;

        for chunk in data.chunks(chunk_samples) {
            let pcm_data = match self.config.sample_format {
                SampleFormat::I16 => {
                    let mut bytes = Vec::with_capacity(chunk.len() * 2);
                    for &s in chunk {
                        let i = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        bytes.extend_from_slice(&i.to_le_bytes());
                    }
                    bytes
                }
                SampleFormat::F32 => {
                    let mut bytes = Vec::with_capacity(chunk.len() * 4);
                    for &s in chunk {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    bytes
                }
            };

            let frame = AudioFrame {
                data: pcm_data,
                frame_count: (chunk.len() / channels) as u32,
                timestamp_us: 0,
                config: self.config.clone(),
            };
            self.write_chunk(&frame)?;
        }
        Ok(())
    }

    /// Get the audio config.
    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// Get the source identifier.
    pub fn source(&self) -> &str {
        &self.source
    }
}
