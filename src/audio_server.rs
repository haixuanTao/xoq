//! Audio server - bridges local mic/speaker to remote clients over iroh P2P.
//!
//! Supports bidirectional audio: captures from mic and sends to client,
//! receives from client and plays to speaker.
//!
//! On macOS with the `audio-macos` feature, supports Voice Processing IO
//! for built-in AEC, noise suppression, and AGC.

use anyhow::Result;
use std::sync::Arc;

use crate::audio::{
    AudioConfig, AudioFrame, AudioInput, AudioOutput, SampleFormat, WIRE_HEADER_SIZE,
};
use crate::iroh::{IrohConnection, IrohServerBuilder};

/// ALPN protocol for audio streaming.
pub const AUDIO_ALPN: &[u8] = b"xoq/audio-pcm/0";

/// Transport type for audio server.
#[derive(Clone)]
pub enum Transport {
    /// Iroh P2P (direct connection)
    Iroh { identity_path: Option<String> },
    /// MoQ relay
    Moq {
        path: String,
        relay_url: Option<String>,
    },
}

impl Default for Transport {
    fn default() -> Self {
        Transport::Iroh {
            identity_path: None,
        }
    }
}

/// Builder for creating an audio server.
pub struct AudioServerBuilder {
    input_device: Option<usize>,
    output_device: Option<usize>,
    sample_rate: u32,
    channels: u16,
    sample_format: SampleFormat,
    chunk_duration_ms: u32,
    transport: Transport,
    #[cfg(feature = "audio-macos")]
    use_vpio: bool,
}

impl AudioServerBuilder {
    /// Create a new audio server builder with defaults.
    pub fn new() -> Self {
        Self {
            input_device: None,
            output_device: None,
            sample_rate: 48000,
            channels: 1,
            sample_format: SampleFormat::I16,
            chunk_duration_ms: 20,
            transport: Transport::default(),
            #[cfg(feature = "audio-macos")]
            use_vpio: true,
        }
    }

    /// Set input (microphone) device index.
    pub fn input_device(mut self, index: usize) -> Self {
        self.input_device = Some(index);
        self
    }

    /// Set output (speaker) device index.
    pub fn output_device(mut self, index: usize) -> Self {
        self.output_device = Some(index);
        self
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

    /// Set chunk duration in milliseconds (default: 20ms).
    pub fn chunk_duration_ms(mut self, ms: u32) -> Self {
        self.chunk_duration_ms = ms;
        self
    }

    /// Use iroh P2P transport (default).
    pub fn iroh(mut self) -> Self {
        self.transport = Transport::Iroh {
            identity_path: None,
        };
        self
    }

    /// Use iroh P2P transport with persistent identity.
    pub fn iroh_with_identity(mut self, path: &str) -> Self {
        self.transport = Transport::Iroh {
            identity_path: Some(path.to_string()),
        };
        self
    }

    /// Use MoQ relay transport.
    pub fn moq(mut self, path: &str) -> Self {
        self.transport = Transport::Moq {
            path: path.to_string(),
            relay_url: None,
        };
        self
    }

    /// Use MoQ relay transport with custom relay URL.
    pub fn moq_with_relay(mut self, path: &str, relay_url: &str) -> Self {
        self.transport = Transport::Moq {
            path: path.to_string(),
            relay_url: Some(relay_url.to_string()),
        };
        self
    }

    /// Use Voice Processing IO on macOS (AEC, noise suppression, AGC).
    #[cfg(feature = "audio-macos")]
    pub fn use_vpio(mut self, enable: bool) -> Self {
        self.use_vpio = enable;
        self
    }

    /// Build the audio server.
    pub async fn build(self) -> Result<AudioServer> {
        let config = AudioConfig {
            sample_rate: self.sample_rate,
            channels: self.channels,
            sample_format: self.sample_format,
        };

        #[cfg(feature = "audio-macos")]
        let backend = if self.use_vpio {
            let vpio = crate::audio_macos::AudioVoiceIO::open(config.clone())?;
            AudioBackend::VoiceProcessing(vpio)
        } else {
            Self::build_separate_backend(&self, &config)?
        };

        #[cfg(not(feature = "audio-macos"))]
        let backend = Self::build_separate_backend(&self, &config)?;

        let inner = match self.transport {
            Transport::Iroh { identity_path } => {
                let mut builder = IrohServerBuilder::new().alpn(AUDIO_ALPN);
                if let Some(path) = identity_path {
                    builder = builder.identity_path(&path);
                }
                let server = builder.bind().await?;
                let id = server.id().to_string();

                AudioServerInner::Iroh {
                    server: Arc::new(server),
                    id,
                }
            }
            Transport::Moq { path, relay_url } => {
                use crate::moq::MoqBuilder;

                let mut builder = MoqBuilder::new().path(&path);
                if let Some(url) = &relay_url {
                    builder = builder.relay(url);
                }
                let mut publisher = builder.connect_publisher().await?;
                let mic_track = publisher.create_track("mic");

                AudioServerInner::Moq {
                    mic_track,
                    path: path.clone(),
                    _publisher: publisher,
                }
            }
        };

        Ok(AudioServer {
            backend,
            config,
            inner,
        })
    }

    fn build_separate_backend(&self, config: &AudioConfig) -> Result<AudioBackend> {
        let input = match self.input_device {
            Some(idx) => AudioInput::open_index(idx, config.clone())?,
            None => AudioInput::open(config.clone())?,
        };

        let output = match self.output_device {
            Some(idx) => Some(AudioOutput::open_index(idx, config.clone())?),
            None => AudioOutput::open(config.clone()).ok(),
        };

        Ok(AudioBackend::Separate { input, output })
    }
}

impl Default for AudioServerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

enum AudioBackend {
    Separate {
        input: AudioInput,
        output: Option<AudioOutput>,
    },
    #[cfg(feature = "audio-macos")]
    VoiceProcessing(crate::audio_macos::AudioVoiceIO),
}

enum AudioServerInner {
    Iroh {
        server: Arc<crate::iroh::IrohServer>,
        id: String,
    },
    Moq {
        mic_track: crate::moq::MoqTrackWriter,
        path: String,
        _publisher: crate::moq::MoqPublisher,
    },
}

/// A server that bridges local audio devices to remote clients.
pub struct AudioServer {
    backend: AudioBackend,
    config: AudioConfig,
    inner: AudioServerInner,
}

impl AudioServer {
    /// Get the server's ID (iroh endpoint ID or MoQ path).
    pub fn id(&self) -> String {
        match &self.inner {
            AudioServerInner::Iroh { id, .. } => id.clone(),
            AudioServerInner::Moq { path, .. } => path.clone(),
        }
    }

