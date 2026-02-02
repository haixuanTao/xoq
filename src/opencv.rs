//! OpenCV-compatible camera client for remote cameras.
//!
//! Supports both iroh P2P and MoQ relay transports.
//! Automatically negotiates H.264 (with NVDEC hardware decode) or JPEG encoding.
//!
//! # Example
//!
//! ```rust,no_run
//! use xoq::opencv::{CameraClient, CameraClientBuilder};
//!
//! #[tokio::main]
//! async fn main() {
//!     // Using iroh (P2P) - auto-negotiates H.264 or JPEG
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

// ============================================================================
// Annex B Parser (shared, no feature gate)
// ============================================================================

/// Parse an Annex B byte stream into individual NAL units.
/// Returns a list of (nal_type, nal_bytes) pairs, where nal_bytes excludes the start code.
#[cfg_attr(not(any(feature = "videotoolbox", test)), allow(dead_code))]
fn parse_annex_b(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut nals = Vec::new();
    let mut i = 0;
    let len = data.len();

    // Find first start code
    while i < len {
        if i + 3 < len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            i += 4;
            break;
        } else if i + 2 < len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            i += 3;
            break;
        }
        i += 1;
    }

    let mut nal_start = i;

    while i < len {
        // Look for next start code
        let found_start_code = if i + 3 < len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            Some(4)
        } else if i + 2 < len && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            Some(3)
        } else {
            None
        };

        if let Some(sc_len) = found_start_code {
            // End of current NAL
            let nal_data = &data[nal_start..i];
            if !nal_data.is_empty() {
                let nal_type = nal_data[0] & 0x1F;
                nals.push((nal_type, nal_data.to_vec()));
            }
            i += sc_len;
            nal_start = i;
        } else {
            i += 1;
        }
    }

    // Last NAL
    if nal_start < len {
        let nal_data = &data[nal_start..len];
        if !nal_data.is_empty() {
            let nal_type = nal_data[0] & 0x1F;
            nals.push((nal_type, nal_data.to_vec()));
        }
    }

    nals
}

/// Convert Annex B H.264 data to AVCC format.
/// Returns (avcc_bytes_without_param_sets, Option<sps>, Option<pps>).
/// SPS/PPS are extracted separately (VideoToolbox wants them in the format description, not in sample data).
#[cfg_attr(not(any(feature = "videotoolbox", test)), allow(dead_code))]
fn annex_b_to_avcc(data: &[u8]) -> (Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>) {
    let nals = parse_annex_b(data);
    let mut avcc = Vec::new();
    let mut sps = None;
    let mut pps = None;

    for (nal_type, nal_data) in &nals {
        match nal_type {
            7 => {
                // SPS
                sps = Some(nal_data.clone());
            }
            8 => {
                // PPS
                pps = Some(nal_data.clone());
            }
            _ => {
                // Slice data - convert to AVCC (4-byte big-endian length prefix)
                let len = nal_data.len() as u32;
                avcc.extend_from_slice(&len.to_be_bytes());
                avcc.extend_from_slice(nal_data);
            }
        }
    }

    (avcc, sps, pps)
}

// ============================================================================
// CMAF Parsing (for H.264 over MoQ)
// ============================================================================

/// Parse a CMAF init segment to extract SPS, PPS, width, and height from the avcC box.
#[cfg_attr(not(any(feature = "videotoolbox", feature = "nvenc")), allow(dead_code))]
fn parse_cmaf_init_segment(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>, u32, u32)> {
    parse_cmaf_init_boxes(data)
        .ok_or_else(|| anyhow::anyhow!("avcC box not found in CMAF init segment"))
}

/// Recursively search MP4 boxes for the avcC box.
fn parse_cmaf_init_boxes(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>, u32, u32)> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let box_size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let box_type = &data[pos + 4..pos + 8];

        if box_size == 0 || pos + box_size > data.len() {
            break;
        }

        if box_type == b"moov" || box_type == b"trak" || box_type == b"mdia"
           || box_type == b"minf" || box_type == b"stbl" {
            if let Some(result) = parse_cmaf_init_boxes(&data[pos + 8..pos + box_size]) {
                return Some(result);
            }
        } else if box_type == b"stsd" {
            // Full box: version(1) + flags(3) + entry_count(4) = 8 extra bytes
            if pos + 16 <= data.len() {
                if let Some(result) = parse_cmaf_init_boxes(&data[pos + 16..pos + box_size]) {
                    return Some(result);
                }
            }
        } else if box_type == b"avc1" {
            // avc1 sample entry layout (after 8-byte box header):
            //   6 bytes reserved, 2 bytes data_ref_index,
            //   2 bytes pre_defined, 2 bytes reserved, 12 bytes pre_defined,
            //   2 bytes width (offset 32 from box start),
            //   2 bytes height (offset 34 from box start),
            //   ... then 44 more bytes before nested boxes (total 78 bytes of data)
            let avc1_width = if pos + 34 <= data.len() {
                u16::from_be_bytes([data[pos + 32], data[pos + 33]]) as u32
            } else { 0 };
            let avc1_height = if pos + 36 <= data.len() {
                u16::from_be_bytes([data[pos + 34], data[pos + 35]]) as u32
            } else { 0 };

            if pos + 86 < pos + box_size {
                if let Some((sps, pps, w, h)) = parse_cmaf_init_boxes(&data[pos + 86..pos + box_size]) {
                    // Use avc1 dimensions if avcc returned 0,0
                    let width = if w > 0 { w } else { avc1_width };
                    let height = if h > 0 { h } else { avc1_height };
                    return Some((sps, pps, width, height));
                }
            }
        } else if box_type == b"avcC" {
            let avcc = &data[pos + 8..pos + box_size];
            return parse_avcc_box(avcc);
        }

        pos += box_size;
    }
    None
}

