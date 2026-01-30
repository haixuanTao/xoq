//! Camera server - streams local cameras to remote clients over P2P
//!
//! Usage: camera_server <camera_index>... [options]
//!
//! Examples:
//!   camera_server 0                       # Single camera (JPEG)
//!   camera_server 0 --h264                # Single camera with NVENC H.264
//!   camera_server 0 2 4                   # Multiple cameras
//!   camera_server 0 --key-dir /etc/xoq    # Custom key directory
//!   camera_server --list                  # List available cameras
//!
//! Each camera gets its own server ID for independent connections.
//! Keys are bound to physical USB ports for stability.

use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use xoq::camera::{list_cameras, Camera, CameraOptions};
use xoq::iroh::IrohServerBuilder;

// NVENC imports
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P4_GUID,
        NV_ENC_TUNING_INFO,
    },
    Bitstream, Buffer, Encoder, EncoderInitParams, EncodePictureParams, Session,
};

const CAMERA_JPEG_ALPN: &[u8] = b"xoq/camera-jpeg/0";
const CAMERA_H264_ALPN: &[u8] = b"xoq/camera-h264/0";

/// Get unique USB path for a video device
fn get_usb_path(video_index: u32) -> Option<String> {
    let device = format!("/dev/video{}", video_index);
    let output = Command::new("udevadm")
        .args(["info", "--query=property", "--name", &device])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix("ID_PATH=") {
            return Some(path.to_string());
        }
    }
    None
}

/// Create a stable key name from USB path
fn make_key_name(video_index: u32) -> String {
    if let Some(usb_path) = get_usb_path(video_index) {
        let safe_path = usb_path.replace(':', "-").replace('.', "_");
        format!(".xoq_camera_key_{}", safe_path)
    } else {
        format!(".xoq_camera_key_idx{}", video_index)
    }
}

#[derive(Clone)]
struct CameraConfig {
    index: u32,
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    quality: u8,
    bitrate: u32,
    use_h264: bool,
    identity_path: PathBuf,
}

fn parse_args() -> Option<(Vec<CameraConfig>, PathBuf)> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        return None;
    }

    if args.iter().any(|a| a == "--list") {
        print_cameras();
        std::process::exit(0);
    }

    let mut indices = Vec::new();
    let mut key_dir = PathBuf::from(".");
    let mut width = 640u32;
    let mut height = 480u32;
    let mut fps = 30u32;
    let mut quality = 80u8;
    let mut bitrate = 2_000_000u32; // 2 Mbps default
    let mut use_h264 = false;
    let mut i = 1;

    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--key-dir" if i + 1 < args.len() => {
                key_dir = PathBuf::from(&args[i + 1]);
                i += 2;
            }
            "--width" if i + 1 < args.len() => {
                width = args[i + 1].parse().unwrap_or(640);
                i += 2;
            }
            "--height" if i + 1 < args.len() => {
                height = args[i + 1].parse().unwrap_or(480);
                i += 2;
            }
            "--fps" if i + 1 < args.len() => {
                fps = args[i + 1].parse().unwrap_or(30);
                i += 2;
            }
            "--quality" if i + 1 < args.len() => {
                quality = args[i + 1].parse().unwrap_or(80);
                i += 2;
            }
            "--bitrate" if i + 1 < args.len() => {
                bitrate = args[i + 1].parse().unwrap_or(2_000_000);
                i += 2;
            }
            "--h264" | "--nvenc" => {
                use_h264 = true;
                i += 1;
            }
            _ => {
                if let Ok(idx) = arg.parse::<u32>() {
                    indices.push(idx);
                }
                i += 1;
            }
        }
    }

    if indices.is_empty() {
        return None;
    }

    let cameras = list_cameras().unwrap_or_default();
    let name_map: std::collections::HashMap<u32, String> =
        cameras.iter().map(|c| (c.index, c.name.clone())).collect();

    let configs = indices
        .into_iter()
        .map(|index| {
            let name = name_map
                .get(&index)
                .cloned()
                .unwrap_or_else(|| format!("Camera {}", index));
            let key_name = make_key_name(index);
            let identity_path = key_dir.join(&key_name);
            CameraConfig {
                index,
                name,
                width,
                height,
                fps,
                quality,
                bitrate,
                use_h264,
                identity_path,
            }
        })
        .collect();

    Some((configs, key_dir))
}

fn print_cameras() {
    println!("Available cameras:");
    match list_cameras() {
        Ok(cameras) if !cameras.is_empty() => {
            for cam in cameras {
                println!("  [{}] {}", cam.index, cam.name);
            }
        }
        _ => println!("  (none found)"),
    }
}

