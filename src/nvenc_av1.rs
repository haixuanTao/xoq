//! Shared NVENC AV1 encoder for camera_server and realsense_server.
//!
//! Supports both 8-bit NV12 (color) and 10-bit P010 (depth) encoding.

use anyhow::Result;
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_AV1_GUID, NV_ENC_PIC_FLAGS, NV_ENC_PIC_TYPE,
        NV_ENC_PRESET_P4_GUID, NV_ENC_PRESET_P7_GUID, NV_ENC_TUNING_INFO, _NV_ENC_PARAMS_RC_MODE,
    },
    Bitstream, Buffer, EncodePictureParams, Encoder, EncoderInitParams, Session,
};

pub struct NvencAv1Encoder {
    input_buffer: std::mem::ManuallyDrop<Buffer<'static>>,
    output_bitstream: std::mem::ManuallyDrop<Bitstream<'static>>,
    session: *mut Session,
    pub width: u32,
    pub height: u32,
    nv12_buffer: Vec<u8>,
    pub frame_count: u64,
    fps: u32,
    ten_bit: bool,
}

unsafe impl Send for NvencAv1Encoder {}

impl Drop for NvencAv1Encoder {
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

enum RateControl {
    Bitrate(u32),
    ConstQP(u32),
}

impl NvencAv1Encoder {
    /// Create a new AV1 encoder with bitrate-based rate control (CBR).
    ///
    /// - `ten_bit`: if true, uses P010 (10-bit YUV420) buffer format; otherwise NV12 (8-bit).
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, ten_bit: bool) -> Result<Self> {
        Self::new_inner(width, height, fps, ten_bit, RateControl::Bitrate(bitrate))
    }

    /// Create a new AV1 encoder with constant QP rate control (best quality).
    ///
    /// - `qp`: quantization parameter (0 = lossless, ~15-20 = very high quality, ~30 = medium).
    /// - Uses P7 preset (highest quality) and high-quality tuning.
    pub fn new_cqp(width: u32, height: u32, fps: u32, qp: u32, ten_bit: bool) -> Result<Self> {
        Self::new_inner(width, height, fps, ten_bit, RateControl::ConstQP(qp))
    }

