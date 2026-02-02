//! Platform-independent CMAF (Common Media Application Format) muxer for H.264 video streams.
//!
//! This module provides a pure-Rust CMAF muxer and H.264 Annex B parser suitable for:
//! - Live streaming (DASH/HLS)
//! - Media Source Extensions (MSE) in browsers
//! - Low-latency video delivery
//!
//! Both NVENC (Linux) and VideoToolbox (macOS) encoders can use this module to produce
//! identical CMAF wire format over MoQ.
//!
//! # CMAF Structure
//!
//! ```text
//! Initialization Segment:
//!   ftyp (file type)
//!   moov (movie header with track info, SPS/PPS)
//!
//! Media Segments:
//!   styp (segment type)
//!   moof (movie fragment header)
//!   mdat (media data - encoded NAL units)
//! ```

/// H.264 NAL unit type constants.
pub mod nal_unit_type {
    /// Non-IDR slice (P/B frame)
    pub const NON_IDR_SLICE: u8 = 1;
    /// IDR slice (keyframe)
    pub const IDR_SLICE: u8 = 5;
    /// Supplemental enhancement information
    pub const SEI: u8 = 6;
    /// Sequence parameter set
    pub const SPS: u8 = 7;
    /// Picture parameter set
    pub const PPS: u8 = 8;
}

/// A single H.264 NAL unit.
#[derive(Debug, Clone)]
pub struct NalUnit {
    /// The raw NAL unit data (without length prefix, without start code).
    pub data: Vec<u8>,
    /// NAL unit type (from first byte & 0x1F).
    pub nal_type: u8,
}

impl NalUnit {
    /// Returns true if this NAL unit is an IDR (keyframe) slice.
    pub fn is_idr(&self) -> bool {
        self.nal_type == nal_unit_type::IDR_SLICE
    }

    /// Returns true if this NAL unit is an SPS.
    pub fn is_sps(&self) -> bool {
        self.nal_type == nal_unit_type::SPS
    }

    /// Returns true if this NAL unit is a PPS.
    pub fn is_pps(&self) -> bool {
        self.nal_type == nal_unit_type::PPS
    }

    /// Returns true if this NAL unit is a video slice (IDR or non-IDR).
    pub fn is_slice(&self) -> bool {
        self.nal_type == nal_unit_type::IDR_SLICE || self.nal_type == nal_unit_type::NON_IDR_SLICE
    }

    /// Convert NAL unit to Annex B format (with 0x00000001 start code).
    pub fn to_annex_b(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(4 + self.data.len());
        result.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        result.extend_from_slice(&self.data);
        result
    }
}

/// A parsed Annex B frame with separated NAL units.
#[derive(Debug)]
pub struct ParsedFrame {
    /// Slice NAL units (non-SPS/PPS NALs).
    pub nals: Vec<NalUnit>,
    /// SPS data if present (without start code).
    pub sps: Option<Vec<u8>>,
    /// PPS data if present (without start code).
    pub pps: Option<Vec<u8>>,
    /// Whether this frame contains a keyframe (IDR slice).
    pub is_keyframe: bool,
}

