//! RealSense depth camera server - publishes color (H.264) + depth (H.264) over MoQ.
//!
//! Captures aligned color + depth frames from an Intel RealSense camera and
//! publishes two MoQ tracks on a single broadcast:
//!   - "video" track: Color frames encoded with NVENC H.264, muxed as CMAF/fMP4
//!   - "depth" track: Depth frames encoded with NVENC H.264, muxed as CMAF/fMP4
//!
//! Depth values are mapped from [min_depth_mm, max_depth_mm] to grayscale [0, 255].
//! Out-of-range pixels become black (0).
//!
//! Usage:
//! ```bash
//! cargo run --example realsense_server --features realsense -- \
//!   --relay https://172.18.133.111:4443 --path anon/realsense
//! ```
//!
//! Requires:
//! - Intel RealSense camera (D435, D455, etc.)
//! - librealsense2-dev system package
//! - NVIDIA GPU with NVENC (RTX 30+ for AV1)

use anyhow::Result;
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_H264_GUID, NV_ENC_PIC_FLAGS, NV_ENC_PIC_TYPE,
        NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO,
    },
    Bitstream, Buffer, EncodePictureParams, Encoder, EncoderInitParams, Session,
};
use xoq::cmaf::{parse_annex_b, CmafConfig, CmafMuxer};
use xoq::realsense::RealSenseCamera;
use xoq::MoqBuilder;

// ============================================================================
// NVENC H.264 encoder (reused from camera_server pattern)
// ============================================================================

struct NvencH264Encoder {
    input_buffer: std::mem::ManuallyDrop<Buffer<'static>>,
    output_bitstream: std::mem::ManuallyDrop<Bitstream<'static>>,
    session: *mut Session,
    width: u32,
    height: u32,
    nv12_buffer: Vec<u8>,
    frame_count: u64,
    fps: u32,
}

unsafe impl Send for NvencH264Encoder {}

impl Drop for NvencH264Encoder {
    fn drop(&mut self) {
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.input_buffer);
            std::mem::ManuallyDrop::drop(&mut self.output_bitstream);
        }
        if !self.session.is_null() {
            let session = unsafe { Box::from_raw(self.session) };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(session)));
        }
    }
}

impl NvencH264Encoder {
    fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let cuda_ctx = CudaContext::new(0)?;
        let encoder = Encoder::initialize_with_cuda(cuda_ctx)?;

        let mut preset_config = encoder.get_preset_config(
            NV_ENC_CODEC_H264_GUID,
            NV_ENC_PRESET_P4_GUID,
            NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        )?;

        let config = &mut preset_config.presetCfg;
        config.gopLength = fps;
        config.frameIntervalP = 1;
        config.rcParams.averageBitRate = bitrate;
        config.rcParams.maxBitRate = bitrate;
        config.rcParams.vbvBufferSize = bitrate / fps;

        unsafe {
            config
                .encodeCodecConfig
                .h264Config
                .set_enableIntraRefresh(0);
            config.encodeCodecConfig.h264Config.idrPeriod = fps;
            config.encodeCodecConfig.h264Config.set_repeatSPSPPS(1);
        }