fn print_usage() {
    println!("Usage: camera_server <camera_index>... [options]");
    println!();
    println!("Examples:");
    println!("  camera_server 0                       # Single camera (JPEG)");
    println!("  camera_server 0 --h264                # NVENC H.264 encoding");
    println!("  camera_server 0 2 4                   # Multiple cameras");
    println!("  camera_server 0 --key-dir /etc/xoq    # Custom key directory");
    println!("  camera_server --list                  # List available cameras");
    println!();
    println!("Options:");
    println!("  --list            List available cameras and exit");
    println!("  --key-dir <path>  Directory for identity keys (default: .)");
    println!("  --width <px>      Frame width (default: 640)");
    println!("  --height <px>     Frame height (default: 480)");
    println!("  --fps <rate>      Framerate (default: 30)");
    println!("  --quality <1-100> JPEG quality (default: 80)");
    println!("  --h264, --nvenc   Use NVENC H.264 encoding (requires NVIDIA GPU)");
    println!("  --bitrate <bps>   H.264 bitrate in bps (default: 2000000)");
    println!();
    print_cameras();
}

/// NVENC encoder wrapper.
/// Uses ManuallyDrop to control drop order: buffers must drop before session.
struct NvencEncoder {
    // Drop order matters: buffers reference the session's encoder pointer,
    // so they must be dropped BEFORE the session.
    input_buffer: std::mem::ManuallyDrop<Buffer<'static>>,
    output_bitstream: std::mem::ManuallyDrop<Bitstream<'static>>,
    // Owned via raw pointer for stable 'static address.
    // Reclaimed and dropped properly in Drop impl.
    session: *mut Session,
    width: u32,
    height: u32,
    nv12_buffer: Vec<u8>,
}

unsafe impl Send for NvencEncoder {}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        // Drop buffers first while the session/encoder is still valid
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.input_buffer);
            std::mem::ManuallyDrop::drop(&mut self.output_bitstream);
        }
        // Reclaim the session and drop it properly.
        // Session::drop() sends EOS - catch_unwind handles the case where
        // the encoder is busy (e.g. task was aborted mid-encode).
        if !self.session.is_null() {
            let session = unsafe { Box::from_raw(self.session) };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(session)));
        }
    }
}

impl NvencEncoder {
    fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        // Initialize CUDA
        let cuda_ctx = CudaContext::new(0)
            .map_err(|e| anyhow::anyhow!("Failed to create CUDA context: {}", e))?;

        // Create encoder
        let encoder = Encoder::initialize_with_cuda(cuda_ctx)
            .map_err(|e| anyhow::anyhow!("Failed to initialize NVENC: {:?}", e))?;

        // Get preset config
        let mut preset_config = encoder
            .get_preset_config(
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            )
            .map_err(|e| anyhow::anyhow!("Failed to get preset config: {:?}", e))?;

        // Configure for low latency
        let config = &mut preset_config.presetCfg;
        config.gopLength = fps;
        config.frameIntervalP = 1;
        config.rcParams.averageBitRate = bitrate;
        config.rcParams.maxBitRate = bitrate;
        config.rcParams.vbvBufferSize = bitrate / fps;

