use super::*;
use crate::capture::SyntheticCapture;
use frames_media::api::{CaptureEvent, CaptureSource};

fn params(slices: u8, gop: Gop) -> EncodeParams {
    EncodeParams {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: 20_000_000,
        vbv_frames: 1.0,
        gop,
        intra_refresh_frames: Some(4),
        slices,
    }
}

struct Rig {
    clock: VirtualClock,
    capture: SyntheticCapture,
    encoder: SyntheticEncoder,
    decoder: SyntheticDecoder,
}

impl Rig {
    fn new(slices: u8, gop: Gop) -> Self {
        let clock = VirtualClock::new(0);
        Self {
            capture: SyntheticCapture::new(&clock, 1920, 1080, 60),
            encoder: SyntheticEncoder::new(&clock, params(slices, gop), EncoderModel::default()),
            decoder: SyntheticDecoder::new(),
            clock,
        }
    }

    /// Captures and encodes one frame; returns the access unit and the
    /// per-slice ready times.
    fn encode(&mut self) -> (AccessUnit, Vec<u64>) {
        self.clock.advance_to(self.capture.next_due_us());
        let CaptureEvent::Frame(frame) = self.capture.next(u64::MAX).unwrap() else {
            panic!("a frame is due");
        };
        let mut data = BytesMut::new();
        let mut ready = Vec::new();
        let mut meta = None;
        self.encoder
            .encode(&frame, &mut |chunk| {
                assert_eq!(usize::from(chunk.slice), ready.len(), "slices in order");
                data.extend_from_slice(chunk.data);
                ready.push(chunk.ready_us);
                meta = Some((
                    chunk.frame,
                    chunk.kind,
                    chunk.reference,
                    chunk.mark_long_term,
                ));
            })
            .unwrap();
        let (frame_number, kind, reference, mark_long_term) = meta.unwrap();
        (
            AccessUnit {
                frame: frame_number,
                kind,
                reference,
                mark_long_term,
                captured_us: frame.captured_us,
                data: data.freeze(),
            },
            ready,
        )
    }

    fn decode(&mut self, unit: AccessUnit) -> Decoded {
        self.decoder.decode(unit).unwrap()
    }

    fn clean(&mut self) -> bool {
        let (unit, _) = self.encode();
        matches!(self.decode(unit), Decoded::Frame(f) if f.clean)
    }
}

#[test]
fn infinite_gop_sends_one_keyframe_and_periodic_gop_repeats_it() {
    let mut rig = Rig::new(1, Gop::Infinite);
    let kinds: Vec<FrameKind> = (0..120).map(|_| rig.encode().0.kind).collect();
    assert_eq!(kinds[0], FrameKind::Keyframe);
    assert!(kinds[1..].iter().all(|k| *k == FrameKind::Predicted));

    let mut periodic = Rig::new(1, Gop::Frames(30));
    let keyframes = (0..120)
        .filter(|_| periodic.encode().0.kind == FrameKind::Keyframe)
        .count();
    assert_eq!(keyframes, 4);
}