        let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, width, height);
        init_params
            .preset_guid(NV_ENC_PRESET_P4_GUID)
            .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
            .framerate(fps, 1)
            .encode_config(config);

        let buffer_format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;
        let session = encoder.start_session(buffer_format, init_params)?;
        let session_ptr = Box::into_raw(Box::new(session));
        let session_ref: &'static Session = unsafe { &*session_ptr };

        let input_buffer = session_ref.create_input_buffer()?;
        let output_bitstream = session_ref.create_output_bitstream()?;
        let nv12_size = (width * height * 3 / 2) as usize;

        Ok(Self {
            session: session_ptr,
            input_buffer: std::mem::ManuallyDrop::new(input_buffer),
            output_bitstream: std::mem::ManuallyDrop::new(output_bitstream),
            width,
            height,
            nv12_buffer: vec![0u8; nv12_size],
            frame_count: 0,
            fps,
        })
    }

    fn encode_rgb(&mut self, rgb: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.rgb_to_nv12(rgb);
        self.encode_nv12(timestamp_us)
    }

    fn encode_nv12(&mut self, timestamp_us: u64) -> Result<Vec<u8>> {
        {
            let mut lock = self.input_buffer.lock()?;
            unsafe { lock.write_nv12(&self.nv12_buffer, self.width, self.height) };
        }

        let is_idr = self.frame_count % self.fps as u64 == 0;
        let picture_type = if is_idr {
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR
        } else {
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_P
        };
        let encode_pic_flags = if is_idr {
            NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
        } else {
            0
        };
        self.frame_count += 1;

        let session: &Session = unsafe { &*self.session };
        session.encode_picture(
            &mut *self.input_buffer,
            &mut *self.output_bitstream,
            EncodePictureParams {
                input_timestamp: timestamp_us,
                picture_type,
                encode_pic_flags,
                ..Default::default()
            },
        )?;

        let lock = self.output_bitstream.lock()?;
        Ok(lock.data().to_vec())
    }

    fn rgb_to_nv12(&mut self, rgb: &[u8]) {
        let width = self.width as usize;
        let height = self.height as usize;
        let y_size = width * height;

        for y in 0..height {
            for x in 0..width {
                let rgb_idx = (y * width + x) * 3;
                let r = rgb.get(rgb_idx).copied().unwrap_or(0) as f32;
                let g = rgb.get(rgb_idx + 1).copied().unwrap_or(0) as f32;
                let b = rgb.get(rgb_idx + 2).copied().unwrap_or(0) as f32;

                let y_val = (0.299 * r + 0.587 * g + 0.114 * b).clamp(0.0, 255.0) as u8;
                self.nv12_buffer[y * width + x] = y_val;

                if y % 2 == 0 && x % 2 == 0 {
                    let u = ((-0.169 * r - 0.331 * g + 0.500 * b) + 128.0).clamp(0.0, 255.0) as u8;
                    let v = ((0.500 * r - 0.419 * g - 0.081 * b) + 128.0).clamp(0.0, 255.0) as u8;
                    let uv_idx = y_size + (y / 2) * width + x;
                    self.nv12_buffer[uv_idx] = u;
                    self.nv12_buffer[uv_idx + 1] = v;
                }
            }
        }
    }
}

// ============================================================================
// Depth → grayscale RGB conversion
// ============================================================================

/// Convert depth u16 mm values to grayscale RGB buffer (R=G=B=depth_8bit).
/// Linearly maps [min_depth_mm, max_depth_mm] → [1, 255].
/// Invalid pixels (0, out of range) are filled with nearest valid neighbor
/// to avoid sharp black spots that waste H.264 bitrate.
fn depth_to_rgb(
    depth_mm: &[u16],
    rgb: &mut Vec<u8>,
    width: u32,
    height: u32,
    min_depth_mm: u32,
    max_depth_mm: u32,
) {
    let w = width as usize;
    let pixel_count = (width * height) as usize;
    rgb.resize(pixel_count * 3, 0);
    let range = (max_depth_mm - min_depth_mm) as f32;

    // First pass: compute grayscale, mark invalid as 0
    // Use a temporary buffer for the grayscale values
    let mut gray = vec![0u8; pixel_count];
    for i in 0..pixel_count.min(depth_mm.len()) {
        let d = depth_mm[i] as u32;
        if d >= min_depth_mm && d <= max_depth_mm {
            // Map to 1-255 (reserve 0 for invalid)
            gray[i] = (((d - min_depth_mm) as f32 / range) * 254.0 + 1.0) as u8;
        }
    }

    // Second pass: fill invalid pixels with nearest valid neighbor (left-to-right per row)
    for y in 0..height as usize {
        let row = y * w;
        let mut last_valid = 0u8;
        // Forward pass
        for x in 0..w {
            if gray[row + x] != 0 {
                last_valid = gray[row + x];
            } else {
                gray[row + x] = last_valid;
            }
        }
        // Backward pass for pixels that had no valid left neighbor
        last_valid = 0;
        for x in (0..w).rev() {
            if gray[row + x] != 0 {
                last_valid = gray[row + x];
            } else {
                gray[row + x] = last_valid;
            }
        }
    }

    // Write to RGB
    for i in 0..pixel_count {
        let val = gray[i];
        let idx = i * 3;
        rgb[idx] = val;
        rgb[idx + 1] = val;
        rgb[idx + 2] = val;
    }
}