        // Create init params
        let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, width, height);
        init_params
            .preset_guid(NV_ENC_PRESET_P4_GUID)
            .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
            .framerate(fps, 1)
            .enable_picture_type_decision()
            .encode_config(config);

        let buffer_format = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12;

        // Start session - own via raw pointer for stable 'static address
        let session = encoder
            .start_session(buffer_format, init_params)
            .map_err(|e| anyhow::anyhow!("Failed to start session: {:?}", e))?;

        let session_ptr = Box::into_raw(Box::new(session));
        let session_ref: &'static Session = unsafe { &*session_ptr };

        let input_buffer = session_ref
            .create_input_buffer()
            .map_err(|e| anyhow::anyhow!("Failed to create input buffer: {:?}", e))?;

        let output_bitstream = session_ref
            .create_output_bitstream()
            .map_err(|e| anyhow::anyhow!("Failed to create output bitstream: {:?}", e))?;

        let nv12_size = (width * height * 3 / 2) as usize;

        Ok(NvencEncoder {
            session: session_ptr,
            input_buffer: std::mem::ManuallyDrop::new(input_buffer),
            output_bitstream: std::mem::ManuallyDrop::new(output_bitstream),
            width,
            height,
            nv12_buffer: vec![0u8; nv12_size],
        })
    }

    fn encode_yuyv(&mut self, yuyv: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.yuyv_to_nv12(yuyv);
        self.encode_nv12(timestamp_us)
    }

    fn encode_rgb(&mut self, rgb: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.rgb_to_nv12(rgb);
        self.encode_nv12(timestamp_us)
    }

    fn encode_nv12(&mut self, timestamp_us: u64) -> Result<Vec<u8>> {
        {
            let mut lock = self
                .input_buffer
                .lock()
                .map_err(|e| anyhow::anyhow!("Failed to lock input: {:?}", e))?;
            unsafe { lock.write(&self.nv12_buffer) };
        }

        let session: &Session = unsafe { &*self.session };
        session
            .encode_picture(
                &mut *self.input_buffer,
                &mut *self.output_bitstream,
                EncodePictureParams {
                    input_timestamp: timestamp_us,
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("Failed to encode: {:?}", e))?;

        let lock = self
            .output_bitstream
            .lock()
            .map_err(|e| anyhow::anyhow!("Failed to lock output: {:?}", e))?;

        Ok(lock.data().to_vec())
    }

    fn yuyv_to_nv12(&mut self, yuyv: &[u8]) {
        let width = self.width as usize;
        let height = self.height as usize;
        let y_size = width * height;

        for y in 0..height {
            for x in (0..width).step_by(2) {
                let yuyv_idx = (y * width + x) * 2;
                let y0 = yuyv.get(yuyv_idx).copied().unwrap_or(0);
                let y1 = yuyv.get(yuyv_idx + 2).copied().unwrap_or(0);

                self.nv12_buffer[y * width + x] = y0;
                self.nv12_buffer[y * width + x + 1] = y1;

                if y % 2 == 0 {
                    let u = yuyv.get(yuyv_idx + 1).copied().unwrap_or(128);
                    let v = yuyv.get(yuyv_idx + 3).copied().unwrap_or(128);
                    let uv_idx = y_size + (y / 2) * width + x;
                    self.nv12_buffer[uv_idx] = u;
                    self.nv12_buffer[uv_idx + 1] = v;
                }
            }
        }
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

/// Run camera server - now fully async since Camera is Send
async fn run_camera_server(config: CameraConfig) -> Result<()> {
    loop {
        tracing::info!("[cam{}] Starting {}...", config.index, config.name);

        let result = if config.use_h264 {
            run_camera_server_h264(&config).await
        } else {
            run_camera_server_jpeg(&config).await
        };

        match result {
            Ok(()) => {
                tracing::info!("[cam{}] Stopped cleanly", config.index);
                break;
            }
            Err(e) => {
                tracing::error!("[cam{}] Failed: {}", config.index, e);
                tracing::info!("[cam{}] Restarting in 5s...", config.index);
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
    Ok(())
}

async fn run_camera_server_jpeg(config: &CameraConfig) -> Result<()> {
    let camera = Camera::open(config.index, config.width, config.height, config.fps)?;

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - JPEG mode",
        config.index,
        camera.width(),
        camera.height(),
        camera.format_name()
    );

    let camera = Arc::new(Mutex::new(camera));

    let server = IrohServerBuilder::new()
        .alpn(CAMERA_JPEG_ALPN)
        .identity_path(&config.identity_path)
        .bind()
        .await?;

    tracing::info!("[cam{}] Server ID: {}", config.index, server.id());

    loop {
        let conn = match server.accept().await? {
            Some(c) => c,
            None => break,
        };

        tracing::info!("[cam{}] Client: {}", config.index, conn.remote_id());

        let stream = match conn.accept_stream().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("[cam{}] Stream error: {}", config.index, e);
                continue;
            }
        };

        let (mut send, _recv) = stream.split();
        let mut frame_count = 0u64;
        let quality = config.quality;
        let cam_idx = config.index;

        loop {
            let (jpeg, width, height, timestamp_us) = {
                let mut cam = camera.lock().await;
                let frame = cam.capture()?;
                let jpeg = frame.to_jpeg(quality)?;
                (jpeg, frame.width, frame.height, frame.timestamp_us)
            };

            // Header: width(4) + height(4) + timestamp(8) + length(4) = 20 bytes
            let mut header = Vec::with_capacity(20);
            header.extend_from_slice(&width.to_le_bytes());
            header.extend_from_slice(&height.to_le_bytes());
            header.extend_from_slice(&timestamp_us.to_le_bytes());
            header.extend_from_slice(&(jpeg.len() as u32).to_le_bytes());

            if send.write_all(&header).await.is_err() || send.write_all(&jpeg).await.is_err() {
                break;
            }

            frame_count += 1;
            if frame_count % 300 == 0 {
                tracing::info!("[cam{}] {} frames sent", cam_idx, frame_count);
            }
        }

        tracing::info!("[cam{}] Client disconnected", cam_idx);
    }

    Ok(())
}

async fn run_camera_server_h264(config: &CameraConfig) -> Result<()> {
    // Open camera with YUYV preferred for hardware encoding
    let camera = Camera::open_with_options(
        config.index,
        config.width,
        config.height,
        config.fps,
        CameraOptions { prefer_yuyv: true },
    )?;

    let actual_width = camera.width();
    let actual_height = camera.height();
    let is_yuyv = camera.is_yuyv();

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - H.264/NVENC mode",
        config.index,
        actual_width,
        actual_height,
        camera.format_name()
    );

    // Create NVENC encoder
    let encoder = NvencEncoder::new(actual_width, actual_height, config.fps, config.bitrate)?;
    tracing::info!("[cam{}] NVENC encoder initialized", config.index);

    let camera = Arc::new(Mutex::new(camera));
    let encoder = Arc::new(Mutex::new(encoder));

    let server = IrohServerBuilder::new()
        .alpn(CAMERA_H264_ALPN)
        .identity_path(&config.identity_path)
        .bind()
        .await?;

    tracing::info!("[cam{}] Server ID: {}", config.index, server.id());

    loop {
        let conn = match server.accept().await? {
            Some(c) => c,
            None => break,
        };

        tracing::info!("[cam{}] Client: {}", config.index, conn.remote_id());

        let stream = match conn.accept_stream().await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("[cam{}] Stream error: {}", config.index, e);
                continue;
            }
        };

        let (mut send, _recv) = stream.split();
        let mut frame_count = 0u64;
        let cam_idx = config.index;

        loop {
            // Capture and encode
            let h264_data = {
                let mut cam = camera.lock().await;
                let raw_frame = cam.capture_raw()?;

                let mut enc = encoder.lock().await;
                if is_yuyv {
                    enc.encode_yuyv(&raw_frame.data, raw_frame.timestamp_us)?
                } else {
                    // MJPEG: decode to RGB first, then encode
                    let frame = cam.capture()?;
                    enc.encode_rgb(&frame.data, frame.timestamp_us)?
                }
            };

            // Header: width(4) + height(4) + timestamp(8) + length(4) = 20 bytes
            // Note: timestamp is frame count for H.264
            let timestamp_us = frame_count * 1_000_000 / config.fps as u64;
            let mut header = Vec::with_capacity(20);
            header.extend_from_slice(&actual_width.to_le_bytes());
            header.extend_from_slice(&actual_height.to_le_bytes());
            header.extend_from_slice(&timestamp_us.to_le_bytes());
            header.extend_from_slice(&(h264_data.len() as u32).to_le_bytes());

            if send.write_all(&header).await.is_err() || send.write_all(&h264_data).await.is_err() {
                break;
            }

            frame_count += 1;
            if frame_count % 300 == 0 {
                tracing::info!("[cam{}] {} H.264 frames sent", cam_idx, frame_count);
            }
        }

        tracing::info!("[cam{}] Client disconnected", cam_idx);
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("xoq=info".parse()?)
                .add_directive("info".parse()?),
        )
        .init();

    let (configs, key_dir) = match parse_args() {
        Some(r) => r,
        None => {
            print_usage();
            return Ok(());
        }
    };

    let use_h264 = configs.first().map(|c| c.use_h264).unwrap_or(false);

    tracing::info!("Camera server starting");
    tracing::info!("Key dir: {}", key_dir.display());
    tracing::info!("Cameras: {}", configs.len());
    tracing::info!("Encoding: {}", if use_h264 { "H.264 (NVENC)" } else { "JPEG" });

    println!("\n========================================");
    println!("Camera Server Starting...");
    println!("Encoding: {}", if use_h264 { "H.264 (NVENC)" } else { "JPEG" });
    println!("========================================\n");

    // Spawn all camera servers as async tasks
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    for config in configs {
        tasks.spawn(run_camera_server(config));
    }

    // Wait for Ctrl+C or all servers to exit
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutting down...");
            tasks.abort_all();
        }
        _ = async {
            while tasks.join_next().await.is_some() {}
        } => {
            tracing::info!("All camera servers stopped");
        }
    }

    // Drain remaining tasks so NvencEncoder destructors run before process exit
    while tasks.join_next().await.is_some() {}

    Ok(())
}