/// Parse an avcC box to extract SPS, PPS, and dimensions.
fn parse_avcc_box(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>, u32, u32)> {
    if data.len() < 7 {
        return None;
    }

    let num_sps = (data[5] & 0x1F) as usize;
    let mut pos = 6;
    let mut sps = Vec::new();
    let mut pps = Vec::new();

    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            return None;
        }
        let sps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + sps_len > data.len() {
            return None;
        }
        sps = data[pos..pos + sps_len].to_vec();
        pos += sps_len;
    }

    if pos >= data.len() {
        return None;
    }
    let num_pps = data[pos] as usize;
    pos += 1;

    for _ in 0..num_pps {
        if pos + 2 > data.len() {
            return None;
        }
        let pps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + pps_len > data.len() {
            return None;
        }
        pps = data[pos..pos + pps_len].to_vec();
        pos += pps_len;
    }

    // Dimensions default - SPS parsing for dimensions is complex (exp-golomb),
    // so we rely on the decoder to determine actual dimensions.
    // Use 0,0 as sentinel; the decoder will figure it out from SPS.
    let (width, height) = (0u32, 0u32);

    if sps.is_empty() || pps.is_empty() {
        return None;
    }

    Some((sps, pps, width, height))
}

/// Parse a CMAF media segment to extract NAL unit data from the mdat box.
/// Returns raw NAL unit byte vectors (without length prefixes).
#[cfg_attr(not(any(feature = "videotoolbox", feature = "nvenc")), allow(dead_code))]
fn parse_cmaf_media_segment(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut nal_units = Vec::new();
    let mut pos = 0;

    while pos + 8 <= data.len() {
        let box_size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let box_type = &data[pos + 4..pos + 8];

        if box_size == 0 || pos + box_size > data.len() {
            break;
        }

        if box_type == b"mdat" {
            let mdat_data = &data[pos + 8..pos + box_size];
            let mut mdat_pos = 0;

            while mdat_pos + 4 <= mdat_data.len() {
                let nal_len = u32::from_be_bytes([
                    mdat_data[mdat_pos],
                    mdat_data[mdat_pos + 1],
                    mdat_data[mdat_pos + 2],
                    mdat_data[mdat_pos + 3],
                ]) as usize;
                mdat_pos += 4;

                if mdat_pos + nal_len > mdat_data.len() {
                    break;
                }

                nal_units.push(mdat_data[mdat_pos..mdat_pos + nal_len].to_vec());
                mdat_pos += nal_len;
            }
            break;
        }

        pos += box_size;
    }

    Ok(nal_units)
}

// ALPN protocols in preference order
const CAMERA_ALPN_H264: &[u8] = b"xoq/camera-h264/0";
const CAMERA_ALPN_JPEG: &[u8] = b"xoq/camera-jpeg/0";
const CAMERA_ALPN: &[u8] = b"xoq/camera/0"; // Legacy

/// Stream encoding type
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StreamEncoding {
    /// JPEG frames
    Jpeg,
    /// H.264 NAL units
    H264,
}

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
    prefer_h264: bool,
}