// ============================================================================
// CLI argument parsing
// ============================================================================

struct Args {
    relay: String,
    path: String,
    serial: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    color_bitrate: u32,
    depth_bitrate: u32,
    min_depth_mm: u32,
    max_depth_mm: u32,
    insecure: bool,
}

fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().collect();
    let mut result = Args {
        relay: "https://cdn.moq.dev".to_string(),
        path: "anon/realsense".to_string(),
        serial: None,
        width: 640,
        height: 480,
        fps: 30,
        color_bitrate: 2_000_000,
        depth_bitrate: 1_000_000,
        min_depth_mm: 0,
        max_depth_mm: 0, // 0 = auto-calibrate
        insecure: false,
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--relay" if i + 1 < args.len() => {
                result.relay = args[i + 1].clone();
                result.insecure = true; // self-hosted relays typically use self-signed certs
                i += 2;
            }
            "--path" if i + 1 < args.len() => {
                result.path = args[i + 1].clone();
                i += 2;
            }
            "--serial" if i + 1 < args.len() => {
                result.serial = Some(args[i + 1].clone());
                i += 2;
            }
            "--width" if i + 1 < args.len() => {
                result.width = args[i + 1].parse().unwrap_or(640);
                i += 2;
            }
            "--height" if i + 1 < args.len() => {
                result.height = args[i + 1].parse().unwrap_or(480);
                i += 2;
            }
            "--fps" if i + 1 < args.len() => {
                result.fps = args[i + 1].parse().unwrap_or(30);
                i += 2;
            }
            "--bitrate" if i + 1 < args.len() => {
                result.color_bitrate = args[i + 1].parse().unwrap_or(2_000_000);
                i += 2;
            }
            "--depth-bitrate" if i + 1 < args.len() => {
                result.depth_bitrate = args[i + 1].parse().unwrap_or(1_000_000);
                i += 2;
            }
            "--min-depth" if i + 1 < args.len() => {
                result.min_depth_mm = args[i + 1].parse().unwrap_or(10);
                i += 2;
            }
            "--max-depth" if i + 1 < args.len() => {
                result.max_depth_mm = args[i + 1].parse().unwrap_or(0);
                i += 2;
            }
            "--insecure" => {
                result.insecure = true;
                i += 1;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            _ => {
                i += 1;
            }
        }
    }

    result
}

