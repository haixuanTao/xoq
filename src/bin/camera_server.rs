//! Camera server - streams local cameras to remote clients over P2P
//!
//! Usage: camera_server <camera_index>... [options]
//!
//! Examples:
//!   camera_server 0                       # Single camera (JPEG)
//!   camera_server 0 --h264                # H.264 encoding (NVENC on Linux, VideoToolbox on macOS)
//!   camera_server 0 2 4                   # Multiple cameras
//!   camera_server 0 --key-dir /etc/xoq    # Custom key directory
//!   camera_server --list                  # List available cameras
//!
//! Each camera gets its own server ID for independent connections.
//! Keys are bound to physical USB ports (Linux) or device uniqueID (macOS) for stability.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use xoq::iroh::IrohServerBuilder;
use xoq::MoqBuilder;

// Platform-conditional camera imports
#[cfg(feature = "camera")]
use xoq::camera::list_cameras;
#[cfg(feature = "camera")]
use xoq::camera::{Camera, CameraOptions, RawFormat};

#[cfg(feature = "camera-macos")]
use xoq::camera_macos::list_cameras as list_cameras_macos;
#[cfg(feature = "camera-macos")]
use xoq::camera_macos::Camera as CameraMacos;

// NVENC imports (Linux)
#[cfg(feature = "nvenc")]
use cudarc::driver::CudaContext;
#[cfg(feature = "nvenc")]
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_H264_GUID, NV_ENC_PIC_FLAGS, NV_ENC_PIC_TYPE,
        NV_ENC_PRESET_P4_GUID, NV_ENC_TUNING_INFO,
    },
    Bitstream, Buffer, EncodePictureParams, Encoder, EncoderInitParams, Session,
};
#[cfg(feature = "nvenc")]
use xoq::camera::CameraOptions as NvencCameraOptions;

// VideoToolbox imports (macOS)
#[cfg(feature = "vtenc")]
use xoq::vtenc::VtEncoder;

const CAMERA_JPEG_ALPN: &[u8] = b"xoq/camera-jpeg/0";
#[cfg(any(feature = "nvenc", feature = "vtenc"))]
const CAMERA_H264_ALPN: &[u8] = b"xoq/camera-h264/0";

/// Get unique USB path for a video device (Linux only).
#[cfg(feature = "camera")]
fn get_usb_path(video_index: u32) -> Option<String> {
    use std::process::Command;
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

/// Create a stable key name for identity files.
fn make_key_name(video_index: u32) -> String {
    #[cfg(feature = "camera")]
    {
        if let Some(usb_path) = get_usb_path(video_index) {
            let safe_path = usb_path.replace(':', "-").replace('.', "_");
            return format!(".xoq_camera_key_{}", safe_path);
        }
    }

    // macOS or fallback: use index-based key name
    format!(".xoq_camera_key_idx{}", video_index)
}

#[derive(Clone)]
struct CameraConfig {
    index: u32,
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    quality: u8,
    #[cfg_attr(not(any(feature = "nvenc", feature = "vtenc")), allow(dead_code))]
    bitrate: u32,
    use_h264: bool,
    identity_path: PathBuf,
    moq_path: Option<String>,
    relay: String,
    insecure: bool,
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
    let mut moq_path: Option<String> = None;
    let mut relay = String::from("https://cdn.moq.dev");
    let mut insecure = false;
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
            "--h264" | "--nvenc" | "--vtenc" => {
                use_h264 = true;
                i += 1;
            }
            "--relay" if i + 1 < args.len() => {
                relay = args[i + 1].clone();
                i += 2;
            }
            "--insecure" => {
                insecure = true;
                i += 1;
            }
            "--moq" => {
                // --moq [path] — next arg is path if it doesn't look like a flag or index
                if i + 1 < args.len()
                    && !args[i + 1].starts_with("--")
                    && args[i + 1].parse::<u32>().is_err()
                {
                    moq_path = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    // Default path set later after indices are known
                    moq_path = Some(String::new());
                    i += 1;
                }
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

    // Skip camera listing (can hang on macOS); just use fallback names
    let name_map = std::collections::HashMap::<u32, String>::new();

    let configs = indices
        .into_iter()
        .map(|index| {
            let name = name_map
                .get(&index)
                .cloned()
                .unwrap_or_else(|| format!("Camera {}", index));
            let key_name = make_key_name(index);
            let identity_path = key_dir.join(&key_name);
            let resolved_moq_path = moq_path.as_ref().map(|p| {
                if p.is_empty() {
                    format!("anon/camera-{}", index)
                } else {
                    p.clone()
                }
            });
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
                moq_path: resolved_moq_path,
                relay: relay.clone(),
                insecure,
            }
        })
        .collect();

    Some((configs, key_dir))
}

/// Get camera name map from platform-appropriate camera listing.
fn get_camera_name_map() -> std::collections::HashMap<u32, String> {
    #[cfg(feature = "camera")]
    {
        if let Ok(cameras) = list_cameras() {
            return cameras.iter().map(|c| (c.index, c.name.clone())).collect();
        }
    }

    #[cfg(feature = "camera-macos")]
    {
        if let Ok(cameras) = list_cameras_macos() {
            return cameras.iter().map(|c| (c.index, c.name.clone())).collect();
        }
    }

    std::collections::HashMap::new()
}

fn print_cameras() {
    println!("Available cameras:");

    #[cfg(feature = "camera")]
    match list_cameras() {
        Ok(cameras) if !cameras.is_empty() => {
            for cam in cameras {
                println!("  [{}] {}", cam.index, cam.name);
            }
            return;
        }
        _ => {}
    }

    #[cfg(feature = "camera-macos")]
    match list_cameras_macos() {
        Ok(cameras) if !cameras.is_empty() => {
            for cam in cameras {
                println!("  [{}] {}", cam.index, cam.name);
            }
            return;
        }
        _ => {}
    }

    println!("  (none found)");
}

fn encoder_name() -> &'static str {
    #[cfg(feature = "nvenc")]
    {
        return "H.264 (NVENC)";
    }
    #[cfg(feature = "vtenc")]
    {
        return "H.264 (VideoToolbox)";
    }
    #[allow(unreachable_code)]
    "H.264"
}

