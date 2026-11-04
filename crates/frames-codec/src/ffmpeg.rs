//! FFmpeg codecs through the v1 runtime in `frames-core`.
//!
//! - [`FfmpegDecoder`]: D3D11VA surfaces on Windows, software elsewhere or as
//!   a fallback.
//! - [`FfmpegEncoder`]: the encoders NVENC direct does not cover (AMF, QSV,
//!   Media Foundation, software), with an infinite GOP and a one-frame VBV.
//!   D3D11 frames go to the encoder as textures when it accepts them.
//!
//! Temporary dependency: at the v2 cutover the runtime moves into this crate
//! and `frames-core` goes away (docs/V2_PLAN.md, Phase 3).

use frames_core::codec::runtime::{Context, set_encoder_shape};
use frames_media::api::{
    AccessUnit, Chunk, Codec, CpuImage, Decoded, DecodedFrame, DecoderCaps, EncodeParams,
    EncoderCaps, EncoderControl, EncoderFeatures, FrameKind, FrameReference, Gop, MediaClock,
    MediaError, PixelFormat, Surface, VideoDecoder, VideoEncoder, VideoFrame,
};
use std::sync::Arc;

pub struct FfmpegDecoder {
    context: Context,
    caps: DecoderCaps,
}

// The FFmpeg context is used by one thread at a time; the decoder is moved,
// never shared.
unsafe impl Send for FfmpegDecoder {}

fn decoder_name(codec: Codec, hardware: bool) -> &'static str {
    match codec {
        Codec::H264 => frames_core::codec::Codec::H264.decoder(),
        Codec::Hevc => frames_core::codec::Codec::Hevc.decoder(),
        // FFmpeg's own AV1 decoder takes D3D11VA; dav1d is software only.
        Codec::Av1 if hardware => "av1",
        Codec::Av1 => frames_core::codec::Codec::Av1.decoder(),
    }
}