    fn new_inner(
        width: u32,
        height: u32,
        fps: u32,
        ten_bit: bool,
        rc: RateControl,
    ) -> Result<Self> {
        let (preset, tuning) = match rc {
            RateControl::Bitrate(_) => (
                NV_ENC_PRESET_P4_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            ),
            RateControl::ConstQP(_) => (
                NV_ENC_PRESET_P7_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY,
            ),
        };

        let cuda_ctx = CudaContext::new(0)
            .map_err(|e| anyhow::anyhow!("Failed to create CUDA context: {}", e))?;

        let encoder = Encoder::initialize_with_cuda(cuda_ctx)
            .map_err(|e| anyhow::anyhow!("Failed to initialize NVENC: {:?}", e))?;

        let mut preset_config = encoder
            .get_preset_config(NV_ENC_CODEC_AV1_GUID, preset, tuning)
            .map_err(|e| anyhow::anyhow!("Failed to get AV1 preset config: {:?}", e))?;

        let config = &mut preset_config.presetCfg;
        config.gopLength = fps;
        config.frameIntervalP = 1;

        match rc {
            RateControl::Bitrate(bitrate) => {
                config.rcParams.averageBitRate = bitrate;
                config.rcParams.maxBitRate = bitrate;
                config.rcParams.vbvBufferSize = bitrate / fps;
            }
            RateControl::ConstQP(qp) => {
                config.rcParams.rateControlMode = _NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
                config.rcParams.constQP.qpInterP = qp;
                config.rcParams.constQP.qpInterB = qp;
                config.rcParams.constQP.qpIntra = qp;
            }
        }

        // AV1-specific config
        unsafe {
            let av1_config = &mut config.encodeCodecConfig.av1Config;
            av1_config.set_enableIntraRefresh(0);
            av1_config.idrPeriod = fps;
            av1_config.set_repeatSeqHdr(1);
            av1_config.colorRange = 1; // full range (0-1023 for 10-bit, 0-255 for 8-bit)
            if ten_bit {
                av1_config.set_inputPixelBitDepthMinus8(2);
                av1_config.set_pixelBitDepthMinus8(2);
            } else {
                av1_config.set_inputPixelBitDepthMinus8(0);
                av1_config.set_pixelBitDepthMinus8(0);
            }
        }

        let mut init_params = EncoderInitParams::new(NV_ENC_CODEC_AV1_GUID, width, height);
        init_params
            .preset_guid(preset)
            .tuning_info(tuning)
            .framerate(fps, 1)
            .encode_config(config);

        let buffer_format = if ten_bit {
            NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
        } else {
            NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
        };

        let session = encoder
            .start_session(buffer_format, init_params)
            .map_err(|e| anyhow::anyhow!("Failed to start AV1 session: {:?}", e))?;

        let session_ptr = Box::into_raw(Box::new(session));
        let session_ref: &'static Session = unsafe { &*session_ptr };

        let input_buffer = session_ref
            .create_input_buffer()
            .map_err(|e| anyhow::anyhow!("Failed to create input buffer: {:?}", e))?;

        let output_bitstream = session_ref
            .create_output_bitstream()
            .map_err(|e| anyhow::anyhow!("Failed to create output bitstream: {:?}", e))?;

        // NV12: w*h*3/2, P010: w*h*3 (u16 per sample)
        let buffer_size = if ten_bit {
            (width * height * 3) as usize
        } else {
            (width * height * 3 / 2) as usize
        };

        Ok(Self {
            session: session_ptr,
            input_buffer: std::mem::ManuallyDrop::new(input_buffer),
            output_bitstream: std::mem::ManuallyDrop::new(output_bitstream),
            width,
            height,
            nv12_buffer: vec![0u8; buffer_size],
            frame_count: 0,
            fps,
            ten_bit,
        })
    }

    /// Encode the current NV12/P010 buffer contents as AV1.
    pub fn encode_frame(&mut self, timestamp_us: u64) -> Result<Vec<u8>> {
        {
            let mut lock = self
                .input_buffer
                .lock()
                .map_err(|e| anyhow::anyhow!("Failed to lock input: {:?}", e))?;
            if self.ten_bit {
                unsafe { lock.write_p010(&self.nv12_buffer, self.width, self.height) };
            } else {
                unsafe { lock.write_nv12(&self.nv12_buffer, self.width, self.height) };
            }
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

    /// Encode P010 data directly (for 10-bit depth).
    /// `data` must be tightly packed P010.
    pub fn encode_p010(&mut self, data: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.nv12_buffer[..data.len()].copy_from_slice(data);
        self.encode_frame(timestamp_us)
    }

    /// Encode RGB data (converts to NV12 first). 8-bit only.
    pub fn encode_rgb(&mut self, rgb: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.rgb_to_nv12(rgb);
        self.encode_frame(timestamp_us)
    }

    /// Encode YUYV data (converts to NV12 first). 8-bit only.
    pub fn encode_yuyv(&mut self, yuyv: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.yuyv_to_nv12(yuyv);
        self.encode_frame(timestamp_us)
    }

    /// Encode greyscale data (converts to NV12 first). 8-bit only.
    pub fn encode_grey(&mut self, grey: &[u8], timestamp_us: u64) -> Result<Vec<u8>> {
        self.grey_to_nv12(grey);
        self.encode_frame(timestamp_us)
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

    fn grey_to_nv12(&mut self, grey: &[u8]) {
        let y_size = (self.width as usize) * (self.height as usize);
        self.nv12_buffer[..y_size].copy_from_slice(&grey[..y_size]);
        self.nv12_buffer[y_size..].fill(128);
    }
}
