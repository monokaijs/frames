//! Hardware VideoToolbox encoding as a [`VideoEncoder`].
//!
//! VideoToolbox's long-term references are token-based: it tags frames it
//! keeps as long-term candidates, the app acknowledges the tokens of frames
//! the receiver holds, and `ForceLTRRefresh` predicts from an acknowledged
//! one. The engine's slots map onto that (verified on hardware, see
//! native/macos/FramesMac/Tests):
//! - `MarkLongTerm(slot)` puts the slot on the next frame carrying a token
//!   (H.264 tags every 2-3 frames without acknowledgements, HEVC nearly all).
//! - `ConfirmLongTerm(slot)` acknowledges that frame's token, only once the
//!   receiver reported holding it.
//! - `UseLongTerm(slot)` refreshes from it, only if confirmed. A refresh with
//!   nothing acknowledged makes VideoToolbox emit a sync frame after which
//!   decoding fails, so an unconfirmed slot falls back to a keyframe.

use crate::ffi;
use crate::pixel::{PixelBuffer, uptime_to_clock_us};
use frames_media::api::{
    Chunk, Codec, EncodeParams, EncoderCaps, EncoderControl, EncoderFeatures, FrameKind,
    FrameReference, MediaError, VideoEncoder, VideoFrame,
};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const SLOTS: usize = 2;

struct Output {
    data: Vec<u8>,
    keyframe: bool,
    token: i64,
    completed_ns: u64,
    status: i32,
}

#[derive(Default)]
struct Shared {
    outputs: Mutex<VecDeque<Output>>,
    ready: Condvar,
}

unsafe extern "C" fn on_encoded(
    context: *mut c_void,
    data: *const u8,
    length: usize,
    flags: u32,
    token: i64,
    _submitted_ns: u64,
    completed_ns: u64,
    status: i32,
) {
    // SAFETY: `context` is the `Shared` owned by the live `MacEncoder`;
    // `data` is valid for `length` bytes during the call.
    let shared = unsafe { &*(context as *const Shared) };
    let data = if data.is_null() || length == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, length) }.to_vec()
    };
    shared.outputs.lock().unwrap().push_back(Output {
        data,
        keyframe: flags & ffi::ENCODED_KEYFRAME != 0,
        token,
        completed_ns,
        status,
    });
    shared.ready.notify_one();
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    token: i64,
    confirmed: bool,
}

pub struct MacEncoder {
    handle: *mut c_void,
    shared: Box<Shared>,
    caps: EncoderCaps,
    params: EncodeParams,
    codec: u32,
    next_frame: u64,
    last_frame: Option<u64>,
    first: bool,
    force_keyframe: bool,
    mark: Option<u8>,
    /// Frames encoded since a mark was requested without a token arriving.
    mark_waiting: u32,
    use_long_term: Option<u8>,
    slots: [Option<Slot>; SLOTS],
    acknowledge: Vec<i64>,
}

// SAFETY: the Swift encoder is safe to call from any thread; this wrapper is
// used from one thread at a time (`&mut self`).
unsafe impl Send for MacEncoder {}

fn codec_id(codec: Codec) -> Result<u32, MediaError> {
    match codec {
        Codec::H264 => Ok(ffi::CODEC_H264),
        Codec::Hevc => Ok(ffi::CODEC_HEVC),
        Codec::Av1 => Err(MediaError::Unsupported("AV1 encoding on macOS")),
    }
}

/// Frames to wait for a first long-term token before concluding this
/// VideoToolbox does not produce them.
const NO_TOKEN_FRAMES: u32 = 30;

impl MacEncoder {
    pub fn new(codec: Codec, params: EncodeParams) -> Result<Self, MediaError> {
        let id = codec_id(codec)?;
        let mut encoder = Self {
            handle: std::ptr::null_mut(),
            shared: Box::new(Shared::default()),
            caps: EncoderCaps {
                codec,
                // Honest capabilities: no reference invalidation, no
                // sub-frame slice output (spike 0-B), long-term references
                // through tokens.
                features: EncoderFeatures::LONG_TERM_REFERENCES
                    .with(EncoderFeatures::DYNAMIC_BITRATE)
                    .with(EncoderFeatures::ZERO_COPY),
                max_fps: 120,
                max_slices: 1,
                long_term_slots: SLOTS as u8,
            },
            params,
            codec: id,
            next_frame: 0,
            last_frame: None,
            first: true,
            force_keyframe: true,
            mark: None,
            mark_waiting: 0,
            use_long_term: None,
            slots: [None; SLOTS],
            acknowledge: Vec::new(),
        };
        encoder.open()?;
        Ok(encoder)
    }

    fn open(&mut self) -> Result<(), MediaError> {
        if !self.handle.is_null() {
            // SAFETY: frees the previous session exactly once.
            unsafe { ffi::frames_mac_encoder_free(self.handle) };
            self.handle = std::ptr::null_mut();
        }
        let callbacks = ffi::Callbacks {
            on_encoded: Some(on_encoded),
            ..ffi::Callbacks::empty(&*self.shared as *const Shared as *mut c_void)
        };
        // SAFETY: `shared` outlives the session (freed in Drop first).
        self.handle = unsafe {
            ffi::frames_mac_encoder_new(
                self.params.width,
                self.params.height,
                self.params.fps,
                self.params.bitrate_bps,
                self.codec,
                ffi::ENCODER_LTR,
                callbacks,
            )
        };
        if self.handle.is_null() {
            return Err(MediaError::Backend(
                "VideoToolbox encoder unavailable".into(),
            ));
        }
        self.first = true;
        self.force_keyframe = true;
        self.slots = [None; SLOTS];
        self.acknowledge.clear();
        Ok(())
    }

