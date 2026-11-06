//! Present one HEVC picture without networking; inspect source pixels and Metal.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use frames_media::api::{
        AccessUnit, Codec, Decoded, FrameKind, FrameReference, PresentMode, Presenter, VideoDecoder,
    };
    use frames_platform_macos::{
        decoder::MacDecoder, pixel::PixelBuffer, presenter::MacPresenter, system,
    };
    use std::time::{Duration, Instant};
    let path = std::env::args().nth(1).expect("HEVC file");
    let worker = std::thread::spawn(move || {
        let mut decoder = MacDecoder::new(Codec::Hevc).unwrap();
        let Decoded::Frame(frame) = decoder
            .decode(AccessUnit {
                frame: 0,
                kind: FrameKind::Keyframe,
                reference: FrameReference::None,
                mark_long_term: None,
                captured_us: 0,
                data: std::fs::read(path).unwrap().into(),
            })
            .unwrap()
        else {
            panic!("no decoded frame")
        };
        let (pixels, w, h) = PixelBuffer::from_surface(&frame.surface)
            .unwrap()
            .bgra()
            .unwrap();
        let alpha = pixels
            .chunks_exact(4)
            .map(|p| p[3])
            .fold((255, 0), |(lo, hi), a| (lo.min(a), hi.max(a)));
        println!(
            "decoded {w}x{h}; alpha min/max={alpha:?}; center BGRA={:?}",
            &pixels[((h / 2 * w + w / 2) * 4) as usize..][..4]
        );
        let mut presenter = MacPresenter::new(w, h, true).unwrap();
        presenter.set_title("Frames Metal Pixel Probe");
        let end = Instant::now() + Duration::from_secs(90);
        while Instant::now() < end {
            presenter
                .present(frame.clone(), PresentMode::Immediate)
                .unwrap();
            std::thread::sleep(Duration::from_millis(16));
        }
        drop(presenter);
        system::stop_app();
    });
    system::run_app(100.0);
    worker.join().unwrap();
}
