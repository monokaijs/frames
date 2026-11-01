//! Runtime loading keeps the updater independent of media initialization.
use std::{
    ffi::{CString, c_char, c_void},
    path::{Path, PathBuf},
    sync::OnceLock,
};
unsafe extern "C" {
    fn frames_media_init(
        util: *const c_char,
        scale: *const c_char,
        resample: *const c_char,
        codec: *const c_char,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn frames_resampler_open(
        in_rate: i32,
        in_channels: i32,
        out_rate: i32,
        out_channels: i32,
    ) -> *mut c_void;
    fn frames_resampler_close(context: *mut c_void);
    fn frames_resampler_capacity(context: *mut c_void, frames: i32) -> i32;
    fn frames_resampler_convert(
        context: *mut c_void,
        input: *const f32,
        frames: i32,
        output: *mut f32,
        capacity: i32,
    ) -> i32;
    fn frames_encoder_open(
        name: *const c_char,
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        error: *mut c_char,
        capacity: i32,
    ) -> *mut c_void;
    fn frames_decoder_open(
        name: *const c_char,
        width: i32,
        height: i32,
        hardware: i32,
        error: *mut c_char,
        capacity: i32,
    ) -> *mut c_void;
    fn frames_encoder_request_keyframe(context: *mut c_void);
    fn frames_encoder_set_rate(context: *mut c_void, bitrate: i32, fps: i32) -> i32;
    fn frames_encoder_send_scaled(
        context: *mut c_void,
        pixels: *const u8,
        width: i32,
        height: i32,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn frames_encoder_receive(
        context: *mut c_void,
        data: *mut *const u8,
        size: *mut i32,
        key: *mut i32,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn frames_decoder_flush(context: *mut c_void);
    fn frames_decoder_send(
        context: *mut c_void,
        data: *const u8,
        size: i32,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn frames_decoder_receive(
        context: *mut c_void,
        pixels: *mut u8,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    #[cfg(windows)]
    pub(crate) fn frames_surface_release(surface: *mut c_void);
    #[cfg(windows)]
    fn frames_decoder_receive_surface(
        context: *mut c_void,
        lease: *mut *mut c_void,
        texture: *mut *mut c_void,
        subresource: *mut i32,
        matrix: *mut i32,
        full: *mut i32,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    #[cfg(windows)]
    fn frames_gpu_converter_open(source: *mut c_void, width: i32, height: i32) -> *mut c_void;
    #[cfg(windows)]
    fn frames_gpu_converter_close(converter: *mut c_void);
    #[cfg(windows)]
    fn frames_gpu_convert(
        converter: *mut c_void,
        source: *mut c_void,
        input_slice: i32,
        target: *mut c_void,
        output_slice: i32,
        width: i32,
        height: i32,
        matrix: i32,
        full: i32,
    ) -> i32;
    #[cfg(windows)]
    fn frames_gpu_readback(
        converter: *mut c_void,
        source: *mut c_void,
        slice: i32,
        width: i32,
        height: i32,
        bytes: *mut u8,
    ) -> i32;
    #[cfg(windows)]
    fn frames_encoder_open_gpu(
        name: *const c_char,
        width: i32,
        height: i32,
        fps: i32,
        bitrate: i32,
        texture: *mut c_void,
        error: *mut c_char,
        capacity: i32,
    ) -> *mut c_void;
    #[cfg(windows)]
    fn frames_encoder_send_gpu(
        context: *mut c_void,
        source: *mut c_void,
        slice: i32,
        width: i32,
        height: i32,
        error: *mut c_char,
        capacity: i32,
    ) -> i32;
    fn frames_media_close(context: *mut c_void);
}

#[cfg(windows)]
include!(concat!(env!("OUT_DIR"), "/media_runtime.rs"));

#[cfg(windows)]
fn directory() -> anyhow::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::{
        fs::{self, OpenOptions},
        io::Write,
    };
    // A digest identifies the entire embedded runtime, independent of app version.
    let mut digest = Sha256::new();
    for (name, _, hash) in LIBRARIES {
        digest.update(name);
        digest.update(hash);
    }
    let identity = format!("{:x}", digest.finalize());
    let base = directories::ProjectDirs::from("dev", "creaton", "Frames")
        .ok_or_else(|| anyhow::anyhow!("No user cache directory"))?
        .cache_dir()
        .join("media");
    fs::create_dir_all(&base)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(base.join("runtime.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let dir = base.join(identity);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("LICENSES.txt"), super::licenses())?;
    for (name, bytes, hash) in LIBRARIES {
        let path = dir.join(name);
        if fs::read(&path).is_ok_and(|data| format!("{:x}", Sha256::digest(data)) == *hash) {
            continue;
        }
        let temporary = dir.join(format!("{}.partial", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        fs::rename(temporary, path)?;
    }
    Ok(dir)
}
#[cfg(not(windows))]
fn directory() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let parent = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("No executable directory"))?;
    if parent.file_name().is_some_and(|n| n == "MacOS") {
        return Ok(parent.join("../Frameworks"));
    }
    Ok(PathBuf::from(env!("FRAMES_MEDIA_SDK")).join("lib"))
}
fn library(dir: &Path, prefix: &str) -> anyhow::Result<CString> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().is_some_and(|n| {
                let n = n.to_string_lossy();
                if cfg!(windows) {
                    n.starts_with(&format!("{prefix}-")) && n.ends_with(".dll")
                } else {
                    n.starts_with(&format!("lib{prefix}.")) && n.ends_with(".dylib")
                }
            })
        })
        .collect();
    entries.sort();
    let path = entries
        .first()
        .ok_or_else(|| anyhow::anyhow!("Missing bundled {prefix} library in {}", dir.display()))?;
    Ok(CString::new(path.canonicalize()?.to_str().ok_or_else(
        || anyhow::anyhow!("Non-UTF8 library path"),
    )?)?)
}
pub fn initialize() -> anyhow::Result<()> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    READY
        .get_or_init(|| {
            (|| -> anyhow::Result<()> {
                // LGPL users can explicitly supply an ABI-compatible replacement build.
                let dir = std::env::var_os("FRAMES_MEDIA_LIBRARY_DIR")
                    .map(PathBuf::from)
                    .map(Ok)
                    .unwrap_or_else(directory)?;
                let util = library(&dir, "avutil")?;
                let scale = library(&dir, "swscale")?;
                let resample = library(&dir, "swresample")?;
                let codec = library(&dir, "avcodec")?;
                let mut error = [0i8; 512];
                let result = unsafe {
                    frames_media_init(
                        util.as_ptr(),
                        scale.as_ptr(),
                        resample.as_ptr(),
                        codec.as_ptr(),
                        error.as_mut_ptr(),
                        512,
                    )
                };
                check(result, &error)?;
                Ok(())
            })()
            .map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}
pub(crate) struct Resampler {
    pointer: *mut c_void,
    input: crate::audio::PcmFormat,
    output: crate::audio::PcmFormat,
}
impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { frames_resampler_close(self.pointer) }
    }
}
impl Resampler {
    pub fn new(
        input: crate::audio::PcmFormat,
        output: crate::audio::PcmFormat,
    ) -> anyhow::Result<Self> {
        initialize()?;
        let pointer = unsafe {
            frames_resampler_open(
                input.rate as i32,
                input.channels as i32,
                output.rate as i32,
                output.channels as i32,
            )
        };
        anyhow::ensure!(!pointer.is_null(), "Cannot initialize audio resampler");
        Ok(Self {
            pointer,
            input,
            output,
        })
    }
    pub fn convert(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        anyhow::ensure!(
            samples.len().is_multiple_of(self.input.channels as usize)
                && samples.len() <= 192000 * 8,
            "Invalid PCM chunk"
        );
        let frames = (samples.len() / self.input.channels as usize) as i32;
        let capacity = unsafe { frames_resampler_capacity(self.pointer, frames) };
        anyhow::ensure!(
            (0..=192000 * 2).contains(&capacity),
            "Invalid resampler capacity"
        );
        let mut out = vec![0.; capacity as usize * self.output.channels as usize];
        let count = unsafe {
            frames_resampler_convert(
                self.pointer,
                samples.as_ptr(),
                frames,
                out.as_mut_ptr(),
                capacity,
            )
        };
        anyhow::ensure!(count >= 0 && count <= capacity, "Audio resampling failed");
        out.truncate(count as usize * self.output.channels as usize);
        Ok(out)
    }
}
fn check(code: i32, error: &[i8]) -> anyhow::Result<i32> {
    if code < 0 {
        let text = unsafe { std::ffi::CStr::from_ptr(error.as_ptr()) }.to_string_lossy();
        anyhow::bail!("Media codec: {text} ({code})");
    }
    Ok(code)
}
/// Constructed and used on one blocking worker only; intentionally not Send.
pub struct Context {
    hardware: bool,
    bitrate: u32,
    pixels: crate::media::PixelPool,
    #[cfg(windows)]
    encoder_name: Option<CString>,
    #[cfg(windows)]
    fps: u32,
    #[cfg(windows)]
    gpu_attempted: bool,
    #[cfg(windows)]
    gpu_encoder: bool,
    #[cfg(windows)]
    converter: Option<GpuConverter>,
    #[cfg(windows)]
    cpu_pixels: Vec<u8>,
    pointer: *mut c_void,
    width: u32,
    height: u32,
}
impl Drop for Context {
    fn drop(&mut self) {
        unsafe { frames_media_close(self.pointer) }
    }
}
impl Context {
    pub fn flush_decoder(&mut self) {
        unsafe { frames_decoder_flush(self.pointer) }
    }
    pub fn encoder(name: &str, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        Self::encoder_at_rate(name, width, height, fps, 12_000_000)
    }
    pub fn encoder_at_rate(
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> anyhow::Result<Self> {
        initialize()?;
        let name = CString::new(name)?;
        let mut error = [0i8; 512];
        let pointer = unsafe {
            frames_encoder_open(
                name.as_ptr(),
                width as i32,
                height as i32,
                fps as i32,
                bitrate as i32,
                error.as_mut_ptr(),
                512,
            )
        };
        if pointer.is_null() {
            check(-1, &error)?;
        }
        Ok(Self {
            hardware: false,
            pixels: Default::default(),
            bitrate,
            pointer,
            width,
            height,
            #[cfg(windows)]
            encoder_name: Some(name),
            #[cfg(windows)]
            fps,
            #[cfg(windows)]
            gpu_attempted: false,
            #[cfg(windows)]
            gpu_encoder: false,
            #[cfg(windows)]
            converter: None,
            #[cfg(windows)]
            cpu_pixels: Vec::new(),
        })
    }
    pub fn decoder(name: &str, width: u32, height: u32, hardware: bool) -> anyhow::Result<Self> {
        initialize()?;
        let name = CString::new(name)?;
        let mut error = [0i8; 512];
        let pointer = unsafe {
            frames_decoder_open(
                name.as_ptr(),
                width as i32,
                height as i32,
                hardware as i32,
                error.as_mut_ptr(),
                512,
            )
        };
        if pointer.is_null() {
            check(-1, &error)?;
        }
        Ok(Self {
            hardware,
            pixels: Default::default(),
            bitrate: 0,
            pointer,
            width,
            height,
            #[cfg(windows)]
            encoder_name: None,
            #[cfg(windows)]
            fps: 60,
            #[cfg(windows)]
            gpu_attempted: false,
            #[cfg(windows)]
            gpu_encoder: false,
            #[cfg(windows)]
            converter: None,
            #[cfg(windows)]
            cpu_pixels: Vec::new(),
        })
    }
    pub fn gpu_encoder_active(&self) -> bool {
        #[cfg(windows)]
        {
            self.gpu_encoder
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
    pub fn request_keyframe(&mut self) {
        unsafe { frames_encoder_request_keyframe(self.pointer) }
    }
    pub fn set_rate(&mut self, bitrate: u32, fps: u32) -> anyhow::Result<super::RateUpdate> {
        self.bitrate = bitrate;
        #[cfg(windows)]
        {
            self.fps = fps;
        }
        match unsafe { frames_encoder_set_rate(self.pointer, bitrate as i32, fps as i32) } {
            0 => Ok(super::RateUpdate::Applied),
            1 => Ok(super::RateUpdate::RestartRequired),
            _ => anyhow::bail!("invalid encoder rate control"),
        }
    }
    pub fn encode(&mut self, frame: &crate::media::RawFrame) -> anyhow::Result<Vec<Vec<u8>>> {
        let result = self.encode_inner(frame);
        #[cfg(windows)]
        if let Err(error) = &result
            && self.gpu_encoder
        {
            let name = self
                .encoder_name
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing encoder name"))?
                .to_string_lossy()
                .into_owned();
            tracing::warn!(%error,"GPU encoder stopped; switching once to bounded readback");
            let mut replacement =
                Self::encoder_at_rate(&name, self.width, self.height, self.fps, self.bitrate)?;
            replacement.gpu_attempted = true;
            replacement.request_keyframe();
            *self = replacement;
            return self.encode_inner(frame);
        }
        result
    }
    fn encode_inner(&mut self, frame: &crate::media::RawFrame) -> anyhow::Result<Vec<Vec<u8>>> {
        anyhow::ensure!(frame.validate(), "Invalid encoder input frame");
        let mut error = [0i8; 512];
        #[cfg(windows)]
        let result = if let Some(surface) = &frame.gpu {
            use windows::core::Interface;
            if !self.gpu_attempted {
                self.gpu_attempted = true;
                if let Some(name) = &self.encoder_name {
                    let pointer = unsafe {
                        frames_encoder_open_gpu(
                            name.as_ptr(),
                            self.width as i32,
                            self.height as i32,
                            self.fps as i32,
                            self.bitrate as i32,
                            surface.texture.as_raw(),
                            error.as_mut_ptr(),
                            512,
                        )
                    };
                    if !pointer.is_null() {
                        unsafe {
                            frames_media_close(self.pointer);
                        }
                        self.pointer = pointer;
                        self.gpu_encoder = true;
                        tracing::info!("D3D11 encoder surface interoperability succeeded");
                    } else {
                        tracing::info!(reason=%unsafe { std::ffi::CStr::from_ptr(error.as_ptr()) }.to_string_lossy(), "D3D11 encoder unavailable; using bounded readback");
                    }
                }
            }
            if self.gpu_encoder {
                unsafe {
                    frames_encoder_send_gpu(
                        self.pointer,
                        surface.texture.as_raw(),
                        surface.subresource as i32,
                        frame.width as i32,
                        frame.height as i32,
                        error.as_mut_ptr(),
                        512,
                    )
                }
            } else {
                if self.converter.is_none() {
                    self.converter = Some(GpuConverter::new(
                        &surface.texture,
                        frame.width,
                        frame.height,
                    )?);
                }
                self.cpu_pixels
                    .resize(frame.width as usize * frame.height as usize * 4, 0);
                self.converter.as_mut().unwrap().readback(
                    surface,
                    frame.width,
                    frame.height,
                    &mut self.cpu_pixels,
                )?;
                unsafe {
                    frames_encoder_send_scaled(
                        self.pointer,
                        self.cpu_pixels.as_ptr(),
                        frame.width as i32,
                        frame.height as i32,
                        error.as_mut_ptr(),
                        512,
                    )
                }
            }
        } else {
            unsafe {
                frames_encoder_send_scaled(
                    self.pointer,
                    frame.bgra.as_ptr(),
                    frame.width as i32,
                    frame.height as i32,
                    error.as_mut_ptr(),
                    512,
                )
            }
        };
        #[cfg(not(windows))]
        let result = unsafe {
            frames_encoder_send_scaled(
                self.pointer,
                frame.bgra.as_ptr(),
                frame.width as i32,
                frame.height as i32,
                error.as_mut_ptr(),
                512,
            )
        };
        check(result, &error)?;
        let mut packets = Vec::new();
        loop {
            let mut data = std::ptr::null();
            let mut size = 0;
            let mut key = 0;
            if check(
                unsafe {
                    frames_encoder_receive(
                        self.pointer,
                        &mut data,
                        &mut size,
                        &mut key,
                        error.as_mut_ptr(),
                        512,
                    )
                },
                &error,
            )? == 0
            {
                break;
            }
            anyhow::ensure!(
                size > 0 && (size as usize) < crate::media::MAX_FRAME_BYTES,
                "Invalid encoder packet size"
            );
            // Protocol 5: flags followed by one complete codec AVPacket, no container.
            let mut packet = Vec::with_capacity(size as usize + 1);
            packet.push(key as u8);
            packet.extend_from_slice(unsafe { std::slice::from_raw_parts(data, size as usize) });
            packets.push(packet);
        }
        anyhow::ensure!(
            packets.len() == 1,
            "Encoder buffered a picture instead of returning one complete packet"
        );
        Ok(packets)
    }
    pub fn decode(&mut self, packet: &[u8]) -> anyhow::Result<Option<crate::media::RawFrame>> {
        self.decode_inner(packet, false)
    }
    pub fn decode_presentable(
        &mut self,
        packet: &[u8],
    ) -> anyhow::Result<Option<crate::media::RawFrame>> {
        self.decode_inner(packet, true)
    }
    fn decode_inner(
        &mut self,
        packet: &[u8],
        surfaces: bool,
    ) -> anyhow::Result<Option<crate::media::RawFrame>> {
        anyhow::ensure!(
            packet.len() > 1 && packet.len() <= crate::media::MAX_FRAME_BYTES && packet[0] <= 1,
            "Invalid decoder packet"
        );
        let mut error = [0i8; 512];
        check(
            unsafe {
                frames_decoder_send(
                    self.pointer,
                    packet[1..].as_ptr(),
                    (packet.len() - 1) as i32,
                    error.as_mut_ptr(),
                    512,
                )
            },
            &error,
        )?;
        #[cfg(windows)]
        if surfaces && self.hardware {
            use windows::core::Interface;
            let mut latest = None;
            loop {
                let (mut lease, mut texture) = (std::ptr::null_mut(), std::ptr::null_mut());
                let (mut subresource, mut matrix, mut full) = (0, 0, 0);
                if check(
                    unsafe {
                        frames_decoder_receive_surface(
                            self.pointer,
                            &mut lease,
                            &mut texture,
                            &mut subresource,
                            &mut matrix,
                            &mut full,
                            error.as_mut_ptr(),
                            512,
                        )
                    },
                    &error,
                )? == 0
                {
                    break;
                }
                let pinned = unsafe { super::HardwareLease::new(lease) };
                let texture = unsafe {
                    windows::Win32::Graphics::Direct3D11::ID3D11Texture2D::from_raw_borrowed(
                        &texture,
                    )
                }
                .ok_or_else(|| anyhow::anyhow!("missing hardware texture"))?
                .clone();
                latest = Some(crate::media::RawFrame {
                    width: self.width,
                    height: self.height,
                    bgra: bytes::Bytes::new(),
                    captured_at: std::time::Instant::now(),
                    frame_number: 0,
                    media_generation: 0,
                    gpu: Some(std::sync::Arc::new(crate::media::WindowsFrame {
                        texture,
                        subresource: subresource as u32,
                        matrix_709: matrix != 0,
                        full_range: full != 0,
                        lease: Some(pinned),
                    })),
                });
            }
            return Ok(latest);
        }
        #[cfg(not(windows))]
        let _ = (surfaces, self.hardware);
        let mut latest = None;
        loop {
            let mut pixels = self
                .pixels
                .acquire(self.width as usize * self.height as usize * 4);
            if check(
                unsafe {
                    frames_decoder_receive(
                        self.pointer,
                        pixels.as_mut_ptr(),
                        error.as_mut_ptr(),
                        512,
                    )
                },
                &error,
            )? == 0
            {
                break;
            }
            latest = Some(crate::media::RawFrame {
                media_generation: 0,
                #[cfg(windows)]
                gpu: None,
                captured_at: std::time::Instant::now(),
                frame_number: 0,
                width: self.width,
                height: self.height,
                bgra: pixels.into_bytes(),
            });
        }
        Ok(latest)
    }
}

#[cfg(windows)]
pub struct GpuConverter(*mut c_void);
#[cfg(windows)]
impl Drop for GpuConverter {
    fn drop(&mut self) {
        unsafe {
            frames_gpu_converter_close(self.0);
        }
    }
}
#[cfg(windows)]
impl GpuConverter {
    pub fn new(
        source: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        use windows::core::Interface;
        initialize()?;
        let pointer =
            unsafe { frames_gpu_converter_open(source.as_raw(), width as i32, height as i32) };
        anyhow::ensure!(!pointer.is_null(), "D3D11 video processor unavailable");
        Ok(Self(pointer))
    }
    pub fn convert(
        &mut self,
        source: &crate::media::WindowsFrame,
        target: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        use windows::core::Interface;
        let result = unsafe {
            frames_gpu_convert(
                self.0,
                source.texture.as_raw(),
                source.subresource as i32,
                target.as_raw(),
                0,
                width as i32,
                height as i32,
                source.matrix_709 as i32,
                source.full_range as i32,
            )
        };
        anyhow::ensure!(result == 0, "D3D11 video conversion failed");
        Ok(())
    }
    pub fn readback(
        &mut self,
        source: &crate::media::WindowsFrame,
        width: u32,
        height: u32,
        bytes: &mut [u8],
    ) -> anyhow::Result<()> {
        use windows::core::Interface;
        anyhow::ensure!(
            bytes.len() == width as usize * height as usize * 4,
            "invalid readback buffer"
        );
        let result = unsafe {
            frames_gpu_readback(
                self.0,
                source.texture.as_raw(),
                source.subresource as i32,
                width as i32,
                height as i32,
                bytes.as_mut_ptr(),
            )
        };
        anyhow::ensure!(result == 0, "D3D11 readback failed");
        Ok(())
    }
}
