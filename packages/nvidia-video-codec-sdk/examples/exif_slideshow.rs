//! EXIF image slideshow encoder example.
//!
//! This example reads JPEG images from a directory, extracts and displays
//! their EXIF metadata, and encodes them into an H.264 video slideshow.
//!
//! Usage:
//! ```bash
//! cargo run --example exif_slideshow -- /path/to/images/
//! # or with a single image repeated:
//! cargo run --example exif_slideshow -- photo.jpg
//! ```
//!
//! Output is written to `exif_slideshow.h264` which can be played with:
//! ```bash
//! ffplay exif_slideshow.h264
//! ```

use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{BufReader, Write},
    path::Path,
    time::Instant,
};

use cudarc::driver::CudaContext;
use exif::{In, Reader, Tag};
use image::GenericImageView;
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
const FRAMES_PER_IMAGE: u32 = 90; // 3 seconds per image at 30fps

/// Extract and display EXIF data from an image file
fn print_exif_data(path: &Path) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return,
    };

    let exif = match Reader::new().read_from_container(&mut BufReader::new(&file)) {
        Ok(e) => e,
        Err(_) => {
            println!("  No EXIF data found");
            return;
        }
    };

    // Print common EXIF fields
    let fields = [
        (Tag::Make, "Camera Make"),
        (Tag::Model, "Camera Model"),
        (Tag::DateTimeOriginal, "Date Taken"),
        (Tag::ExposureTime, "Exposure"),
        (Tag::FNumber, "F-Number"),
        (Tag::ISOSpeed, "ISO"),
        (Tag::FocalLength, "Focal Length"),
        (Tag::GPSLatitude, "GPS Latitude"),
        (Tag::GPSLongitude, "GPS Longitude"),
    ];

    for (tag, name) in fields {
        if let Some(field) = exif.get_field(tag, In::PRIMARY) {
            println!("  {}: {}", name, field.display_value().with_unit(&exif));
        }
    }
}

/// Load an image and resize/pad it to fit the target dimensions
fn load_and_resize_image(path: &Path, target_width: u32, target_height: u32) -> Option<Vec<u8>> {
    let img = image::open(path).ok()?;

    // Calculate scaling to fit while maintaining aspect ratio
    let (img_width, img_height) = img.dimensions();
    let scale_x = target_width as f32 / img_width as f32;
    let scale_y = target_height as f32 / img_height as f32;
    let scale = scale_x.min(scale_y);

    let new_width = (img_width as f32 * scale) as u32;
    let new_height = (img_height as f32 * scale) as u32;

    // Resize the image
    let resized = img.resize_exact(new_width, new_height, image::imageops::FilterType::Lanczos3);

    // Create output buffer (black background)
    let mut output = vec![0u8; (target_width * target_height * 4) as usize];

    // Calculate offset to center the image
    let offset_x = (target_width - new_width) / 2;
    let offset_y = (target_height - new_height) / 2;

    // Copy pixels to output buffer in ARGB format (BGRA in memory for NVENC)
    let rgba = resized.to_rgba8();
    for y in 0..new_height {
        for x in 0..new_width {
            let pixel = rgba.get_pixel(x, y);
            let out_x = x + offset_x;
            let out_y = y + offset_y;
            let idx = ((out_y * target_width + out_x) * 4) as usize;

            // NVENC ARGB is word-ordered, so bytes are [B, G, R, A] on little-endian
            output[idx] = pixel[2];     // B
            output[idx + 1] = pixel[1]; // G
            output[idx + 2] = pixel[0]; // R
            output[idx + 3] = pixel[3]; // A
        }
    }

    Some(output)
}

/// Find all JPEG images in a directory
fn find_images(path: &Path) -> Vec<std::path::PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }

    let mut images = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                let ext = ext.to_string_lossy().to_lowercase();
                if ext == "jpg" || ext == "jpeg" || ext == "png" {
                    images.push(path);
                }
            }
        }
    }
    images.sort();
    images
}

