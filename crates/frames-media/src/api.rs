//! Boundary between the media engine and its backends.
//!
//! Hardware backends (frames-codec, frames-platform-*) and the synthetic ones
//! in frames-testkit implement these traits; the engine is written only
//! against them. All calls are synchronous: each pipeline stage owns one
//! thread (or a virtual-time loop in tests).
//!
//! Times are microseconds on a [`MediaClock`].

use bytes::Bytes;
use std::any::Any;
use std::fmt;
use std::sync::Arc;

/// Monotonic microseconds. Real clocks count from process start; the
/// testkit's virtual clock is advanced explicitly.
pub trait MediaClock: Send + Sync {
    fn now_us(&self) -> u64;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    /// The backend cannot continue (device lost, driver error). The engine
    /// falls back to another backend or restarts the stream generation.
    Backend(String),
    /// The request is outside what [`EncoderCaps`] / [`DecoderCaps`] allow.
    Unsupported(&'static str),
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(reason) => write!(f, "media backend failed: {reason}"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

impl std::error::Error for MediaError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Nv12,
}

/// Pixels in CPU memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuImage {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
    pub data: Bytes,
}

/// A GPU surface owned by a platform backend (a D3D11 texture plus fence, an
/// IOSurface-backed CVPixelBuffer). Only backends of the same platform can
/// use it; they downcast through [`PlatformSurface::as_any`].
pub trait PlatformSurface: Send + Sync + fmt::Debug {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug)]
pub enum Surface {
    Cpu(CpuImage),
    Platform(Arc<dyn PlatformSurface>),
}

impl Surface {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Cpu(image) => (image.width, image.height),
            Self::Platform(surface) => (surface.width(), surface.height()),
        }
    }
}

/// A rectangle in source pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct VideoFrame {
    /// Capture sequence number, increasing per source.
    pub id: u64,
    pub captured_us: u64,
    pub surface: Surface,
    /// Changed regions since the previous frame, when the source knows them.
    pub dirty: Option<Vec<Rect>>,
}

/// Cursor shape and position, sent separately from video so the client can
/// draw it locally with no video latency.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorUpdate {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    /// New shape (premultiplied BGRA, hotspot) only when it changed.
    pub shape: Option<CursorShape>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub bgra: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureCaps {
    pub max_fps: u32,
    pub separate_cursor: bool,
    pub dirty_rects: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    pub fps: u32,
    /// Capture without the cursor; it is reported through `cursor()`.
    pub separate_cursor: bool,
}

#[derive(Debug)]
pub enum CaptureEvent {
    Frame(VideoFrame),
    /// Nothing changed before the deadline.
    Timeout,
    Resized {
        width: u32,
        height: u32,
    },
    /// The source went away (window closed, display removed).
    Lost,
}

pub trait CaptureSource: Send {
    fn caps(&self) -> CaptureCaps;
    fn configure(&mut self, config: CaptureConfig) -> Result<(), MediaError>;
    /// Waits until `deadline_us` for the next frame.
    fn next(&mut self, deadline_us: u64) -> Result<CaptureEvent, MediaError>;
    fn cursor(&mut self) -> Option<CursorUpdate>;
}

/// Optional encoder features, as a set of flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EncoderFeatures(u32);

