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
use std::sync::{Arc, mpsc};

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
    /// A bitrate this encoder can only take by restarting (Media Foundation,
    /// QSV), waiting for [`restart_due`].
    restart_bps: Option<u32>,
    restarted_us: Option<u64>,
    /// The replacement opening on another thread: a restart takes QSV
    /// 0.25-1.1 s and Media Foundation 0.1 s (Iris Xe, RTX 4050 laptop), which
    /// would freeze the stream; this encoder keeps encoding meanwhile.
    replacement: Option<mpsc::Receiver<Result<FfmpegEncoder, MediaError>>>,
    /// Frames in a row QSV skipped (see `encode`).
    skipped: u32,
    #[cfg(windows)]
    zero_copy_reported: bool,
}

/// Whether an encoder that restarts (with a keyframe) for every bitrate
/// change should restart now, `since_us` after its last restart: soon to fall
/// to a rate the path needs, rarely and only for a large step to climb.
fn restart_due(running_bps: u32, wanted_bps: u32, since_us: Option<u64>) -> bool {
    let since = since_us.unwrap_or(u64::MAX);
    let (running, wanted) = (u64::from(running_bps), u64::from(wanted_bps));
    if wanted < running {
        wanted * 10 <= running * 9 && since >= 1_000_000
    } else {
        wanted * 4 >= running * 5 && since >= 5_000_000
    }
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
            restart_bps: None,
            restarted_us: None,
            replacement: None,
            skipped: 0,
            #[cfg(windows)]
            zero_copy_reported: false,
        })
    }

    /// A new encoder with `params` that carries on this one's frame numbers.
    fn reopen(&mut self, params: EncodeParams) -> Result<(), MediaError> {
        let next_frame = self.next_frame;
        *self = Self::new(
            &self.name.clone(),
            self.caps.codec,
            params,
            self.clock.clone(),
        )?;
        self.next_frame = next_frame;
        self.restarted_us = Some(self.clock.now_us());
        Ok(())
    }

    /// Opens an encoder at `bps` on another thread, warmed up with `frame`
    /// (the D3D11 encoder opens on its first frame).
    fn start_replacement(&mut self, bps: u32, frame: &VideoFrame) {
        let (name, codec, clock, frame) = (
            self.name.clone(),
            self.caps.codec,
            self.clock.clone(),
            frame.clone(),
        );
        let params = EncodeParams {
            bitrate_bps: bps,
            ..self.params
        };
        let (done, replacement) = mpsc::channel();
        self.replacement = Some(replacement);
        std::thread::spawn(move || {
            let opened = Self::new(&name, codec, params, clock).and_then(|mut encoder| {
                encoder.encode(&frame, &mut |_| {})?;
                Ok(encoder)
            });
            let _ = done.send(opened);
        });
    }

    /// Switches to the replacement once it is open, at a keyframe (it never
    /// sent the frames its references point to).
    fn take_replacement(&mut self) {
        let Some(replacement) = &self.replacement else {
            return;
        };
        match replacement.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Ok(Ok(mut encoder)) => {
                encoder.next_frame = self.next_frame;
                encoder.keyframe = true;
                encoder.restart_bps = self
                    .restart_bps
                    .filter(|&bps| bps != encoder.params.bitrate_bps);
                encoder.restarted_us = Some(self.clock.now_us());
                *self = encoder;
            }
            // Keep this one; the next due restart tries again.
            Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                self.replacement = None;
                self.restarted_us = Some(self.clock.now_us());
            }
        }
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
                let bps = bps.max(100_000);
                apply_shape(&self.params);
                // QSV fails its reset at the first rate change (Iris Xe,
                // driver of 2026-11), so it restarts like Media Foundation.
                let update = if self.name.ends_with("_qsv") {
                    frames_core::codec::RateUpdate::RestartRequired
                } else {
                    self.context
                        .set_rate(bps, self.params.fps)
                        .map_err(backend_error)?
                };
                if matches!(update, frames_core::codec::RateUpdate::RestartRequired) {
                    self.restart_bps = Some(bps);
                } else {
                    self.params.bitrate_bps = bps;
                    self.restart_bps = None;
                }
            }
            EncoderControl::ForceKeyframe => self.keyframe = true,
            EncoderControl::Reconfigure(params) => self.reopen(params)?,
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
        self.take_replacement();
        if let (Some(bps), None) = (self.restart_bps, &self.replacement) {
            let since = self
                .restarted_us
                .map(|at| self.clock.now_us().saturating_sub(at));
            if restart_due(self.params.bitrate_bps, bps, since) {
                self.start_replacement(bps, frame);
            }
        }
        let raw = self.raw_frame(frame)?;
        if std::mem::take(&mut self.keyframe) {
            self.context.request_keyframe();
        }
        // The D3D11 encoder opens on the first GPU frame, on this thread.
        apply_shape(&self.params);
        let packets = match self.context.encode(&raw) {
            Ok(packets) => packets,
            // QSV reports a frame its rate control skipped (seen right after
            // a bitrate drop) as invalid data, "Invalid FrameType:0"; the
            // next frame still encodes. Many in a row is a broken encoder.
            Err(error)
                if self.name.ends_with("_qsv")
                    && self.skipped < 30
                    && error.to_string().contains("Invalid data") =>
            {
                self.skipped += 1;
                return Ok(());
            }
            Err(error) => return Err(backend_error(error)),
        };
        self.skipped = 0;
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
    fn restarting_encoders_fall_soon_and_climb_rarely() {
        let second = Some(1_000_000);
        // Falling: at most once a second, and not for a few percent.
        assert!(restart_due(10_000_000, 8_000_000, None));
        assert!(restart_due(10_000_000, 8_000_000, second));
        assert!(!restart_due(10_000_000, 8_000_000, Some(500_000)));
        assert!(!restart_due(10_000_000, 9_500_000, second));
        // Climbing: only by a quarter or more, every five seconds.
        assert!(!restart_due(10_000_000, 12_000_000, Some(10_000_000)));
        assert!(!restart_due(10_000_000, 13_000_000, Some(4_000_000)));
        assert!(restart_due(10_000_000, 13_000_000, Some(5_000_000)));
    }

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
