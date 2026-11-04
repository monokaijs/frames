//! Synthetic encoder and decoder that model reference chains.
//!
//! The encoder emits chunks whose payload is a deterministic function of the
//! frame number, so the decoder can verify every byte it receives. The
//! decoder keeps the set of decoded frames and long-term slots; a predicted
//! frame whose reference it never decoded fails with
//! [`Decoded::BrokenReference`], exactly the event the engine's recovery
//! ladder must handle. Intra-refresh frames still decode after a loss but
//! report `clean: false` until the wave completes.
//!
//! Model simplifications (documented so tests do not over-trust them): a
//! completed refresh wave always heals, and corrupt-but-decodable frames
//! never fail decoding.

use crate::clock::VirtualClock;
use bytes::{Bytes, BytesMut};
use frames_media::api::{
    AccessUnit, Chunk, Codec, CpuImage, Decoded, DecodedFrame, DecoderCaps, EncodeParams,
    EncoderCaps, EncoderControl, EncoderFeatures, FrameKind, FrameReference, Gop, MediaClock,
    MediaError, PixelFormat, Surface, VideoDecoder, VideoEncoder, VideoFrame,
};
use std::collections::{BTreeSet, VecDeque};

const LONG_TERM_SLOTS: usize = 16;
/// Frames the encoder may still predict from after an invalidation.
const DECODED_PICTURE_BUFFER: usize = 16;
const CHUNK_HEADER: usize = 11;

/// Output sizes and timing of the synthetic encoder. Sizes follow the
/// configured bitrate, as a rate-controlled encoder's do: a predicted frame
/// is one frame's share of the bitrate; keyframes and refresh frames are
/// multiples of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncoderModel {
    pub keyframe_factor: f64,
    pub refresh_factor: f64,
    /// Time from submit to the last slice.
    pub encode_us: u64,
}

impl Default for EncoderModel {
    fn default() -> Self {
        Self {
            keyframe_factor: 5.0,
            refresh_factor: 1.5,
            encode_us: 3_000,
        }
    }
}

impl EncoderModel {
    fn bytes(&self, params: &EncodeParams, kind: FrameKind) -> usize {
        let share = f64::from(params.bitrate_bps) / 8.0 / f64::from(params.fps.max(1));
        let factor = match kind {
            FrameKind::Keyframe => self.keyframe_factor,
            FrameKind::Refresh { .. } => self.refresh_factor,
            FrameKind::Predicted => 1.0,
        };
        (share * factor).round().max(64.0) as usize
    }
}

pub struct SyntheticEncoder {
    caps: EncoderCaps,
    params: EncodeParams,
    model: EncoderModel,
    clock: VirtualClock,
    next_frame: u64,
    since_keyframe: u32,
    recent: VecDeque<u64>,
    long_term: [Option<u64>; LONG_TERM_SLOTS],
    force_keyframe: bool,
    invalidated: Option<(u64, u64)>,
    refresh_left: Option<u16>,
    mark: Option<u8>,
    use_long_term: Option<u8>,
}

impl SyntheticEncoder {
    pub fn new(clock: &VirtualClock, params: EncodeParams, model: EncoderModel) -> Self {
        Self {
            caps: EncoderCaps {
                codec: Codec::Hevc,
                features: EncoderFeatures::INTRA_REFRESH
                    .with(EncoderFeatures::INVALIDATE)
                    .with(EncoderFeatures::LONG_TERM_REFERENCES)
                    .with(EncoderFeatures::SLICE_OUTPUT)
                    .with(EncoderFeatures::DYNAMIC_BITRATE)
                    .with(EncoderFeatures::ONE_FRAME_VBV),
                max_fps: 240,
                max_slices: 8,
                long_term_slots: LONG_TERM_SLOTS as u8,
            },
            params,
            model,
            clock: clock.clone(),
            next_frame: 0,
            since_keyframe: 0,
            recent: VecDeque::new(),
            long_term: [None; LONG_TERM_SLOTS],
            force_keyframe: true,
            invalidated: None,
            refresh_left: None,
            mark: None,
            use_long_term: None,
        }
    }

    pub fn params(&self) -> &EncodeParams {
        &self.params
    }

    /// Advertises only `features`, e.g. to exercise recovery without
    /// reference invalidation. Controls stay functional either way.
    pub fn set_features(&mut self, features: EncoderFeatures) {
        self.caps.features = features;
    }