/// Parse raw Annex B H.264 data into structured NAL units.
///
/// Splits on 3-byte (0x000001) and 4-byte (0x00000001) start codes,
/// extracts SPS/PPS/slice NALs, and determines keyframe status.
pub fn parse_annex_b(data: &[u8]) -> ParsedFrame {
    let mut nals = Vec::new();
    let mut sps = None;
    let mut pps = None;
    let mut is_keyframe = false;

    // Find all NAL unit boundaries by scanning for start codes
    let mut nal_starts = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + 3 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            // 4-byte start code
            nal_starts.push(i + 4);
            i += 4;
        } else if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // 3-byte start code
            nal_starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }

    for (idx, &start) in nal_starts.iter().enumerate() {
        if start >= data.len() {
            continue;
        }

        let end = if idx + 1 < nal_starts.len() {
            // Find the start code position (not the NAL data start) for the next NAL
            let next_start = nal_starts[idx + 1];
            // Back up past the start code to find where this NAL's data ends
            if next_start >= 4 && data[next_start - 4] == 0 && data[next_start - 3] == 0 && data[next_start - 2] == 0 && data[next_start - 1] == 1 {
                next_start - 4
            } else if next_start >= 3 && data[next_start - 3] == 0 && data[next_start - 2] == 0 && data[next_start - 1] == 1 {
                next_start - 3
            } else {
                next_start
            }
        } else {
            data.len()
        };

        if start >= end {
            continue;
        }

        let nal_data = &data[start..end];
        let nal_type = nal_data[0] & 0x1F;

        match nal_type {
            nal_unit_type::SPS => {
                sps = Some(nal_data.to_vec());
            }
            nal_unit_type::PPS => {
                pps = Some(nal_data.to_vec());
            }
            nal_unit_type::IDR_SLICE => {
                is_keyframe = true;
                nals.push(NalUnit {
                    data: nal_data.to_vec(),
                    nal_type,
                });
            }
            nal_unit_type::NON_IDR_SLICE => {
                nals.push(NalUnit {
                    data: nal_data.to_vec(),
                    nal_type,
                });
            }
            _ => {
                // SEI and other NAL types: include as-is
                nals.push(NalUnit {
                    data: nal_data.to_vec(),
                    nal_type,
                });
            }
        }
    }

    ParsedFrame {
        nals,
        sps,
        pps,
        is_keyframe,
    }
}

// ============================================================================
// CMAF Muxer
// ============================================================================

/// Configuration for the CMAF muxer.
#[derive(Debug, Clone)]
pub struct CmafConfig {
    /// Target fragment duration in milliseconds.
    /// Fragments are aligned to keyframes, so actual duration may vary.
    pub fragment_duration_ms: u32,
    /// Timescale for timestamps (e.g., 90000 for standard video).
    pub timescale: u32,
}

impl Default for CmafConfig {
    fn default() -> Self {
        Self {
            fragment_duration_ms: 2000,
            timescale: 90000,
        }
    }
}

/// A pending frame waiting to be muxed.
#[derive(Debug, Clone)]
struct PendingFrame {
    /// Encoded NAL unit data (in AVCC format for mdat)
    data: Vec<u8>,
    /// Duration in timescale units
    duration: u32,
    /// Is this a sync sample (keyframe)
    is_sync: bool,
    /// Composition time offset (PTS - DTS)
    composition_offset: i32,
}

/// Fragmented MP4 muxer for H.264 video streams.
pub struct CmafMuxer {
    config: CmafConfig,
    /// Whether initialization segment has been created
    initialized: bool,
    /// Width in pixels
    width: u32,
    /// Height in pixels
    height: u32,
    /// SPS data (without NAL start code)
    sps: Vec<u8>,
    /// PPS data (without NAL start code)
    pps: Vec<u8>,
    /// Pending frames for current fragment
    pending_frames: Vec<PendingFrame>,
    /// Current fragment sequence number
    sequence_number: u32,
    /// Base DTS for current fragment
    fragment_base_dts: i64,
    /// Last frame's DTS
    last_dts: i64,
    /// Track ID
    track_id: u32,
}

impl CmafMuxer {
    /// Create a new CMAF muxer with the given configuration.
    pub fn new(config: CmafConfig) -> Self {
        Self {
            config,
            initialized: false,
            width: 0,
            height: 0,
            sps: Vec::new(),
            pps: Vec::new(),
            pending_frames: Vec::new(),
            sequence_number: 1,
            fragment_base_dts: 0,
            last_dts: 0,
            track_id: 1,
        }
    }

    /// Create the initialization segment (ftyp + moov).
    ///
    /// This must be called once before adding frames. The initialization segment
    /// contains codec configuration (SPS/PPS) and must be sent before any media
    /// segments.
    pub fn create_init_segment(&mut self, sps: &[u8], pps: &[u8], width: u32, height: u32) -> Vec<u8> {
        self.sps = sps.to_vec();
        self.pps = pps.to_vec();
        self.width = width;
        self.height = height;
        self.initialized = true;

        let mut buf = Vec::new();

        // ftyp box
        self.write_ftyp(&mut buf);

        // moov box
        self.write_moov(&mut buf);

        buf
    }