fn print_usage() {
    println!("Usage: camera_server <camera_index>... [options]");
    println!();
    println!("Examples:");
    println!("  camera_server 0                       # Single camera (JPEG)");

    #[cfg(feature = "nvenc")]
    println!("  camera_server 0 --h264                # NVENC H.264 encoding");
    #[cfg(feature = "vtenc")]
    println!("  camera_server 0 --h264                # VideoToolbox H.264 encoding");
    #[cfg(not(any(feature = "nvenc", feature = "vtenc")))]
    println!("  camera_server 0 --h264                # H.264 encoding (requires nvenc or vtenc feature)");

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
    println!("  --h264            Use H.264 encoding (NVENC on Linux, VideoToolbox on macOS)");
    println!("  --bitrate <bps>   H.264 bitrate in bps (default: 2000000)");
    println!("  --moq [path]      Use MoQ relay transport (default: anon/camera-<index>)");
    println!("  --relay <url>     MoQ relay URL (default: https://cdn.moq.dev)");
    println!("  --insecure        Disable TLS verification (for self-signed certs)");
    println!();
    print_cameras();
}

// ============================================================================
// NVENC encoder (Linux)
// ============================================================================

#[cfg(feature = "nvenc")]
struct NvencEncoder {
    input_buffer: std::mem::ManuallyDrop<Buffer<'static>>,
    output_bitstream: std::mem::ManuallyDrop<Bitstream<'static>>,
    session: *mut Session,
    width: u32,
    height: u32,
    nv12_buffer: Vec<u8>,
    frame_count: u64,
    fps: u32,
}

#[cfg(feature = "nvenc")]
unsafe impl Send for NvencEncoder {}

#[cfg(feature = "nvenc")]
impl Drop for NvencEncoder {
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

#[cfg(feature = "nvenc")]
impl NvencEncoder {
    fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let cuda_ctx = CudaContext::new(0)
            .map_err(|e| anyhow::anyhow!("Failed to create CUDA context: {}", e))?;

        let encoder = Encoder::initialize_with_cuda(cuda_ctx)
            .map_err(|e| anyhow::anyhow!("Failed to initialize NVENC: {:?}", e))?;

        let mut preset_config = encoder
            .get_preset_config(
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            )
            .map_err(|e| anyhow::anyhow!("Failed to get preset config: {:?}", e))?;