#[test]
fn a_clean_chain_decodes_and_slices_arrive_progressively() {
    let mut rig = Rig::new(4, Gop::Infinite);
    let (unit, ready) = rig.encode();
    assert_eq!(ready.len(), 4);
    assert!(ready.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(
        ready[3] - ready[0],
        3 * 3_000 / 4,
        "slices spread over the encode"
    );
    assert!(matches!(rig.decode(unit), Decoded::Frame(f) if f.clean));
    for _ in 0..30 {
        assert!(rig.clean());
    }
}

#[test]
fn a_lost_frame_breaks_every_frame_that_depends_on_it() {
    let mut rig = Rig::new(2, Gop::Infinite);
    for _ in 0..5 {
        assert!(rig.clean());
    }
    let _lost = rig.encode(); // frame 5 never reaches the decoder
    for _ in 0..3 {
        let (unit, _) = rig.encode();
        assert!(matches!(
            rig.decode(unit),
            Decoded::BrokenReference { last_good: Some(4) }
        ));
    }
}

#[test]
fn invalidation_recovers_with_a_small_predicted_frame() {
    let mut rig = Rig::new(1, Gop::Infinite);
    let (keyframe, _) = rig.encode();
    let keyframe_size = keyframe.data.len();
    assert!(matches!(rig.decode(keyframe), Decoded::Frame(f) if f.clean));
    for _ in 1..5 {
        assert!(rig.clean());
    }
    let _ = rig.encode(); // 5 lost
    let (six, _) = rig.encode();
    assert!(matches!(rig.decode(six), Decoded::BrokenReference { .. }));
    rig.encoder
        .control(EncoderControl::Invalidate { first: 5, last: 6 })
        .unwrap();
    let (recovery, _) = rig.encode();
    assert_eq!(recovery.reference, FrameReference::Frame(4));
    assert_eq!(recovery.kind, FrameKind::Predicted);
    assert!(recovery.data.len() * 4 < keyframe_size);
    assert!(matches!(rig.decode(recovery), Decoded::Frame(f) if f.clean));
    assert!(rig.clean(), "the chain continues from the recovery frame");
}

#[test]
fn invalidation_past_the_reference_buffer_falls_back_to_a_keyframe() {
    let mut rig = Rig::new(1, Gop::Infinite);
    for _ in 0..40 {
        assert!(rig.clean());
    }
    rig.encoder
        .control(EncoderControl::Invalidate { first: 0, last: 39 })
        .unwrap();
    assert_eq!(rig.encode().0.kind, FrameKind::Keyframe);
}

#[test]
fn long_term_references_recover_after_a_long_loss() {
    let mut rig = Rig::new(1, Gop::Infinite);
    assert!(rig.clean());
    rig.encoder
        .control(EncoderControl::MarkLongTerm(3))
        .unwrap();
    let (marked, _) = rig.encode();
    assert_eq!(marked.mark_long_term, Some(3));
    assert!(matches!(rig.decode(marked), Decoded::Frame(f) if f.clean));
    for _ in 0..100 {
        assert!(rig.clean());
    }
    for _ in 0..30 {
        let _ = rig.encode(); // a long outage, far past the 64-frame window
    }
    rig.encoder.control(EncoderControl::UseLongTerm(3)).unwrap();
    let (recovery, _) = rig.encode();
    assert_eq!(recovery.reference, FrameReference::LongTerm(3));
    assert!(matches!(rig.decode(recovery), Decoded::Frame(f) if f.clean));
    assert!(rig.encoder.control(EncoderControl::UseLongTerm(4)).is_err());
}

#[test]
fn intra_refresh_heals_over_the_wave() {
    let mut rig = Rig::new(1, Gop::Infinite);
    for _ in 0..5 {
        assert!(rig.clean());
    }
    let _ = rig.encode(); // 5 lost
    rig.encoder.control(EncoderControl::Refresh).unwrap();
    let mut clean = Vec::new();
    for _ in 0..4 {
        let (unit, _) = rig.encode();
        assert!(matches!(unit.kind, FrameKind::Refresh { .. }));
        let Decoded::Frame(frame) = rig.decode(unit) else {
            panic!("refresh frames decode despite the loss");
        };
        clean.push(frame.clean);
    }
    assert_eq!(clean, [false, false, false, true]);
    assert!(rig.clean());
}

#[test]
fn keyframes_reset_long_term_slots_and_corruption_is_detected() {
    let mut rig = Rig::new(3, Gop::Infinite);
    rig.encoder
        .control(EncoderControl::MarkLongTerm(1))
        .unwrap();
    assert!(rig.clean());
    rig.encoder.control(EncoderControl::ForceKeyframe).unwrap();
    assert!(rig.clean());
    assert!(
        rig.encoder.control(EncoderControl::UseLongTerm(1)).is_err(),
        "the keyframe cleared slot 1"
    );

    let (unit, _) = rig.encode();
    let mut bytes = unit.data.to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let corrupt = AccessUnit {
        data: Bytes::from(bytes),
        ..unit.clone()
    };
    assert!(rig.decoder.decode(corrupt).is_err());
    let truncated = AccessUnit {
        data: unit.data.slice(..unit.data.len() - 1),
        ..unit
    };
    assert!(rig.decoder.decode(truncated).is_err());
}