    fn plan(&mut self) -> (FrameKind, FrameReference) {
        let periodic = matches!(self.params.gop, Gop::Frames(n) if self.since_keyframe >= n);
        let previous = self.recent.back().copied();
        if self.force_keyframe || periodic || previous.is_none() {
            self.force_keyframe = false;
            self.invalidated = None;
            self.refresh_left = None;
            self.use_long_term = None;
            return (FrameKind::Keyframe, FrameReference::None);
        }
        if let Some(slot) = self.use_long_term.take() {
            self.invalidated = None;
            let reference = FrameReference::LongTerm(slot);
            return (FrameKind::Predicted, reference);
        }
        if let Some((first, _)) = self.invalidated.take() {
            // Predict from the newest frame before the loss, if still held.
            match self.recent.iter().rev().find(|f| **f < first) {
                Some(&good) => {
                    let reference = FrameReference::Frame(good);
                    return (FrameKind::Predicted, reference);
                }
                None => return (FrameKind::Keyframe, FrameReference::None),
            }
        }
        let reference = FrameReference::Frame(previous.expect("checked above"));
        if let Some(left) = self.refresh_left {
            self.refresh_left = (left > 1).then(|| left - 1);
            let kind = FrameKind::Refresh {
                complete: left == 1,
            };
            return (kind, reference);
        }
        (FrameKind::Predicted, reference)
    }
}

/// Deterministic payload: the decoder recomputes and compares it.
fn payload_byte(frame: u64, index: usize) -> u8 {
    (frame.wrapping_mul(31).wrapping_add(index as u64 * 7)) as u8
}

fn kind_code(kind: FrameKind) -> u8 {
    match kind {
        FrameKind::Keyframe => 0,
        FrameKind::Predicted => 1,
        FrameKind::Refresh { complete: false } => 2,
        FrameKind::Refresh { complete: true } => 3,
    }
}

impl VideoEncoder for SyntheticEncoder {
    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }

    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
        match control {
            EncoderControl::Bitrate(bps) => self.params.bitrate_bps = bps,
            EncoderControl::ForceKeyframe => self.force_keyframe = true,
            EncoderControl::Invalidate { first, last } => {
                if first > last {
                    return Err(MediaError::Unsupported("empty invalidation range"));
                }
                self.recent.retain(|f| *f < first || *f > last);
                self.invalidated = Some((first, last));
            }
            EncoderControl::Refresh => {
                let frames = self
                    .params
                    .intra_refresh_frames
                    .ok_or(MediaError::Unsupported("intra refresh is not configured"))?;
                self.refresh_left = Some(frames.max(1));
            }
            EncoderControl::MarkLongTerm(slot) => {
                if usize::from(slot) >= LONG_TERM_SLOTS {
                    return Err(MediaError::Unsupported("long-term slot"));
                }
                self.mark = Some(slot);
            }
            // Synthetic long-term references need no acknowledgement.
            EncoderControl::ConfirmLongTerm(_) => {}
            EncoderControl::UseLongTerm(slot) => {
                if self
                    .long_term
                    .get(usize::from(slot))
                    .copied()
                    .flatten()
                    .is_none()
                {
                    return Err(MediaError::Unsupported("empty long-term slot"));
                }
                self.use_long_term = Some(slot);
            }
            EncoderControl::Reconfigure(params) => {
                if params.slices == 0 || params.slices > self.caps.max_slices {
                    return Err(MediaError::Unsupported("slice count"));
                }
                self.params = params;
                self.force_keyframe = true;
            }
        }
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        let number = self.next_frame;
        self.next_frame += 1;
        let (kind, reference) = self.plan();
        let bytes = self.model.bytes(&self.params, kind);
        if kind == FrameKind::Keyframe {
            // An IDR resets every reference, long-term ones included.
            self.since_keyframe = 0;
            self.recent.clear();
            self.long_term = [None; LONG_TERM_SLOTS];
        }
        self.since_keyframe += 1;
        let mark = self.mark.take();
        if let Some(slot) = mark {
            self.long_term[usize::from(slot)] = Some(number);
        }
        self.recent.push_back(number);
        while self.recent.len() > DECODED_PICTURE_BUFFER {
            self.recent.pop_front();
        }

        let slices = self.params.slices.max(1);
        let per_slice = (bytes / usize::from(slices)).max(CHUNK_HEADER + 1);
        let start = self.clock.now_us();
        for slice in 0..slices {
            let mut data = BytesMut::with_capacity(per_slice);
            data.extend_from_slice(&number.to_le_bytes());
            data.extend_from_slice(&[slice, slices, kind_code(kind)]);
            for i in CHUNK_HEADER..per_slice {
                data.extend_from_slice(&[payload_byte(number, usize::from(slice) * per_slice + i)]);
            }
            out(Chunk {
                frame: number,
                slice,
                slices,
                kind,
                reference,
                mark_long_term: mark,
                data: &data,
                captured_us: frame.captured_us,
                ready_us: start + self.model.encode_us * (u64::from(slice) + 1) / u64::from(slices),
            });
        }
        Ok(())
    }
}

