//! Save one real window's unencoded pixels for capture/decoder comparisons.
//! Run as a signed Aqua app with existing Screen Recording permission.
#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use frames_media::api::{
        AccessUnit, CaptureConfig, CaptureEvent, CaptureSource, Codec, Decoded, EncodeParams, Gop,
        MediaClock, VideoDecoder, VideoEncoder,
    };
    use frames_media::clock::SystemClock;
    use frames_platform_macos::{
        capture::MacCapture, decoder::MacDecoder, encoder::MacEncoder, pixel::PixelBuffer, system,
    };
    use std::io::Write;
    let args: Vec<_> = std::env::args().collect();
    let id: u32 = args[1].parse().expect("window id");
    let path = args[2].clone();
    let immediate = args.iter().any(|arg| arg == "--immediate");
    let worker = std::thread::spawn(move || {
        let clock = SystemClock::new();
        let mut capture = MacCapture::window(id, 30).expect("capture");
        capture
            .configure(CaptureConfig {
                fps: 30,
                separate_cursor: true,
            })
            .unwrap();
        capture.set_options(false, false);
        let stop = clock.now_us() + 5_000_000;
        let ready = clock.now_us() + if immediate { 0 } else { 500_000 };
        let mut saved = false;
        while clock.now_us() < stop {
            if let Ok(CaptureEvent::Frame(frame)) = capture.next(clock.now_us() + 50_000)
                && clock.now_us() >= ready
            {
                let (bytes, width, height) = PixelBuffer::from_surface(&frame.surface)
                    .unwrap()
                    .bgra()
                    .unwrap();
                let mut out = std::fs::File::create(&path).unwrap();
                write!(out, "P6\n{width} {height}\n255\n").unwrap();
                let rgb: Vec<u8> = bytes
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0]])
                    .collect();
                out.write_all(&rgb).unwrap();
                println!("saved {width}x{height} to {path}");
                let codec = Codec::Hevc;
                let mut encoder = MacEncoder::new(
                    codec,
                    EncodeParams {
                        width,
                        height,
                        fps: 30,
                        bitrate_bps: 20_000_000,
                        vbv_frames: 1.0,
                        gop: Gop::Infinite,
                        intra_refresh_frames: None,
                        slices: 1,
                    },
                )
                .unwrap();
                let mut decoder = MacDecoder::new(codec).unwrap();
                encoder
                    .encode(&frame, &mut |chunk| {
                        std::fs::write(format!("{path}.hevc"), chunk.data).unwrap();
                        let decoded = decoder
                            .decode(AccessUnit {
                                frame: chunk.frame,
                                kind: chunk.kind,
                                reference: chunk.reference,
                                mark_long_term: chunk.mark_long_term,
                                captured_us: chunk.captured_us,
                                data: bytes::Bytes::copy_from_slice(chunk.data),
                            })
                            .unwrap();
                        if let Decoded::Frame(decoded) = decoded {
                            let decoded = PixelBuffer::from_surface(&decoded.surface).unwrap();
                            let (bytes, w, h) = decoded.bgra().unwrap();
                            let mut out =
                                std::fs::File::create(format!("{path}.decoded.ppm")).unwrap();
                            write!(out, "P6\n{w} {h}\n255\n").unwrap();
                            let rgb: Vec<u8> = bytes
                                .chunks_exact(4)
                                .flat_map(|p| [p[2], p[1], p[0]])
                                .collect();
                            out.write_all(&rgb).unwrap();
                            println!(
                                "decoded {w}x{h}, mean difference {:?}",
                                PixelBuffer::from_surface(&frame.surface)
                                    .unwrap()
                                    .difference(decoded)
                            );
                        }
                    })
                    .unwrap();
                saved = true;
                break;
            }
        }
        if !saved {
            eprintln!("no capture pixels before timeout");
        }
        system::stop_app();
    });
    system::run_app(8.0);
    worker.join().unwrap();
}