        let config = &mut preset_config.presetCfg;
        config.gopLength = fps;
        config.frameIntervalP = 1;
        config.rcParams.averageBitRate = bitrate;
        config.rcParams.maxBitRate = bitrate;
        config.rcParams.vbvBufferSize = bitrate / fps;

        // H.264-specific: ensure IDR frames with SPS/PPS so decoders can initialize.
        // Ultra-low-latency preset enables intra refresh (gradual refresh without IDR),
        // which must be disabled for cross-platform decoding compatibility.
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
            frame_count: 0,
            fps,
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

    fn encode_grey(&mut self, grey: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.grey_to_nv12(grey);
        self.encode_nv12(timestamp_us)
    }

    fn encode_nv12(&mut self, timestamp_us: u64) -> Result<Vec<u8>> {
        {
            let mut lock = self
                .input_buffer
                .lock()
                .map_err(|e| anyhow::anyhow!("Failed to lock input: {:?}", e))?;
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
        session
            .encode_picture(
                &mut *self.input_buffer,
                &mut *self.output_bitstream,
                EncodePictureParams {
                    input_timestamp: timestamp_us,
                    picture_type,
                    encode_pic_flags,
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

    fn grey_to_nv12(&mut self, grey: &[u8]) {
        let y_size = (self.width as usize) * (self.height as usize);
        // Y plane: copy directly
        self.nv12_buffer[..y_size].copy_from_slice(&grey[..y_size]);
        // UV plane: neutral chroma (128 = grey)
        self.nv12_buffer[y_size..].fill(128);
    }
}

// ============================================================================
// Camera server functions
// ============================================================================

/// Run camera server with automatic restart on failure.
async fn run_camera_server(config: CameraConfig) -> Result<()> {
    loop {
        tracing::info!("[cam{}] Starting {}...", config.index, config.name);

        let result = if let Some(ref moq_path) = config.moq_path {
            if config.use_h264 {
                #[cfg(all(feature = "nvenc", feature = "camera"))]
                {
                    run_camera_server_moq_h264_nvenc(&config, moq_path).await
                }
                #[cfg(all(feature = "vtenc", any(feature = "camera", feature = "camera-macos")))]
                {
                    run_camera_server_moq_h264_vtenc(&config, moq_path).await
                }
                #[cfg(not(any(
                    all(feature = "nvenc", feature = "camera"),
                    all(feature = "vtenc", any(feature = "camera", feature = "camera-macos"))
                )))]
                {
                    anyhow::bail!(
                        "MoQ H.264 requires the 'nvenc' or 'vtenc' feature and a camera feature"
                    )
                }
            } else {
                run_camera_server_moq(&config, moq_path).await
            }
        } else if config.use_h264 {
            #[cfg(feature = "nvenc")]
            {
                run_camera_server_h264_nvenc(&config).await
            }
            #[cfg(feature = "vtenc")]
            {
                run_camera_server_h264_vtenc(&config).await
            }
            #[cfg(not(any(feature = "nvenc", feature = "vtenc")))]
            {
                anyhow::bail!("H.264 requires the 'nvenc' or 'vtenc' feature")
            }
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

// ============================================================================
// MoQ server (JPEG via relay, platform-conditional camera open)
// ============================================================================

#[cfg(not(any(feature = "camera", feature = "camera-macos")))]
async fn run_camera_server_moq(_config: &CameraConfig, _moq_path: &str) -> Result<()> {
    anyhow::bail!("MoQ mode requires the 'camera' or 'camera-macos' feature")
}

#[cfg(any(feature = "camera", feature = "camera-macos"))]
async fn run_camera_server_moq(config: &CameraConfig, moq_path: &str) -> Result<()> {
    #[cfg(feature = "camera")]
    let camera = Camera::open(config.index, config.width, config.height, config.fps)?;
    #[cfg(all(feature = "camera-macos", not(feature = "camera")))]
    let camera = CameraMacos::open(config.index, config.width, config.height, config.fps)?;

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - MoQ JPEG mode",
        config.index,
        camera.width(),
        camera.height(),
        camera.format_name()
    );

    let camera = Arc::new(Mutex::new(camera));

    let mut builder = MoqBuilder::new().relay(&config.relay).path(moq_path);
    if config.insecure {
        builder = builder.disable_tls_verify();
    }
    let mut publisher = builder.connect_publisher().await?;

    tracing::info!(
        "[cam{}] MoQ path: {} (relay: {})",
        config.index,
        moq_path,
        config.relay
    );

    let mut track = publisher.create_track("camera");
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

        // MoQ frame format: width(4) + height(4) + timestamp(4) + JPEG data
        let mut buf = Vec::with_capacity(12 + jpeg.len());
        buf.extend_from_slice(&width.to_le_bytes());
        buf.extend_from_slice(&height.to_le_bytes());
        buf.extend_from_slice(&(timestamp_us as u32).to_le_bytes());
        buf.extend_from_slice(&jpeg);

        track.write(buf);

        frame_count += 1;
    }
}

// ============================================================================
// MoQ H.264 CMAF server (macOS VideoToolbox)
// ============================================================================

#[cfg(all(feature = "vtenc", any(feature = "camera", feature = "camera-macos")))]
async fn run_camera_server_moq_h264_vtenc(config: &CameraConfig, moq_path: &str) -> Result<()> {
    use xoq::cmaf::{CmafConfig, CmafMuxer, NalUnit as CmafNalUnit};

    #[cfg(feature = "camera")]
    let camera = Camera::open(config.index, config.width, config.height, config.fps)?;
    #[cfg(all(feature = "camera-macos", not(feature = "camera")))]
    let camera = CameraMacos::open(config.index, config.width, config.height, config.fps)?;

    let actual_width = camera.width();
    let actual_height = camera.height();

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - MoQ H.264/CMAF mode",
        config.index,
        actual_width,
        actual_height,
        camera.format_name()
    );