    /// Get the audio config.
    pub fn config(&self) -> &AudioConfig {
        &self.config
    }

    /// Run the audio server (blocks forever, handling connections).
    pub async fn run(&mut self) -> Result<()> {
        match &mut self.inner {
            AudioServerInner::Iroh { server, .. } => {
                let server = server.clone();
                let mut active_cancel: Option<tokio_util::sync::CancellationToken> = None;
                let mut active_task: Option<tokio::task::JoinHandle<()>> = None;

                loop {
                    let conn = match server.accept().await {
                        Ok(Some(c)) => c,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::warn!("Accept error (retrying): {}", e);
                            continue;
                        }
                    };

                    tracing::info!("Audio client connected: {}", conn.remote_id());

                    // Cancel previous connection if still active
                    if let Some(cancel) = active_cancel.take() {
                        tracing::info!("Disconnecting previous audio client");
                        cancel.cancel();
                    }
                    if let Some(task) = active_task.take() {
                        // Timeout so a stuck mic thread doesn't block new connections
                        match tokio::time::timeout(std::time::Duration::from_secs(3), task).await {
                            Ok(_) => {}
                            Err(_) => {
                                tracing::warn!("Previous handler cleanup timed out, proceeding");
                            }
                        }
                    }

                    let external_cancel = tokio_util::sync::CancellationToken::new();
                    active_cancel = Some(external_cancel.clone());

                    // Spawn connection handler so we can accept new connections immediately.
                    // We pass raw pointers (as usize) because AudioInput/AudioOutput/AudioVoiceIO
                    // contain non-Send types. Safety: the backend lives in AudioServer which
                    // outlives the task, and previous task is cancelled+awaited first.
                    let config = self.config.clone();
                    match &self.backend {
                        AudioBackend::Separate { input, output } => {
                            let input_ptr = input as *const AudioInput as usize;
                            let output_ptr =
                                output.as_ref().map(|o| o as *const AudioOutput as usize);
                            let cancel = external_cancel;
                            active_task = Some(tokio::spawn(async move {
                                if let Err(e) = handle_iroh_connection_separate(
                                    input_ptr, output_ptr, conn, cancel, config,
                                )
                                .await
                                {
                                    tracing::error!("Audio connection error: {}", e);
                                }
                                tracing::info!("Audio client disconnected");
                            }));
                        }
                        #[cfg(feature = "audio-macos")]
                        AudioBackend::VoiceProcessing(vpio) => {
                            let vpio_ptr = vpio as *const crate::audio_macos::AudioVoiceIO as usize;
                            let cancel = external_cancel;
                            active_task = Some(tokio::spawn(async move {
                                if let Err(e) =
                                    handle_iroh_connection_vpio(vpio_ptr, conn, cancel, config)
                                        .await
                                {
                                    tracing::error!("Audio connection error: {}", e);
                                }
                                tracing::info!("Audio client disconnected");
                            }));
                        }
                    }
                }
            }
            AudioServerInner::Moq { mic_track, .. } => match &self.backend {
                AudioBackend::Separate { input, .. } => loop {
                    let frame = input.read()?;
                    let data = frame.encode_moq();
                    mic_track.write(data);
                },
                #[cfg(feature = "audio-macos")]
                AudioBackend::VoiceProcessing(vpio) => loop {
                    let frame = vpio.read()?;
                    let data = frame.encode_moq();
                    mic_track.write(data);
                },
            },
        }
    }
}