impl EncoderFeatures {
    pub const INTRA_REFRESH: Self = Self(1);
    /// Reference frame invalidation: predict from a frame before a loss.
    pub const INVALIDATE: Self = Self(1 << 1);
    pub const LONG_TERM_REFERENCES: Self = Self(1 << 2);
    /// Slices are emitted as soon as each one is encoded.
    pub const SLICE_OUTPUT: Self = Self(1 << 3);
    pub const DYNAMIC_BITRATE: Self = Self(1 << 4);
    pub const DYNAMIC_RESOLUTION: Self = Self(1 << 5);
    pub const ONE_FRAME_VBV: Self = Self(1 << 6);
    pub const ZERO_COPY: Self = Self(1 << 7);
    pub const HDR10: Self = Self(1 << 8);
    pub const YUV444: Self = Self(1 << 9);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderCaps {
    pub codec: Codec,
    pub features: EncoderFeatures,
    pub max_fps: u32,
    pub max_slices: u8,
    pub long_term_slots: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gop {
    /// Keyframes only on request.
    Infinite,
    Frames(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodeParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// Rate-control buffer in frames; 1.0 bounds every frame to its share.
    pub vbv_frames: f32,
    pub gop: Gop,
    /// Spread a refresh over this many frames instead of sending an IDR.
    pub intra_refresh_frames: Option<u16>,
    pub slices: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EncoderControl {
    Bitrate(u32),
    ForceKeyframe,
    /// Frames `first..=last` were lost: the next frame must not use them.
    Invalidate {
        first: u64,
        last: u64,
    },
    /// Start an intra-refresh wave; requires [`EncoderFeatures::INTRA_REFRESH`].
    Refresh,
    /// Keep the next frame in long-term slot `slot`.
    MarkLongTerm(u8),
    /// The receiver confirmed decoding the frame in long-term slot `slot`.
    /// Encoders whose long-term references need acknowledgement (VideoToolbox
    /// tokens) only predict from confirmed frames; others may ignore this.
    ConfirmLongTerm(u8),
    /// Predict the next frame from long-term slot `slot`.
    UseLongTerm(u8),
    Reconfigure(EncodeParams),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// Instantaneous decoder refresh.
    Keyframe,
    /// Part of an intra-refresh wave; `complete` on its last frame.
    Refresh {
        complete: bool,
    },
    Predicted,
}

/// What a frame was predicted from, in 64-bit frame numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameReference {
    None,
    Frame(u64),
    LongTerm(u8),
}

/// One encoder output unit: a whole frame, or one slice of it.
#[derive(Clone, Copy, Debug)]
pub struct Chunk<'a> {
    pub frame: u64,
    pub slice: u8,
    pub slices: u8,
    pub kind: FrameKind,
    pub reference: FrameReference,
    pub mark_long_term: Option<u8>,
    pub data: &'a [u8],
    pub captured_us: u64,
    /// When the encoder produced this chunk.
    pub ready_us: u64,
}

pub trait VideoEncoder: Send {
    fn caps(&self) -> &EncoderCaps;
    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError>;
    /// Encodes one frame, calling `out` once per slice as each is ready.
    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError>;
}

/// A reassembled frame (all slices) ready to decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    pub frame: u64,
    pub kind: FrameKind,
    pub reference: FrameReference,
    pub mark_long_term: Option<u8>,
    pub captured_us: u64,
    pub data: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecoderCaps {
    pub codec: Codec,
    pub max_width: u32,
    pub max_height: u32,
    pub hardware: bool,
}

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub frame: u64,
    pub captured_us: u64,
    pub surface: Surface,
    /// False while an intra-refresh wave is still healing a reference loss.
    pub clean: bool,
}

#[derive(Debug)]
pub enum Decoded {
    Frame(DecodedFrame),
    /// Accepted; output will follow (pipelined hardware decoders).
    Pending,
    /// The unit's reference is missing or corrupt; the engine asks for
    /// recovery from `last_good`.
    BrokenReference {
        last_good: Option<u64>,
    },
}

pub trait VideoDecoder: Send {
    fn caps(&self) -> DecoderCaps;
    fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentCaps {
    pub tearing: bool,
    pub variable_refresh: bool,
    pub refresh_hz: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentMode {
    /// Present immediately; may tear. Lowest latency.
    Immediate,
    /// Wait for the next vertical blank.
    Vsync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PresentStamp {
    pub frame: u64,
    pub presented_us: u64,
}

pub trait Presenter: Send {
    fn caps(&self) -> PresentCaps;
    /// Waits until the presenter can accept a frame without queueing.
    fn wait_ready(&mut self, deadline_us: u64) -> bool;
    fn present(
        &mut self,
        frame: DecodedFrame,
        mode: PresentMode,
    ) -> Result<PresentStamp, MediaError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Direct,
    Relay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathInfo {
    pub kind: PathKind,
    pub rtt_us: u64,
    pub max_datagram: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backpressure;

/// Unreliable datagram transport for media.
pub trait MediaTransport: Send + Sync {
    fn path(&self) -> PathInfo;
    /// Hands one datagram to the transport. Never queues beyond the
    /// congestion window: returns [`Backpressure`] instead.
    fn try_send(&self, datagram: Bytes) -> Result<(), Backpressure>;
    /// The next received datagram, if one is ready.
    fn try_recv(&self) -> Option<Bytes>;
    /// The media rate controller's target; drives the transport's pacing.
    fn set_target_rate(&self, bits_per_second: u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flags_compose() {
        let features = EncoderFeatures::INVALIDATE.with(EncoderFeatures::SLICE_OUTPUT);
        assert!(features.contains(EncoderFeatures::INVALIDATE));
        assert!(features.contains(EncoderFeatures::SLICE_OUTPUT));
        assert!(!features.contains(EncoderFeatures::INTRA_REFRESH));
        assert!(features.contains(EncoderFeatures::empty()));
    }

    #[derive(Debug)]
    struct Texture;
    impl PlatformSurface for Texture {
        fn width(&self) -> u32 {
            64
        }
        fn height(&self) -> u32 {
            32
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn platform_surfaces_are_opaque_but_downcastable() {
        let surface = Surface::Platform(Arc::new(Texture));
        assert_eq!(surface.size(), (64, 32));
        let Surface::Platform(inner) = &surface else {
            unreachable!()
        };
        assert!(inner.as_any().downcast_ref::<Texture>().is_some());
    }
}
