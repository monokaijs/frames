//! Real-hardware v2 pipeline on this Mac, end to end:
//! capture (generated 1080p, or ScreenCaptureKit with --capture when this
//! binary has Screen Recording permission) → VideoToolbox encode →
//! frames_media HostSession → simulated LAN link (real clock) →
//! ClientSession → VideoToolbox decode → Metal presentation.
//!
//! Per-stage latency is reported over the run, plus recovery from a scripted
//! outage and a pixel check of decoded generated frames against their source.
//!
//! cargo run --release -p frames-platform-macos --example mac_pipeline -- \
//!     [--seconds 12] [--fps 60] [--codec h264|hevc] [--window] [--vsync] \
//!     [--capture] [--outage-at 6 --outage-ms 300]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_pipeline runs on macOS only");
}

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(target_os = "macos")]
mod mac {
    use frames_media::api::{
        AccessUnit, CaptureEvent, CaptureSource, Chunk, Codec, Decoded, DecoderCaps, EncodeParams,
        EncoderCaps, EncoderControl, Gop, MediaClock, MediaError, MediaTransport, PresentMode,
        Presenter, VideoDecoder, VideoEncoder, VideoFrame,
    };
    use frames_media::client::{ClientConfig, ClientSession};
    use frames_media::clock::SystemClock;
    use frames_media::host::{HostConfig, HostSession};
    use frames_media::rate::RateConfig;
    use frames_platform_macos::capture::MacCapture;
    use frames_platform_macos::decoder::MacDecoder;
    use frames_platform_macos::encoder::MacEncoder;
    use frames_platform_macos::pixel::PixelBuffer;
    use frames_platform_macos::presenter::MacPresenter;
    use frames_platform_macos::system;
    use frames_testkit::clock::VirtualClock;
    use frames_testkit::link::LinkConfig;
    use frames_testkit::netsim::percentile;
    use frames_testkit::transport;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    struct Options {
        seconds: f64,
        fps: u32,
        codec: Codec,
        window: bool,
        vsync: bool,
        capture: bool,
        outage_at: Option<f64>,
        outage_ms: u64,
    }

    fn options() -> Options {
        let mut o = Options {
            seconds: 12.0,
            fps: 60,
            codec: Codec::H264,
            window: false,
            vsync: false,
            capture: false,
            outage_at: None,
            outage_ms: 300,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            let value = || args.get(i + 1).cloned().unwrap_or_default();
            match args[i].as_str() {
                "--seconds" => o.seconds = value().parse().unwrap(),
                "--fps" => o.fps = value().parse().unwrap(),
                "--codec" => {
                    o.codec = if value() == "hevc" {
                        Codec::Hevc
                    } else {
                        Codec::H264
                    }
                }
                "--outage-at" => o.outage_at = Some(value().parse().unwrap()),
                "--outage-ms" => o.outage_ms = value().parse().unwrap(),
                "--window" => {
                    o.window = true;
                    i -= 1;
                }
                "--vsync" => {
                    o.vsync = true;
                    i -= 1;
                }
                "--capture" => {
                    o.capture = true;
                    i -= 1;
                }
                other => panic!("unknown option {other}"),
            }
            i += 2;
        }
        o
    }

    /// Per-frame stage timestamps, keyed by capture time.
    #[derive(Clone, Copy, Debug, Default)]
    struct Stages {
        captured: u64,
        encoded: u64,
        decode_start: u64,
        decoded: u64,
        presented: u64,
    }

    type Ledger = Arc<Mutex<HashMap<u64, Stages>>>;

    /// Records when each frame finished encoding.
    struct TimedEncoder {
        inner: MacEncoder,
        ledger: Ledger,
    }

