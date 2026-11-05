//! NVENC driven directly through `nvEncodeAPI64.dll`, with D3D11 textures as
//! input. Ported from Phase 0 spike 0-C (`spikes/nvenc-direct`): P1 with
//! ultra-low-latency tuning, an infinite GOP, a one-frame VBV, slice output
//! read back while the picture is still encoding, reference-frame
//! invalidation, and on-demand intra refresh.
//!
//! The same encoder (`session.rs`, `encode.rs`) is compiled against each
//! supported API version's bindings, since a driver refuses structs from an
//! API newer than its own: 13.1 (driver 570 or newer), 12.2 (551), 12.1 (531)
//! and 12.0 (522). The newest one the driver supports is used.

use frames_media::api::{
    Chunk, Codec, EncodeParams, EncoderCaps, EncoderControl, MediaClock, MediaError, VideoEncoder,
    VideoFrame,
};
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

/// One module per API version (bindings in `v<major>_<minor>/`), and the
/// encoder that dispatches to whichever was opened, newest first.
macro_rules! versions {
    ($($module:ident $variant:ident $name:literal),+) => {
        $(
            mod $module {
                #[allow(clippy::all, unsafe_op_in_unsafe_fn, unnecessary_transmutes)]
                mod sys;
                #[rustfmt::skip]
                mod consts;
                // Deliberately compiled once per version, against its `sys`.
                #[allow(clippy::duplicate_mod)]
                #[path = "../session.rs"]
                mod session;
                pub use session::{NvencEncoder, available};
            }
        )+

        /// The NVENC API version this driver gets (such as "12.2"), or why
        /// it gets none (no NVIDIA driver, or one older than 522).
        /// `FRAMES_NVENC_API=12.0` (say) tries only that version, so older
        /// versions can be tested on a newer driver, which accepts them.
        pub fn api_version() -> Result<&'static str, MediaError> {
            let versions: &[(&'static str, fn() -> Result<(), MediaError>)] =
                &[$(($name, $module::available)),+];
            let forced = std::env::var("FRAMES_NVENC_API").ok();
            if let Some(forced) = &forced
                && !versions.iter().any(|(name, _)| name == forced)
            {
                return Err(MediaError::Backend(format!(
                    "FRAMES_NVENC_API={forced} is not a version this build has"
                )));
            }
            let mut oldest = None;
            for &(name, available) in versions
                .iter()
                .filter(|(name, _)| forced.as_deref().is_none_or(|f| f == *name))
            {
                match available() {
                    Ok(()) => return Ok(name),
                    Err(error) => oldest = Some(error),
                }
            }
            Err(oldest.expect("at least one version"))
        }

        enum Session {
            $($variant($module::NvencEncoder),)+
        }

        impl Session {
            fn open(
                device: &ID3D11Device,
                codec: Codec,
                params: EncodeParams,
                clock: Arc<dyn MediaClock>,
            ) -> Result<Self, MediaError> {
                let version = api_version()?;
                $(
                    if version == $name {
                        return Ok(Self::$variant($module::NvencEncoder::new(
                            device, codec, params, clock,
                        )?));
                    }
                )+
                unreachable!("api_version names a compiled version")
            }

            fn encoder(&mut self) -> &mut dyn VideoEncoder {
                match self {
                    $(Self::$variant(e) => e,)+
                }
            }

            fn caps(&self) -> &EncoderCaps {
                match self {
                    $(Self::$variant(e) => e.caps(),)+
                }
            }

            fn last_timing(&self) -> EncodeTiming {
                match self {
                    $(Self::$variant(e) => e.last_timing(),)+
                }
            }
        }
    };
}

versions!(v13_1 V13_1 "13.1", v12_2 V12_2 "12.2", v12_1 V12_1 "12.1", v12_0 V12_0 "12.0");

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

/// Whether this machine's NVIDIA driver can run this encoder, with the
/// reason when it cannot.
pub fn available() -> Result<(), MediaError> {
    api_version().map(drop)
}

pub struct NvencEncoder(Session);

impl NvencEncoder {
    /// Opens an encoder on `device`, which must belong to an NVIDIA adapter
    /// (see [`crate::d3d11::nvidia_device`]).
    pub fn new(
        device: &ID3D11Device,
        codec: Codec,
        params: EncodeParams,
        clock: Arc<dyn MediaClock>,
    ) -> Result<Self, MediaError> {
        Session::open(device, codec, params, clock).map(Self)
    }

    /// Timings of the most recent picture.
    pub fn last_timing(&self) -> EncodeTiming {
        self.0.last_timing()
    }
}

impl VideoEncoder for NvencEncoder {
    fn caps(&self) -> &EncoderCaps {
        self.0.caps()
    }

    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
        self.0.encoder().control(control)
    }

    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        self.0.encoder().encode(frame, out)
    }
}
