//! FFmpeg decoding through the v1 runtime in `frames-core` (D3D11VA surfaces
//! on Windows, software elsewhere or as a fallback).
//!
//! Temporary dependency: at the v2 cutover the runtime moves into this crate
//! and `frames-core` goes away (docs/V2_PLAN.md, Phase 3).

use frames_core::codec::runtime::Context;
use frames_media::api::{
    AccessUnit, Codec, CpuImage, Decoded, DecodedFrame, DecoderCaps, FrameKind, MediaError,
    PixelFormat, Surface, VideoDecoder,
};

pub struct FfmpegDecoder {
    context: Context,
    caps: DecoderCaps,
}

// The FFmpeg context is used by one thread at a time; the decoder is moved,
// never shared.
unsafe impl Send for FfmpegDecoder {}

fn decoder_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => frames_core::codec::Codec::H264.decoder(),
        Codec::Hevc => frames_core::codec::Codec::Hevc.decoder(),
        Codec::Av1 => frames_core::codec::Codec::Av1.decoder(),
    }
}

impl FfmpegDecoder {
    /// `hardware` asks for D3D11VA (Windows) with GPU surfaces as output.
    pub fn new(codec: Codec, width: u32, height: u32, hardware: bool) -> Result<Self, MediaError> {
        let context = Context::decoder(decoder_name(codec), width, height, hardware)
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