    /// Add an encoded frame to the muxer.
    ///
    /// Returns a media segment when enough frames have accumulated or when a
    /// new keyframe arrives after the target fragment duration.
    pub fn add_frame(
        &mut self,
        nal_units: &[NalUnit],
        pts: i64,
        dts: i64,
        duration: u32,
        is_keyframe: bool,
    ) -> Option<Vec<u8>> {
        if !self.initialized {
            return None;
        }

        // Check if we should start a new fragment
        let should_flush = if self.pending_frames.is_empty() {
            false
        } else {
            // Flush if we have a keyframe and exceeded target duration
            let fragment_duration =
                (dts - self.fragment_base_dts) * 1000 / self.config.timescale as i64;
            is_keyframe && fragment_duration >= self.config.fragment_duration_ms as i64
        };

        let segment = if should_flush {
            Some(self.flush_fragment())
        } else {
            None
        };

        // Convert NAL units to AVCC format for mdat
        let data = self.nal_units_to_avcc(nal_units);

        // If this is the first frame in a fragment, record base DTS
        if self.pending_frames.is_empty() {
            self.fragment_base_dts = dts;
        }

        let composition_offset = (pts - dts) as i32;

        self.pending_frames.push(PendingFrame {
            data,
            duration,
            is_sync: is_keyframe,
            composition_offset,
        });

        self.last_dts = dts;

        segment
    }

