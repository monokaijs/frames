//! macOS host: ScreenCaptureKit → VideoToolbox → v2 host engine → frames-net.
//!
//! `--source qa` uses the generated source; screen capture falls back to it
//! too when this binary lacks Screen Recording permission. A viewer trusted
//! for control drives the real cursor and keyboard (Quartz events, which need
//! the Accessibility permission); QA runs only count its input, so they never
//! move the real cursor.

use crate::{Options, StreamInfo, send_info};
use anyhow::{Context, Result, bail};
use frames_input::Pointer;
use frames_media::api::{
    CaptureConfig, CaptureEvent, CaptureSource, EncodeParams, Gop, MediaClock, MediaTransport,
    PathKind, VideoFrame,
};
use frames_media::clock::SystemClock;
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_platform_macos::capture::MacCapture;
use frames_platform_macos::encoder::MacEncoder;
use frames_platform_macos::input::MacInjector;
use frames_platform_macos::system;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    // Input, clipboard, and files arrive on the viewer's tagged streams.
    let services = crate::services::HostServices::start(&session, &o);
    tokio::task::spawn_blocking(move || stream(&handle, &session, &o, &services)).await?
}

fn first_frame(capture: &mut MacCapture, clock: &SystemClock) -> Result<VideoFrame> {
    let give_up = clock.now_us() + 5_000_000;
    while clock.now_us() < give_up {
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
    services: &crate::services::HostServices,
) -> Result<()> {
    let (events, inputs, clipboard_streams) = (
        &*services.event_count,
        &services.events,
        &services.clipboard,
    );
    let frame_time = Duration::from_micros(1_000_000 / u64::from(o.fps.max(1)));
    let _ = system::set_realtime_thread(frame_time, frame_time / 4, frame_time / 2);
    let clock = SystemClock::new();
    // QA runs never touch the real cursor or keyboard.
    let mut injector = (o.control && !o.qa_source).then(MacInjector::new);
    if injector.is_some() && !MacInjector::trusted() {
        println!(
            "input: this program lacks the Accessibility permission; macOS will ignore the viewer's input"
        );
    }
    let mut capture = MacCapture::start(o.fps, o.qa_source)?;
    capture.configure(CaptureConfig {
        fps: o.fps,
        separate_cursor: true,
    })?;
    let first = first_frame(&mut capture, &clock)?;
    let (width, height) = first.surface.size();
    let source = match capture.synthetic() {
        Some(true) => "generated source",
        Some(false) => "screen",
        None => "unknown source",
    };
    println!(
        "capturing {width}x{height} ({source}) at up to {} fps",
        o.fps
    );
    let codec = crate::choose_codec(
        o,
        &[crate::Codec::Hevc, crate::Codec::H264],
        session.media().path().kind == PathKind::Relay,
    );
    println!("codec: {codec:?}");
    let info = StreamInfo {
        codec,
        width,
        height,
        fps: o.fps,
        platform: crate::platform().into(),
    };
    handle
        .block_on(send_info(session, info))
        .context("sending the stream description")?;
    // The viewer accepts a cursor stream after the description.
    let (mut cursor_stream, _) =
        handle.block_on(crate::streams::open(session, crate::streams::CURSOR))?;
    if o.adb && o.control {
        handle.spawn(crate::streams::adb_listener(session.clone(), o.adb_listen));
    }
    handle.spawn(async move {
        let mut bytes = Vec::new();
        frames_input::cursor::CursorMessage::Visible(true).encode(&mut bytes);
        let _ = cursor_stream.write_all(&bytes).await;
        std::future::pending::<()>().await;
    });

    let params = EncodeParams {
        width,
        height,
        fps: o.fps,
        bitrate_bps: o.mbps * 1_000_000,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: None,
        slices: 1,
    };
    let mut encoder = MacEncoder::new(codec.into(), params)?;
    let media = session.media();
    let rate = RateConfig {
        min_bps: 2_000_000,
        max_bps: (o.mbps * 2).max(20) * 1_000_000,
        start_bps: o.mbps * 1_000_000,
    };
    let mut config = HostConfig::new(1, 1, rate);
    if let Some(pacing) = o.pacing {
        config.pacing_factor = pacing;
    }
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
    let mut path_key = media.selected_path_key();
    if path_kind == PathKind::Relay {
        host.set_max_bps(relay_max_bps);
    }

    let mut clipboard: Option<crate::clip::ClipboardLink> = None;
    let started = Instant::now();
    let mut next_report = Duration::from_secs(1);
    let mut pending = Some(first);
    let (mut frames, mut pointers) = (0u64, 0u64);
    let mut encode_us = Vec::new();
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        if session.is_closed() {
            println!("the viewer left");
            break;
        }
        let path = media.path();
        let key = media.selected_path_key();
        if path.kind != path_kind || (key.is_some() && key != path_key) {
            path_kind = path.kind;
            path_key = key;
            // Rate control restarts on the new path and probes it.
            host.path_changed();
            host.set_max_bps(match path.kind {
                PathKind::Relay => relay_max_bps,
                PathKind::Direct => rate.max_bps,
            });
            println!(
                "path changed to {path_kind:?} {}",
                path_key.as_deref().unwrap_or("")
            );
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
        if let Some(frame) = frame {
            let before = clock.now_us();
            host.encode(before, &mut encoder, &frame)?;
            encode_us.push(clock.now_us().saturating_sub(before));
            frames += 1;
        }
        if clipboard.is_none()
            && let Ok(streams) = clipboard_streams.try_recv()
        {
            clipboard = Some(crate::clip::ClipboardLink::start(
                handle,
                streams,
                o.clipboard,
                false,
            ));
        }
        if let Some(clipboard) = &mut clipboard {
            clipboard.tick();
        }
        let now = clock.now_us();
        // Every iteration, not per frame: probe clusters go out between
        // frames and need the transport paced at their rate.
        media.set_target_rate(u64::from(host.transport_target_bps()));
        while let Some(datagram) = host.poll_transmit(now) {
            let _ = media.try_send(datagram);
        }
        let rtt = path.rtt_us;
        while let Some((datagram, arrived)) = media.try_recv_timed() {
            let arrived = arrived.unwrap_or_else(|| clock.now_us());
            if let Some(pointer) = Pointer::decode(&datagram) {
                pointers += 1;
                if let Some(injector) = &mut injector {
                    injector.pointer(&pointer);
                }
            } else {
                host.on_datagram(arrived, &datagram, rtt);
            }
        }
        while let Ok(event) = inputs.try_recv() {
            if let Some(injector) = &mut injector {
                injector.event(event);
            }
        }
        if started.elapsed() >= next_report {
            if next_report.as_secs() == 2 {
                for (offered, sent, received, delivered) in host.probe_report() {
                    println!(
                        "  probe {:.1} Mbps: sent {sent} received {received} delivered {}",
                        offered / 1e6,
                        delivered.map_or("-".into(), |d| format!("{:.1} Mbps", d / 1e6))
                    );
                }
            }
            let stats = host.stats();
            let p50 = crate::stats::percentile_ms(&mut encode_us, 50);
            println!(
                "host t={:>3}s frames {frames:>5} encode p50 {p50:.2} ms target {:>5.1} Mbps keyframes {} long-term {} retx {} | input pointers {pointers} events {} ({}) | {:?} rtt {:.1} ms",
                next_report.as_secs(),
                f64::from(host.target_bps()) / 1e6,
                stats.keyframes,
                stats.long_term_recoveries,
                stats.retransmissions,
                events.load(Ordering::Relaxed),
                if injector.is_some() {
                    "applied"
                } else {
                    "counted only"
                },
                path.kind,
                path.rtt_us as f64 / 1e3,
            );
            encode_us.clear();
            if next_report.as_secs() % 5 == 1 {
                for line in media.describe_paths() {
                    println!("  path {line}");
                }
            }
            next_report += Duration::from_secs(1);
        }
    }
    if let Some(clipboard) = &clipboard {
        println!(
            "CLIPBOARD sent={} received={} now={:?}",
            clipboard.sent,
            clipboard.received,
            clipboard.current()
        );
    }
    session.close("host stopped");
    Ok(())
}
