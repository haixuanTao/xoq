//! RealSense depth camera server - publishes color (AV1) + depth (AV1 10-bit) over MoQ.
//!
//! Captures aligned color + depth frames from an Intel RealSense camera and
//! publishes two MoQ tracks on a single broadcast:
//!   - "video" track: Color frames encoded with NVENC AV1 (8-bit), muxed as CMAF/fMP4
//!   - "depth" track: Depth frames encoded with NVENC AV1 (10-bit P010), muxed as CMAF/fMP4
//!
//! Depth values are mapped to 10-bit grayscale: gray10 = min(depth_mm >> 2, 1023).
//! P010 format stores these as MSB-aligned u16 (val << 6).
//!
//! Usage:
//! ```bash
//! cargo run --bin realsense_server --features realsense -- \
//!   --relay https://172.18.133.111:4443 --path anon/realsense
//! ```
//!
//! Requires:
//! - Intel RealSense camera (D435, D455, etc.)
//! - librealsense2-dev system package
//! - NVIDIA GPU with NVENC (RTX 30+ for AV1)

use anyhow::Result;
use xoq::cmaf::{parse_av1_frame, Av1CmafMuxer, CmafConfig};
use xoq::nvenc_av1::NvencAv1Encoder;
use xoq::realsense::RealSenseCamera;
use xoq::MoqBuilder;

// ============================================================================
// Depth → P010 conversion (10-bit)
// ============================================================================

/// Bit shift for depth encoding: gray10 = depth_mm >> SHIFT, clamped to 1023.
/// Client decodes: depth_mm = gray10 << SHIFT.
/// SHIFT=0 → 1mm/step, max 1023mm (~1m).
const DEPTH_SHIFT: u32 = 0;