/// Handle an iroh connection using separate AudioInput/AudioOutput (cpal backend).
///
/// Takes raw pointers as usize to satisfy Send bounds for tokio::spawn.
/// Safety: callers must ensure the pointed-to AudioInput/AudioOutput outlive this future.
async fn handle_iroh_connection_separate(
    input_ptr: usize,
    output_ptr: Option<usize>,
    conn: IrohConnection,
    external_cancel: tokio_util::sync::CancellationToken,
    config: AudioConfig,
) -> Result<()> {
    let stream = tokio::select! {
        result = conn.accept_stream() => result?,
        _ = external_cancel.cancelled() => {
            tracing::info!("Connection cancelled while waiting for stream");
            return Ok(());
        }
    };
    let (mut send, mut recv) = stream.split();

    // Send an initial silence frame so the client knows the connection is alive,
    // even before the mic thread produces real audio data.
    let silence = make_silence_frame(&config);
    let header = silence.encode_header();
    send.write_all(&header).await?;
    send.write_all(&silence.data).await?;
    tracing::debug!("Sent initial silence frame to client");

    let cancel_token = conn.cancellation_token();

    // Task: mic → network (read from AudioInput, write to stream)
    // AudioInput::read() is blocking, so we use a dedicated thread
    let input_rx = {
        let (tx, rx) = tokio::sync::mpsc::channel::<AudioFrame>(32);
        let cancel = cancel_token.clone();
        let ext_cancel = external_cancel.clone();
        std::thread::spawn(move || {
            // Safety: input_ptr is valid for the lifetime of this task
            let input = unsafe { &*(input_ptr as *const AudioInput) };
            loop {
                if cancel.is_cancelled() || ext_cancel.is_cancelled() {
                    break;
                }
                // Use try_read (non-blocking) so we can check cancellation tokens
                // promptly. Blocking read() would hang forever if no mic data
                // is produced (e.g. headless server without mic permission).
                match input.try_read() {
                    Some(frame) => {
                        if tx.blocking_send(frame).is_err() {
                            break;
                        }
                    }
                    None => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        });
        rx
    };

    let cancel_clone = cancel_token.clone();
    let ext_clone = external_cancel.clone();
    let mic_to_net = tokio::spawn(async move {
        let mut rx = input_rx;
        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => break,
                _ = ext_clone.cancelled() => break,
                frame = rx.recv() => {
                    match frame {
                        Some(frame) => {
                            let header = frame.encode_header();
                            if send.write_all(&header).await.is_err() {
                                break;
                            }
                            if send.write_all(&frame.data).await.is_err() {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        None => break,
                    }
                }
            }
        }
    });

    // Main task: network → speaker
    if let Some(out_ptr) = output_ptr {
        let mut header_buf = [0u8; WIRE_HEADER_SIZE];
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = external_cancel.cancelled() => break,
                result = recv.read_exact(&mut header_buf) => {
                    match result {
                        Ok(()) => {
                            let (config, frame_count, timestamp_us, data_length) =
                                AudioFrame::decode_header(&header_buf)?;
                            let mut data = vec![0u8; data_length as usize];
                            recv.read_exact(&mut data).await?;
                            let frame = AudioFrame {
                                data,
                                frame_count,
                                timestamp_us,
                                config,
                            };
                            // Momentary reference — no await between creation and use
                            let output = unsafe { &*(out_ptr as *const AudioOutput) };
                            if let Err(e) = output.write(&frame) {
                                tracing::debug!("Audio output write error: {}", e);
                            }
                        }
                        Err(e) => {
                            tracing::info!("Audio client disconnected: {}", e);
                            break;
                        }
                    }
                }
            }
        }
    } else {
        // No output device — just drain incoming data
        let mut buf = vec![0u8; 4096];
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = external_cancel.cancelled() => break,
                result = recv.read(&mut buf) => {
                    match result {
                        Ok(Some(0)) | Ok(None) | Err(_) => break,
                        _ => {}
                    }
                }
            }
        }
    }

    cancel_token.cancel();
    let _ = mic_to_net.await;
    Ok(())
}