fn main() {
    let input_path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("Usage: exif_slideshow <image_or_directory>");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  cargo run --example exif_slideshow -- photo.jpg");
        eprintln!("  cargo run --example exif_slideshow -- /path/to/photos/");
        std::process::exit(1);
    });

    let input_path = Path::new(&input_path);

    println!("EXIF Slideshow Encoder");
    println!("======================");
    println!("Resolution: {}x{}", WIDTH, HEIGHT);
    println!("Frames per image: {} ({:.1}s)", FRAMES_PER_IMAGE, FRAMES_PER_IMAGE as f32 / 30.0);
    println!();

    // Find images
    let images = find_images(input_path);
    if images.is_empty() {
        eprintln!("No images found in {:?}", input_path);
        std::process::exit(1);
    }
    println!("Found {} image(s)", images.len());
    println!();

    // Print EXIF data for each image
    println!("EXIF Data:");
    println!("──────────");
    for image_path in &images {
        println!("{}:", image_path.file_name().unwrap_or_default().to_string_lossy());
        print_exif_data(image_path);
        println!();
    }

    // Initialize CUDA
    println!("Initializing encoder...");
    let cuda_ctx = CudaContext::new(0).expect("Failed to create CUDA context");
    let encoder = Encoder::initialize_with_cuda(cuda_ctx).expect("Failed to initialize encoder");

    // Verify H.264 support
    let encode_guids = encoder.get_encode_guids().expect("Failed to get encode GUIDs");
    assert!(encode_guids.contains(&NV_ENC_CODEC_H264_GUID), "H.264 not supported");

    // Configure encoder (use low-latency to avoid frame buffering)
    let tuning_info = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID, tuning_info)
        .expect("Failed to get preset config");

    // Disable B-frames for immediate output
    preset_config.presetCfg.frameIntervalP = 1;
    preset_config.presetCfg.gopLength = 30; // Keyframe every second

    let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, WIDTH, HEIGHT);
    init_params
        .preset_guid(NV_ENC_PRESET_P4_GUID)
        .tuning_info(tuning_info)
        .display_aspect_ratio(16, 9)
        .framerate(30, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);

    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT_ARGB, init_params)
        .expect("Failed to start encoder session");

    let mut input_buffer = session.create_input_buffer().expect("Failed to create input buffer");
    let mut output_bitstream = session.create_output_bitstream().expect("Failed to create output bitstream");

    // Open output file
    let mut out_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("exif_slideshow.h264")
        .expect("Failed to create output file");

    println!();
    println!("Encoding slideshow...");

    let start_time = Instant::now();
    let mut frame_count = 0u32;
    let mut total_bytes = 0usize;

    for (img_idx, image_path) in images.iter().enumerate() {
        // Load and resize image
        let frame_data = match load_and_resize_image(image_path, WIDTH, HEIGHT) {
            Some(data) => data,
            None => {
                eprintln!("Failed to load image: {:?}", image_path);
                continue;
            }
        };

        // Encode multiple frames for this image (slideshow effect)
        for f in 0..FRAMES_PER_IMAGE {
            unsafe {
                input_buffer.lock().unwrap().write(&frame_data);
            }

            let encode_start = Instant::now();
            session
                .encode_picture(&mut input_buffer, &mut output_bitstream, Default::default())
                .expect("Failed to encode frame");

            let lock = output_bitstream.lock().expect("Failed to lock bitstream");
            let encode_time = encode_start.elapsed().as_secs_f32() * 1000.0;
            let encoded_data = lock.data();
            total_bytes += encoded_data.len();

            out_file.write_all(encoded_data).expect("Failed to write frame");

            frame_count += 1;

            // Progress for first frame of each image
            if f == 0 {
                println!(
                    "Image {}/{}: {} | {:.2}ms | {} KB",
                    img_idx + 1,
                    images.len(),
                    image_path.file_name().unwrap_or_default().to_string_lossy(),
                    encode_time,
                    encoded_data.len() / 1024
                );
            }
        }
    }

    let total_time = start_time.elapsed();

    println!();
    println!("Encoding complete!");
    println!("═══════════════════════════════════════════════════");
    println!("Total frames:  {}", frame_count);
    println!("Total time:    {:.2}s ({:.1} fps)", total_time.as_secs_f32(), frame_count as f32 / total_time.as_secs_f32());
    println!("Total size:    {:.2} MB", total_bytes as f32 / 1024.0 / 1024.0);
    println!("Duration:      {:.1}s", frame_count as f32 / 30.0);
    println!("Bitrate:       {:.2} Mbps", (total_bytes as f32 * 8.0) / (frame_count as f32 / 30.0) / 1_000_000.0);
    println!("═══════════════════════════════════════════════════");
    println!();
    println!("Output written to: exif_slideshow.h264");
    println!();
    println!("To play: ffplay exif_slideshow.h264");
    println!("To MP4:  ffmpeg -i exif_slideshow.h264 -c copy exif_slideshow.mp4");
}