/// Convert depth u16 mm values to P010 buffer (10-bit grayscale in YUV420).
/// gray10 = min(depth_mm >> DEPTH_SHIFT, 1023). 0 = no measurement.
/// Invalid pixels are filled with nearest valid neighbor to help compression.
fn depth_to_p010(depth_mm: &[u16], p010_buf: &mut Vec<u8>, width: u32, height: u32) {
    let w = width as usize;
    let h = height as usize;
    let y_bytes = w * h * 2; // Y plane: u16 per pixel
    let uv_bytes = w * (h / 2) * 2; // UV plane: interleaved u16 U/V, half height
    p010_buf.resize(y_bytes + uv_bytes, 0);

    // Build 10-bit gray values
    let mut gray = vec![0u16; w * h];
    for i in 0..(w * h).min(depth_mm.len()) {
        let d = depth_mm[i] as u32;
        if d > 0 {
            gray[i] = ((d >> DEPTH_SHIFT).min(1023)) as u16;
        }
    }

    // Fill invalid pixels with nearest valid neighbor (horizontal then vertical)
    // Horizontal pass (bidirectional per row)
    for y in 0..h {
        let row = y * w;
        let mut last_valid = 0u16;
        for x in 0..w {
            if gray[row + x] != 0 {
                last_valid = gray[row + x];
            } else {
                gray[row + x] = last_valid;
            }
        }
        last_valid = 0;
        for x in (0..w).rev() {
            if gray[row + x] != 0 {
                last_valid = gray[row + x];
            } else {
                gray[row + x] = last_valid;
            }
        }
    }
    // Vertical pass (fills remaining gaps from rows that were entirely empty)
    for x in 0..w {
        let mut last_valid = 0u16;
        for y in 0..h {
            if gray[y * w + x] != 0 {
                last_valid = gray[y * w + x];
            } else {
                gray[y * w + x] = last_valid;
            }
        }
        last_valid = 0;
        for y in (0..h).rev() {
            if gray[y * w + x] != 0 {
                last_valid = gray[y * w + x];
            } else {
                gray[y * w + x] = last_valid;
            }
        }
    }

    // Write Y plane as MSB-aligned P010: val << 6 (10-bit in top bits of u16)
    for i in 0..(w * h) {
        let val = (gray[i] << 6).to_le_bytes(); // little-endian u16
        p010_buf[i * 2] = val[0];
        p010_buf[i * 2 + 1] = val[1];
    }

    // UV plane: neutral chroma (512 in 10-bit = 0x8000 in P010 MSB-aligned)
    for i in 0..(w * (h / 2)) {
        let offset = y_bytes + i * 2;
        p010_buf[offset] = 0x00; // 0x8000 LE
        p010_buf[offset + 1] = 0x80;
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
        color_bitrate: 4_000_000,
        depth_bitrate: 4_000_000,
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
    println!("RealSense Server - Color (AV1) + Depth (AV1 10-bit) over MoQ");
    println!();
    println!("Usage: realsense_server [options]");
    println!();
    println!("Options:");
    println!("  --relay <url>           MoQ relay URL (default: https://cdn.moq.dev)");
    println!("  --path <path>           MoQ broadcast path (default: anon/realsense)");
    println!("  --width <px>            Resolution width (default: 640)");
    println!("  --height <px>           Resolution height (default: 480)");
    println!("  --fps <rate>            Framerate (default: 30)");
    println!("  --bitrate <bps>         AV1 color bitrate (default: 4000000)");
    println!("  --depth-bitrate <bps>   AV1 depth bitrate (default: 4000000)");
    println!("  --serial <serial>       RealSense serial number (default: first device)");
    println!("  --insecure              Disable TLS verification");
    println!();
    println!("Tracks published:");
    println!("  \"video\" - Color AV1/CMAF (fMP4), 8-bit");
    println!(
        "  \"depth\" - Depth AV1/CMAF (fMP4), 10-bit P010, gray10 = depth_mm >> {} ({}mm/step, max {}mm)",
        DEPTH_SHIFT,
        1u32 << DEPTH_SHIFT,
        1023u32 << DEPTH_SHIFT
    );
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
    println!(
        "Color:     AV1 NVENC 8-bit, {} kbps",
        args.color_bitrate / 1000
    );
    println!(
        "Depth:     AV1 NVENC 10-bit P010, {} kbps, shift={} ({}mm/step, max {}mm)",
        args.depth_bitrate / 1000,
        DEPTH_SHIFT,
        1u32 << DEPTH_SHIFT,
        1023u32 << DEPTH_SHIFT,
    );
    println!("Tracks:    \"video\" (AV1/CMAF), \"depth\" (AV1 10-bit/CMAF)");
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

    // Initialize AV1 encoder for color (8-bit NV12)
    tracing::info!("Initializing AV1 encoder for color (8-bit)...");
    let mut color_encoder =
        NvencAv1Encoder::new(args.width, args.height, args.fps, args.color_bitrate, false)?;
    tracing::info!("AV1 color encoder ready");

    // Initialize AV1 encoder for depth (10-bit P010)
    tracing::info!("Initializing AV1 encoder for depth (10-bit P010)...");
    let mut depth_encoder =
        NvencAv1Encoder::new(args.width, args.height, args.fps, args.depth_bitrate, true)?;
    tracing::info!("AV1 depth encoder ready");

    // Initialize AV1 CMAF muxers
    let mut color_muxer = Av1CmafMuxer::new(CmafConfig {
        fragment_duration_ms: 33,
        timescale: 90000,
    });
    let mut depth_muxer = Av1CmafMuxer::new(CmafConfig {
        fragment_duration_ms: 33,
        timescale: 90000,
    });
    depth_muxer.set_high_bitdepth(true); // 10-bit depth encoding

    // Connect to MoQ relay — try :4443 (QUIC-only) first, fall back to default port
    let parsed_relay = url::Url::parse(&args.relay)?;
    let relay_4443 = if parsed_relay.port().is_none() {
        // No explicit port — try :4443 first
        format!(
            "{}://{}:4443{}",
            parsed_relay.scheme(),
            parsed_relay.host_str().unwrap_or("localhost"),
            parsed_relay.path()
        )
    } else {
        args.relay.clone()
    };

    tracing::info!(
        "Connecting to MoQ relay: {} (trying :4443 first)",
        args.relay
    );
    let mut builder_4443 = MoqBuilder::new().relay(&relay_4443).path(&args.path);
    if args.insecure {
        builder_4443 = builder_4443.disable_tls_verify();
    }

    let mut publisher = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        builder_4443.connect_publisher(),
    )
    .await
    {
        Ok(Ok(pub_)) => {
            tracing::info!("Connected to MoQ relay on :4443");
            pub_
        }
        Ok(Err(e)) => {
            tracing::warn!("MoQ :4443 failed: {}, falling back to {}", e, args.relay);
            let mut builder = MoqBuilder::new().relay(&args.relay).path(&args.path);
            if args.insecure {
                builder = builder.disable_tls_verify();
            }
            builder.connect_publisher().await?
        }
        Err(_) => {
            tracing::warn!("MoQ :4443 timed out, falling back to {}", args.relay);
            let mut builder = MoqBuilder::new().relay(&args.relay).path(&args.path);
            if args.insecure {
                builder = builder.disable_tls_verify();
            }
            builder.connect_publisher().await?
        }
    };
    let mut video_track = publisher.create_track("video");
    let mut depth_track = publisher.create_track("depth");
    tracing::info!("MoQ connected, tracks: video + depth");

    // State for init segments
    let mut color_init_segment: Option<Vec<u8>> = None;
    let mut depth_init_segment: Option<Vec<u8>> = None;
    let mut depth_p010 = Vec::new();
    let mut frame_count = 0u64;

    loop {
        // Capture aligned color + depth
        let frames = camera.capture()?;
        let timestamp_us = frames.timestamp_us;

        // ---- Color pipeline: RGB → NV12 → AV1 → CMAF → "video" track ----
        let av1_data = color_encoder.encode_rgb(&frames.color_rgb, timestamp_us)?;

        let parsed = parse_av1_frame(&av1_data);

        // Create AV1 init segment on first keyframe with sequence header
        if color_init_segment.is_none() {
            if let Some(ref seq_hdr) = parsed.sequence_header {
                let init = color_muxer.create_init_segment(seq_hdr, frames.width, frames.height);
                video_track.write(init.clone());
                color_init_segment = Some(init);
                tracing::info!("Sent AV1 CMAF init segment (color)");
            }
        }

        let pts = (frame_count as i64) * 90000 / args.fps as i64;
        let dts = pts;
        let duration = (90000 / args.fps) as u32;

        if let Some(segment) =
            color_muxer.add_frame(&parsed.data, pts, dts, duration, parsed.is_keyframe)
        {
            if parsed.is_keyframe {
                if let Some(ref init) = color_init_segment {
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

        // ---- Depth pipeline: u16 mm → P010 → AV1 10-bit → CMAF → "depth" track ----
        depth_to_p010(
            &frames.depth_mm,
            &mut depth_p010,
            frames.width,
            frames.height,
        );
        let depth_av1 = depth_encoder.encode_p010(&depth_p010, timestamp_us)?;

        let depth_parsed = parse_av1_frame(&depth_av1);

        // Create depth init segment on first keyframe
        if depth_init_segment.is_none() {
            if let Some(ref seq_hdr) = depth_parsed.sequence_header {
                let init = depth_muxer.create_init_segment(seq_hdr, frames.width, frames.height);
                depth_track.write(init.clone());
                depth_init_segment = Some(init);
                tracing::info!("Sent AV1 CMAF init segment (depth, 10-bit)");
            }
        }

        if let Some(segment) = depth_muxer.add_frame(
            &depth_parsed.data,
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

        if (frame_count) % (args.fps as u64) == 0 {
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
                frame_count,
                av1_data.len(),
                depth_av1.len(),
                dmin,
                dmax,
                nonzero,
                frames.depth_mm.len()
            );
        }
    }
}