fn print_usage() {
    println!("RealSense Server - Color (H.264) + Depth (H.264) over MoQ");
    println!();
    println!("Usage: realsense_server [options]");
    println!();
    println!("Options:");
    println!("  --relay <url>           MoQ relay URL (default: https://cdn.moq.dev)");
    println!("  --path <path>           MoQ broadcast path (default: anon/realsense)");
    println!("  --width <px>            Resolution width (default: 640)");
    println!("  --height <px>           Resolution height (default: 480)");
    println!("  --fps <rate>            Framerate (default: 30)");
    println!("  --bitrate <bps>         H.264 color bitrate (default: 2000000)");
    println!("  --depth-bitrate <bps>   H.264 depth bitrate (default: 1000000)");
    println!("  --min-depth <mm>        Min depth in mm (default: 0)");
    println!("  --max-depth <mm>        Max depth in mm (default: auto-calibrate)");
    println!("  --serial <serial>       RealSense serial number (default: first device)");
    println!("  --insecure              Disable TLS verification");
    println!();
    println!("Tracks published:");
    println!("  \"video\" - Color H.264/CMAF (fMP4)");
    println!("  \"depth\" - Depth H.264/CMAF (fMP4), grayscale 0-255 = linear map [min, max] mm");
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=info".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let args = parse_args();

    println!();
    println!("========================================");
    println!("RealSense Server");
    println!("========================================");
    println!("Relay:     {}", args.relay);
    println!("Path:      {}", args.path);
    println!(
        "Resolution: {}x{} @ {}fps",
        args.width, args.height, args.fps
    );
    println!("Color:     H.264 NVENC, {} kbps", args.color_bitrate / 1000);
    if args.max_depth_mm > 0 {
        let step = (args.max_depth_mm - args.min_depth_mm) as f32 / 255.0;
        println!(
            "Depth:     H.264 NVENC, {} kbps, {}–{}mm ({:.1}mm/step)",
            args.depth_bitrate / 1000,
            args.min_depth_mm,
            args.max_depth_mm,
            step
        );
    } else {
        println!(
            "Depth:     H.264 NVENC, {} kbps, auto-calibrating...",
            args.depth_bitrate / 1000
        );
    }
    println!("Tracks:    \"video\" (H.264/CMAF), \"depth\" (H.264/CMAF)");
    println!("========================================");
    println!();

    // List connected devices
    match RealSenseCamera::list_devices() {
        Ok(devices) => {
            println!("Connected RealSense devices:");
            for (i, (name, serial)) in devices.iter().enumerate() {
                println!("  [{}] {} (serial: {})", i, name, serial);
            }
            println!();
        }
        Err(e) => tracing::warn!("Could not list devices: {}", e),
    }

    // Open RealSense camera
    tracing::info!("Opening RealSense camera...");
    let mut camera =
        RealSenseCamera::open(args.width, args.height, args.fps, args.serial.as_deref())?;
    let intr = camera.intrinsics();
    tracing::info!(
        "RealSense opened: {}x{} @ {}fps, depth_scale={}, fx={:.1} fy={:.1} ppx={:.1} ppy={:.1}",
        camera.width(),
        camera.height(),
        args.fps,
        camera.depth_scale(),
        intr.fx,
        intr.fy,
        intr.ppx,
        intr.ppy,
    );

    // Initialize H.264 encoder for color
    tracing::info!("Initializing H.264 encoder for color...");
    let mut h264_encoder =
        NvencH264Encoder::new(args.width, args.height, args.fps, args.color_bitrate)?;
    tracing::info!("H.264 encoder ready");

    // Initialize H.264 encoder for depth (grayscale)
    tracing::info!("Initializing H.264 encoder for depth...");
    let mut depth_encoder =
        NvencH264Encoder::new(args.width, args.height, args.fps, args.depth_bitrate)?;
    tracing::info!("Depth H.264 encoder ready");

    // Initialize CMAF muxers (both H.264)
    let mut h264_muxer = CmafMuxer::new(CmafConfig {
        fragment_duration_ms: 33,
        timescale: 90000,
    });
    let mut depth_muxer = CmafMuxer::new(CmafConfig {
        fragment_duration_ms: 33,
        timescale: 90000,
    });

    // Connect to MoQ relay and create both tracks
    tracing::info!("Connecting to MoQ relay: {}", args.relay);
    let mut builder = MoqBuilder::new().relay(&args.relay).path(&args.path);
    if args.insecure {
        builder = builder.disable_tls_verify();
    }
    let mut publisher = builder.connect_publisher().await?;
    let mut video_track = publisher.create_track("video");
    let mut depth_track = publisher.create_track("depth");
    tracing::info!("MoQ connected, tracks: video + depth");

    // State for init segments
    let mut h264_init_segment: Option<Vec<u8>> = None;
    let mut depth_init_segment: Option<Vec<u8>> = None;
    let mut depth_rgb = Vec::new();
    let mut frame_count = 0u64;

    // Auto-calibrate depth range from first fps frames (if max_depth not set)
    let auto_calibrate = args.max_depth_mm == 0;
    let calibration_frames = if auto_calibrate { args.fps as u64 } else { 0 };
    let mut cal_min = u16::MAX;
    let mut cal_max = 0u16;
    let mut min_depth_mm = args.min_depth_mm;
    let mut max_depth_mm = args.max_depth_mm;

    if auto_calibrate {
        tracing::info!(
            "Auto-calibrating depth range from first {} frames...",
            calibration_frames
        );
    }

    loop {
        // Capture aligned color + depth
        let frames = camera.capture()?;
        let timestamp_us = frames.timestamp_us;

        // During calibration, collect min/max depth
        if auto_calibrate && frame_count < calibration_frames {
            for &d in &frames.depth_mm {
                if d > 0 {
                    if d < cal_min {
                        cal_min = d;
                    }
                    if d > cal_max {
                        cal_max = d;
                    }
                }
            }

            // At end of calibration, set min/max
            if frame_count == calibration_frames - 1 {
                let range = cal_max.saturating_sub(cal_min) as u32;
                let margin = range / 10;
                min_depth_mm = (cal_min as u32).saturating_sub(margin);
                max_depth_mm = cal_max as u32 + margin;
                let step = (max_depth_mm - min_depth_mm) as f32 / 255.0;

                println!();
                println!("========================================");
                println!("Depth auto-calibrated:");
                println!("  Observed: {}–{}mm", cal_min, cal_max);
                println!(
                    "  Mapping:  {}–{}mm ({:.1}mm/step)",
                    min_depth_mm, max_depth_mm, step
                );
                println!("========================================");
                println!();
            }

            frame_count += 1;
            continue;
        }

        // ---- Color pipeline: RGB → NV12 → H.264 → CMAF → "video" track ----
        let h264_data = h264_encoder.encode_rgb(&frames.color_rgb, timestamp_us)?;

        let parsed = parse_annex_b(&h264_data);

        // Create H.264 init segment on first keyframe
        if h264_init_segment.is_none() {
            if let (Some(ref sps), Some(ref pps)) = (&parsed.sps, &parsed.pps) {
                let init = h264_muxer.create_init_segment(sps, pps, frames.width, frames.height);
                video_track.write(init.clone());
                h264_init_segment = Some(init);
                tracing::info!("Sent H.264 CMAF init segment");
            }
        }

        let pts = ((frame_count - calibration_frames) as i64) * 90000 / args.fps as i64;
        let dts = pts;
        let duration = (90000 / args.fps) as u32;

        if let Some(segment) =
            h264_muxer.add_frame(&parsed.nals, pts, dts, duration, parsed.is_keyframe)
        {
            if parsed.is_keyframe {
                if let Some(ref init) = h264_init_segment {
                    let mut combined = init.clone();
                    combined.extend_from_slice(&segment);
                    video_track.write(combined);
                } else {
                    video_track.write(segment);
                }
            } else {
                video_track.write(segment);
            }
        }

        // ---- Depth pipeline: u16 mm → grayscale RGB → NV12 → H.264 → CMAF → "depth" track ----
        depth_to_rgb(
            &frames.depth_mm,
            &mut depth_rgb,
            frames.width,
            frames.height,
            min_depth_mm,
            max_depth_mm,
        );
        let depth_h264 = depth_encoder.encode_rgb(&depth_rgb, timestamp_us)?;

        let depth_parsed = parse_annex_b(&depth_h264);

        // Create depth init segment on first keyframe
        if depth_init_segment.is_none() {
            if let (Some(ref sps), Some(ref pps)) = (&depth_parsed.sps, &depth_parsed.pps) {
                let init = depth_muxer.create_init_segment(sps, pps, frames.width, frames.height);
                depth_track.write(init.clone());
                depth_init_segment = Some(init);
                tracing::info!("Sent depth H.264 CMAF init segment");
            }
        }

        if let Some(segment) = depth_muxer.add_frame(
            &depth_parsed.nals,
            pts,
            dts,
            duration,
            depth_parsed.is_keyframe,
        ) {
            if depth_parsed.is_keyframe {
                if let Some(ref init) = depth_init_segment {
                    let mut combined = init.clone();
                    combined.extend_from_slice(&segment);
                    depth_track.write(combined);
                } else {
                    depth_track.write(segment);
                }
            } else {
                depth_track.write(segment);
            }
        }

        frame_count += 1;

        if (frame_count - calibration_frames) % (args.fps as u64) == 0 {
            let mut dmin = u16::MAX;
            let mut dmax = 0u16;
            let mut nonzero = 0u32;
            for &d in &frames.depth_mm {
                if d > 0 {
                    nonzero += 1;
                    if d < dmin {
                        dmin = d;
                    }
                    if d > dmax {
                        dmax = d;
                    }
                }
            }
            tracing::info!(
                "Frame {}: color {}B, depth {}B | depth: {}–{}mm, {}/{} valid",
                frame_count - calibration_frames,
                h264_data.len(),
                depth_h264.len(),
                dmin,
                dmax,
                nonzero,
                frames.depth_mm.len()
            );
        }
    }
}
