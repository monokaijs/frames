//! NVENC driven directly through `nvEncodeAPI64.dll`, with D3D11 textures as
//! input. Ported from Phase 0 spike 0-C (`spikes/nvenc-direct`): P1 with
//! ultra-low-latency tuning, an infinite GOP, a one-frame VBV, slice output
//! read back while the picture is still encoding, reference-frame
//! invalidation, and on-demand intra refresh.
//!
//! The same encoder (`session.rs`, `encode.rs`) is compiled against two API
//! versions' bindings: 13.1 (driver 570 or newer) and 12.2 (driver 551 or
//! newer), since a driver refuses structs from a newer API than its own. The
//! newest one the driver supports is used.

use frames_media::api::{
    Chunk, Codec, EncodeParams, EncoderCaps, EncoderControl, MediaClock, MediaError, VideoEncoder,
    VideoFrame,
};
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

mod v13 {
    #[allow(clippy::all, unsafe_op_in_unsafe_fn, unnecessary_transmutes)]
    mod sys;
    #[rustfmt::skip]
    mod consts;
    // Deliberately compiled once per version, against that version's `sys`.
    #[allow(clippy::duplicate_mod)]
    #[path = "../session.rs"]
    mod session;
    pub use session::{NvencEncoder, available};
}

mod v12 {
    #[allow(clippy::all, unsafe_op_in_unsafe_fn, unnecessary_transmutes)]
    mod sys;
    #[rustfmt::skip]
    mod consts;
    // Deliberately compiled once per version, against that version's `sys`.
    #[allow(clippy::duplicate_mod)]
    #[path = "../session.rs"]
    mod session;
    pub use session::{NvencEncoder, available};
}

/// Per-picture timings of the last encode, for diagnostics and the Phase 3
/// latency harness.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeTiming {
    pub submit_us: u64,
    pub first_slice_us: u64,
    pub done_us: u64,
    pub polls: u32,
    pub bytes: u32,
    pub average_qp: u32,
}

/// The NVENC API version this driver gets ("13.1" or "12.2"), or why it
/// gets none (no NVIDIA driver, or one older than 551).
pub fn api_version() -> Result<&'static str, MediaError> {
    match v13::available() {
        Ok(()) => Ok("13.1"),
        Err(newest) => v12::available().map(|()| "12.2").map_err(|_| newest),
    }
}

/// Whether this machine's NVIDIA driver can run this encoder, with the
/// reason when it cannot.
pub fn available() -> Result<(), MediaError> {
    api_version().map(drop)
}

pub struct NvencEncoder(Session);

enum Session {
    V13(v13::NvencEncoder),
    V12(v12::NvencEncoder),
}

impl NvencEncoder {
    /// Opens an encoder on `device`, which must belong to an NVIDIA adapter
    /// (see [`crate::d3d11::nvidia_device`]).
    pub fn new(
        device: &ID3D11Device,
        codec: Codec,
        params: EncodeParams,
        clock: Arc<dyn MediaClock>,
    ) -> Result<Self, MediaError> {
        Ok(Self(match api_version()? {
            "13.1" => Session::V13(v13::NvencEncoder::new(device, codec, params, clock)?),
            _ => Session::V12(v12::NvencEncoder::new(device, codec, params, clock)?),
        }))
    }

    /// Timings of the most recent picture.
    pub fn last_timing(&self) -> EncodeTiming {
        match &self.0 {
            Session::V13(e) => e.last_timing(),
            Session::V12(e) => e.last_timing(),
        }
    }
}

impl VideoEncoder for NvencEncoder {
    fn caps(&self) -> &EncoderCaps {
        match &self.0 {
            Session::V13(e) => e.caps(),
            Session::V12(e) => e.caps(),
        }
    }

    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
        match &mut self.0 {
            Session::V13(e) => e.control(control),
            Session::V12(e) => e.control(control),
        }
    }

    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        match &mut self.0 {
            Session::V13(e) => e.encode(frame, out),
            Session::V12(e) => e.encode(frame, out),
        }
    }
}