/// Create a silence frame (20ms of zeros) for the given audio config.
fn make_silence_frame(config: &AudioConfig) -> AudioFrame {
    let frame_count = (config.sample_rate * 20) / 1000; // 20ms
    let data_len =
        frame_count as usize * config.channels as usize * config.sample_format.bytes_per_sample();
    AudioFrame {
        data: vec![0u8; data_len],
        frame_count,
        timestamp_us: 0,
        config: config.clone(),
    }
}

/// Handle an iroh connection using VPIO backend (macOS Voice Processing IO).
///
/// Takes raw pointer as usize to satisfy Send bounds for tokio::spawn.
/// Safety: callers must ensure the pointed-to AudioVoiceIO outlives this future.
#[cfg(feature = "audio-macos")]
async fn handle_iroh_connection_vpio(
    vpio_ptr: usize,
    conn: IrohConnection,
    external_cancel: tokio_util::sync::CancellationToken,
    config: AudioConfig,
) -> Result<()> {
    let stream = tokio::select! {
        result = conn.accept_stream() => result?,
        _ = external_cancel.cancelled() => {
            tracing::info!("Connection cancelled while waiting for stream");
            return Ok(());
        }
    };
    let (mut send, mut recv) = stream.split();

    // Send an initial silence frame so the client knows the connection is alive,
    // even before the mic thread produces real audio data.
    let silence = make_silence_frame(&config);
    let header = silence.encode_header();
    send.write_all(&header).await?;
    send.write_all(&silence.data).await?;
    tracing::debug!("Sent initial silence frame to client");

    let cancel_token = conn.cancellation_token();

    // Task: VPIO mic → network
    // AudioVoiceIO::read() is blocking, so we use a dedicated thread
    let input_rx = {
        let (tx, rx) = tokio::sync::mpsc::channel::<AudioFrame>(32);
        let cancel = cancel_token.clone();
        let ext_cancel = external_cancel.clone();
        std::thread::spawn(move || {
            // Safety: vpio_ptr is valid for the lifetime of this task
            let vpio = unsafe { &*(vpio_ptr as *const crate::audio_macos::AudioVoiceIO) };
            loop {
                if cancel.is_cancelled() || ext_cancel.is_cancelled() {
                    break;
                }
                // Use try_read (non-blocking) so we can check cancellation tokens
                // promptly. Blocking read() would hang forever if no mic data
                // is produced (e.g. headless server without mic permission).
                match vpio.try_read() {
                    Some(frame) => {
                        if tx.blocking_send(frame).is_err() {
                            break;
                        }
                    }
                    None => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                }
            }
        });
        rx
    };

    let cancel_clone = cancel_token.clone();
    let ext_clone = external_cancel.clone();
    let mic_to_net = tokio::spawn(async move {
        let mut rx = input_rx;
        loop {
            tokio::select! {
                _ = cancel_clone.cancelled() => break,
                _ = ext_clone.cancelled() => break,
                frame = rx.recv() => {
                    match frame {
                        Some(frame) => {
                            let header = frame.encode_header();
                            if send.write_all(&header).await.is_err() {
                                break;
                            }
                            if send.write_all(&frame.data).await.is_err() {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        None => break,
                    }
                }
            }
        }
    });

    // Main task: network → VPIO speaker (with AEC reference)
    let mut header_buf = [0u8; WIRE_HEADER_SIZE];
    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => break,
            _ = external_cancel.cancelled() => break,
            result = recv.read_exact(&mut header_buf) => {
                match result {
                    Ok(()) => {
                        let (_config, _frame_count, _timestamp_us, data_length) =
                            AudioFrame::decode_header(&header_buf)?;
                        let mut data = vec![0u8; data_length as usize];
                        recv.read_exact(&mut data).await?;
                        // Momentary reference — no await between creation and use
                        let vpio =
                            unsafe { &*(vpio_ptr as *const crate::audio_macos::AudioVoiceIO) };
                        if let Err(e) = vpio.write_raw(data) {
                            tracing::debug!("VPIO output write error: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::info!("Audio client disconnected: {}", e);
                        break;
                    }
                }
            }
        }
    }

    cancel_token.cancel();
    let _ = mic_to_net.await;
    Ok(())
}