    let mut encoder = VtEncoder::new(actual_width, actual_height, config.fps, config.bitrate)?;
    tracing::info!("[cam{}] VideoToolbox encoder initialized", config.index);

    let mut muxer = CmafMuxer::new(CmafConfig {
        fragment_duration_ms: 33, // 1 frame @ 30fps for lowest latency
        timescale: 90000,
    });

    let camera = Arc::new(Mutex::new(camera));

    let mut builder = MoqBuilder::new().relay(&config.relay).path(moq_path);
    if config.insecure {
        builder = builder.disable_tls_verify();
    }
    let mut publisher = builder.connect_publisher().await?;

    tracing::info!(
        "[cam{}] MoQ path: {} (H.264 CMAF, relay: {})",
        config.index,
        moq_path,
        config.relay
    );

    let mut track = publisher.create_track("video");
    let mut init_segment: Option<Vec<u8>> = None;
    let mut frame_count = 0u64;
    let cam_idx = config.index;

    loop {
        let encoded = {
            let mut cam = camera.lock().await;
            let pixel_buffer = cam.capture_pixel_buffer()?;
            encoder.encode_pixel_buffer_nals(pixel_buffer.as_ptr(), pixel_buffer.timestamp_us)?
        };

        // On first keyframe with SPS/PPS: create and send init segment
        if init_segment.is_none() {
            if let (Some(ref sps), Some(ref pps)) = (&encoded.sps, &encoded.pps) {
                let init = muxer.create_init_segment(sps, pps, actual_width, actual_height);
                track.write(init.clone());
                init_segment = Some(init);
                tracing::info!("[cam{}] Sent CMAF init segment", cam_idx);
            }
        }

        // Compute timing in timescale 90000
        let pts = (frame_count as i64) * 90000 / config.fps as i64;
        let dts = pts;
        let duration = (90000 / config.fps) as u32;

        // Convert video_toolbox_sys NalUnits to xoq::cmaf NalUnits
        let cmaf_nals: Vec<CmafNalUnit> = encoded
            .nals
            .iter()
            .map(|n| CmafNalUnit {
                data: n.data.clone(),
                nal_type: n.nal_type,
            })
            .collect();

        // Feed NALs to CmafMuxer; when a segment is ready, send it
        if let Some(segment) = muxer.add_frame(&cmaf_nals, pts, dts, duration, encoded.is_keyframe)
        {
            // Prepend init segment on keyframes for late-joiner support
            if encoded.is_keyframe {
                if let Some(ref init) = init_segment {
                    let mut combined = init.clone();
                    combined.extend_from_slice(&segment);
                    track.write(combined);
                } else {
                    track.write(segment);
                }
            } else {
                track.write(segment);
            }
        }

        frame_count += 1;
    }
}

// ============================================================================
// MoQ H.264 CMAF server (Linux NVENC)
// ============================================================================

#[cfg(all(feature = "nvenc", feature = "camera"))]
async fn run_camera_server_moq_h264_nvenc(config: &CameraConfig, moq_path: &str) -> Result<()> {
    use xoq::cmaf::{parse_annex_b, CmafConfig, CmafMuxer};

    let camera = Camera::open_with_options(
        config.index,
        config.width,
        config.height,
        config.fps,
        CameraOptions { prefer_yuyv: true },
    )?;

    let actual_width = camera.width();
    let actual_height = camera.height();
    let mut use_raw = camera.is_yuyv() || camera.is_grey();

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - MoQ H.264/CMAF NVENC mode",
        config.index,
        actual_width,
        actual_height,
        camera.format_name()
    );

