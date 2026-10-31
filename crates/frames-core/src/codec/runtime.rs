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
    fn frames_encoder_send(
        context: *mut c_void,
        pixels: *const u8,
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
    pub fn encoder(name: &str, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        initialize()?;
        let name = CString::new(name)?;
        let mut error = [0i8; 512];
        let pointer = unsafe {
            frames_encoder_open(
                name.as_ptr(),
                width as i32,
                height as i32,
                fps as i32,
                error.as_mut_ptr(),
                512,
            )
        };
        if pointer.is_null() {
            check(-1, &error)?;
        }
        Ok(Self {
            pointer,
            width,
            height,
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
            pointer,
            width,
            height,
        })
    }
    pub fn encode(&mut self, frame: &crate::media::RawFrame) -> anyhow::Result<Vec<Vec<u8>>> {
        anyhow::ensure!(
            frame.validate() && frame.width == self.width && frame.height == self.height,
            "Invalid encoder input frame"
        );
        let mut error = [0i8; 512];
        check(
            unsafe {
                frames_encoder_send(self.pointer, frame.bgra.as_ptr(), error.as_mut_ptr(), 512)
            },
            &error,
        )?;
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
        let mut latest = None;
        loop {
            let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
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
                width: self.width,
                height: self.height,
                bgra: pixels.into(),
            });
        }
        Ok(latest)
    }
}
