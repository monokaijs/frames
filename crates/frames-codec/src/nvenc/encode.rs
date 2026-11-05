//! `VideoEncoder` for [`NvencEncoder`]: per-picture control and sub-frame
//! readback.

use super::consts::*;
use super::sys::*;
use super::{DPB_FRAMES, EncodeTiming, NvencEncoder, POLL_SLEEP, READ_TIMEOUT_US, nv_error};
use crate::d3d11::D3D11Surface;
use frames_media::api::{
    Chunk, Codec, EncodeParams, EncoderCaps, EncoderControl, EncoderFeatures, FrameKind,
    FrameReference, MediaError, Surface, VideoEncoder, VideoFrame,
};
use windows::core::Interface;

impl VideoEncoder for NvencEncoder {
    fn caps(&self) -> &EncoderCaps {
        &self.caps
    }

    fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
        match control {
            EncoderControl::Bitrate(bps) => self.pending.bitrate = Some(bps.max(100_000)),
            EncoderControl::ForceKeyframe => self.pending.keyframe = true,
            EncoderControl::Invalidate { first, last } => {
                if first > last {
                    return Err(MediaError::Unsupported("empty invalidation range"));
                }
                let range = match self.pending.invalidate {
                    Some((a, b)) => (a.min(first), b.max(last)),
                    None => (first, last),
                };
                self.pending.invalidate = Some(range);
            }
            EncoderControl::Refresh => {
                if !self.caps.features.contains(EncoderFeatures::INTRA_REFRESH)
                    || self.params.intra_refresh_frames.is_none()
                {
                    return Err(MediaError::Unsupported("intra refresh is not configured"));
                }
                self.pending.refresh = true;
            }
            EncoderControl::MarkLongTerm(_) | EncoderControl::UseLongTerm(_) => {
                return Err(MediaError::Unsupported("long-term references"));
            }
            // Only encoders that advertise long-term references receive it.
            EncoderControl::ConfirmLongTerm(_) => {}
            EncoderControl::Reconfigure(params) => self.pending.reconfigure = Some(params),
        }
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &VideoFrame,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        let Surface::Platform(surface) = &frame.surface else {
            return Err(MediaError::Unsupported("NVENC encodes D3D11 textures"));
        };
        let surface = surface
            .as_any()
            .downcast_ref::<D3D11Surface>()
            .ok_or(MediaError::Unsupported("NVENC encodes D3D11 textures"))?;
        let same_device = surface
            .device()
            .is_ok_and(|device| device.as_raw() == self.device.as_raw());
        if !same_device {
            return Err(MediaError::Unsupported(
                "texture is not on the encoder's device",
            ));
        }
        if let Some(params) = self.pending.reconfigure.take() {
            self.apply(params)?;
        }
        if let Some(bps) = self.pending.bitrate.take()
            && bps != self.params.bitrate_bps
        {
            self.reconfigure_rate(bps)?;
        }

