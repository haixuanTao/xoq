//! Simple example demonstrating how to encode video using NVENC.
//!
//! This example generates dummy frames with a color gradient and encodes them
//! to H.264 using the NVIDIA Video Encoder API. No external dependencies like
//! Vulkan are required - it uses NVENC's built-in input buffers.
//!
//! Output is written to `simple_encode_output.h264` which can be played with:
//! ```bash
//! ffplay simple_encode_output.h264
//! # or convert to mp4:
//! ffmpeg -i simple_encode_output.h264 -c copy output.mp4
//! ```

use std::{fs::OpenOptions, io::Write};

use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
        NV_ENC_CODEC_H264_GUID,
        NV_ENC_PRESET_P4_GUID,
        NV_ENC_TUNING_INFO,
    },
    Encoder,
    EncoderInitParams,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FRAMES: u32 = 300; // 10 seconds at 30fps

/// Generate a test frame with a color gradient.
/// The color shifts over time to create an animated effect.
fn generate_frame(buf: &mut [u8], width: u32, height: u32, frame: u32, total_frames: u32) {
    let time = frame as f32 / total_frames as f32;

    for y in 0..height {
        for x in 0..width {
            let pixel = (y * width + x) as usize * 4;

            // ARGB format (actually BGRA in memory on little-endian)
            let red = ((x as f32 / width as f32) * 255.0) as u8;
            let green = ((y as f32 / height as f32) * 255.0) as u8;
            let blue = (time * 255.0) as u8;

            buf[pixel] = blue;     // B
            buf[pixel + 1] = green; // G
            buf[pixel + 2] = red;   // R
            buf[pixel + 3] = 255;   // A
        }
    }
}

fn main() {
    println!("Simple NVENC encoding example");
    println!("Resolution: {}x{}", WIDTH, HEIGHT);
    println!("Frames: {}", FRAMES);

    // Initialize CUDA context
    let cuda_ctx = CudaContext::new(0).expect("Failed to create CUDA context. Is CUDA installed?");
    println!("CUDA context created on device 0");

    // Initialize encoder
    let encoder = Encoder::initialize_with_cuda(cuda_ctx)
        .expect("Failed to initialize encoder. Is NVIDIA Video Codec SDK installed?");

    // Verify H.264 encoding is supported
    let encode_guids = encoder
        .get_encode_guids()
        .expect("Failed to get encode GUIDs");
    assert!(
        encode_guids.contains(&NV_ENC_CODEC_H264_GUID),
        "H.264 encoding not supported on this GPU"
    );

    // Verify ARGB input format is supported
    let input_formats = encoder
        .get_supported_input_formats(NV_ENC_CODEC_H264_GUID)
        .expect("Failed to get supported input formats");
    assert!(
        input_formats.contains(&NV_ENC_BUFFER_FORMAT_ARGB),
        "ARGB input format not supported"
    );

    // Get preset configuration
    let tuning_info = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID, tuning_info)
        .expect("Failed to get preset config");

    // Configure encoder session
    let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, WIDTH, HEIGHT);
    init_params
        .preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(tuning_info)
        .display_aspect_ratio(16, 9)
        .framerate(30, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);

    // Start encoding session
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT_ARGB, init_params)
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
    let mut out_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("simple_encode_output.h264")
        .expect("Failed to create output file");

    // Buffer for generating frames
    let mut frame_data = vec![0u8; (WIDTH * HEIGHT * 4) as usize];

    println!("Encoding {} frames...", FRAMES);

    for frame in 0..FRAMES {
        // Generate test frame
        generate_frame(&mut frame_data, WIDTH, HEIGHT, frame, FRAMES);

        // Write frame data to input buffer
        unsafe {
            input_buffer.lock().unwrap().write(&frame_data);
        }

        // Encode the frame
        session
            .encode_picture(&mut input_buffer, &mut output_bitstream, Default::default())
            .expect("Failed to encode frame");

        // Get encoded data and write to file
        let lock = output_bitstream.lock().expect("Failed to lock bitstream");
        out_file
            .write_all(lock.data())
            .expect("Failed to write to output file");

        // Print progress every 30 frames (1 second)
        if (frame + 1) % 30 == 0 {
            println!(
                "Encoded frame {} / {} ({:.0}%)",
                frame + 1,
                FRAMES,
                (frame + 1) as f32 / FRAMES as f32 * 100.0
            );
        }
    }

    println!("Encoding complete!");
    println!("Output written to: simple_encode_output.h264");
    println!();
    println!("To play the video:");
    println!("  ffplay simple_encode_output.h264");
    println!();
    println!("To convert to MP4:");
    println!("  ffmpeg -i simple_encode_output.h264 -c copy output.mp4");
}
