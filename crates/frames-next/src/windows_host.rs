//! Windows host: WGC capture → NVENC → v2 host engine → frames-net.

use crate::{Options, StreamInfo, send_info, win};
use anyhow::{Context, Result, bail};
use frames_codec::d3d11::{AMD, INTEL, nvidia_device, vendor_device};
use frames_codec::ffmpeg::{FfmpegEncoder, encoder_name};
use frames_codec::nvenc::NvencEncoder;
use frames_input::cursor::CursorMessage;
use frames_input::{Event, EventReader, Pointer};
use frames_media::api::{
    CaptureConfig, CaptureEvent, CaptureSource, EncodeParams, Gop, MediaClock, MediaTransport,
    PathKind, VideoEncoder, VideoFrame,
};
use frames_media::clock::SystemClock;
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_platform_windows::capture::WgcCapture;
use frames_platform_windows::input::{InputInjector, InputTarget};
use frames_platform_windows::system::{GpuClockHold, Mmcss, TimerResolution, raise_gpu_priority};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    // The viewer's reliable input stream, decoded here and applied on the
    // streaming thread with the pointer datagrams.
    let (events_tx, events_rx) = std::sync::mpsc::channel::<Event>();
    let reader = {
        let session = session.clone();
        tokio::spawn(async move {
            let Ok((_send, mut recv)) = session.accept_channel().await else {
                return;
            };
            let mut decoder = EventReader::default();
            let mut buffer = [0u8; 1024];
            while let Ok(Some(n)) = recv.read(&mut buffer).await {
                for event in decoder.push(&buffer[..n]) {
                    if events_tx.send(event).is_err() {
                        return;
                    }
                }
            }
        })
    };
    let result =
        tokio::task::spawn_blocking(move || stream(&handle, &session, &o, events_rx)).await?;
    reader.abort();
    result
}

fn primary_monitor_rect() -> windows::Win32::Foundation::RECT {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: unsafe { GetSystemMetrics(SM_CXSCREEN) },
        bottom: unsafe { GetSystemMetrics(SM_CYSCREEN) },
    }
}

fn first_frame(capture: &mut WgcCapture, clock: &SystemClock) -> Result<VideoFrame> {
    let give_up = clock.now_us() + 5_000_000;
    while clock.now_us() < give_up {
        win::pump();
        match capture.next(clock.now_us() + 20_000) {
            Ok(CaptureEvent::Frame(frame)) => return Ok(frame),
            Ok(CaptureEvent::Lost) => bail!("the capture source went away"),
            Ok(_) => {}
            Err(error) => bail!("capture failed: {error}"),
        }
    }
    bail!("no frame captured within 5 s")
}