    let mut encoder = NvencEncoder::new(actual_width, actual_height, config.fps, config.bitrate)?;
    tracing::info!("[cam{}] NVENC encoder initialized", config.index);

    let mut muxer = CmafMuxer::new(CmafConfig {
        fragment_duration_ms: 33, // 1 frame @ 30fps for lowest latency
        timescale: 90000,
    });

    let camera = Arc::new(Mutex::new(camera));

    let mut builder = MoqBuilder::new().relay(&config.relay).path(moq_path);
    if config.insecure {
        builder = builder.disable_tls_verify();
    }
    let mut publisher = builder.connect_publisher().await?;

    tracing::info!(
        "[cam{}] MoQ path: {} (H.264 CMAF NVENC, relay: {})",
        config.index,
        moq_path,
        config.relay
    );

    let mut track = publisher.create_track("video");
    let mut init_segment: Option<Vec<u8>> = None;
    let mut frame_count = 0u64;
    let cam_idx = config.index;

    loop {
        let h264_data = {
            let mut cam = camera.lock().await;

            if use_raw {
                let raw_frame = cam.capture_raw()?;
                match raw_frame.format {
                    RawFormat::Yuyv => {
                        encoder.encode_yuyv(&raw_frame.data, raw_frame.timestamp_us)?
                    }
                    RawFormat::Grey => {
                        encoder.encode_grey(&raw_frame.data, raw_frame.timestamp_us)?
                    }
                    _ => {
                        use_raw = false;
                        let frame = cam.capture()?;
                        encoder.encode_rgb(&frame.data, frame.timestamp_us)?
                    }
                }
            } else {
                let frame = cam.capture()?;
                encoder.encode_rgb(&frame.data, frame.timestamp_us)?
            }
        };

        // Parse Annex B output into structured NAL units
        let parsed = parse_annex_b(&h264_data);

        // On first keyframe with SPS/PPS: create and send init segment
        if init_segment.is_none() {
            if let (Some(ref sps), Some(ref pps)) = (&parsed.sps, &parsed.pps) {
                let init = muxer.create_init_segment(sps, pps, actual_width, actual_height);
                track.write(init.clone());
                init_segment = Some(init);
                tracing::info!("[cam{}] Sent CMAF init segment", cam_idx);
            }
        }

        // Compute timing in timescale 90000
        let pts = (frame_count as i64) * 90000 / config.fps as i64;
        let dts = pts;
        let duration = (90000 / config.fps) as u32;

        // Feed NALs to CmafMuxer; when a segment is ready, send it
        if let Some(segment) = muxer.add_frame(&parsed.nals, pts, dts, duration, parsed.is_keyframe)
        {
            // Prepend init segment on keyframes for late-joiner support
            if parsed.is_keyframe {
                if let Some(ref init) = init_segment {
                    let mut combined = init.clone();
                    combined.extend_from_slice(&segment);
                    track.write(combined);
                } else {
                    track.write(segment);
                }
            } else {
                track.write(segment);
            }
        }

        frame_count += 1;
    }
}

// ============================================================================
// JPEG server (platform-conditional camera open)
// ============================================================================

#[cfg(not(any(feature = "camera", feature = "camera-macos")))]
async fn run_camera_server_jpeg(_config: &CameraConfig) -> Result<()> {
    anyhow::bail!("JPEG mode requires the 'camera' or 'camera-macos' feature")
}