    impl VideoEncoder for TimedEncoder {
        fn caps(&self) -> &EncoderCaps {
            self.inner.caps()
        }
        fn control(&mut self, control: EncoderControl) -> Result<(), MediaError> {
            self.inner.control(control)
        }
        fn encode(
            &mut self,
            frame: &VideoFrame,
            out: &mut dyn FnMut(Chunk<'_>),
        ) -> Result<(), MediaError> {
            let ledger = self.ledger.clone();
            self.inner.encode(frame, &mut |chunk| {
                ledger
                    .lock()
                    .unwrap()
                    .entry(chunk.captured_us)
                    .or_default()
                    .encoded = chunk.ready_us;
                out(chunk);
            })
        }
    }

    /// Records decode start and end.
    struct TimedDecoder {
        inner: MacDecoder,
        ledger: Ledger,
    }

    impl VideoDecoder for TimedDecoder {
        fn caps(&self) -> DecoderCaps {
            self.inner.caps()
        }
        fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError> {
            let start = SystemClock::new().now_us();
            let captured = unit.captured_us;
            let result = self.inner.decode(unit);
            let mut ledger = self.ledger.lock().unwrap();
            let entry = ledger.entry(captured).or_default();
            entry.decode_start = start;
            entry.decoded = SystemClock::new().now_us();
            result
        }
    }

    fn report(name: &str, values: &mut [u64]) {
        values.sort_unstable();
        println!(
            "  {name:<22} p50 {:>6.2} ms  p95 {:>6.2} ms  p99 {:>6.2} ms  (n={})",
            percentile(values, 50) as f64 / 1e3,
            percentile(values, 95) as f64 / 1e3,
            percentile(values, 99) as f64 / 1e3,
            values.len()
        );
    }