impl FfmpegDecoder {
    /// `hardware` asks for D3D11VA (Windows) with GPU surfaces as output.
    pub fn new(codec: Codec, width: u32, height: u32, hardware: bool) -> Result<Self, MediaError> {
        let context = Context::decoder(decoder_name(codec, hardware), width, height, hardware)
            .map_err(|e| MediaError::Backend(e.to_string()))?;
        Ok(Self {
            context,
            caps: DecoderCaps {
                codec,
                max_width: width,
                max_height: height,
                hardware,
            },
        })
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn caps(&self) -> DecoderCaps {
        self.caps
    }

    fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError> {
        // The v1 runtime takes a one-byte keyframe flag before the bitstream.
        let mut packet = Vec::with_capacity(unit.data.len() + 1);
        packet.push(u8::from(unit.kind == FrameKind::Keyframe));
        packet.extend_from_slice(&unit.data);
        let decoded = if self.caps.hardware {
            self.context.decode_presentable(&packet)
        } else {
            self.context.decode(&packet)
        }
        .map_err(|e| MediaError::Backend(e.to_string()))?;
        let Some(raw) = decoded else {
            return Ok(Decoded::Pending);
        };
        #[cfg(windows)]
        let surface = match raw.gpu.clone() {
            Some(gpu) => Surface::Platform(std::sync::Arc::new(crate::d3d11::D3D11Surface {
                texture: gpu.texture.clone(),
                subresource: gpu.subresource,
                width: raw.width,
                height: raw.height,
                matrix_709: gpu.matrix_709,
                full_range: gpu.full_range,
                keepalive: Some(gpu),
            })),
            None => cpu(&raw),
        };
        #[cfg(not(windows))]
        let surface = cpu(&raw);
        Ok(Decoded::Frame(DecodedFrame {
            frame: unit.frame,
            captured_us: unit.captured_us,
            surface,
            clean: true,
        }))
    }
}

fn cpu(raw: &frames_core::media::RawFrame) -> Surface {
    Surface::Cpu(CpuImage {
        width: raw.width,
        height: raw.height,
        stride: raw.width * 4,
        format: PixelFormat::Bgra8,
        data: raw.bgra.clone(),
    })
}

/// An FFmpeg encoder driven by the v2 engine. Without reference invalidation
/// or long-term references, the engine recovers from loss with a keyframe.
pub struct FfmpegEncoder {
    name: String,
    context: Context,
    caps: EncoderCaps,
    params: EncodeParams,
    clock: Arc<dyn MediaClock>,
    next_frame: u64,
    keyframe: bool,
    #[cfg(windows)]
    zero_copy_reported: bool,
}

// One thread at a time uses the context; the encoder is moved, never shared.
unsafe impl Send for FfmpegEncoder {}

/// The FFmpeg encoder name for `codec` and `backend` (`amf`, `qsv`, `mf`,
/// `nvenc`, or `software`, which is AV1 only).
pub fn encoder_name(codec: Codec, backend: &str) -> Option<String> {
    let base = match codec {
        Codec::H264 => "h264",
        Codec::Hevc => "hevc",
        Codec::Av1 => "av1",
    };
    Some(match (backend, codec) {
        // The bundled LGPL FFmpeg has no software H.264 or HEVC encoder.
        ("software", Codec::Av1) => "libaom-av1".into(),
        ("software", _) => return None,
        ("amf" | "qsv" | "mf" | "nvenc", _) => format!("{base}_{backend}"),
        _ => return None,
    })
}

fn backend_error(error: impl std::fmt::Display) -> MediaError {
    MediaError::Backend(error.to_string())
}

fn apply_shape(params: &EncodeParams) {
    let gop = match params.gop {
        Gop::Infinite => None,
        Gop::Frames(frames) => Some(frames),
    };
    // A VBV of `vbv_frames` frames holds bitrate * vbv_frames / fps bits.
    let divisor = (f64::from(params.fps) / params.vbv_frames.max(0.25) as f64).round() as u32;
    set_encoder_shape(gop, divisor.max(1));
}

impl FfmpegEncoder {
    /// Opens `name` (see [`encoder_name`]). Fails if FFmpeg lacks it or the
    /// hardware is absent.
    pub fn new(
        name: &str,
        codec: Codec,
        params: EncodeParams,
        clock: Arc<dyn MediaClock>,
    ) -> Result<Self, MediaError> {
        apply_shape(&params);
        let context = Context::encoder_at_rate(
            name,
            params.width,
            params.height,
            params.fps,
            params.bitrate_bps,
        )
        .map_err(backend_error)?;
        let mut features = EncoderFeatures::DYNAMIC_BITRATE;
        if params.vbv_frames <= 1.0 {
            features = features.with(EncoderFeatures::ONE_FRAME_VBV);
        }
        Ok(Self {
            name: name.to_string(),
            context,
            caps: EncoderCaps {
                codec,
                features,
                max_fps: 240,
                max_slices: 1,
                long_term_slots: 0,
            },
            params,
            clock,
            next_frame: 0,
            keyframe: true,
            #[cfg(windows)]
            zero_copy_reported: false,
        })
    }

    /// The FFmpeg encoder in use.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn raw_frame(&self, frame: &VideoFrame) -> Result<frames_core::media::RawFrame, MediaError> {
        let (width, height) = frame.surface.size();
        let mut raw = frames_core::media::RawFrame {
            width,
            height,
            bgra: bytes::Bytes::new(),
            captured_at: std::time::Instant::now(),
            frame_number: frame.id,
            media_generation: 0,
            #[cfg(windows)]
            gpu: None,
        };
        match &frame.surface {
            Surface::Cpu(image) => {
                if image.format != PixelFormat::Bgra8 || image.stride != width * 4 {
                    return Err(MediaError::Unsupported("CPU frames must be packed BGRA"));
                }
                raw.bgra = image.data.clone();
            }
            #[cfg(windows)]
            Surface::Platform(surface) => {
                let surface = surface
                    .as_any()
                    .downcast_ref::<crate::d3d11::D3D11Surface>()
                    .ok_or(MediaError::Unsupported("platform surface is not D3D11"))?;
                raw.gpu = Some(Arc::new(frames_core::media::WindowsFrame {
                    texture: surface.texture.clone(),
                    subresource: surface.subresource,
                    matrix_709: surface.matrix_709,
                    full_range: surface.full_range,
                    lease: None,
                }));
            }
            #[cfg(not(windows))]
            Surface::Platform(_) => {
                return Err(MediaError::Unsupported("platform surfaces need Windows"));
            }
        }
        Ok(raw)
    }
}