#[cfg(any(feature = "camera", feature = "camera-macos"))]
async fn run_camera_server_jpeg(config: &CameraConfig) -> Result<()> {
    // Open camera using platform-appropriate type
    #[cfg(feature = "camera")]
    let camera = Camera::open(config.index, config.width, config.height, config.fps)?;
    #[cfg(all(feature = "camera-macos", not(feature = "camera")))]
    let camera = CameraMacos::open(config.index, config.width, config.height, config.fps)?;

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
        let conn = match server.accept().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("[cam{}] Accept error (retrying): {}", config.index, e);
                continue;
            }
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
        }

        tracing::info!("[cam{}] Client disconnected", cam_idx);
    }

    Ok(())
}

// ============================================================================
// H.264 server - NVENC (Linux)
// ============================================================================

#[cfg(feature = "nvenc")]
async fn run_camera_server_h264_nvenc(config: &CameraConfig) -> Result<()> {
    let camera = Camera::open_with_options(
        config.index,
        config.width,
        config.height,
        config.fps,
        CameraOptions { prefer_yuyv: true },
    )?;

    let actual_width = camera.width();
    let actual_height = camera.height();
    let mut use_raw = camera.is_yuyv() || camera.is_grey();

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - H.264/NVENC mode",
        config.index,
        actual_width,
        actual_height,
        camera.format_name()
    );

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
        let conn = match server.accept().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("[cam{}] Accept error (retrying): {}", config.index, e);
                continue;
            }
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

        // Reset encoder frame counter so first frame is IDR with SPS/PPS
        encoder.lock().await.frame_count = 0;

        loop {
            let h264_data = {
                let mut cam = camera.lock().await;
                let mut enc = encoder.lock().await;

                if use_raw {
                    let raw_frame = cam.capture_raw()?;
                    match raw_frame.format {
                        RawFormat::Yuyv => {
                            enc.encode_yuyv(&raw_frame.data, raw_frame.timestamp_us)?
                        }
                        RawFormat::Grey => {
                            enc.encode_grey(&raw_frame.data, raw_frame.timestamp_us)?
                        }
                        _ => {
                            use_raw = false;
                            let frame = cam.capture()?;
                            enc.encode_rgb(&frame.data, frame.timestamp_us)?
                        }
                    }
                } else {
                    let frame = cam.capture()?;
                    enc.encode_rgb(&frame.data, frame.timestamp_us)?
                }
            };

            if frame_count < 3 {
                let nal_type = h264_data.get(4).map(|b| b & 0x1F).unwrap_or(0);
                tracing::info!(
                    "[cam{}] Frame {}: {} bytes, NAL type {}",
                    cam_idx,
                    frame_count,
                    h264_data.len(),
                    nal_type
                );
            }

            let timestamp_us = frame_count * 1_000_000 / config.fps as u64;
            let mut header = Vec::with_capacity(20);
            header.extend_from_slice(&actual_width.to_le_bytes());
            header.extend_from_slice(&actual_height.to_le_bytes());
            header.extend_from_slice(&timestamp_us.to_le_bytes());
            header.extend_from_slice(&(h264_data.len() as u32).to_le_bytes());

            let write_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                send.write_all(&header).await?;
                send.write_all(&h264_data).await?;
                Ok::<(), std::io::Error>(())
            })
            .await;

            match write_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("[cam{}] Write error: {}", cam_idx, e);
                    break;
                }
                Err(_) => {
                    tracing::warn!("[cam{}] Write timeout (5s), dropping connection", cam_idx);
                    break;
                }
            }

            frame_count += 1;

            // Pace frames so iroh/QUIC can flush the send buffer.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }

        tracing::info!("[cam{}] Client disconnected", cam_idx);
    }

    Ok(())
}

// ============================================================================
// H.264 server - VideoToolbox (macOS)
// ============================================================================