    fn wait_output(&self) -> Option<Output> {
        let timeout = Duration::from_millis(if self.first { 1_000 } else { 500 });
        let outputs = self.shared.outputs.lock().unwrap();
        let (mut outputs, _) = self
            .shared
            .ready
            .wait_timeout_while(outputs, timeout, |o| o.is_empty())
            .unwrap();
        outputs.pop_front()
    }
}

impl VideoEncoder for MacEncoder {
    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }

    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
        match control {
            EncoderControl::Bitrate(bps) => {
                self.params.bitrate_bps = bps;
                // SAFETY: live session.
                let status = unsafe {
                    ffi::frames_mac_encoder_set_bitrate(self.handle, bps, self.params.fps)
                };
                if status != 0 {
                    return Err(MediaError::Backend(format!(
                        "bitrate update rejected ({status})"
                    )));
                }
            }
            EncoderControl::ForceKeyframe => self.force_keyframe = true,
            EncoderControl::MarkLongTerm(slot) => {
                if usize::from(slot) >= SLOTS {
                    return Err(MediaError::Unsupported("long-term slot"));
                }
                self.mark = Some(slot);
            }
            EncoderControl::ConfirmLongTerm(slot) => {
                if let Some(Some(entry)) = self.slots.get_mut(usize::from(slot))
                    && !entry.confirmed
                {
                    entry.confirmed = true;
                    self.acknowledge.push(entry.token);
                }
            }
            EncoderControl::UseLongTerm(slot) => {
                let confirmed = self
                    .slots
                    .get(usize::from(slot))
                    .copied()
                    .flatten()
                    .is_some_and(|s| s.confirmed);
                if confirmed {
                    self.use_long_term = Some(slot);
                } else {
                    self.force_keyframe = true;
                }
            }
            EncoderControl::Invalidate { .. } => {
                return Err(MediaError::Unsupported(
                    "reference invalidation on VideoToolbox",
                ));
            }
            EncoderControl::Refresh => {
                return Err(MediaError::Unsupported("intra refresh on VideoToolbox"));
            }
            EncoderControl::Reconfigure(params) => {
                self.params = params;
                self.open()?;
            }
        }
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        let buffer = PixelBuffer::from_surface(&frame.surface)
            .ok_or(MediaError::Unsupported("encoding a non-IOSurface frame"))?;
        let mut options = 0;
        let refresh = self.use_long_term.take();
        if self.force_keyframe {
            options |= ffi::ENCODE_FORCE_KEYFRAME;
        } else if refresh.is_some() {
            options |= ffi::ENCODE_FORCE_LTR_REFRESH;
        }
        let acknowledge = std::mem::take(&mut self.acknowledge);
        // SAFETY: live session; the buffer and token array outlive the call.
        let status = unsafe {
            ffi::frames_mac_encoder_encode(
                self.handle,
                buffer.as_ptr(),
                frame.captured_us * 1000,
                options,
                acknowledge.as_ptr(),
                acknowledge.len(),
            )
        };
        if status != 0 {
            return Err(MediaError::Backend(format!(
                "VideoToolbox encode failed ({status})"
            )));
        }
        let output = self
            .wait_output()
            .ok_or_else(|| MediaError::Backend("VideoToolbox produced no output".into()))?;
        self.first = false;
        if output.status != 0 || output.data.is_empty() {
            // A dropped picture: nothing to send; the next frame continues
            // from the last one that was encoded.
            return Ok(());
        }
        self.force_keyframe = false;
        let number = self.next_frame;
        self.next_frame += 1;
        let (kind, reference) = if output.keyframe {
            // An IDR resets every reference, long-term ones included.
            self.slots = [None; SLOTS];
            (FrameKind::Keyframe, FrameReference::None)
        } else if let Some(slot) = refresh.filter(|_| options & ffi::ENCODE_FORCE_LTR_REFRESH != 0)
        {
            (FrameKind::Predicted, FrameReference::LongTerm(slot))
        } else {
            let previous = self.last_frame.ok_or(MediaError::Backend(
                "VideoToolbox produced a predicted first frame".into(),
            ))?;
            (FrameKind::Predicted, FrameReference::Frame(previous))
        };
        if self.mark.is_some() && output.token < 0 {
            self.mark_waiting += 1;
            // Some VideoToolbox implementations (paravirtualized Macs, such
            // as CI runners) accept long-term references but hand out no
            // token after a mark (at most one on the first keyframe). Stop advertising them so the engine recovers with
            // keyframes instead of waiting for a slot that never comes.
            if self.mark_waiting >= NO_TOKEN_FRAMES
                && self
                    .caps
                    .features
                    .contains(EncoderFeatures::LONG_TERM_REFERENCES)
            {
                self.caps.features =
                    EncoderFeatures::DYNAMIC_BITRATE.with(EncoderFeatures::ZERO_COPY);
                self.caps.long_term_slots = 0;
                self.mark = None;
            }
        }
        let mut mark = None;
        if output.token >= 0
            && let Some(slot) = self.mark.take()
        {
            self.mark_waiting = 0;
            self.slots[usize::from(slot)] = Some(Slot {
                token: output.token,
                confirmed: false,
            });
            mark = Some(slot);
        }
        self.last_frame = Some(number);
        out(Chunk {
            frame: number,
            slice: 0,
            slices: 1,
            kind,
            reference,
            mark_long_term: mark,
            data: &output.data,
            captured_us: frame.captured_us,
            ready_us: uptime_to_clock_us(output.completed_ns),
        });
        Ok(())
    }
}

impl Drop for MacEncoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: completes pending frames and frees the session once,
            // before `shared` is dropped.
            unsafe { ffi::frames_mac_encoder_free(self.handle) };
        }
    }
}
