//! Hardware VideoToolbox decoding as a [`VideoDecoder`].

use crate::ffi;
use crate::pixel::PixelBuffer;
use frames_media::api::{
    AccessUnit, Codec, Decoded, DecodedFrame, DecoderCaps, FrameKind, MediaError, VideoDecoder,
};
use std::ffi::c_void;

pub struct MacDecoder {
    handle: *mut c_void,
    codec: Codec,
    last_good: Option<u64>,
}

// SAFETY: the Swift decoder is used from one thread at a time (`&mut self`)
// and freed once.
unsafe impl Send for MacDecoder {}

impl MacDecoder {
    pub fn new(codec: Codec) -> Result<Self, MediaError> {
        let id = match codec {
            Codec::H264 => ffi::CODEC_H264,
            Codec::Hevc => ffi::CODEC_HEVC,
            Codec::Av1 => return Err(MediaError::Unsupported("AV1 decoding on macOS")),
        };
        // SAFETY: returns a retained Swift object.
        let handle = unsafe { ffi::frames_mac_decoder_new(id) };
        if handle.is_null() {
            return Err(MediaError::Backend(
                "VideoToolbox decoder unavailable".into(),
            ));
        }
        Ok(Self {
            handle,
            codec,
            last_good: None,
        })
    }
}

impl VideoDecoder for MacDecoder {
    fn caps(&self) -> DecoderCaps {
        DecoderCaps {
            codec: self.codec,
            max_width: 8192,
            max_height: 4320,
            hardware: true,
        }
    }

    fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError> {
        let mut output = std::ptr::null_mut();
        // SAFETY: `unit.data` is valid for the call; on success `output`
        // receives a +1 buffer.
        let status = unsafe {
            ffi::frames_mac_decoder_decode(
                self.handle,
                unit.data.as_ptr(),
                unit.data.len(),
                unit.kind == FrameKind::Keyframe,
                &mut output,
            )
        };
        match status {
            ffi::DECODE_OK => {
                let buffer = unsafe { PixelBuffer::from_retained(output) }
                    .ok_or_else(|| MediaError::Backend("decoder returned no surface".into()))?;
                self.last_good = Some(unit.frame);
                Ok(Decoded::Frame(DecodedFrame {
                    frame: unit.frame,
                    captured_us: unit.captured_us,
                    surface: buffer.into_surface(),
                    clean: true,
                }))
            }
            // No session yet: only a keyframe can start one.
            ffi::DECODE_NEED_KEYFRAME => Ok(Decoded::BrokenReference {
                last_good: self.last_good,
            }),
            ffi::DECODE_TIMEOUT => Err(MediaError::Backend(
                "hardware decoder missed its deadline".into(),
            )),
            other => Err(MediaError::Backend(format!(
                "hardware decode failed ({other})"
            ))),
        }
    }
}

impl Drop for MacDecoder {
    fn drop(&mut self) {
        // SAFETY: frees the Swift decoder exactly once.
        unsafe { ffi::frames_mac_decoder_free(self.handle) };
    }
}
