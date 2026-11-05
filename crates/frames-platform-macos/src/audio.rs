//! Host audio from ScreenCaptureKit (FramesMac ABI 7), as v1's
//! SystemAudioCapture: everything this Mac plays except Frames, or one
//! application's. Needs the Screen Recording permission.

use crate::ffi;
use std::ffi::c_void;

/// The PCM format delivered: interleaved f32.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;

/// Called with interleaved f32 stereo samples as they arrive, and with
/// `None` once the capture ended.
type Sink = Box<dyn Fn(Option<&[f32]>) + Send + Sync>;

/// Audio being captured; stops when dropped.
pub struct MacAudio {
    handle: *mut c_void,
    // Read by the capture queue until `frames_mac_audio_stop` returns.
    sink: *mut Sink,
}

// The handle is only stopped (once, on drop); the sink is Send + Sync.
unsafe impl Send for MacAudio {}

unsafe extern "C" fn on_audio(
    context: *mut c_void,
    samples: *const f32,
    frames: usize,
    channels: u32,
) {
    // SAFETY: `context` is the boxed sink, alive until the capture stops.
    let sink = unsafe { &*(context as *const Sink) };
    if samples.is_null() || frames == 0 {
        sink(None);
        return;
    }
    // SAFETY: Swift passes `frames * channels` samples for this call.
    let samples = unsafe { std::slice::from_raw_parts(samples, frames * channels as usize) };
    sink(Some(samples));
}

impl MacAudio {
    /// Starts capturing everything (`pid` `None`) or one process's audio.
    pub fn start(
        pid: Option<i32>,
        sink: impl Fn(Option<&[f32]>) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let sink: *mut Sink = Box::into_raw(Box::new(Box::new(sink)));
        let mut error = [0u8; 512];
        // SAFETY: the callback and context stay valid until stop (drop);
        // `error` is writable for its length.
        let handle = unsafe {
            ffi::frames_mac_audio_start(
                pid.unwrap_or(0),
                Some(on_audio),
                sink.cast(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if handle.is_null() {
            // SAFETY: no capture holds it.
            drop(unsafe { Box::from_raw(sink) });
            let end = error.iter().position(|&b| b == 0).unwrap_or(error.len());
            let reason = String::from_utf8_lossy(&error[..end]).into_owned();
            return Err(if reason.is_empty() {
                "audio capture failed".into()
            } else {
                reason
            });
        }
        Ok(Self { handle, sink })
    }
}

impl Drop for MacAudio {
    fn drop(&mut self) {
        // SAFETY: started by `start`; after stop returns no callback runs.
        unsafe {
            ffi::frames_mac_audio_stop(self.handle);
            drop(Box::from_raw(self.sink));
        }
    }
}