        let number = self.next_frame;
        let mut reference = self.history.back().copied().map(FrameReference::Frame);
        if let Some((first, last)) = self.pending.invalidate.take() {
            for lost in first..=last {
                if self.history.contains(&lost) {
                    nv!(
                        self.api,
                        self.encoder,
                        nvEncInvalidateRefFrames(self.encoder, lost)
                    )?;
                }
            }
            self.history.retain(|f| *f < first || *f > last);
            match self.history.iter().rev().find(|f| **f < first) {
                Some(&good) => reference = Some(FrameReference::Frame(good)),
                // Every frame the encoder could predict from is lost. With a
                // one-frame VBV an IDR costs no more bytes than a P frame.
                None => self.pending.keyframe = true,
            }
        }
        let keyframe = std::mem::take(&mut self.pending.keyframe) || reference.is_none();
        let mut pic: NV_ENC_PIC_PARAMS = unsafe { std::mem::zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        if keyframe {
            pic.encodePicFlags =
                NV_ENC_PIC_FLAG_FORCEIDR as u32 | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
            self.refresh_left = None;
            self.pending.refresh = false;
        } else if std::mem::take(&mut self.pending.refresh) {
            let frames = self.params.intra_refresh_frames.unwrap_or(2).max(2);
            // Writing a union field is safe; NVENC reads the codec's member.
            match self.caps.codec {
                Codec::H264 => {
                    pic.codecPicParams
                        .h264PicParams
                        .forceIntraRefreshWithFrameCnt = u32::from(frames)
                }
                Codec::Hevc => {
                    pic.codecPicParams
                        .hevcPicParams
                        .forceIntraRefreshWithFrameCnt = u32::from(frames)
                }
                Codec::Av1 => {}
            }
            self.refresh_left = Some(frames);
        }
        let kind = if keyframe {
            FrameKind::Keyframe
        } else if let Some(left) = self.refresh_left {
            self.refresh_left = (left > 1).then(|| left - 1);
            FrameKind::Refresh {
                complete: left == 1,
            }
        } else {
            FrameKind::Predicted
        };
        let reference = if keyframe {
            FrameReference::None
        } else {
            reference.unwrap_or(FrameReference::None)
        };

        let registered = self.register(&surface.texture, surface.width, surface.height)?;
        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { std::mem::zeroed() };
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = registered;
        nv!(
            self.api,
            self.encoder,
            nvEncMapInputResource(self.encoder, &mut map)
        )?;
        pic.inputWidth = surface.width;
        pic.inputHeight = surface.height;
        pic.inputBuffer = map.mappedResource;
        pic.bufferFmt = map.mappedBufferFmt;
        pic.outputBitstream = self.bitstream;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        pic.inputTimeStamp = number;
        pic.frameIdx = number as u32;
        let submit_us = self.clock.now_us();
        let picture = Picture {
            number,
            kind,
            reference,
            captured_us: frame.captured_us,
            submit_us,
        };
        let result = self.read(&mut pic, picture, out);
        let _ = nv!(
            self.api,
            self.encoder,
            nvEncUnmapInputResource(self.encoder, map.mappedResource)
        );
        result?;

        self.next_frame += 1;
        if keyframe {
            self.history.clear();
        }
        self.history.push_back(number);
        while self.history.len() > DPB_FRAMES {
            self.history.pop_front();
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Picture {
    number: u64,
    kind: FrameKind,
    reference: FrameReference,
    captured_us: u64,
    submit_us: u64,
}

impl NvencEncoder {
    fn apply(&mut self, params: EncodeParams) -> Result<(), MediaError> {
        let resize = (params.width, params.height, params.fps)
            != (self.params.width, self.params.height, self.params.fps);
        self.params = params;
        if !resize {
            return self.reconfigure_rate(params.bitrate_bps);
        }
        self.unregister_all();
        self.init.encodeWidth = params.width;
        self.init.darWidth = params.width;
        self.init.encodeHeight = params.height;
        self.init.darHeight = params.height;
        self.init.frameRateNum = params.fps.max(1);
        self.rate(params.bitrate_bps);
        let mut reconfigure: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        reconfigure.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconfigure.reInitEncodeParams = unsafe { std::ptr::read(&*self.init) };
        reconfigure.reInitEncodeParams.encodeConfig = &mut *self.config;
        reconfigure.set_resetEncoder(1);
        reconfigure.set_forceIDR(1);
        nv!(
            self.api,
            self.encoder,
            nvEncReconfigureEncoder(self.encoder, &mut reconfigure)
        )?;
        self.pending.keyframe = true;
        Ok(())
    }

    fn read(
        &mut self,
        pic: &mut NV_ENC_PIC_PARAMS,
        picture: Picture,
        out: &mut dyn FnMut(Chunk<'_>),
    ) -> Result<(), MediaError> {
        nv!(
            self.api,
            self.encoder,
            nvEncEncodePicture(self.encoder, pic)
        )?;
        let slices = if self.subframe {
            self.params.slices.clamp(1, self.caps.max_slices)
        } else {
            1
        };
        let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { std::mem::zeroed() };
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        lock.sliceOffsets = self.offsets.as_mut_ptr();
        lock.set_doNotWait(u32::from(self.subframe));
        let mut emitted = 0u8;
        let mut position = 0usize;
        let mut timing = EncodeTiming {
            submit_us: picture.submit_us,
            ..EncodeTiming::default()
        };
        let clock = self.clock.clone();
        loop {
            let lock_bitstream = self.api.list.nvEncLockBitstream.expect("present");
            let status = unsafe { lock_bitstream(self.encoder, &mut lock) };
            timing.polls += 1;
            if status == NV_ENC_ERR_LOCK_BUSY {
                if clock.now_us().saturating_sub(picture.submit_us) > READ_TIMEOUT_US {
                    return Err(MediaError::Backend(
                        "NVENC did not finish the picture within 2 s".into(),
                    ));
                }
                std::thread::sleep(POLL_SLEEP);
                continue;
            }
            if status != NV_ENC_SUCCESS {
                return Err(nv_error(
                    self.api,
                    self.encoder,
                    "nvEncLockBitstream",
                    status,
                ));
            }
            let done = !self.subframe || lock.hwEncodeStatus == 2;
            let size = lock.bitstreamSizeInBytes as usize;
            if picture.number == 100 && std::env::var_os("FRAMES_NVENC_TRACE").is_some() {
                eprintln!(
                    "NVENC trace frame 100 poll {} t={}us status={} slices={} size={} offsets={:?}",
                    timing.polls,
                    clock.now_us().saturating_sub(picture.submit_us),
                    lock.hwEncodeStatus,
                    lock.numSlices,
                    size,
                    &self.offsets[..(lock.numSlices as usize).min(8)]
                );
            }
            let data = if size == 0 {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, size) }
            };
            let mut emit = |start: usize, end: usize, slice: u8, timing: &mut EncodeTiming| {
                let now = clock.now_us();
                if slice == 0 {
                    timing.first_slice_us = now;
                }
                out(Chunk {
                    frame: picture.number,
                    slice,
                    slices,
                    kind: picture.kind,
                    reference: picture.reference,
                    mark_long_term: None,
                    data: &data[start..end],
                    captured_us: picture.captured_us,
                    ready_us: now,
                });
            };
            // While encoding, NVENC reports `numSlices = 0` and fills slice
            // offsets only at completion, but `bitstreamSizeInBytes` grows
            // slice by slice. Send what is written as soon as it is: wire
            // chunks need not align with NAL slices, since the receiver
            // reassembles the whole access unit. Enough bytes are held back
            // that every remaining chunk can be non-empty.
            if !done && usize::from(emitted) + 1 < usize::from(slices) {
                let still_needed = usize::from(slices - emitted - 1);
                let end = size.saturating_sub(still_needed);
                if end > position {
                    emit(position, end, emitted, &mut timing);
                    position = end;
                    emitted += 1;
                }
            }
            if done {
                // The rest is split so exactly `slices` chunks go out, each
                // non-empty: the receiver waits for the announced count.
                let left = usize::from(slices - emitted);
                let remaining = size.saturating_sub(position);
                let picture_type = lock.pictureType;
                let average_qp = lock.frameAvgQP;
                if remaining < left {
                    let _ = nv!(
                        self.api,
                        self.encoder,
                        nvEncUnlockBitstream(self.encoder, self.bitstream)
                    );
                    return Err(MediaError::Backend(
                        "encoded picture smaller than its slice count".into(),
                    ));
                }
                let step = remaining.div_ceil(left);
                for i in 0..left {
                    let end = if i + 1 == left {
                        size
                    } else {
                        (position + step).min(size)
                    };
                    emit(position, end, emitted, &mut timing);
                    position = end;
                    emitted += 1;
                }
                timing.done_us = clock.now_us();
                timing.bytes = size as u32;
                timing.average_qp = average_qp;
                nv!(
                    self.api,
                    self.encoder,
                    nvEncUnlockBitstream(self.encoder, self.bitstream)
                )?;
                let expected_idr = picture.kind == FrameKind::Keyframe;
                let got_idr = picture_type == NV_ENC_PIC_TYPE_IDR;
                if expected_idr != got_idr {
                    return Err(MediaError::Backend(format!(
                        "NVENC produced picture type {picture_type} for frame {}, expected {:?}",
                        picture.number, picture.kind
                    )));
                }
                self.timing = timing;
                return Ok(());
            }
            nv!(
                self.api,
                self.encoder,
                nvEncUnlockBitstream(self.encoder, self.bitstream)
            )?;
            std::thread::sleep(POLL_SLEEP);
        }
    }
}