    /// Flush any remaining frames as a final segment.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.pending_frames.is_empty() {
            return None;
        }
        Some(self.flush_fragment())
    }

    /// Convert NAL units to AVCC format (length-prefixed).
    fn nal_units_to_avcc(&self, nal_units: &[NalUnit]) -> Vec<u8> {
        let total_size: usize = nal_units
            .iter()
            .filter(|n| n.is_slice()) // Only include video slices
            .map(|n| 4 + n.data.len())
            .sum();

        let mut buf = Vec::with_capacity(total_size);

        for nal in nal_units.iter().filter(|n| n.is_slice()) {
            let len = nal.data.len() as u32;
            buf.extend_from_slice(&len.to_be_bytes());
            buf.extend_from_slice(&nal.data);
        }

        buf
    }

    /// Create a media segment from pending frames.
    fn flush_fragment(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();

        // styp box
        self.write_styp(&mut buf);

        // moof box
        self.write_moof(&mut buf);

        // mdat box
        self.write_mdat(&mut buf);

        self.sequence_number += 1;
        self.pending_frames.clear();

        buf
    }

    // ========================================
    // Box writing helpers
    // ========================================

    fn write_ftyp(&self, buf: &mut Vec<u8>) {
        let brands = [
            b"isom", // ISO Base Media
            b"iso6", // ISO with fragments
            b"cmfc", // CMAF compliant
            b"cmfv", // CMAF video track
            b"avc1", // H.264
            b"mp41", // MP4 v1
        ];

        let size = 8 + 4 + 4 + (brands.len() * 4);
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"ftyp");
        buf.extend_from_slice(b"isom"); // major brand
        buf.extend_from_slice(&0u32.to_be_bytes()); // minor version
        for brand in &brands {
            buf.extend_from_slice(*brand);
        }
    }

    fn write_styp(&self, buf: &mut Vec<u8>) {
        let brands = [
            b"msdh", // Media Segment Data Handler
            b"msix", // Media Segment Index
            b"cmfc", // CMAF compliant
            b"cmfv", // CMAF video track
        ];
        let size = 8 + 4 + 4 + (brands.len() * 4);
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"styp");
        buf.extend_from_slice(b"cmfv"); // major brand (CMAF video)
        buf.extend_from_slice(&0u32.to_be_bytes()); // minor version
        for brand in &brands {
            buf.extend_from_slice(*brand);
        }
    }

    fn write_moov(&self, buf: &mut Vec<u8>) {
        let mut moov_content = Vec::new();

        // mvhd (movie header)
        self.write_mvhd(&mut moov_content);

        // trak (track)
        self.write_trak(&mut moov_content);

        // mvex (movie extends - required for fragmented MP4)
        self.write_mvex(&mut moov_content);

        let size = 8 + moov_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"moov");
        buf.extend_from_slice(&moov_content);
    }

    fn write_mvhd(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags

        content.extend_from_slice(&0u32.to_be_bytes()); // creation time
        content.extend_from_slice(&0u32.to_be_bytes()); // modification time
        content.extend_from_slice(&self.config.timescale.to_be_bytes()); // timescale
        content.extend_from_slice(&0u32.to_be_bytes()); // duration (unknown for live)

        content.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate (1.0)
        content.extend_from_slice(&0x0100u16.to_be_bytes()); // volume (1.0)
        content.extend_from_slice(&[0; 2]); // reserved
        content.extend_from_slice(&[0; 8]); // reserved

        // Matrix (identity)
        let matrix: [u32; 9] = [
            0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000,
        ];
        for m in &matrix {
            content.extend_from_slice(&m.to_be_bytes());
        }

        content.extend_from_slice(&[0; 24]); // pre_defined
        content.extend_from_slice(&2u32.to_be_bytes()); // next_track_id

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"mvhd");
        buf.extend_from_slice(&content);
    }

    fn write_trak(&self, buf: &mut Vec<u8>) {
        let mut trak_content = Vec::new();

        self.write_tkhd(&mut trak_content);
        self.write_mdia(&mut trak_content);

        let size = 8 + trak_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"trak");
        buf.extend_from_slice(&trak_content);
    }

    fn write_tkhd(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 3]); // flags (track enabled, in movie)

        content.extend_from_slice(&0u32.to_be_bytes()); // creation time
        content.extend_from_slice(&0u32.to_be_bytes()); // modification time
        content.extend_from_slice(&self.track_id.to_be_bytes()); // track id
        content.extend_from_slice(&0u32.to_be_bytes()); // reserved
        content.extend_from_slice(&0u32.to_be_bytes()); // duration (unknown)

        content.extend_from_slice(&[0; 8]); // reserved
        content.extend_from_slice(&0i16.to_be_bytes()); // layer
        content.extend_from_slice(&0i16.to_be_bytes()); // alternate_group
        content.extend_from_slice(&0i16.to_be_bytes()); // volume (video = 0)
        content.extend_from_slice(&0u16.to_be_bytes()); // reserved

        // Matrix
        let matrix: [u32; 9] = [
            0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000,
        ];
        for m in &matrix {
            content.extend_from_slice(&m.to_be_bytes());
        }

        // Width and height as 16.16 fixed point
        content.extend_from_slice(&((self.width as u32) << 16).to_be_bytes());
        content.extend_from_slice(&((self.height as u32) << 16).to_be_bytes());

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"tkhd");
        buf.extend_from_slice(&content);
    }

    fn write_mdia(&self, buf: &mut Vec<u8>) {
        let mut mdia_content = Vec::new();

        self.write_mdhd(&mut mdia_content);
        self.write_hdlr(&mut mdia_content);
        self.write_minf(&mut mdia_content);

        let size = 8 + mdia_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"mdia");
        buf.extend_from_slice(&mdia_content);
    }

    fn write_mdhd(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags

        content.extend_from_slice(&0u32.to_be_bytes()); // creation time
        content.extend_from_slice(&0u32.to_be_bytes()); // modification time
        content.extend_from_slice(&self.config.timescale.to_be_bytes()); // timescale
        content.extend_from_slice(&0u32.to_be_bytes()); // duration

        content.extend_from_slice(&0x55c4u16.to_be_bytes()); // language (und)
        content.extend_from_slice(&0u16.to_be_bytes()); // pre_defined

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"mdhd");
        buf.extend_from_slice(&content);
    }

    fn write_hdlr(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
        content.extend_from_slice(b"vide"); // handler_type
        content.extend_from_slice(&[0; 12]); // reserved
        content.extend_from_slice(b"VideoHandler\0"); // name

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"hdlr");
        buf.extend_from_slice(&content);
    }

    fn write_minf(&self, buf: &mut Vec<u8>) {
        let mut minf_content = Vec::new();

        self.write_vmhd(&mut minf_content);
        self.write_dinf(&mut minf_content);
        self.write_stbl(&mut minf_content);

        let size = 8 + minf_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"minf");
        buf.extend_from_slice(&minf_content);
    }

    fn write_vmhd(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 1]); // flags
        content.extend_from_slice(&0u16.to_be_bytes()); // graphics_mode
        content.extend_from_slice(&[0; 6]); // opcolor

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"vmhd");
        buf.extend_from_slice(&content);
    }

    fn write_dinf(&self, buf: &mut Vec<u8>) {
        let mut dinf_content = Vec::new();

        // dref box
        let mut dref_content = Vec::new();
        dref_content.push(0); // version
        dref_content.extend_from_slice(&[0, 0, 0]); // flags
        dref_content.extend_from_slice(&1u32.to_be_bytes()); // entry_count

        // url entry (self-contained)
        dref_content.extend_from_slice(&12u32.to_be_bytes()); // size
        dref_content.extend_from_slice(b"url ");
        dref_content.push(0); // version
        dref_content.extend_from_slice(&[0, 0, 1]); // flags (self-contained)

        let dref_size = 8 + dref_content.len();
        dinf_content.extend_from_slice(&(dref_size as u32).to_be_bytes());
        dinf_content.extend_from_slice(b"dref");
        dinf_content.extend_from_slice(&dref_content);

        let size = 8 + dinf_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"dinf");
        buf.extend_from_slice(&dinf_content);
    }

    fn write_stbl(&self, buf: &mut Vec<u8>) {
        let mut stbl_content = Vec::new();

        self.write_stsd(&mut stbl_content);
        self.write_empty_stts(&mut stbl_content);
        self.write_empty_stsc(&mut stbl_content);
        self.write_empty_stsz(&mut stbl_content);
        self.write_empty_stco(&mut stbl_content);

        let size = 8 + stbl_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"stbl");
        buf.extend_from_slice(&stbl_content);
    }

    fn write_stsd(&self, buf: &mut Vec<u8>) {
        let mut stsd_content = Vec::new();

        stsd_content.push(0); // version
        stsd_content.extend_from_slice(&[0, 0, 0]); // flags
        stsd_content.extend_from_slice(&1u32.to_be_bytes()); // entry_count

        // avc1 sample entry
        self.write_avc1(&mut stsd_content);

        let size = 8 + stsd_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"stsd");
        buf.extend_from_slice(&stsd_content);
    }

    fn write_avc1(&self, buf: &mut Vec<u8>) {
        let mut avc1_content = Vec::new();

        avc1_content.extend_from_slice(&[0; 6]); // reserved
        avc1_content.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index

        avc1_content.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
        avc1_content.extend_from_slice(&0u16.to_be_bytes()); // reserved
        avc1_content.extend_from_slice(&[0; 12]); // pre_defined

        avc1_content.extend_from_slice(&(self.width as u16).to_be_bytes());
        avc1_content.extend_from_slice(&(self.height as u16).to_be_bytes());

        avc1_content.extend_from_slice(&0x00480000u32.to_be_bytes()); // horiz resolution 72 dpi
        avc1_content.extend_from_slice(&0x00480000u32.to_be_bytes()); // vert resolution 72 dpi
        avc1_content.extend_from_slice(&0u32.to_be_bytes()); // reserved
        avc1_content.extend_from_slice(&1u16.to_be_bytes()); // frame_count

        // Compressor name (32 bytes)
        let mut compressor = [0u8; 32];
        let name = b"xoq-cmaf";
        compressor[0] = name.len() as u8;
        compressor[1..1 + name.len()].copy_from_slice(name);
        avc1_content.extend_from_slice(&compressor);

        avc1_content.extend_from_slice(&0x0018u16.to_be_bytes()); // depth (24-bit)
        avc1_content.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined

        // avcC box
        self.write_avcc(&mut avc1_content);

        let size = 8 + avc1_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"avc1");
        buf.extend_from_slice(&avc1_content);
    }

    fn write_avcc(&self, buf: &mut Vec<u8>) {
        let mut avcc_content = Vec::new();

        avcc_content.push(1); // configuration_version

        // Profile, compatibility, and level from SPS
        if self.sps.len() >= 4 {
            avcc_content.push(self.sps[1]); // profile_idc
            avcc_content.push(self.sps[2]); // profile_compatibility
            avcc_content.push(self.sps[3]); // level_idc
        } else {
            avcc_content.extend_from_slice(&[0x64, 0x00, 0x1f]); // High profile, level 3.1
        }

        avcc_content.push(0xFF); // length_size_minus_one (3 = 4 bytes) | reserved (0b111111)

        // SPS
        avcc_content.push(0xE1); // num_sps | reserved (0b111)
        avcc_content.extend_from_slice(&(self.sps.len() as u16).to_be_bytes());
        avcc_content.extend_from_slice(&self.sps);

        // PPS
        avcc_content.push(1); // num_pps
        avcc_content.extend_from_slice(&(self.pps.len() as u16).to_be_bytes());
        avcc_content.extend_from_slice(&self.pps);

        let size = 8 + avcc_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"avcC");
        buf.extend_from_slice(&avcc_content);
    }

    fn write_empty_stts(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();
        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&0u32.to_be_bytes()); // entry_count

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"stts");
        buf.extend_from_slice(&content);
    }

    fn write_empty_stsc(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();
        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&0u32.to_be_bytes()); // entry_count

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"stsc");
        buf.extend_from_slice(&content);
    }

    fn write_empty_stsz(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();
        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&0u32.to_be_bytes()); // sample_size
        content.extend_from_slice(&0u32.to_be_bytes()); // sample_count

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"stsz");
        buf.extend_from_slice(&content);
    }

    fn write_empty_stco(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();
        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&0u32.to_be_bytes()); // entry_count

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"stco");
        buf.extend_from_slice(&content);
    }

    fn write_mvex(&self, buf: &mut Vec<u8>) {
        let mut mvex_content = Vec::new();

        // trex box
        self.write_trex(&mut mvex_content);

        let size = 8 + mvex_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"mvex");
        buf.extend_from_slice(&mvex_content);
    }

    fn write_trex(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&self.track_id.to_be_bytes()); // track_id
        content.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
        content.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration
        content.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
        content.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"trex");
        buf.extend_from_slice(&content);
    }

    fn write_moof(&self, buf: &mut Vec<u8>) {
        let mut moof_content = Vec::new();

        // mfhd (movie fragment header)
        self.write_mfhd(&mut moof_content);

        // traf (track fragment)
        self.write_traf(&mut moof_content);

        let size = 8 + moof_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"moof");
        buf.extend_from_slice(&moof_content);
    }

    fn write_mfhd(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&self.sequence_number.to_be_bytes());

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"mfhd");
        buf.extend_from_slice(&content);
    }

    fn write_traf(&self, buf: &mut Vec<u8>) {
        let mut traf_content = Vec::new();

        // tfhd (track fragment header)
        self.write_tfhd(&mut traf_content);

        // tfdt (track fragment decode time)
        self.write_tfdt(&mut traf_content);

        // trun (track run)
        self.write_trun(&mut traf_content, buf.len());

        let size = 8 + traf_content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"traf");
        buf.extend_from_slice(&traf_content);
    }

    fn write_tfhd(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(0); // version
        // flags: default-base-is-moof (0x020000)
        content.extend_from_slice(&[0x02, 0x00, 0x00]);
        content.extend_from_slice(&self.track_id.to_be_bytes());

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"tfhd");
        buf.extend_from_slice(&content);
    }

    fn write_tfdt(&self, buf: &mut Vec<u8>) {
        let mut content = Vec::new();

        content.push(1); // version (1 for 64-bit time)
        content.extend_from_slice(&[0, 0, 0]); // flags
        content.extend_from_slice(&(self.fragment_base_dts as u64).to_be_bytes());

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"tfdt");
        buf.extend_from_slice(&content);
    }

    fn write_trun(&self, buf: &mut Vec<u8>, _moof_offset: usize) {
        let sample_count = self.pending_frames.len() as u32;

        // Calculate trun size to determine data_offset
        let trun_content_size = 4 + 4 + 4 + (sample_count as usize * 16);
        let trun_size = 8 + trun_content_size;

        // Calculate data_offset from start of moof to start of mdat data
        let tfhd_size = 8 + 8; // version/flags + track_id
        let tfdt_size = 8 + 12; // version/flags + 64-bit time
        let traf_size = 8 + tfhd_size + tfdt_size + trun_size;
        let mfhd_size = 8 + 8;
        let moof_size = 8 + mfhd_size + traf_size;

        // data_offset is from start of moof to first byte of mdat data
        // = moof_size + 8 (mdat header)
        let data_offset = moof_size + 8;

        let mut content = Vec::new();

        content.push(0); // version
        // flags: data-offset-present (0x01), sample-duration (0x100),
        //        sample-size (0x200), sample-flags (0x400),
        //        sample-composition-time-offset (0x800)
        content.extend_from_slice(&[0x00, 0x0F, 0x01]); // all flags
        content.extend_from_slice(&sample_count.to_be_bytes());
        content.extend_from_slice(&(data_offset as u32).to_be_bytes());

        for frame in &self.pending_frames {
            content.extend_from_slice(&frame.duration.to_be_bytes());
            content.extend_from_slice(&(frame.data.len() as u32).to_be_bytes());

            // Sample flags
            let flags = if frame.is_sync {
                0x02000000u32 // depends_on=2 (no other)
            } else {
                0x01010000u32 // depends_on=1 (yes), is_depended_on=1
            };
            content.extend_from_slice(&flags.to_be_bytes());

            content.extend_from_slice(&frame.composition_offset.to_be_bytes());
        }

        let size = 8 + content.len();
        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"trun");
        buf.extend_from_slice(&content);
    }

    fn write_mdat(&self, buf: &mut Vec<u8>) {
        let total_data_size: usize = self.pending_frames.iter().map(|f| f.data.len()).sum();
        let size = 8 + total_data_size;

        buf.extend_from_slice(&(size as u32).to_be_bytes());
        buf.extend_from_slice(b"mdat");

        for frame in &self.pending_frames {
            buf.extend_from_slice(&frame.data);
        }
    }

    /// Get the current sequence number.
    pub fn sequence_number(&self) -> u32 {
        self.sequence_number
    }

    /// Check if the muxer has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Get the number of pending frames.
    pub fn pending_frame_count(&self) -> usize {
        self.pending_frames.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nal_unit_types() {
        let idr = NalUnit { data: vec![0x65, 0x00], nal_type: nal_unit_type::IDR_SLICE };
        assert!(idr.is_idr());
        assert!(idr.is_slice());
        assert!(!idr.is_sps());
        assert!(!idr.is_pps());

        let non_idr = NalUnit { data: vec![0x41, 0x00], nal_type: nal_unit_type::NON_IDR_SLICE };
        assert!(!non_idr.is_idr());
        assert!(non_idr.is_slice());

        let sps = NalUnit { data: vec![0x67, 0x64, 0x00, 0x1f], nal_type: nal_unit_type::SPS };
        assert!(sps.is_sps());
        assert!(!sps.is_slice());

        let pps = NalUnit { data: vec![0x68, 0xee, 0x3c], nal_type: nal_unit_type::PPS };
        assert!(pps.is_pps());
        assert!(!pps.is_slice());
    }

    #[test]
    fn test_nal_unit_to_annex_b() {
        let nal = NalUnit { data: vec![0x65, 0xAA, 0xBB], nal_type: nal_unit_type::IDR_SLICE };
        let annex_b = nal.to_annex_b();
        assert_eq!(&annex_b[..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&annex_b[4..], &[0x65, 0xAA, 0xBB]);
    }

    #[test]
    fn test_parse_annex_b_keyframe() {
        // Build an Annex B stream: SPS + PPS + IDR slice
        let mut data = Vec::new();
        // SPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x67, 0x64, 0x00, 0x1f, 0xAC]);
        // PPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x68, 0xEE, 0x3C, 0x80]);
        // IDR slice
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x65, 0x88, 0x80, 0x40, 0x00]);

        let parsed = parse_annex_b(&data);
        assert!(parsed.is_keyframe);
        assert!(parsed.sps.is_some());
        assert!(parsed.pps.is_some());
        assert_eq!(parsed.sps.unwrap(), vec![0x67, 0x64, 0x00, 0x1f, 0xAC]);
        assert_eq!(parsed.pps.unwrap(), vec![0x68, 0xEE, 0x3C, 0x80]);
        assert_eq!(parsed.nals.len(), 1);
        assert!(parsed.nals[0].is_idr());
    }

    #[test]
    fn test_parse_annex_b_non_keyframe() {
        let mut data = Vec::new();
        // Non-IDR slice only
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x41, 0x9A, 0x00, 0x10]);

        let parsed = parse_annex_b(&data);
        assert!(!parsed.is_keyframe);
        assert!(parsed.sps.is_none());
        assert!(parsed.pps.is_none());
        assert_eq!(parsed.nals.len(), 1);
        assert!(!parsed.nals[0].is_idr());
    }

    #[test]
    fn test_parse_annex_b_3byte_start_codes() {
        let mut data = Vec::new();
        // 3-byte start code + non-IDR slice
        data.extend_from_slice(&[0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x41, 0x9A, 0x00]);

        let parsed = parse_annex_b(&data);
        assert_eq!(parsed.nals.len(), 1);
        assert_eq!(parsed.nals[0].nal_type, nal_unit_type::NON_IDR_SLICE);
    }

    #[test]
    fn test_parse_annex_b_empty() {
        let parsed = parse_annex_b(&[]);
        assert!(!parsed.is_keyframe);
        assert!(parsed.sps.is_none());
        assert!(parsed.pps.is_none());
        assert!(parsed.nals.is_empty());
    }

    #[test]
    fn test_default_config() {
        let config = CmafConfig::default();
        assert_eq!(config.fragment_duration_ms, 2000);
        assert_eq!(config.timescale, 90000);
    }

    #[test]
    fn test_muxer_initialization() {
        let mut muxer = CmafMuxer::new(CmafConfig::default());
        assert!(!muxer.is_initialized());

        let sps = vec![0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50];
        let pps = vec![0x68, 0xee, 0x3c, 0x80];

        let init = muxer.create_init_segment(&sps, &pps, 1920, 1080);
        assert!(muxer.is_initialized());
        assert!(!init.is_empty());

        // Check ftyp box
        assert_eq!(&init[4..8], b"ftyp");
        // Check moov box exists
        assert!(init.windows(4).any(|w| w == b"moov"));
    }

    #[test]
    fn test_ftyp_box() {
        let muxer = CmafMuxer::new(CmafConfig::default());
        let mut buf = Vec::new();
        muxer.write_ftyp(&mut buf);

        let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        assert_eq!(&buf[4..8], b"ftyp");
        assert_eq!(size as usize, buf.len());
    }

    #[test]
    fn test_add_frame_before_init() {
        let mut muxer = CmafMuxer::new(CmafConfig::default());
        let nals = vec![NalUnit { data: vec![0x65, 0x88], nal_type: nal_unit_type::IDR_SLICE }];
        let result = muxer.add_frame(&nals, 0, 0, 3000, true);
        assert!(result.is_none());
    }

    #[test]
    fn test_muxer_add_frame_and_flush() {
        let mut muxer = CmafMuxer::new(CmafConfig {
            fragment_duration_ms: 33,
            timescale: 90000,
        });

        let sps = vec![0x67, 0x64, 0x00, 0x1f];
        let pps = vec![0x68, 0xee, 0x3c, 0x80];
        muxer.create_init_segment(&sps, &pps, 640, 480);

        // First frame (keyframe) - no segment returned yet
        let nals = vec![NalUnit { data: vec![0x65, 0x88, 0x80], nal_type: nal_unit_type::IDR_SLICE }];
        let seg = muxer.add_frame(&nals, 0, 0, 3000, true);
        assert!(seg.is_none());
        assert_eq!(muxer.pending_frame_count(), 1);

        // Second frame (non-keyframe)
        let nals = vec![NalUnit { data: vec![0x41, 0x9A, 0x00], nal_type: nal_unit_type::NON_IDR_SLICE }];
        let seg = muxer.add_frame(&nals, 3000, 3000, 3000, false);
        assert!(seg.is_none());
        assert_eq!(muxer.pending_frame_count(), 2);

        // Flush remaining
        let seg = muxer.flush();
        assert!(seg.is_some());
        let seg = seg.unwrap();
        // Check styp box
        assert!(seg.windows(4).any(|w| w == b"styp"));
        // Check moof box
        assert!(seg.windows(4).any(|w| w == b"moof"));
        // Check mdat box
        assert!(seg.windows(4).any(|w| w == b"mdat"));
    }
}
