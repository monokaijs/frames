//! One NVENC API version's encoder (compiled once per version against its
//! own bindings, see `super`).

use super::{consts, sys};
use crate::nvenc::EncodeTiming;
use consts::*;
use frames_media::api::{
    Codec, EncodeParams, EncoderCaps, EncoderFeatures, Gop, MediaClock, MediaError,
};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use sys::*;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{Interface, s, w};

/// Reference frames kept in the decoded picture buffer, so invalidation has
/// older frames to fall back on (spike 0-C verified 4 with RFI).
const DPB_FRAMES: usize = 4;
/// Registered input textures kept before the cache is reset.
const MAX_REGISTERED: usize = 16;
/// Sleep between sub-frame polls: a slice takes ~0.4-0.8 ms at 1080p, and
/// spinning cost a full core in the spike.
const POLL_SLEEP: Duration = Duration::from_micros(100);

struct Api {
    list: NV_ENCODE_API_FUNCTION_LIST,
}

// The function table is immutable after creation.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Whether this machine's NVIDIA driver supports this API version, with the
/// reason when it does not.
pub fn available() -> Result<(), MediaError> {
    api().map(|_| ())
}

fn api() -> Result<&'static Api, MediaError> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let library = LoadLibraryW(w!("nvEncodeAPI64.dll"))
            .map_err(|e| format!("nvEncodeAPI64.dll unavailable: {e}"))?;
        let max = GetProcAddress(library, s!("NvEncodeAPIGetMaxSupportedVersion"))
            .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?;
        let create = GetProcAddress(library, s!("NvEncodeAPICreateInstance"))
            .ok_or("NvEncodeAPICreateInstance missing")?;
        let max: unsafe extern "C" fn(*mut u32) -> NVENCSTATUS = std::mem::transmute(max);
        let create: unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS =
            std::mem::transmute(create);
        let mut driver = 0u32;
        if max(&mut driver) != NV_ENC_SUCCESS {
            return Err("cannot query the NVENC driver version".into());
        }
        let header = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
        if header > driver {
            return Err(format!(
                "NVENC driver supports API {}.{}, Frames needs {}.{}",
                driver >> 4,
                driver & 15,
                NVENCAPI_MAJOR_VERSION,
                NVENCAPI_MINOR_VERSION
            ));
        }
        let mut list: NV_ENCODE_API_FUNCTION_LIST = std::mem::zeroed();
        list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        if create(&mut list) != NV_ENC_SUCCESS {
            return Err("NvEncodeAPICreateInstance failed".into());
        }
        Ok(Api { list })
    })
    .as_ref()
    .map_err(|e| MediaError::Backend(e.clone()))
}

macro_rules! nv {
    ($api:expr, $encoder:expr, $fn:ident ( $($arg:expr),* )) => {{
        let function = $api.list.$fn.expect(concat!(stringify!($fn), " present"));
        let status = unsafe { function($($arg),*) };
        if status == NV_ENC_SUCCESS {
            Ok(())
        } else {
            Err(nv_error($api, $encoder, stringify!($fn), status))
        }
    }};
}

fn nv_error(api: &Api, encoder: *mut c_void, call: &str, status: NVENCSTATUS) -> MediaError {
    let detail = (!encoder.is_null())
        .then_some(api.list.nvEncGetLastErrorString)
        .flatten()
        .map(|f| unsafe {
            let text = f(encoder);
            if text.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(text)
                    .to_string_lossy()
                    .into_owned()
            }
        })
        .unwrap_or_default();
    MediaError::Backend(format!("{call} failed ({status}): {detail}"))
}

#[derive(Default)]
struct Pending {
    keyframe: bool,
    invalidate: Option<(u64, u64)>,
    refresh: bool,
    bitrate: Option<u32>,
    reconfigure: Option<EncodeParams>,
}

pub struct NvencEncoder {
    api: &'static Api,
    encoder: *mut c_void,
    device: ID3D11Device,
    guid: GUID,
    params: EncodeParams,
    caps: EncoderCaps,
    config: Box<NV_ENC_CONFIG>,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    subframe: bool,
    bitstream: *mut c_void,
    registered: HashMap<usize, (NV_ENC_REGISTERED_PTR, ID3D11Texture2D)>,
    clock: Arc<dyn MediaClock>,
    next_frame: u64,
    history: VecDeque<u64>,
    pending: Pending,
    refresh_left: Option<u16>,
    offsets: Vec<u32>,
    timing: EncodeTiming,
}

