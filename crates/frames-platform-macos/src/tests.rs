use crate::decoder::MacDecoder;
use crate::encoder::MacEncoder;
use crate::pixel::PixelBuffer;
use crate::presenter::MacPresenter;
use crate::{ffi, system};
use bytes::Bytes;
use frames_media::api::{
    AccessUnit, Codec, Decoded, EncodeParams, EncoderControl, FrameKind, FrameReference, Gop,
    PresentMode, Presenter, VideoDecoder, VideoEncoder, VideoFrame,
};
use std::time::Duration;

const W: u32 = 1280;
const H: u32 = 720;

fn params() -> EncodeParams {
    EncodeParams {
        width: W,
        height: H,
        fps: 60,
        bitrate_bps: 8_000_000,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: None,
        slices: 1,
    }
}

fn frame(index: u64) -> (VideoFrame, PixelBuffer) {
    let buffer = PixelBuffer::synthetic(W, H, index).expect("synthetic frame");
    let frame = VideoFrame {
        id: index,
        captured_us: 1_000 + index * 16_667,
        surface: buffer.clone().into_surface(),
        dirty: None,
    };
    (frame, buffer)
}

fn encode(encoder: &mut MacEncoder, index: u64) -> (AccessUnit, PixelBuffer) {
    let (frame, source) = frame(index);
    let mut unit = None;
    encoder
        .encode(&frame, &mut |chunk| {
            unit = Some(AccessUnit {
                frame: chunk.frame,
                kind: chunk.kind,
                reference: chunk.reference,
                mark_long_term: chunk.mark_long_term,
                captured_us: chunk.captured_us,
                data: Bytes::copy_from_slice(chunk.data),
            });
        })
        .expect("encode");
    (unit.expect("one access unit per frame"), source)
}

fn decode(decoder: &mut MacDecoder, unit: AccessUnit, source: &PixelBuffer) -> f64 {
    let Decoded::Frame(decoded) = decoder.decode(unit).expect("decode") else {
        panic!("expected a decoded frame");
    };
    PixelBuffer::from_surface(&decoded.surface)
        .unwrap()
        .difference(source)
        .unwrap()
}

#[test]
fn abi_version_matches() {
    assert_eq!(system::abi_version(), ffi::ABI_VERSION);
    assert!(system::abi_compatible());
}

#[test]
fn round_trip_through_the_traits_with_one_keyframe() {
    for codec in [Codec::H264, Codec::Hevc] {
        let mut encoder = MacEncoder::new(codec, params()).unwrap();
        let mut decoder = MacDecoder::new(codec).unwrap();
        let mut keyframes = 0;
        for index in 0..60 {
            let (unit, source) = encode(&mut encoder, index);
            assert_eq!(unit.frame, index);
            if unit.kind == FrameKind::Keyframe {
                keyframes += 1;
            } else {
                assert_eq!(unit.reference, FrameReference::Frame(index - 1));
            }
            let error = decode(&mut decoder, unit, &source);
            assert!(error < 12.0, "{codec:?} frame {index}: {error}");
        }
        assert_eq!(keyframes, 1, "{codec:?}");
        encoder.control(EncoderControl::Bitrate(4_000_000)).unwrap();
        assert!(encoder.control(EncoderControl::Refresh).is_err());
    }
}

#[test]
fn confirmed_long_term_slot_recovers_without_a_keyframe() {
    for codec in [Codec::H264, Codec::Hevc] {
        let mut encoder = MacEncoder::new(codec, params()).unwrap();
        let mut decoder = MacDecoder::new(codec).unwrap();
        let mut marked = None;
        for index in 0..40 {
            // As the engine does: mark right after the keyframe.
            if index == 1 {
                encoder.control(EncoderControl::MarkLongTerm(0)).unwrap();
            }
            let (unit, source) = encode(&mut encoder, index);
            if unit.mark_long_term == Some(0) {
                marked = Some(unit.frame);
            }
            // The receiver confirms the marked frame a few frames later.
            if marked.is_some_and(|m| index == m + 3) {
                encoder.control(EncoderControl::ConfirmLongTerm(0)).unwrap();
            }
            decode(&mut decoder, unit, &source);
        }
        if !encoder
            .caps()
            .features
            .contains(frames_media::api::EncoderFeatures::LONG_TERM_REFERENCES)
        {
            // This VideoToolbox (e.g. a paravirtualized CI Mac) produced no
            // long-term tokens; the encoder stopped advertising them.
            assert!(marked.is_none());
            eprintln!("skipped for {codec:?}: no long-term tokens on this Mac");
            continue;
        }
        let marked = marked.expect("a token arrived after the mark");
        assert!(marked <= 20, "{codec:?}: mark landed on frame {marked}");
        for index in 40..45 {
            let _lost = encode(&mut encoder, index);
        }
        encoder.control(EncoderControl::UseLongTerm(0)).unwrap();
        let (recovery, source) = encode(&mut encoder, 45);
        assert_eq!(recovery.kind, FrameKind::Predicted, "{codec:?}");
        assert_eq!(recovery.reference, FrameReference::LongTerm(0));
        let error = decode(&mut decoder, recovery, &source);
        assert!(error < 12.0, "{codec:?}: recovery frame error {error}");
        for index in 46..50 {
            let (unit, source) = encode(&mut encoder, index);
            assert!(decode(&mut decoder, unit, &source) < 12.0);
        }
    }
}

#[test]
fn an_unconfirmed_slot_falls_back_to_a_keyframe() {
    let mut encoder = MacEncoder::new(Codec::H264, params()).unwrap();
    encoder.control(EncoderControl::MarkLongTerm(1)).unwrap();
    for index in 0..10 {
        encode(&mut encoder, index);
    }
    encoder.control(EncoderControl::UseLongTerm(1)).unwrap();
    let (unit, _) = encode(&mut encoder, 10);
    assert_eq!(unit.kind, FrameKind::Keyframe);
    assert!(encoder.control(EncoderControl::MarkLongTerm(2)).is_err());
}

#[test]
fn offscreen_presentation_returns_a_stamp() {
    let mut presenter = MacPresenter::new(W, H, false).unwrap();
    let (frame, _) = frame(1);
    let stamp = presenter
        .present(
            frames_media::api::DecodedFrame {
                frame: 1,
                captured_us: frame.captured_us,
                surface: frame.surface,
                clean: true,
            },
            PresentMode::Immediate,
        )
        .unwrap();
    assert_eq!(stamp.frame, 1);
    assert!(presenter.caps().refresh_hz >= 30);
}

#[test]
fn real_time_threads_and_video_service_type() {
    std::thread::spawn(|| {
        system::set_realtime_thread(
            Duration::from_micros(16_667),
            Duration::from_millis(4),
            Duration::from_millis(8),
        )
    })
    .join()
    .unwrap()
    .expect("time-constraint policy");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    use std::os::fd::AsRawFd;
    system::set_video_service_type(socket.as_raw_fd()).expect("SO_NET_SERVICE_TYPE");
}