/// Decoder that verifies payloads and tracks references.
#[derive(Default)]
pub struct SyntheticDecoder {
    decoded: BTreeSet<u64>,
    long_term: [Option<u64>; LONG_TERM_SLOTS],
    healing: bool,
    last_good: Option<u64>,
}

impl SyntheticDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    fn verify(unit: &AccessUnit) -> Result<(), MediaError> {
        let corrupt = || {
            Err(MediaError::Backend(format!(
                "corrupt access unit {}",
                unit.frame
            )))
        };
        let mut offset = 0;
        let mut expected_slice = 0u8;
        while offset < unit.data.len() {
            let rest = &unit.data[offset..];
            if rest.len() < CHUNK_HEADER {
                return corrupt();
            }
            let frame = u64::from_le_bytes(rest[..8].try_into().expect("8 bytes"));
            let (slice, slices, kind) = (rest[8], rest[9], rest[10]);
            if frame != unit.frame || slice != expected_slice || kind != kind_code(unit.kind) {
                return corrupt();
            }
            // Slices are equal-sized; the count tells the size.
            let per_slice = unit.data.len() / usize::from(slices.max(1));
            if per_slice < CHUNK_HEADER || rest.len() < per_slice {
                return corrupt();
            }
            let base = usize::from(slice) * per_slice;
            let body = &rest[CHUNK_HEADER..per_slice];
            if body
                .iter()
                .enumerate()
                .any(|(i, byte)| *byte != payload_byte(frame, base + CHUNK_HEADER + i))
            {
                return corrupt();
            }
            offset += per_slice;
            expected_slice += 1;
            if expected_slice == slices {
                return if offset == unit.data.len() {
                    Ok(())
                } else {
                    corrupt()
                };
            }
        }
        corrupt()
    }

    fn reference_available(&self, reference: FrameReference) -> bool {
        match reference {
            FrameReference::None => true,
            FrameReference::Frame(frame) => self.decoded.contains(&frame),
            // Slots only hold cleanly decoded frames and are never trimmed.
            FrameReference::LongTerm(slot) => self
                .long_term
                .get(usize::from(slot))
                .copied()
                .flatten()
                .is_some(),
        }
    }
}

impl VideoDecoder for SyntheticDecoder {
    fn caps(&self) -> DecoderCaps {
        DecoderCaps {
            codec: Codec::Hevc,
            max_width: 7680,
            max_height: 4320,
            hardware: false,
        }
    }

    fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError> {
        Self::verify(&unit)?;
        let available = self.reference_available(unit.reference);
        match unit.kind {
            FrameKind::Keyframe => {
                self.decoded.clear();
                self.long_term = [None; LONG_TERM_SLOTS];
                self.healing = false;
            }
            FrameKind::Predicted if !available => {
                return Ok(Decoded::BrokenReference {
                    last_good: self.last_good,
                });
            }
            FrameKind::Refresh { .. } if !available => self.healing = true,
            _ => {}
        }
        if unit.kind == (FrameKind::Refresh { complete: true }) {
            self.healing = false;
        }
        let clean = !self.healing;
        self.decoded.insert(unit.frame);
        while self.decoded.len() > 64 {
            self.decoded.pop_first();
        }
        if clean {
            self.last_good = Some(unit.frame);
        }
        if let Some(slot) = unit.mark_long_term
            && clean
            && let Some(entry) = self.long_term.get_mut(usize::from(slot))
        {
            *entry = Some(unit.frame);
        }
        Ok(Decoded::Frame(DecodedFrame {
            frame: unit.frame,
            captured_us: unit.captured_us,
            surface: Surface::Cpu(CpuImage {
                width: 0,
                height: 0,
                stride: 0,
                format: PixelFormat::Nv12,
                data: Bytes::new(),
            }),
            clean,
        }))
    }
}

#[cfg(test)]
mod tests;