// The session is only ever used from the thread that calls `encode`/`control`.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Opens an encoder on `device`, which must belong to an NVIDIA adapter
    /// (see [`crate::d3d11::nvidia_device`]).
    pub fn new(
        device: &ID3D11Device,
        codec: Codec,
        params: EncodeParams,
        clock: Arc<dyn MediaClock>,
    ) -> Result<Self, MediaError> {
        let api = api()?;
        let guid = match codec {
            Codec::H264 => NV_ENC_CODEC_H264_GUID,
            Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
            Codec::Av1 => NV_ENC_CODEC_AV1_GUID,
        };
        let mut session: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { std::mem::zeroed() };
        session.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        session.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        session.device = device.as_raw();
        session.apiVersion = NVENCAPI_VERSION;
        let mut encoder = std::ptr::null_mut();
        nv!(
            api,
            std::ptr::null_mut(),
            nvEncOpenEncodeSessionEx(&mut session, &mut encoder)
        )?;
        let mut this = Self {
            api,
            encoder,
            device: device.clone(),
            guid,
            params,
            caps: EncoderCaps {
                codec,
                features: EncoderFeatures::empty(),
                max_fps: 240,
                max_slices: 1,
                long_term_slots: 0,
            },
            config: Box::new(unsafe { std::mem::zeroed() }),
            init: Box::new(unsafe { std::mem::zeroed() }),
            subframe: false,
            bitstream: std::ptr::null_mut(),
            registered: HashMap::new(),
            clock,
            next_frame: 0,
            history: VecDeque::new(),
            pending: Pending {
                keyframe: true,
                ..Pending::default()
            },
            refresh_left: None,
            offsets: vec![0; 256],
            timing: EncodeTiming::default(),
        };
        this.caps = this.query_caps(codec);
        this.initialize()?;
        let mut buffer: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { std::mem::zeroed() };
        buffer.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        nv!(
            api,
            this.encoder,
            nvEncCreateBitstreamBuffer(this.encoder, &mut buffer)
        )?;
        this.bitstream = buffer.bitstreamBuffer;
        Ok(this)
    }

    fn cap(&self, which: NV_ENC_CAPS) -> i32 {
        let mut param: NV_ENC_CAPS_PARAM = unsafe { std::mem::zeroed() };
        param.version = NV_ENC_CAPS_PARAM_VER;
        param.capsToQuery = which;
        let mut value = 0;
        let ok = nv!(
            self.api,
            self.encoder,
            nvEncGetEncodeCaps(self.encoder, self.guid, &mut param, &mut value)
        );
        if ok.is_ok() { value } else { 0 }
    }

    fn query_caps(&self, codec: Codec) -> EncoderCaps {
        let mut features = EncoderFeatures::ZERO_COPY.with(EncoderFeatures::DYNAMIC_BITRATE);
        let mut flag = |cap, feature| {
            if self.cap(cap) != 0 {
                features = features.with(feature);
            }
        };
        flag(
            NV_ENC_CAPS_SUPPORT_INTRA_REFRESH,
            EncoderFeatures::INTRA_REFRESH,
        );
        flag(
            NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION,
            EncoderFeatures::INVALIDATE,
        );
        flag(
            NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK,
            EncoderFeatures::SLICE_OUTPUT,
        );
        flag(
            NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE,
            EncoderFeatures::ONE_FRAME_VBV,
        );
        flag(
            NV_ENC_CAPS_SUPPORT_DYN_RES_CHANGE,
            EncoderFeatures::DYNAMIC_RESOLUTION,
        );
        flag(NV_ENC_CAPS_SUPPORT_YUV444_ENCODE, EncoderFeatures::YUV444);
        // Long-term references are available in the hardware but were not
        // verified end to end (spike 0-C); they are not advertised, so the
        // engine recovers by invalidation and, past the DPB, by keyframe,
        // which costs no extra bytes with a one-frame VBV.
        EncoderCaps {
            codec,
            features,
            max_fps: 240,
            max_slices: 16,
            long_term_slots: 0,
        }
    }

    fn rate(&mut self, bitrate: u32) {
        let fps = self.params.fps.max(1);
        let vbv = (f64::from(bitrate) / f64::from(fps) * f64::from(self.params.vbv_frames.max(0.5)))
            as u32;
        let rc = &mut self.config.rcParams;
        rc.rateControlMode = NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = bitrate;
        rc.maxBitRate = bitrate;
        rc.vbvBufferSize = vbv;
        rc.vbvInitialDelay = vbv;
        self.params.bitrate_bps = bitrate;
    }

    fn initialize(&mut self) -> Result<(), MediaError> {
        let p = self.params;
        let mut preset: NV_ENC_PRESET_CONFIG = unsafe { std::mem::zeroed() };
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.presetCfg.version = NV_ENC_CONFIG_VER;
        nv!(
            self.api,
            self.encoder,
            nvEncGetEncodePresetConfigEx(
                self.encoder,
                self.guid,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset
            )
        )?;
        *self.config = preset.presetCfg;
        self.config.version = NV_ENC_CONFIG_VER;
        let gop = match p.gop {
            Gop::Infinite => NVENC_INFINITE_GOPLENGTH,
            Gop::Frames(n) => n.max(1),
        };
        self.config.gopLength = gop;
        self.config.frameIntervalP = 1;
        self.rate(p.bitrate_bps);
        self.config.rcParams.set_zeroReorderDelay(1);
        self.config.rcParams.set_enableLookahead(0);
        let slices = u32::from(p.slices.clamp(1, self.caps.max_slices));
        let intra_refresh = self.caps.features.contains(EncoderFeatures::INTRA_REFRESH)
            && p.intra_refresh_frames.is_some();
        let refresh_frames = u32::from(p.intra_refresh_frames.unwrap_or(0).max(2));
        unsafe {
            match self.caps.codec {
                Codec::H264 => {
                    let c = &mut self.config.encodeCodecConfig.h264Config;
                    c.idrPeriod = gop;
                    c.set_repeatSPSPPS(1);
                    c.maxNumRefFrames = DPB_FRAMES as u32;
                    c.sliceMode = 3;
                    c.sliceModeData = slices;
                    c.set_enableIntraRefresh(u32::from(intra_refresh));
                    // On demand only: the period is effectively never.
                    c.intraRefreshPeriod = 1_000_000;
                    c.intraRefreshCnt = refresh_frames;
                }
                Codec::Hevc => {
                    let c = &mut self.config.encodeCodecConfig.hevcConfig;
                    c.idrPeriod = gop;
                    c.set_repeatSPSPPS(1);
                    c.maxNumRefFramesInDPB = DPB_FRAMES as u32;
                    c.sliceMode = 3;
                    c.sliceModeData = slices;
                    c.set_enableIntraRefresh(u32::from(intra_refresh));
                    c.intraRefreshPeriod = 1_000_000;
                    c.intraRefreshCnt = refresh_frames;
                }
                Codec::Av1 => {
                    let c = &mut self.config.encodeCodecConfig.av1Config;
                    c.idrPeriod = gop;
                    c.set_repeatSeqHdr(1);
                    c.maxNumRefFramesInDPB = DPB_FRAMES as u32;
                }
            }
        }
        self.subframe = slices > 1 && self.caps.features.contains(EncoderFeatures::SLICE_OUTPUT);
        let init = &mut *self.init;
        *init = unsafe { std::mem::zeroed() };
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = self.guid;
        init.presetGUID = NV_ENC_PRESET_P1_GUID;
        init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.encodeWidth = p.width;
        init.darWidth = p.width;
        init.maxEncodeWidth = p.width;
        init.encodeHeight = p.height;
        init.darHeight = p.height;
        init.maxEncodeHeight = p.height;
        init.frameRateNum = p.fps.max(1);
        init.frameRateDen = 1;
        init.enablePTD = 1;
        init.enableEncodeAsync = 0;
        init.set_reportSliceOffsets(u32::from(self.subframe));
        init.set_enableSubFrameWrite(u32::from(self.subframe));
        init.encodeConfig = &mut *self.config;
        nv!(
            self.api,
            self.encoder,
            nvEncInitializeEncoder(self.encoder, &mut *self.init)
        )
    }

    fn reconfigure_rate(&mut self, bitrate: u32) -> Result<(), MediaError> {
        self.rate(bitrate);
        let mut reconfigure: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        reconfigure.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconfigure.reInitEncodeParams = unsafe { std::ptr::read(&*self.init) };
        reconfigure.reInitEncodeParams.encodeConfig = &mut *self.config;
        reconfigure.set_resetEncoder(0);
        reconfigure.set_forceIDR(0);
        nv!(
            self.api,
            self.encoder,
            nvEncReconfigureEncoder(self.encoder, &mut reconfigure)
        )
    }

    fn register(
        &mut self,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<NV_ENC_REGISTERED_PTR, MediaError> {
        let key = texture.as_raw() as usize;
        if let Some((registered, _)) = self.registered.get(&key) {
            return Ok(*registered);
        }
        if self.registered.len() >= MAX_REGISTERED {
            self.unregister_all();
        }
        let mut resource: NV_ENC_REGISTER_RESOURCE = unsafe { std::mem::zeroed() };
        resource.version = NV_ENC_REGISTER_RESOURCE_VER;
        resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        resource.width = width;
        resource.height = height;
        resource.resourceToRegister = texture.as_raw();
        resource.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
        resource.bufferUsage = NV_ENC_INPUT_IMAGE;
        nv!(
            self.api,
            self.encoder,
            nvEncRegisterResource(self.encoder, &mut resource)
        )?;
        self.registered
            .insert(key, (resource.registeredResource, texture.clone()));
        Ok(resource.registeredResource)
    }

    fn unregister_all(&mut self) {
        for (_, (registered, _)) in self.registered.drain() {
            let _ = nv!(
                self.api,
                self.encoder,
                nvEncUnregisterResource(self.encoder, registered)
            );
        }
    }

    /// Timings of the most recent picture.
    pub fn last_timing(&self) -> EncodeTiming {
        self.timing
    }
}

// Declared after `nv!` so the macro is in scope there.
mod encode;

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        self.unregister_all();
        if !self.bitstream.is_null() {
            let _ = nv!(
                self.api,
                self.encoder,
                nvEncDestroyBitstreamBuffer(self.encoder, self.bitstream)
            );
        }
        let _ = nv!(self.api, self.encoder, nvEncDestroyEncoder(self.encoder));
    }
}