impl VideoEncoder for FfmpegEncoder {
    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }

    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
        match control {
            EncoderControl::Bitrate(bps) => {
                self.params.bitrate_bps = bps.max(100_000);
                apply_shape(&self.params);
                let update = self
                    .context
                    .set_rate(self.params.bitrate_bps, self.params.fps)
                    .map_err(backend_error)?;
                if matches!(update, frames_core::codec::RateUpdate::RestartRequired) {
                    *self = Self::new(
                        &self.name.clone(),
                        self.caps.codec,
                        self.params,
                        self.clock.clone(),
                    )?;
                }
            }
            EncoderControl::ForceKeyframe => self.keyframe = true,
            EncoderControl::Reconfigure(params) => {
                *self = Self::new(
                    &self.name.clone(),
                    self.caps.codec,
                    params,
                    self.clock.clone(),
                )?;
            }
            // Only encoders that advertise long-term references receive it.
            EncoderControl::ConfirmLongTerm(_) => {}
            EncoderControl::Invalidate { .. }
            | EncoderControl::Refresh
            | EncoderControl::MarkLongTerm(_)
            | EncoderControl::UseLongTerm(_) => {
                return Err(MediaError::Unsupported(
                    "FFmpeg encoders recover with keyframes",
                ));
            }
        }
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        let raw = self.raw_frame(frame)?;
        if std::mem::take(&mut self.keyframe) {
            self.context.request_keyframe();
        }
        // The D3D11 encoder opens on the first GPU frame, on this thread.
        apply_shape(&self.params);
        let packets = self.context.encode(&raw).map_err(backend_error)?;
        #[cfg(windows)]
        if !self.zero_copy_reported && raw.gpu.is_some() {
            self.zero_copy_reported = true;
            if self.context.gpu_encoder_active() {
                self.caps.features = self.caps.features.with(EncoderFeatures::ZERO_COPY);
            }
        }
        for packet in packets {
            let Some((&flags, data)) = packet.split_first() else {
                continue;
            };
            let number = self.next_frame;
            self.next_frame += 1;
            let keyframe = flags & 1 == 1;
            out(Chunk {
                frame: number,
                slice: 0,
                slices: 1,
                kind: if keyframe {
                    FrameKind::Keyframe
                } else {
                    FrameKind::Predicted
                },
                reference: if keyframe || number == 0 {
                    FrameReference::None
                } else {
                    FrameReference::Frame(number - 1)
                },
                mark_long_term: None,
                data,
                captured_us: frame.captured_us,
                ready_us: self.clock.now_us(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod encoder_tests {
    use super::*;

    #[test]
    fn backend_names_map_to_ffmpeg_encoders() {
        assert_eq!(
            encoder_name(Codec::Hevc, "amf").as_deref(),
            Some("hevc_amf")
        );
        assert_eq!(
            encoder_name(Codec::H264, "qsv").as_deref(),
            Some("h264_qsv")
        );
        assert_eq!(
            encoder_name(Codec::Av1, "software").as_deref(),
            Some("libaom-av1")
        );
        assert_eq!(encoder_name(Codec::H264, "software"), None);
        assert_eq!(encoder_name(Codec::H264, "vaapi"), None);
    }
}