fn stream(
    handle: &tokio::runtime::Handle,
    session: &frames_net::Session,
    o: &Options,
    events: std::sync::mpsc::Receiver<Event>,
) -> Result<()> {
    win::dpi_aware();
    let _mmcss = Mmcss::games();
    let _timer = TimerResolution::one_millisecond();
    let clock = SystemClock::new();
    let shared: Arc<dyn MediaClock> = Arc::new(clock);
    // Capture on the encoder's GPU: hardware encoders only accept textures
    // from their own adapter.
    let nvenc = o.encoder == "nvenc";
    // NVENC slows 2-2.5x when the GPU idles in a low power state; ask the
    // driver to keep this program at full clocks, before the device exists.
    let _gpu_profile = (nvenc && o.gpu_profile).then(|| {
        match frames_platform_windows::nvprofile::PreferMaxPerformance::apply() {
            Ok(guard) => {
                println!("GPU power: prefer maximum performance (restored on exit)");
                Some(guard)
            }
            Err(error) => {
                println!("GPU power profile unavailable: {error}");
                None
            }
        }
    });
    let (device, _context, adapter) = match o.encoder.as_str() {
        "amf" => vendor_device(AMD)?,
        "qsv" => vendor_device(INTEL)?,
        _ => nvidia_device()?,
    };
    raise_gpu_priority(&device);
    // NVENC runs 2-2.5x slower when the GPU idles in a low power state
    // (spike 0-C). This holds clocks with a small GPU load for the session.
    let _hold = (nvenc && o.hold > 0)
        .then(|| GpuClockHold::start(&device, Duration::from_millis(1), o.hold))
        .transpose()?;
    println!("adapter: {adapter}");

    let qa = if o.qa_source {
        Some(win::QaSource::start(1920, 1080, o.fps)?)
    } else {
        None
    };
    let mut capture = match &qa {
        Some(source) => WgcCapture::window(source.hwnd(), &device, shared.clone())?,
        None => WgcCapture::primary_monitor(&device, shared.clone())?,
    };
    let mut injector = InputInjector::new(match &qa {
        Some(source) => InputTarget::Window(source.hwnd()),
        None => InputTarget::Desktop(primary_monitor_rect()),
    });
    capture.configure(CaptureConfig {
        fps: o.fps,
        separate_cursor: true,
    })?;
    let first = first_frame(&mut capture, &clock)?;
    let (width, height) = first.surface.size();
    println!("capturing {width}x{height} at up to {} fps", o.fps);
    let info = StreamInfo {
        codec: o.codec,
        width,
        height,
        fps: o.fps,
    };
    handle
        .block_on(send_info(session, info))
        .context("sending the stream description")?;
    // Cursor shape and visibility, opened after the description so the
    // viewer accepts the two streams in that order.
    let (mut cursor_stream, _) = handle.block_on(session.open_channel())?;
    let (cursor_tx, mut cursor_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    handle.spawn(async move {
        while let Some(bytes) = cursor_rx.recv().await {
            if cursor_stream.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let send_cursor = |message: CursorMessage| {
        let mut bytes = Vec::new();
        message.encode(&mut bytes);
        let _ = cursor_tx.send(bytes);
    };
    if qa.is_some() {
        // The real cursor is not over the QA window; show a known shape.
        send_cursor(win::qa_cursor());
        send_cursor(CursorMessage::Visible(true));
    }
    let mut cursor_visible = None;

    let params = EncodeParams {
        width,
        height,
        fps: o.fps,
        bitrate_bps: o.mbps * 1_000_000,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: Some(6),
        slices: 4,
    };
    let mut encoder: Box<dyn VideoEncoder> = if nvenc {
        Box::new(NvencEncoder::new(
            &device,
            o.codec.into(),
            params,
            shared.clone(),
        )?)
    } else {
        let name = encoder_name(o.codec.into(), &o.encoder).with_context(|| {
            format!(
                "no {} encoder for {:?} (software encodes AV1 only)",
                o.encoder, o.codec
            )
        })?;
        let encoder = FfmpegEncoder::new(&name, o.codec.into(), params, shared.clone())?;
        println!("encoder: FFmpeg {}", encoder.name());
        Box::new(encoder)
    };
    let media = session.media();
    let rate = RateConfig {
        min_bps: 2_000_000,
        max_bps: (o.mbps * 2).max(20) * 1_000_000,
        start_bps: o.mbps * 1_000_000,
    };
    let mut config = HostConfig::new(1, 1, rate);
    config.max_datagram = media.path().max_datagram.min(1200);
    let mut host = HostSession::new(config);
    // n0's public relays deliver about 8-10 Mbps; self-hosted ones are
    // sized by whoever runs them, so only the public ones are capped.
    let relay_max_bps = if o.relays.is_empty() {
        frames_net::N0_RELAY_MAX_BPS
    } else {
        rate.max_bps
    };
    let mut path_kind = media.path().kind;
    if path_kind == PathKind::Relay {
        host.set_max_bps(relay_max_bps);
    }

    let started = Instant::now();
    let mut next_report = Duration::from_secs(1);
    let mut pending = Some(first);
    let mut frames = 0u64;
    let mut encode_us = Vec::new();
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        if session.is_closed() {
            println!("the viewer left");
            break;
        }
        win::pump();
        let path = media.path();
        if path.kind != path_kind {
            path_kind = path.kind;
            host.path_changed();
            host.set_max_bps(match path.kind {
                PathKind::Relay => relay_max_bps,
                PathKind::Direct => rate.max_bps,
            });
            println!("path changed to {path_kind:?}");
        }
        let frame = match pending.take() {
            Some(frame) => Some(frame),
            None => match capture.next(clock.now_us() + 500) {
                Ok(CaptureEvent::Frame(frame)) => Some(frame),
                Ok(CaptureEvent::Lost) => bail!("the capture source went away"),
                Ok(CaptureEvent::Resized { width, height }) => {
                    bail!("the source resized to {width}x{height}; restart the host")
                }
                Ok(CaptureEvent::Timeout) => None,
                Err(error) => bail!("capture failed: {error}"),
            },
        };
        if qa.is_none()
            && let Some(update) = capture.cursor()
        {
            if let Some(shape) = update.shape {
                send_cursor(CursorMessage::Shape {
                    width: shape.width as u16,
                    height: shape.height as u16,
                    hotspot_x: shape.hotspot_x as u16,
                    hotspot_y: shape.hotspot_y as u16,
                    bgra: shape.bgra.to_vec(),
                });
            }
            if cursor_visible != Some(update.visible) {
                cursor_visible = Some(update.visible);
                send_cursor(CursorMessage::Visible(update.visible));
            }
        }
        if let Some(frame) = frame {
            let before = clock.now_us();
            host.encode(before, encoder.as_mut(), &frame)?;
            encode_us.push(clock.now_us().saturating_sub(before));
            media.set_target_rate(u64::from(host.target_bps()));
            frames += 1;
        }
        let now = clock.now_us();
        while let Some(datagram) = host.poll_transmit(now) {
            let _ = media.try_send(datagram);
        }
        let rtt = path.rtt_us;
        while let Some(datagram) = media.try_recv() {
            match Pointer::decode(&datagram) {
                // A view-only device's input is dropped here, on the host.
                Some(pointer) if o.control => {
                    injector.pointer(&pointer);
                }
                Some(_) => {}
                None => host.on_datagram(clock.now_us(), &datagram, rtt),
            }
        }
        while let Ok(event) = events.try_recv() {
            if o.control {
                injector.event(event);
            }
        }
        if started.elapsed() >= next_report {
            let stats = host.stats();
            let p50 = crate::stats::percentile_ms(&mut encode_us, 50);
            println!(
                "host t={:>3}s frames {frames:>5} encode p50 {p50:.2} ms target {:>5.1} Mbps keyframes {} invalidations {} retx {} | {:?} rtt {:.1} ms",
                next_report.as_secs(),
                f64::from(host.target_bps()) / 1e6,
                stats.keyframes,
                stats.invalidations,
                stats.retransmissions,
                path.kind,
                path.rtt_us as f64 / 1e3,
            );
            encode_us.clear();
            if qa.is_some() {
                let counts = &win::QA_INPUT;
                let load =
                    |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
                println!(
                    "host input: applied {} | QA window received moves {} buttons {} wheels {} keys {}",
                    injector.applied,
                    load(&counts.moves),
                    load(&counts.buttons),
                    load(&counts.wheels),
                    load(&counts.keys),
                );
            }
            next_report += Duration::from_secs(1);
        }
    }
    injector.release_all();
    win::pump();
    session.close("host stopped");
    Ok(())
}
