//! Webcam capture and AV1 encoding example.
//!
//! This example captures frames from a webcam using V4L2 (Video4Linux2) and
//! encodes them to AV1 using NVENC. AV1 encoding requires an NVIDIA RTX 30
//! series GPU or newer.
//!
//! Usage:
//! ```bash
//! cargo run --example webcam_av1
//! # or specify a different video device:
//! cargo run --example webcam_av1 -- /dev/video2
//! ```
//!
//! Output is written to `webcam_av1_output.ivf` which can be played with:
//! ```bash
//! ffplay webcam_av1_output.ivf
//! # or convert to mp4:
//! ffmpeg -i webcam_av1_output.ivf -c copy output.mp4
//! ```
//!
//! Note: AV1 encoding requires RTX 30 series (Ampere) or newer GPU.
//! If your GPU doesn't support AV1, use the H.264 example instead.

use std::{
    env,
    fs::File,
    io::{self, Write},
    time::Instant,
};

use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
        NV_ENC_CODEC_AV1_GUID,
        NV_ENC_PRESET_P4_GUID,
        NV_ENC_TUNING_INFO,
    },
    Encoder,
    EncoderInitParams,
};
use v4l::{
    buffer::Type,
    io::mmap::Stream,
    io::traits::CaptureStream,
    video::Capture,
    Device,
    FourCC,
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const FRAME_COUNT: u32 = 300; // ~10 seconds at 30fps

/// IVF file header for AV1 streams
fn write_ivf_header(file: &mut File, width: u32, height: u32, fps: u32) -> io::Result<()> {
    // IVF signature
    file.write_all(b"DKIF")?;
    // Version (2 bytes)
    file.write_all(&0u16.to_le_bytes())?;
    // Header size (2 bytes)
    file.write_all(&32u16.to_le_bytes())?;
    // FourCC codec (AV01 for AV1)
    file.write_all(b"AV01")?;
    // Width (2 bytes)
    file.write_all(&(width as u16).to_le_bytes())?;
    // Height (2 bytes)
    file.write_all(&(height as u16).to_le_bytes())?;
    // Framerate numerator (4 bytes)
    file.write_all(&fps.to_le_bytes())?;
    // Framerate denominator (4 bytes)
    file.write_all(&1u32.to_le_bytes())?;
    // Number of frames (4 bytes) - we'll update this later or leave as 0
    file.write_all(&0u32.to_le_bytes())?;
    // Unused (4 bytes)
    file.write_all(&0u32.to_le_bytes())?;
    Ok(())
}

/// IVF frame header
fn write_ivf_frame_header(file: &mut File, frame_size: u32, timestamp: u64) -> io::Result<()> {
    // Frame size (4 bytes)
    file.write_all(&frame_size.to_le_bytes())?;
    // Timestamp (8 bytes)
    file.write_all(&timestamp.to_le_bytes())?;
    Ok(())
}

/// Convert YUYV (YUV 4:2:2) to ABGR (for NVENC input)
/// YUYV format: [Y0 U0 Y1 V0] [Y2 U1 Y3 V1] ...
/// NVENC ABGR is "word-ordered" A8B8G8R8, which on little-endian means bytes [R, G, B, A]
fn yuyv_to_abgr(yuyv: &[u8], abgr: &mut [u8], width: u32, height: u32) {
    let width = width as usize;
    let height = height as usize;

    for y in 0..height {
        for x in (0..width).step_by(2) {
            let yuyv_idx = (y * width + x) * 2;
            let y0 = yuyv[yuyv_idx] as f32;
            let u = yuyv[yuyv_idx + 1] as f32;
            let y1 = yuyv[yuyv_idx + 2] as f32;
            let v = yuyv[yuyv_idx + 3] as f32;

            // YUV to RGB conversion (BT.601)
            let c0 = y0 - 16.0;
            let c1 = y1 - 16.0;
            let d = u - 128.0;
            let e = v - 128.0;

            // First pixel
            let r0 = (1.164 * c0 + 1.596 * e).clamp(0.0, 255.0) as u8;
            let g0 = (1.164 * c0 - 0.392 * d - 0.813 * e).clamp(0.0, 255.0) as u8;
            let b0 = (1.164 * c0 + 2.017 * d).clamp(0.0, 255.0) as u8;

            // Second pixel
            let r1 = (1.164 * c1 + 1.596 * e).clamp(0.0, 255.0) as u8;
            let g1 = (1.164 * c1 - 0.392 * d - 0.813 * e).clamp(0.0, 255.0) as u8;
            let b1 = (1.164 * c1 + 2.017 * d).clamp(0.0, 255.0) as u8;

            // Write ABGR - word-ordered A8B8G8R8 means bytes are [R, G, B, A] on little-endian
            let abgr_idx0 = (y * width + x) * 4;
            let abgr_idx1 = (y * width + x + 1) * 4;

            abgr[abgr_idx0] = r0;      // R
            abgr[abgr_idx0 + 1] = g0;  // G
            abgr[abgr_idx0 + 2] = b0;  // B
            abgr[abgr_idx0 + 3] = 255; // A

            abgr[abgr_idx1] = r1;      // R
            abgr[abgr_idx1 + 1] = g1;  // G
            abgr[abgr_idx1 + 2] = b1;  // B
            abgr[abgr_idx1 + 3] = 255; // A
        }
    }
}

fn main() {
    // Get video device from command line or use default
    let device_path = env::args().nth(1).unwrap_or_else(|| "/dev/video0".to_string());

    println!("Webcam AV1 Encoding Example");
    println!("============================");
    println!("Video device: {}", device_path);
    println!("Resolution: {}x{}", WIDTH, HEIGHT);
    println!("Frames to capture: {}", FRAME_COUNT);
    println!();

    // Open webcam
    println!("Opening webcam...");
    let device = Device::with_path(&device_path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {}", device_path, e));

    // Set format to YUYV (most webcams support this)
    let mut format = device.format().expect("Failed to get format");
    format.width = WIDTH;
    format.height = HEIGHT;
    format.fourcc = FourCC::new(b"YUYV");
    let format = device.set_format(&format).expect("Failed to set format");
    println!(
        "Webcam format: {}x{} {:?}",
        format.width, format.height, format.fourcc
    );

    // Verify we got the format we wanted
    if format.fourcc != FourCC::new(b"YUYV") {
        eprintln!(
            "Warning: Webcam doesn't support YUYV format, got {:?}",
            format.fourcc
        );
        eprintln!("This example requires YUYV format. Try a different webcam.");
        return;
    }

    // Initialize CUDA
    println!("Initializing CUDA...");
    let cuda_ctx =
        CudaContext::new(0).expect("Failed to create CUDA context. Is CUDA installed?");

    // Initialize encoder
    println!("Initializing NVENC...");
    let encoder = Encoder::initialize_with_cuda(cuda_ctx)
        .expect("Failed to initialize encoder. Is NVIDIA Video Codec SDK installed?");

    // Check if AV1 is supported
    let encode_guids = encoder
        .get_encode_guids()
        .expect("Failed to get encode GUIDs");

    if !encode_guids.contains(&NV_ENC_CODEC_AV1_GUID) {
        eprintln!("ERROR: AV1 encoding is not supported on this GPU.");
        eprintln!("AV1 encoding requires an NVIDIA RTX 30 series (Ampere) or newer GPU.");
        eprintln!();
        eprintln!("Supported codecs on this GPU:");
        for guid in &encode_guids {
            if *guid == nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_CODEC_H264_GUID {
                eprintln!("  - H.264");
            } else if *guid == nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_CODEC_HEVC_GUID {
                eprintln!("  - HEVC (H.265)");
            }
        }
        eprintln!();
        eprintln!("Try the 'simple_encode' example for H.264 encoding instead.");
        return;
    }
    println!("AV1 encoding supported!");

    // Check input format support
    let input_formats = encoder
        .get_supported_input_formats(NV_ENC_CODEC_AV1_GUID)
        .expect("Failed to get supported input formats");

    if !input_formats.contains(&NV_ENC_BUFFER_FORMAT_ABGR) {
        eprintln!("ERROR: ABGR input format not supported for AV1");
        return;
    }

    // Configure encoder
    let tuning_info = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(NV_ENC_CODEC_AV1_GUID, NV_ENC_PRESET_P4_GUID, tuning_info)
        .expect("Failed to get preset config");

    let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_AV1_GUID, WIDTH, HEIGHT);
    init_params
        .preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(tuning_info)
        .display_aspect_ratio(4, 3)
        .framerate(30, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);

    // Start encoding session
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT_ABGR, init_params)
        .expect("Failed to start encoder session");
    println!("Encoder session started");

    // Create input and output buffers
    let mut input_buffer = session
        .create_input_buffer()
        .expect("Failed to create input buffer");
    let mut output_bitstream = session
        .create_output_bitstream()
        .expect("Failed to create output bitstream");

    // Open output file
    let mut out_file =
        File::create("webcam_av1_output.ivf").expect("Failed to create output file");

    // Write IVF header
    write_ivf_header(&mut out_file, WIDTH, HEIGHT, 30).expect("Failed to write IVF header");

    // Start webcam stream
    let mut stream = Stream::with_buffers(&device, Type::VideoCapture, 4)
        .expect("Failed to create capture stream");

    // Buffer for format conversion
    let mut abgr_buffer = vec![0u8; (WIDTH * HEIGHT * 4) as usize];

    println!();
    println!("Starting capture and encoding...");
    println!("Press Ctrl+C to stop early.");
    println!();

    let start_time = Instant::now();
    let mut encode_times: Vec<f32> = Vec::with_capacity(FRAME_COUNT as usize);
    let mut packet_sizes: Vec<usize> = Vec::with_capacity(FRAME_COUNT as usize);

    for frame_num in 0..FRAME_COUNT {
        // Capture frame from webcam
        let (yuyv_data, _meta) = stream.next().expect("Failed to capture frame");

        // Convert YUYV to ABGR
        yuyv_to_abgr(yuyv_data, &mut abgr_buffer, WIDTH, HEIGHT);

        // Write to input buffer
        unsafe {
            input_buffer.lock().unwrap().write(&abgr_buffer);
        }

        // Encode and measure latency
        let encode_start = Instant::now();
        session
            .encode_picture(&mut input_buffer, &mut output_bitstream, Default::default())
            .expect("Failed to encode frame");

        // Get encoded data (this includes waiting for the encode to complete)
        let lock = output_bitstream.lock().expect("Failed to lock bitstream");
        let encode_time_ms = encode_start.elapsed().as_secs_f32() * 1000.0;
        encode_times.push(encode_time_ms);

        let encoded_data = lock.data();
        packet_sizes.push(encoded_data.len());

        // Write IVF frame
        write_ivf_frame_header(&mut out_file, encoded_data.len() as u32, frame_num as u64)
            .expect("Failed to write frame header");
        out_file
            .write_all(encoded_data)
            .expect("Failed to write frame data");

        // Print progress with latency and packet size
        if (frame_num + 1) % 30 == 0 {
            let elapsed = start_time.elapsed().as_secs_f32();
            let fps = (frame_num + 1) as f32 / elapsed;

            // Calculate averages for last 30 frames
            let recent_times = &encode_times[encode_times.len().saturating_sub(30)..];
            let recent_sizes = &packet_sizes[packet_sizes.len().saturating_sub(30)..];
            let avg_latency: f32 = recent_times.iter().sum::<f32>() / recent_times.len() as f32;
            let avg_size: f32 = recent_sizes.iter().sum::<usize>() as f32 / recent_sizes.len() as f32;

            println!(
                "Frame {:>3}/{} | {:.1} fps | {:.2}ms | {:.1} KB",
                frame_num + 1,
                FRAME_COUNT,
                fps,
                avg_latency,
                avg_size / 1024.0
            );
        }
    }

    let total_time = start_time.elapsed();

    // Calculate overall statistics
    let avg_latency: f32 = encode_times.iter().sum::<f32>() / encode_times.len() as f32;
    let avg_packet_size: f32 = packet_sizes.iter().sum::<usize>() as f32 / packet_sizes.len() as f32;
    let total_size: usize = packet_sizes.iter().sum();

    println!();
    println!("Encoding complete!");
    println!("═══════════════════════════════════════════════════");
    println!(
        "Total time:    {:.2}s ({:.1} fps)",
        total_time.as_secs_f32(),
        FRAME_COUNT as f32 / total_time.as_secs_f32()
    );
    println!("Avg latency:   {:.2} ms", avg_latency);
    println!("Avg packet:    {:.1} KB", avg_packet_size / 1024.0);
    println!("Total size:    {:.2} MB", total_size as f32 / 1024.0 / 1024.0);
    println!("Bitrate:       {:.2} Mbps", (total_size as f32 * 8.0) / total_time.as_secs_f32() / 1_000_000.0);
    println!("═══════════════════════════════════════════════════");
    println!();
    println!("Output written to: webcam_av1_output.ivf");
    println!();
    println!("To play the video:");
    println!("  ffplay webcam_av1_output.ivf");
    println!();
    println!("To convert to MP4:");
    println!("  ffmpeg -i webcam_av1_output.ivf -c copy output.mp4");
}