#[cfg(feature = "vtenc")]
async fn run_camera_server_h264_vtenc(config: &CameraConfig) -> Result<()> {
    let camera = CameraMacos::open(config.index, config.width, config.height, config.fps)?;

    let actual_width = camera.width();
    let actual_height = camera.height();

    tracing::info!(
        "[cam{}] Opened: {}x{} ({}) - H.264/VideoToolbox mode",
        config.index,
        actual_width,
        actual_height,
        camera.format_name()
    );

    let encoder = VtEncoder::new(actual_width, actual_height, config.fps, config.bitrate)?;
    tracing::info!("[cam{}] VideoToolbox encoder initialized", config.index);

    let camera = Arc::new(Mutex::new(camera));
    let encoder = Arc::new(Mutex::new(encoder));

    let server = IrohServerBuilder::new()
        .alpn(CAMERA_H264_ALPN)
        .identity_path(&config.identity_path)
        .bind()
        .await?;

    tracing::info!("[cam{}] Server ID: {}", config.index, server.id());

    loop {
        let conn = match server.accept().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("[cam{}] Accept error (retrying): {}", config.index, e);
                continue;
            }
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
            let h264_data = {
                let mut cam = camera.lock().await;
                let pixel_buffer = cam.capture_pixel_buffer()?;

                let mut enc = encoder.lock().await;
                // Zero-copy: pass CVPixelBuffer directly to VideoToolbox
                enc.encode_pixel_buffer(pixel_buffer.as_ptr(), pixel_buffer.timestamp_us)?
                // pixel_buffer dropped here → CFRelease
            };

            let timestamp_us = frame_count * 1_000_000 / config.fps as u64;
            let mut header = Vec::with_capacity(20);
            header.extend_from_slice(&actual_width.to_le_bytes());
            header.extend_from_slice(&actual_height.to_le_bytes());
            header.extend_from_slice(&timestamp_us.to_le_bytes());
            header.extend_from_slice(&(h264_data.len() as u32).to_le_bytes());

            if send.write_all(&header).await.is_err() || send.write_all(&h264_data).await.is_err() {
                break;
            }
            use tokio::time::sleep;
            use tokio::time::Duration;
            sleep(Duration::from_millis(30)).await; // Yield to allow other tasks to run
            frame_count += 1;
        }

        tracing::info!("[cam{}] Client disconnected", cam_idx);
    }

    Ok(())
}

// ============================================================================
// Main
// ============================================================================

fn main() -> Result<()> {
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
    let use_moq = configs.first().and_then(|c| c.moq_path.as_ref()).is_some();

    #[cfg(not(any(feature = "nvenc", feature = "vtenc")))]
    if use_h264 && !use_moq {
        eprintln!("Error: H.264 encoding requires the 'nvenc' or 'vtenc' feature.");
        #[cfg(target_os = "macos")]
        eprintln!("Rebuild with: cargo run --example camera_server --features iroh,vtenc");
        #[cfg(not(target_os = "macos"))]
        eprintln!("Rebuild with: cargo run --example camera_server --features iroh,nvenc");
        return Ok(());
    }

    let encoding_str = if use_moq && use_h264 {
        "H.264 CMAF (MoQ relay)"
    } else if use_moq {
        "JPEG (MoQ relay)"
    } else if use_h264 {
        encoder_name()
    } else {
        "JPEG"
    };

    tracing::info!("Camera server starting");
    tracing::info!("Key dir: {}", key_dir.display());
    tracing::info!("Cameras: {}", configs.len());
    tracing::info!("Encoding: {}", encoding_str);

    println!("\n========================================");
    println!("Camera Server Starting...");
    println!("Encoding: {}", encoding_str);
    println!("========================================\n");

    // On macOS, AVFoundation requires the main thread's RunLoop to be running.
    // Run the tokio runtime on a background thread and keep the main thread
    // pumping the CFRunLoop.
    let rt = tokio::runtime::Runtime::new()?;

    rt.spawn(async move {
        let mut tasks: JoinSet<Result<()>> = JoinSet::new();
        for config in configs {
            tasks.spawn(run_camera_server(config));
        }

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

        while tasks.join_next().await.is_some() {}
        std::process::exit(0);
    });

    // Run the main thread's RunLoop so AVFoundation dispatches can execute
    #[cfg(target_os = "macos")]
    unsafe {
        extern "C" {
            fn CFRunLoopRun();
        }
        CFRunLoopRun();
    }

    // On non-macOS, just block forever (tokio handles shutdown via process::exit)
    #[cfg(not(target_os = "macos"))]
    std::thread::park();

    Ok(())
}