    pub fn main() {
        let options = options();
        let done = Arc::new(AtomicBool::new(false));
        let worker = {
            let options = options.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                run(&options);
                done.store(true, Ordering::SeqCst);
                system::stop_app();
            })
        };
        if options.window {
            // The presenter's window needs NSApplication on the main thread.
            system::run_app(options.seconds + 30.0);
        }
        worker.join().unwrap();
    }

    fn run(o: &Options) {
        assert!(system::abi_compatible(), "FramesMac ABI mismatch");
        let (width, height) = (1920u32, 1080u32);
        let clock = VirtualClock::new(SystemClock::new().now_us());
        let sync = |clock: &VirtualClock| {
            clock.advance_to(SystemClock::new().now_us());
            clock.now_us()
        };
        let lan = LinkConfig::lan();
        let (host_end, client_end) = transport::pair(&clock, lan, lan, 1);
        let ledger: Ledger = Arc::default();
        let sources: Arc<Mutex<BTreeMap<u64, PixelBuffer>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let rate = RateConfig {
            min_bps: 2_000_000,
            max_bps: 60_000_000,
            start_bps: 20_000_000,
        };
        let stream_start = SystemClock::new().now_us();

        let host = {
            let (o, clock, ledger, sources, stop) = (
                o.clone(),
                clock.clone(),
                ledger.clone(),
                sources.clone(),
                stop.clone(),
            );
            let host_end = host_end.clone();
            std::thread::spawn(move || {
                let interval = Duration::from_micros(1_000_000 / u64::from(o.fps));
                let _ = system::set_realtime_thread(interval, interval / 3, interval / 2);
                let params = EncodeParams {
                    width,
                    height,
                    fps: o.fps,
                    bitrate_bps: rate.start_bps,
                    vbv_frames: 1.0,
                    gop: Gop::Infinite,
                    intra_refresh_frames: None,
                    slices: 1,
                };
                let mut encoder = TimedEncoder {
                    inner: MacEncoder::new(o.codec, params).expect("encoder"),
                    ledger: ledger.clone(),
                };
                let mut capture = o
                    .capture
                    .then(|| MacCapture::start(o.fps, false).expect("capture"));
                let mut session = HostSession::new(HostConfig::new(1, 1, rate));
                let mut index = 0u64;
                let mut next_frame = SystemClock::new().now_us();
                let mut outage = None;
                while !stop.load(Ordering::SeqCst) {
                    let now = sync(&clock);
                    if let Some(at) = o.outage_at {
                        let start = stream_start + (at * 1e6) as u64;
                        let end = start + o.outage_ms * 1000;
                        let active = (start..end).contains(&now);
                        if outage != Some(active) {
                            host_end.set_send_config(LinkConfig {
                                loss: if active { 1.0 } else { 0.0 },
                                ..lan
                            });
                            outage = Some(active);
                        }
                    }
                    let frame = match &mut capture {
                        Some(capture) => match capture.next(now + 1_000) {
                            Ok(CaptureEvent::Frame(frame)) => Some(frame),
                            _ => None,
                        },
                        None if now >= next_frame => {
                            next_frame += 1_000_000 / u64::from(o.fps);
                            let buffer =
                                PixelBuffer::synthetic(width, height, index).expect("frame");
                            let captured_us = SystemClock::new().now_us();
                            let mut map = sources.lock().unwrap();
                            map.insert(captured_us, buffer.clone());
                            while map.len() > 240 {
                                map.pop_first();
                            }
                            drop(map);
                            index += 1;
                            Some(VideoFrame {
                                id: index,
                                captured_us,
                                surface: buffer.into_surface(),
                                dirty: None,
                            })
                        }
                        None => None,
                    };
                    if let Some(frame) = frame {
                        ledger
                            .lock()
                            .unwrap()
                            .entry(frame.captured_us)
                            .or_default()
                            .captured = frame.captured_us;
                        let now = sync(&clock);
                        session.encode(now, &mut encoder, &frame).expect("encode");
                    }
                    let now = sync(&clock);
                    while let Some(datagram) = session.poll_transmit(now) {
                        let _ = host_end.try_send(datagram);
                    }
                    while let Some(datagram) = host_end.try_recv() {
                        session.on_datagram(now, &datagram, host_end.path().rtt_us);
                    }
                    if capture.is_none() {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
                (session.stats(), capture.and_then(|c| c.synthetic()))
            })
        };

        let client = {
            let (o, clock, ledger, sources, stop) = (
                o.clone(),
                clock.clone(),
                ledger.clone(),
                sources.clone(),
                stop.clone(),
            );
            std::thread::spawn(move || {
                let interval = Duration::from_micros(1_000_000 / u64::from(o.fps));
                let _ = system::set_realtime_thread(interval, interval / 3, interval / 2);
                let mut decoder = TimedDecoder {
                    inner: MacDecoder::new(o.codec).expect("decoder"),
                    ledger: ledger.clone(),
                };
                let mut presenter = MacPresenter::new(width, height, o.window).expect("presenter");
                let refresh = presenter.caps().refresh_hz;
                // Host and client share this process's clock: no offset to
                // estimate, and capture times stay exact ledger keys.
                let mut config = ClientConfig::new(1, 1);
                config.clock_offset_us = Some(0);
                let mut session = ClientSession::new(config);
                let mut errors: Vec<(u64, f64)> = Vec::new();
                let mode = if o.vsync {
                    PresentMode::Vsync
                } else {
                    PresentMode::Immediate
                };
                let mut checked = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    let now = sync(&clock);
                    session.set_rtt(client_end.path().rtt_us);
                    while let Some(datagram) = client_end.try_recv() {
                        session.on_datagram(now, &datagram);
                    }
                    for frame in session.poll(now, &mut decoder) {
                        let captured = frame.captured_us;
                        let decoded = PixelBuffer::from_surface(&frame.surface).cloned();
                        if let Ok(stamp) = presenter.present(frame, mode) {
                            ledger
                                .lock()
                                .unwrap()
                                .entry(captured)
                                .or_default()
                                .presented = stamp.presented_us;
                        }
                        // Check every 10th generated frame against its source.
                        checked += 1;
                        if checked.is_multiple_of(10)
                            && let Some(decoded) = decoded
                            && let Some(source) = sources.lock().unwrap().get(&captured).cloned()
                            && let Some(error) = decoded.difference(&source)
                        {
                            errors.push((captured, error));
                        }
                    }
                    let now = sync(&clock);
                    if let Some(feedback) = session.poll_feedback(now) {
                        let _ = client_end.try_send(feedback);
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                (session.stats().clone(), errors, refresh)
            })
        };

        std::thread::sleep(Duration::from_secs_f64(o.seconds));
        stop.store(true, Ordering::SeqCst);
        let (host_stats, synthetic) = host.join().unwrap();
        let (client_stats, errors, refresh) = client.join().unwrap();

        let ledger = ledger.lock().unwrap();
        // Skip the first second (session and decoder start-up).
        let warm = stream_start + 1_000_000;
        let complete: Vec<&Stages> = ledger
            .values()
            .filter(|s| s.captured >= warm && s.presented > 0 && s.encoded > 0 && s.decoded > 0)
            .collect();
        let mut encode: Vec<u64> = complete
            .iter()
            .map(|s| s.encoded.saturating_sub(s.captured))
            .collect();
        let mut transport: Vec<u64> = complete
            .iter()
            .map(|s| s.decode_start.saturating_sub(s.encoded))
            .collect();
        let mut decode: Vec<u64> = complete
            .iter()
            .map(|s| s.decoded.saturating_sub(s.decode_start))
            .collect();
        let mut present: Vec<u64> = complete
            .iter()
            .map(|s| s.presented.saturating_sub(s.decoded))
            .collect();
        let mut total: Vec<u64> = complete
            .iter()
            .map(|s| s.presented.saturating_sub(s.captured))
            .collect();
        let captured = ledger.values().filter(|s| s.captured > 0).count();
        if std::env::var_os("MAC_PIPELINE_SLOW").is_some() {
            let mut slow: Vec<&&Stages> = complete
                .iter()
                .filter(|s| s.presented.saturating_sub(s.captured) > 50_000)
                .collect();
            slow.sort_by_key(|s| s.captured);
            for s in slow.iter().take(40) {
                println!(
                    "  slow t={:.3}s encode {:.1} transport {:.1} decode {:.1} present {:.1} ms",
                    (s.captured - stream_start) as f64 / 1e6,
                    (s.encoded.saturating_sub(s.captured)) as f64 / 1e3,
                    (s.decode_start.saturating_sub(s.encoded)) as f64 / 1e3,
                    (s.decoded.saturating_sub(s.decode_start)) as f64 / 1e3,
                    (s.presented.saturating_sub(s.decoded)) as f64 / 1e3,
                );
            }
        }
        println!(
            "MAC_PIPELINE codec {:?} {}x{}@{} present {} (display {} Hz) source {} seconds {}",
            o.codec,
            width,
            height,
            o.fps,
            if o.window {
                if o.vsync {
                    "window+vsync"
                } else {
                    "window+immediate"
                }
            } else {
                "offscreen"
            },
            refresh,
            match synthetic {
                Some(false) => "screencapturekit",
                Some(true) => "generated (no Screen Recording permission)",
                None => "generated",
            },
            o.seconds
        );
        report("capture->encoded", &mut encode);
        report("encoded->decode start", &mut transport);
        report("decode", &mut decode);
        report("decoded->presented", &mut present);
        report("capture->presented", &mut total);
        println!(
            "  frames captured {captured} presented {} | keyframes {} long-term recoveries {} forced {} | lost {} skipped {} broken {} recoveries {} (p95 {:.1} ms)",
            client_stats.frames_decoded,
            host_stats.keyframes,
            host_stats.long_term_recoveries,
            host_stats.forced_keyframes,
            client_stats.frames_lost,
            client_stats.frames_skipped,
            client_stats.decoder_broken,
            client_stats.recoveries,
            percentile(&client_stats.recovery_us, 95) as f64 / 1e3,
        );
        if !errors.is_empty() {
            let worst = errors.iter().map(|(_, e)| *e).fold(0.0, f64::max);
            let mean = errors.iter().map(|(_, e)| *e).sum::<f64>() / errors.len() as f64;
            println!(
                "  pixel check: {} frames, mean error {mean:.2}, worst {worst:.2} (0-255 per channel)",
                errors.len()
            );
        }
    }
}