impl CameraClientBuilder {
    /// Create a new camera client builder.
    pub fn new() -> Self {
        Self {
            transport: None,
            prefer_h264: true, // Prefer H.264 by default if available
        }
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

    /// Prefer H.264 encoding (default: true).
    /// If false, will only try JPEG.
    pub fn prefer_h264(mut self, prefer: bool) -> Self {
        self.prefer_h264 = prefer;
        self
    }

    /// Force JPEG only (no H.264 negotiation).
    pub fn jpeg_only(mut self) -> Self {
        self.prefer_h264 = false;
        self
    }

    /// Connect to the camera server.
    pub async fn connect(self) -> Result<CameraClient> {
        let transport = self
            .transport
            .ok_or_else(|| anyhow::anyhow!("Transport not specified"))?;

        let prefer_h264 = self.prefer_h264;

        let inner = match transport {
            Transport::Iroh { server_id } => {
                Self::connect_iroh_inner(&server_id, prefer_h264).await?
            }
            Transport::Moq { path, relay_url } => {
                Self::connect_moq_inner(&path, relay_url.as_deref(), prefer_h264).await?
            }
        };

        Ok(CameraClient { inner })
    }

    async fn connect_iroh_inner(server_id: &str, prefer_h264: bool) -> Result<CameraClientInner> {
        use crate::iroh::IrohClientBuilder;

        let alpns: Vec<&[u8]> = if prefer_h264 {
            vec![CAMERA_ALPN_H264, CAMERA_ALPN_JPEG, CAMERA_ALPN]
        } else {
            vec![CAMERA_ALPN_JPEG, CAMERA_ALPN]
        };

        let mut last_error = None;
        for attempt in 0..3u32 {
            if attempt > 0 {
                tracing::info!("Retrying iroh connection (attempt {}/3)...", attempt + 1);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }

            for alpn in &alpns {
                match IrohClientBuilder::new()
                    .alpn(alpn)
                    .connect_str(server_id)
                    .await
                {
                    Ok(conn) => {
                        let encoding = if *alpn == CAMERA_ALPN_H264 {
                            StreamEncoding::H264
                        } else {
                            StreamEncoding::Jpeg
                        };

                        tracing::info!(
                            "Connected with {} encoding",
                            if encoding == StreamEncoding::H264 { "H.264" } else { "JPEG" }
                        );

                        let stream = conn.open_stream().await?;
                        let (_send, recv) = stream.split();

                        // Create decoder if H.264
                        #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
                        let decoder = if encoding == StreamEncoding::H264 {
                            Some(Arc::new(Mutex::new(H264Decoder::new()?)))
                        } else {
                            None
                        };

                        return Ok(CameraClientInner::Iroh {
                            recv: Arc::new(Mutex::new(recv)),
                            _conn: conn,
                            encoding,
                            #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
                            decoder,
                        });
                    }
                    Err(e) => {
                        tracing::debug!("Failed to connect with ALPN {:?}: {}",
                            String::from_utf8_lossy(alpn), e);
                        last_error = Some(e);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No ALPN protocols available")))
    }

    async fn connect_moq_inner(path: &str, relay_url: Option<&str>, prefer_h264: bool) -> Result<CameraClientInner> {
        use crate::moq::MoqBuilder;

        eprintln!("[xoq] MoQ connecting to '{}' (prefer_h264={})", path, prefer_h264);

        let mut builder = MoqBuilder::new().path(path);
        if let Some(url) = relay_url {
            builder = builder.relay(url);
        }
        let mut conn = builder.connect_subscriber().await?;
        eprintln!("[xoq] MoQ connected to relay, subscribing to track...");

        // Try "video" (H.264 CMAF) track first if H.264 is preferred
        let (track, encoding) = if prefer_h264 {
            match conn.subscribe_track("video").await? {
                Some(t) => {
                    eprintln!("[xoq] Subscribed to H.264 CMAF 'video' track");
                    (t, StreamEncoding::H264)
                }
                None => {
                    eprintln!("[xoq] No 'video' track, falling back to 'camera' (JPEG)");
                    let t = conn
                        .subscribe_track("camera")
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("No camera track found"))?;
                    eprintln!("[xoq] Subscribed to JPEG 'camera' track");
                    (t, StreamEncoding::Jpeg)
                }
            }
        } else {
            let t = conn
                .subscribe_track("camera")
                .await?
                .ok_or_else(|| anyhow::anyhow!("Camera track not found"))?;
            (t, StreamEncoding::Jpeg)
        };

        // Create decoder lazily for H.264 (from init segment's SPS/PPS)
        #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
        let decoder = None;

        Ok(CameraClientInner::Moq {
            track,
            _conn: conn,
            encoding,
            #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
            decoder,
            cmaf_initialized: false,
            cmaf_width: 0,
            cmaf_height: 0,
        })
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
        encoding: StreamEncoding,
        #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
        decoder: Option<Arc<Mutex<H264Decoder>>>,
    },
    Moq {
        track: crate::moq::MoqTrackReader,
        _conn: crate::moq::MoqSubscriber,
        encoding: StreamEncoding,
        #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
        decoder: Option<Arc<Mutex<H264Decoder>>>,
        cmaf_initialized: bool,
        cmaf_width: u32,
        cmaf_height: u32,
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

    /// Get the stream encoding type.
    pub fn encoding(&self) -> StreamEncoding {
        match &self.inner {
            CameraClientInner::Iroh { encoding, .. } => *encoding,
            CameraClientInner::Moq { encoding, .. } => *encoding,
        }
    }

    /// Request and read a single frame from the server.
    pub async fn read_frame(&mut self) -> Result<Frame> {
        match &mut self.inner {
            CameraClientInner::Iroh {
                recv,
                encoding,
                #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
                decoder,
                ..
            } => {
                // Read frame header and data
                let (width, height, timestamp, data) = {
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

                    let mut data = vec![0u8; length as usize];
                    recv.read_exact(&mut data).await?;

                    (width, height, timestamp, data)
                };

                // Decode based on encoding
                let frame = match encoding {
                    StreamEncoding::Jpeg => {
                        let mut frame = Frame::from_jpeg(&data)?;
                        frame.timestamp_us = timestamp;
                        frame
                    }
                    StreamEncoding::H264 => {
                        #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
                        {
                            if let Some(decoder) = decoder {
                                let mut dec = decoder.lock().await;
                                dec.decode(&data, width, height, timestamp)?
                            } else {
                                anyhow::bail!("H.264 stream but no decoder available");
                            }
                        }
                        #[cfg(not(any(feature = "nvenc", feature = "videotoolbox")))]
                        {
                            anyhow::bail!("H.264 decoding requires nvenc or videotoolbox feature");
                        }
                    }
                };

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
            CameraClientInner::Moq {
                track,
                encoding,
                #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
                decoder,
                cmaf_initialized,
                cmaf_width,
                cmaf_height,
                ..
            } => {
                match encoding {
                    StreamEncoding::Jpeg => {
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
                        let frame_data = &data[12..];

                        let mut frame = Frame::from_jpeg(frame_data)?;
                        frame.timestamp_us = timestamp as u64;

                        if frame.width != width || frame.height != height {
                            tracing::warn!(
                                "Frame dimension mismatch: expected {}x{}, got {}x{}",
                                width, height, frame.width, frame.height
                            );
                        }

                        Ok(frame)
                    }
                    StreamEncoding::H264 => {
                        #[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
                        {
                            // CMAF H.264 over MoQ
                            let mut cmaf_reads = 0u64;
                            loop {
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

                                cmaf_reads += 1;
                                // Log first few reads and box types for debugging
                                if cmaf_reads <= 5 {
                                    let box_type = if data.len() >= 8 {
                                        String::from_utf8_lossy(&data[4..8]).to_string()
                                    } else {
                                        "???".to_string()
                                    };
                                    eprintln!(
                                        "[xoq] CMAF read #{}: {} bytes, first box: '{}', hex: {:02x?}",
                                        cmaf_reads, data.len(), box_type,
                                        &data[..data.len().min(16)]
                                    );
                                }

                                if !*cmaf_initialized {
                                    // Parse init segment (may be standalone or prepended to keyframe)
                                    match parse_cmaf_init_segment(&data) {
                                        Ok((sps, pps, width, height)) => {
                                            let mut annex_b = Vec::new();
                                            annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                                            annex_b.extend_from_slice(&sps);
                                            annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                                            annex_b.extend_from_slice(&pps);

                                            let mut dec = H264Decoder::new()?;
                                            let w = if width > 0 { width } else { 1280 };
                                            let h = if height > 0 { height } else { 720 };
                                            let _ = dec.decode(&annex_b, w, h, 0);
                                            *decoder = Some(Arc::new(Mutex::new(dec)));
                                            *cmaf_initialized = true;
                                            *cmaf_width = w;
                                            *cmaf_height = h;

                                            eprintln!("[xoq] CMAF H.264 decoder initialized ({}x{})", w, h);

                                            // Check if this data also contains media segments (late-joiner combined packet)
                                            match parse_cmaf_media_segment(&data) {
                                                Ok(nal_units) if !nal_units.is_empty() => {
                                                    let mut h264_data = Vec::new();
                                                    for nal in &nal_units {
                                                        h264_data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                                                        h264_data.extend_from_slice(nal);
                                                    }
                                                    let mut full = annex_b.clone();
                                                    full.extend_from_slice(&h264_data);

                                                    if let Some(dec) = decoder {
                                                        let mut dec = dec.lock().await;
                                                        return Ok(dec.decode(&full, *cmaf_width, *cmaf_height, 0)?);
                                                    }
                                                }
                                                _ => {
                                                    continue;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            if cmaf_reads <= 10 {
                                                eprintln!(
                                                    "[xoq] CMAF read #{}: not init segment ({}) - waiting...",
                                                    cmaf_reads, e
                                                );
                                            }
                                            continue;
                                        }
                                    }
                                }

                                // Decode media segment
                                let nal_units = parse_cmaf_media_segment(&data)?;
                                if nal_units.is_empty() {
                                    // Might be a standalone init segment or empty - try init parse
                                    if parse_cmaf_init_segment(&data).is_ok() {
                                        // Re-initialization (new SPS/PPS), skip
                                        continue;
                                    }
                                    continue;
                                }

                                // Convert NAL units to Annex B for the existing decoder
                                let mut h264_data = Vec::new();
                                for nal in &nal_units {
                                    h264_data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                                    h264_data.extend_from_slice(nal);
                                }

                                if let Some(dec) = decoder {
                                    let mut dec = dec.lock().await;
                                    return Ok(dec.decode(&h264_data, *cmaf_width, *cmaf_height, 0)?);
                                } else {
                                    anyhow::bail!("H.264 decoder not initialized");
                                }
                            }
                        }
                        #[cfg(not(any(feature = "nvenc", feature = "videotoolbox")))]
                        {
                            anyhow::bail!("H.264 decoding requires nvenc or videotoolbox feature");
                        }
                    }
                }
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

// ============================================================================
// NVDEC Hardware Decoder
// ============================================================================

#[cfg(feature = "nvenc")]
mod nvdec {
    use super::*;
    use cudarc::driver::CudaContext;
    use cudarc::driver::sys::CUresult;
    use nvidia_video_codec_sdk::sys::cuviddec::*;
    use nvidia_video_codec_sdk::sys::nvcuvid::*;
    use std::ffi::c_void;
    use std::ptr;

    // CUDA_SUCCESS = 0
    const CUDA_SUCCESS: CUresult = CUresult::CUDA_SUCCESS;

    /// NVDEC hardware H.264 decoder
    pub struct NvdecDecoder {
        parser: CUvideoparser,
        decoder: CUvideodecoder,
        _ctx: std::sync::Arc<CudaContext>,
        width: u32,
        height: u32,
        // Decoded frame storage
        decoded_frames: Vec<DecodedFrame>,
        // NV12 to RGB conversion buffer
        rgb_buffer: Vec<u8>,
    }

    struct DecodedFrame {
        data: Vec<u8>,
        width: u32,
        height: u32,
        timestamp: u64,
    }

    impl NvdecDecoder {
        pub fn new() -> Result<Self> {
            let ctx = CudaContext::new(0)
                .map_err(|e| anyhow::anyhow!("Failed to create CUDA context: {}", e))?;

            Ok(NvdecDecoder {
                parser: ptr::null_mut(),
                decoder: ptr::null_mut(),
                _ctx: ctx,
                width: 0,
                height: 0,
                decoded_frames: Vec::new(),
                rgb_buffer: Vec::new(),
            })
        }

        fn ensure_decoder(&mut self, width: u32, height: u32) -> Result<()> {
            if self.width == width && self.height == height && !self.parser.is_null() {
                return Ok(());
            }

            // Destroy old decoder/parser if exists
            self.destroy_decoder();

            self.width = width;
            self.height = height;

            // Create video parser
            let mut parser_params: CUVIDPARSERPARAMS = unsafe { std::mem::zeroed() };
            parser_params.CodecType = cudaVideoCodec::cudaVideoCodec_H264;
            parser_params.ulMaxNumDecodeSurfaces = 4;
            parser_params.ulMaxDisplayDelay = 0; // Low latency
            parser_params.pUserData = self as *mut _ as *mut c_void;
            parser_params.pfnSequenceCallback = Some(Self::sequence_callback);
            parser_params.pfnDecodePicture = Some(Self::decode_callback);
            parser_params.pfnDisplayPicture = Some(Self::display_callback);

            let result = unsafe { cuvidCreateVideoParser(&mut self.parser, &mut parser_params) };
            if result != CUDA_SUCCESS {
                anyhow::bail!("Failed to create video parser: {:?}", result);
            }

            Ok(())
        }

        fn destroy_decoder(&mut self) {
            if !self.parser.is_null() {
                let _ = unsafe { cuvidDestroyVideoParser(self.parser) };
                self.parser = ptr::null_mut();
            }
            if !self.decoder.is_null() {
                let _ = unsafe { cuvidDestroyDecoder(self.decoder) };
                self.decoder = ptr::null_mut();
            }
        }

        pub fn decode(&mut self, h264_data: &[u8], width: u32, height: u32, timestamp: u64) -> Result<Frame> {
            self.ensure_decoder(width, height)?;

            // Create packet
            let mut packet: CUVIDSOURCEDATAPACKET = unsafe { std::mem::zeroed() };
            packet.payload = h264_data.as_ptr();
            packet.payload_size = h264_data.len() as u64;
            packet.timestamp = timestamp as i64;

            // Parse the data (this triggers callbacks)
            let result = unsafe { cuvidParseVideoData(self.parser, &mut packet) };
            if result != CUDA_SUCCESS {
                anyhow::bail!("Failed to parse video data: {:?}", result);
            }

            // Get decoded frame
            if let Some(decoded) = self.decoded_frames.pop() {
                // Convert NV12 to RGB
                self.nv12_to_rgb(&decoded.data, decoded.width, decoded.height);

                Ok(Frame {
                    width: decoded.width,
                    height: decoded.height,
                    data: self.rgb_buffer.clone(),
                    timestamp_us: decoded.timestamp,
                })
            } else {
                anyhow::bail!("NvdecDecoder: no frame decoded (data_len={}, {}x{}, frame_count={})", h264_data.len(), width, height, self.decoded_frames.len());
            }
        }

        fn nv12_to_rgb(&mut self, nv12: &[u8], width: u32, height: u32) {
            let width = width as usize;
            let height = height as usize;
            let y_size = width * height;

            self.rgb_buffer.resize(width * height * 3, 0);

            for y in 0..height {
                for x in 0..width {
                    let y_val = nv12.get(y * width + x).copied().unwrap_or(0) as f32;

                    let uv_idx = y_size + (y / 2) * width + (x / 2) * 2;
                    let u = nv12.get(uv_idx).copied().unwrap_or(128) as f32;
                    let v = nv12.get(uv_idx + 1).copied().unwrap_or(128) as f32;

                    // YUV to RGB (BT.601)
                    let c = y_val - 16.0;
                    let d = u - 128.0;
                    let e = v - 128.0;

                    let r = (1.164 * c + 1.596 * e).clamp(0.0, 255.0) as u8;
                    let g = (1.164 * c - 0.392 * d - 0.813 * e).clamp(0.0, 255.0) as u8;
                    let b = (1.164 * c + 2.017 * d).clamp(0.0, 255.0) as u8;

                    let rgb_idx = (y * width + x) * 3;
                    self.rgb_buffer[rgb_idx] = r;
                    self.rgb_buffer[rgb_idx + 1] = g;
                    self.rgb_buffer[rgb_idx + 2] = b;
                }
            }
        }

        // Callback when sequence header is parsed - creates the actual decoder
        extern "C" fn sequence_callback(
            user_data: *mut c_void,
            video_format: *mut CUVIDEOFORMAT,
        ) -> i32 {
            let decoder = unsafe { &mut *(user_data as *mut NvdecDecoder) };
            let format = unsafe { &*video_format };

            // Create decoder with these parameters
            let mut create_info: CUVIDDECODECREATEINFO = unsafe { std::mem::zeroed() };
            create_info.ulWidth = format.coded_width as u64;
            create_info.ulHeight = format.coded_height as u64;
            create_info.ulNumDecodeSurfaces = 4;
            create_info.CodecType = format.codec;
            create_info.ChromaFormat = format.chroma_format;
            create_info.ulCreationFlags = 0;
            create_info.OutputFormat = cudaVideoSurfaceFormat::cudaVideoSurfaceFormat_NV12;
            create_info.DeinterlaceMode = cudaVideoDeinterlaceMode::cudaVideoDeinterlaceMode_Adaptive;
            create_info.ulTargetWidth = format.coded_width as u64;
            create_info.ulTargetHeight = format.coded_height as u64;
            create_info.ulNumOutputSurfaces = 2;

            if !decoder.decoder.is_null() {
                let _ = unsafe { cuvidDestroyDecoder(decoder.decoder) };
                decoder.decoder = ptr::null_mut();
            }

            let result = unsafe { cuvidCreateDecoder(&mut decoder.decoder, &mut create_info) };
            if result != CUDA_SUCCESS {
                tracing::error!("Failed to create decoder: {:?}", result);
                return 0;
            }

            decoder.width = format.coded_width;
            decoder.height = format.coded_height;

            format.min_num_decode_surfaces as i32
        }

        // Callback when a picture is ready to decode
        extern "C" fn decode_callback(
            user_data: *mut c_void,
            pic_params: *mut CUVIDPICPARAMS,
        ) -> i32 {
            let decoder = unsafe { &mut *(user_data as *mut NvdecDecoder) };

            if decoder.decoder.is_null() {
                return 0;
            }

            let result = unsafe { cuvidDecodePicture(decoder.decoder, pic_params) };
            if result != CUDA_SUCCESS {
                tracing::error!("Failed to decode picture: {:?}", result);
                return 0;
            }

            1
        }

        // Callback when a picture is ready to display
        extern "C" fn display_callback(
            user_data: *mut c_void,
            disp_info: *mut CUVIDPARSERDISPINFO,
        ) -> i32 {
            let decoder = unsafe { &mut *(user_data as *mut NvdecDecoder) };
            let info = unsafe { &*disp_info };

            if decoder.decoder.is_null() || info.picture_index < 0 {
                return 0;
            }

            // Map the video frame
            let mut proc_params: CUVIDPROCPARAMS = unsafe { std::mem::zeroed() };
            proc_params.progressive_frame = info.progressive_frame as i32;
            proc_params.top_field_first = info.top_field_first as i32;

            let mut dev_ptr: u64 = 0;
            let mut pitch: u32 = 0;

            let result = unsafe {
                cuvidMapVideoFrame64(
                    decoder.decoder,
                    info.picture_index,
                    &mut dev_ptr,
                    &mut pitch,
                    &mut proc_params,
                )
            };

            if result != CUDA_SUCCESS {
                tracing::error!("Failed to map video frame: {:?}", result);
                return 0;
            }

            // Copy NV12 data from GPU to CPU
            let width = decoder.width as usize;
            let height = decoder.height as usize;
            let nv12_size = width * height * 3 / 2;
            let mut nv12_data = vec![0u8; nv12_size];

            // Note: dev_ptr is a device pointer, we need cuMemcpy to copy from GPU
            // For now, this is a simplified version - proper implementation would use
            // cuMemcpyDtoH or cuMemcpy2D
            // TODO: Use proper CUDA memory copy

            // Copy Y plane (row by row due to pitch)
            for y in 0..height {
                let src_offset = y * pitch as usize;
                let dst_offset = y * width;
                unsafe {
                    cudarc::driver::sys::cuMemcpyDtoH_v2(
                        nv12_data.as_mut_ptr().add(dst_offset) as *mut c_void,
                        (dev_ptr + src_offset as u64) as cudarc::driver::sys::CUdeviceptr,
                        width,
                    );
                }
            }

            // Copy UV plane
            let uv_height = height / 2;
            for y in 0..uv_height {
                let src_offset = (height * pitch as usize) + y * pitch as usize;
                let dst_offset = width * height + y * width;
                unsafe {
                    cudarc::driver::sys::cuMemcpyDtoH_v2(
                        nv12_data.as_mut_ptr().add(dst_offset) as *mut c_void,
                        (dev_ptr + src_offset as u64) as cudarc::driver::sys::CUdeviceptr,
                        width,
                    );
                }
            }

            // Unmap
            let _ = unsafe { cuvidUnmapVideoFrame64(decoder.decoder, dev_ptr) };

            // Store decoded frame
            decoder.decoded_frames.push(DecodedFrame {
                data: nv12_data,
                width: decoder.width,
                height: decoder.height,
                timestamp: info.timestamp as u64,
            });

            1
        }
    }

    impl Drop for NvdecDecoder {
        fn drop(&mut self) {
            self.destroy_decoder();
        }
    }

    unsafe impl Send for NvdecDecoder {}
}

#[cfg(feature = "nvenc")]
pub use nvdec::NvdecDecoder;

// ============================================================================
// VideoToolbox Hardware Decoder (macOS)
// ============================================================================

#[cfg(feature = "videotoolbox")]
mod vtdec {
    use super::*;
    use crate::frame::Frame;
    use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::OSStatus;
    use core_media_sys::CMTime;
    use libc::c_void;
    use std::ptr;
    use std::sync::{Arc, Mutex as StdMutex};
    use video_toolbox_sys::cv_types::CVPixelBufferRef;
    use video_toolbox_sys::decompression::{
        VTDecompressionOutputCallbackRecord, VTDecompressionSessionCreate,
        VTDecompressionSessionDecodeFrame, VTDecompressionSessionInvalidate,
        VTDecompressionSessionRef,
    };

    // CoreMedia FFI
    #[link(name = "CoreMedia", kind = "framework")]
    extern "C" {
        fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
            allocator: *const c_void,
            parameter_set_count: usize,
            parameter_set_pointers: *const *const u8,
            parameter_set_sizes: *const usize,
            nal_unit_header_length: i32,
            format_description_out: *mut *mut c_void,
        ) -> OSStatus;

        fn CMSampleBufferCreate(
            allocator: *const c_void,
            data_buffer: *const c_void,
            data_ready: bool,
            make_data_ready_callback: *const c_void,
            make_data_ready_refcon: *const c_void,
            format_description: *const c_void,
            num_samples: i64,
            num_sample_timing_entries: i64,
            sample_timing_array: *const CMSampleTimingInfo,
            num_sample_size_entries: i64,
            sample_size_array: *const usize,
            sample_buffer_out: *mut *mut c_void,
        ) -> OSStatus;

        fn CMBlockBufferCreateWithMemoryBlock(
            allocator: *const c_void,
            memory_block: *mut c_void,
            block_length: usize,
            block_allocator: *const c_void,
            custom_block_source: *const c_void,
            offset_to_data: usize,
            data_length: usize,
            flags: u32,
            block_buffer_out: *mut *mut c_void,
        ) -> OSStatus;
    }

    // CoreVideo FFI (re-import the ones we need for pixel buffer access)
    #[link(name = "CoreVideo", kind = "framework")]
    extern "C" {
        fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> i32;
        fn CVPixelBufferUnlockBaseAddress(pixel_buffer: CVPixelBufferRef, unlock_flags: u64) -> i32;
        fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut c_void;
        fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
        fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    struct CMSampleTimingInfo {
        duration: CMTime,
        presentation_time_stamp: CMTime,
        decode_time_stamp: CMTime,
    }

    /// VideoToolbox hardware H.264 decoder for macOS.
    pub struct VtDecoder {
        session: VTDecompressionSessionRef,
        format_desc: *mut c_void,
        frame_slot: Arc<StdMutex<Option<Frame>>>,
        sps: Vec<u8>,
        pps: Vec<u8>,
        frame_count: u64,
    }

    unsafe impl Send for VtDecoder {}

    impl VtDecoder {
        pub fn new() -> Result<Self> {
            Ok(VtDecoder {
                session: ptr::null_mut(),
                format_desc: ptr::null_mut(),
                frame_slot: Arc::new(StdMutex::new(None)),
                sps: Vec::new(),
                pps: Vec::new(),
                frame_count: 0,
            })
        }

        fn ensure_session(&mut self, sps: &[u8], pps: &[u8], width: u32, height: u32) -> Result<()> {
            // If SPS/PPS haven't changed and session exists, reuse it
            if !self.session.is_null() && self.sps == sps && self.pps == pps {
                return Ok(());
            }

            // Destroy old session
            self.destroy_session();

            self.sps = sps.to_vec();
            self.pps = pps.to_vec();

            unsafe {
                // Create format description from SPS/PPS
                let parameter_sets = [sps.as_ptr(), pps.as_ptr()];
                let parameter_set_sizes = [sps.len(), pps.len()];

                let mut format_desc: *mut c_void = ptr::null_mut();
                let status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null(),
                    2,
                    parameter_sets.as_ptr(),
                    parameter_set_sizes.as_ptr(),
                    4, // NAL unit header length
                    &mut format_desc,
                );

                if status != 0 {
                    anyhow::bail!("Failed to create format description: {}", status);
                }

                self.format_desc = format_desc;

                // Build destination pixel buffer attributes (request BGRA)
                let pixel_format_key = CFString::new("PixelFormatType");
                let pixel_format_value = CFNumber::from(0x42475241i32); // kCVPixelFormatType_32BGRA
                let width_key = CFString::new("Width");
                let width_value = CFNumber::from(width as i32);
                let height_key = CFString::new("Height");
                let height_value = CFNumber::from(height as i32);

                let keys = vec![
                    pixel_format_key.as_CFType(),
                    width_key.as_CFType(),
                    height_key.as_CFType(),
                ];
                let values = vec![
                    pixel_format_value.as_CFType(),
                    width_value.as_CFType(),
                    height_value.as_CFType(),
                ];

                let dest_attrs = CFDictionary::from_CFType_pairs(
                    &keys
                        .iter()
                        .zip(values.iter())
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect::<Vec<_>>(),
                );

                // Create callback record - use frame_slot as ref con
                let frame_slot_ptr = Arc::into_raw(self.frame_slot.clone()) as *mut c_void;
                let callback = VTDecompressionOutputCallbackRecord {
                    decompressionOutputCallback: vt_decompression_callback,
                    decompressionOutputRefCon: frame_slot_ptr,
                };

                let mut session: VTDecompressionSessionRef = ptr::null_mut();
                let status = VTDecompressionSessionCreate(
                    ptr::null(),
                    format_desc as *mut _,
                    ptr::null(),
                    dest_attrs.as_concrete_TypeRef() as *const _,
                    &callback,
                    &mut session,
                );

                // We passed an Arc::into_raw, we need to reconstruct it to avoid leak
                // The callback will use the raw pointer directly, but we keep our own Arc
                let _ = Arc::from_raw(frame_slot_ptr as *const StdMutex<Option<Frame>>);

                if status != 0 {
                    CFRelease(format_desc as CFTypeRef);
                    self.format_desc = ptr::null_mut();
                    anyhow::bail!("Failed to create decompression session: {}", status);
                }

                self.session = session;
            }

            Ok(())
        }

        pub fn decode(
            &mut self,
            h264_data: &[u8],
            width: u32,
            height: u32,
            timestamp: u64,
        ) -> Result<Frame> {
            // Parse Annex B to extract NALs, SPS/PPS, and AVCC data
            let (avcc_data, sps, pps) = annex_b_to_avcc(h264_data);

            // Resolve current SPS/PPS: use newly parsed if available, else cached
            let current_sps = sps.unwrap_or_else(|| self.sps.clone());
            let current_pps = pps.unwrap_or_else(|| self.pps.clone());

            if current_sps.is_empty() || current_pps.is_empty() {
                // No SPS/PPS yet and none in this frame - can't decode
                if self.session.is_null() {
                    anyhow::bail!("VtDecoder: no SPS/PPS available and no session initialized (frame {})", self.frame_count);
                }
            } else {
                self.ensure_session(&current_sps, &current_pps, width, height)?;
            }

            if avcc_data.is_empty() {
                anyhow::bail!("VtDecoder: frame {} has no slice data (only SPS/PPS), h264_data len={}", self.frame_count, h264_data.len());
            }

            // Clear the frame slot
            if let Ok(mut slot) = self.frame_slot.lock() {
                *slot = None;
            }

            unsafe {
                // Create block buffer from AVCC data
                let mut avcc_buf: Box<Vec<u8>> = Box::new(avcc_data);

                let mut block_buffer: *mut c_void = ptr::null_mut();
                let status = CMBlockBufferCreateWithMemoryBlock(
                    ptr::null(),
                    avcc_buf.as_mut_ptr() as *mut c_void,
                    avcc_buf.len(),
                    ptr::null(),
                    ptr::null(),
                    0,
                    avcc_buf.len(),
                    0,
                    &mut block_buffer,
                );

                if status != 0 {
                    anyhow::bail!("Failed to create block buffer: {}", status);
                }

                // Create sample timing
                let timing = CMSampleTimingInfo {
                    duration: CMTime {
                        value: 1,
                        timescale: 30,
                        flags: 1,
                        epoch: 0,
                    },
                    presentation_time_stamp: CMTime {
                        value: self.frame_count as i64,
                        timescale: 30,
                        flags: 1,
                        epoch: 0,
                    },
                    decode_time_stamp: CMTime {
                        value: self.frame_count as i64,
                        timescale: 30,
                        flags: 1,
                        epoch: 0,
                    },
                };

                let sample_size = avcc_buf.len();
                let mut sample_buffer: *mut c_void = ptr::null_mut();

                let status = CMSampleBufferCreate(
                    ptr::null(),
                    block_buffer,
                    true,
                    ptr::null(),
                    ptr::null(),
                    self.format_desc,
                    1,
                    1,
                    &timing,
                    1,
                    &sample_size,
                    &mut sample_buffer,
                );

                if status != 0 {
                    CFRelease(block_buffer as CFTypeRef);
                    anyhow::bail!("Failed to create sample buffer: {}", status);
                }

                // Decode synchronously
                let mut info_flags: u32 = 0;
                let status = VTDecompressionSessionDecodeFrame(
                    self.session,
                    sample_buffer as *mut _,
                    0, // Synchronous
                    ptr::null_mut(),
                    &mut info_flags,
                );

                // Clean up - CMSampleBufferCreate with dataReady=true takes ownership of block_buffer
                CFRelease(sample_buffer as CFTypeRef);
                // avcc_buf (Box) dropped here, safe after sync decode

                if status != 0 {
                    anyhow::bail!("VideoToolbox decode failed: {}", status);
                }
            }

            self.frame_count += 1;

            // Read decoded frame from slot
            if let Ok(mut slot) = self.frame_slot.lock() {
                if let Some(mut frame) = slot.take() {
                    frame.timestamp_us = timestamp;
                    return Ok(frame);
                }
            }

            // Callback didn't fire or produced no frame
            anyhow::bail!("VtDecoder: decompression callback produced no frame (frame {}, avcc_len={}, {}x{})", self.frame_count - 1, h264_data.len(), width, height);
        }

        fn destroy_session(&mut self) {
            unsafe {
                if !self.session.is_null() {
                    VTDecompressionSessionInvalidate(self.session);
                    self.session = ptr::null_mut();
                }
                if !self.format_desc.is_null() {
                    CFRelease(self.format_desc as CFTypeRef);
                    self.format_desc = ptr::null_mut();
                }
            }
        }
    }

    impl Drop for VtDecoder {
        fn drop(&mut self) {
            self.destroy_session();
        }
    }

    /// Decompression output callback - writes decoded frame to the shared slot.
    extern "C" fn vt_decompression_callback(
        ref_con: *mut c_void,
        _source_frame_ref_con: *mut c_void,
        status: OSStatus,
        _info_flags: u32,
        image_buffer: CVPixelBufferRef,
        _pts: CMTime,
        _duration: CMTime,
    ) {
        if status != 0 {
            tracing::warn!("VideoToolbox decode callback failed with status: {}", status);
            return;
        }
        if image_buffer.is_null() {
            tracing::warn!("VideoToolbox decode callback: null image buffer");
            return;
        }

        unsafe {
            CVPixelBufferLockBaseAddress(image_buffer, 0);

            let base_address = CVPixelBufferGetBaseAddress(image_buffer);
            let width = CVPixelBufferGetWidth(image_buffer);
            let height = CVPixelBufferGetHeight(image_buffer);
            let bytes_per_row = CVPixelBufferGetBytesPerRow(image_buffer);

            if !base_address.is_null() && width > 0 && height > 0 {
                let src = std::slice::from_raw_parts(
                    base_address as *const u8,
                    bytes_per_row * height,
                );

                // Convert BGRA to RGB
                let mut rgb_data = Vec::with_capacity(width * height * 3);
                if bytes_per_row == width * 4 {
                    // No padding — process all pixels in bulk via chunks
                    for pixel in src.chunks_exact(4) {
                        rgb_data.push(pixel[2]); // R
                        rgb_data.push(pixel[1]); // G
                        rgb_data.push(pixel[0]); // B
                    }
                } else {
                    // Stride padding — process row by row
                    for y in 0..height {
                        let row = &src[y * bytes_per_row..y * bytes_per_row + width * 4];
                        for pixel in row.chunks_exact(4) {
                            rgb_data.push(pixel[2]); // R
                            rgb_data.push(pixel[1]); // G
                            rgb_data.push(pixel[0]); // B
                        }
                    }
                }

                let frame = Frame {
                    width: width as u32,
                    height: height as u32,
                    data: rgb_data,
                    timestamp_us: 0,
                };

                // Write to frame slot
                let slot_ptr = ref_con as *const StdMutex<Option<Frame>>;
                if let Ok(mut slot) = (*slot_ptr).lock() {
                    *slot = Some(frame);
                }
            }

            CVPixelBufferUnlockBaseAddress(image_buffer, 0);
        }
    }
}

#[cfg(feature = "videotoolbox")]
pub use vtdec::VtDecoder;

// ============================================================================
// H264Decoder - Unifying enum for hardware decoders
// ============================================================================

#[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
enum H264Decoder {
    #[cfg(feature = "nvenc")]
    Nvdec(NvdecDecoder),
    #[cfg(feature = "videotoolbox")]
    VideoToolbox(VtDecoder),
}

#[cfg(any(feature = "nvenc", feature = "videotoolbox"))]
impl H264Decoder {
    fn new() -> Result<Self> {
        #[cfg(feature = "nvenc")]
        {
            return Ok(H264Decoder::Nvdec(NvdecDecoder::new()?));
        }
        #[cfg(all(feature = "videotoolbox", not(feature = "nvenc")))]
        {
            return Ok(H264Decoder::VideoToolbox(VtDecoder::new()?));
        }
    }

    fn decode(&mut self, data: &[u8], width: u32, height: u32, timestamp: u64) -> Result<Frame> {
        match self {
            #[cfg(feature = "nvenc")]
            H264Decoder::Nvdec(dec) => dec.decode(data, width, height, timestamp),
            #[cfg(feature = "videotoolbox")]
            H264Decoder::VideoToolbox(dec) => dec.decode(data, width, height, timestamp),
        }
    }
}

// ============================================================================
// Legacy API
// ============================================================================

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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_annex_b_parse_single_nal() {
        // Single NAL unit with 4-byte start code
        let data = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB];
        let nals = parse_annex_b(&data);
        assert_eq!(nals.len(), 1);
        assert_eq!(nals[0].0, 5); // NAL type 5 = IDR slice
        assert_eq!(nals[0].1, vec![0x65, 0xAA, 0xBB]);
    }

    #[test]
    fn test_annex_b_parse_multiple_nals() {
        // SPS (type 7) + PPS (type 8) + IDR (type 5) with 4-byte start codes
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
        data.extend_from_slice(&[0x67, 0x42, 0x00, 0x1E]); // SPS (type 7)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
        data.extend_from_slice(&[0x68, 0xCE, 0x38, 0x80]); // PPS (type 8)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
        data.extend_from_slice(&[0x65, 0x88, 0x84]);        // IDR (type 5)

        let nals = parse_annex_b(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].0, 7); // SPS
        assert_eq!(nals[1].0, 8); // PPS
        assert_eq!(nals[2].0, 5); // IDR
        assert_eq!(nals[0].1, vec![0x67, 0x42, 0x00, 0x1E]);
        assert_eq!(nals[1].1, vec![0x68, 0xCE, 0x38, 0x80]);
        assert_eq!(nals[2].1, vec![0x65, 0x88, 0x84]);
    }

    #[test]
    fn test_annex_b_parse_3byte_start_code() {
        // 3-byte start codes
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // 3-byte start code
        data.extend_from_slice(&[0x67, 0x42]);        // SPS
        data.extend_from_slice(&[0x00, 0x00, 0x01]); // 3-byte start code
        data.extend_from_slice(&[0x68, 0xCE]);        // PPS

        let nals = parse_annex_b(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].0, 7); // SPS
        assert_eq!(nals[1].0, 8); // PPS
    }

    #[test]
    fn test_annex_b_parse_empty_input() {
        let nals = parse_annex_b(&[]);
        assert!(nals.is_empty());
    }

    #[test]
    fn test_annex_b_to_avcc_extracts_sps_pps() {
        // SPS + PPS + IDR
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x67, 0x42, 0x00, 0x1E]); // SPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x68, 0xCE, 0x38, 0x80]); // PPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x65, 0x88, 0x84]);        // IDR

        let (avcc, sps, pps) = annex_b_to_avcc(&data);

        // SPS/PPS should be extracted
        assert_eq!(sps, Some(vec![0x67, 0x42, 0x00, 0x1E]));
        assert_eq!(pps, Some(vec![0x68, 0xCE, 0x38, 0x80]));

        // AVCC should contain only the IDR NAL with 4-byte length prefix
        assert_eq!(avcc.len(), 4 + 3); // 4-byte length + 3-byte NAL
        assert_eq!(&avcc[0..4], &(3u32).to_be_bytes()); // length = 3
        assert_eq!(&avcc[4..], &[0x65, 0x88, 0x84]);
    }

    #[test]
    fn test_annex_b_to_avcc_no_sps_pps() {
        // Only non-IDR slice (no SPS/PPS)
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x41, 0x9A, 0x01]); // non-IDR (type 1)

        let (avcc, sps, pps) = annex_b_to_avcc(&data);

        assert!(sps.is_none());
        assert!(pps.is_none());
        assert_eq!(avcc.len(), 4 + 3);
        assert_eq!(&avcc[4..], &[0x41, 0x9A, 0x01]);
    }
}
