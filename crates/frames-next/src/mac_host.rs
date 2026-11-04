//! macOS host: ScreenCaptureKit → VideoToolbox → v2 host engine → frames-net.
//!
//! `--source qa` uses the generated source; screen capture falls back to it
//! too when this binary lacks Screen Recording permission. Input from the
//! viewer is received and counted but not applied yet (macOS injection needs
//! Accessibility permission and lands with the macOS input backend).

use crate::{Options, StreamInfo, send_info};
use anyhow::{Context, Result, bail};
use frames_input::{EventReader, Pointer};
use frames_media::api::{
    CaptureConfig, CaptureEvent, CaptureSource, EncodeParams, Gop, MediaClock, MediaTransport,
    PathKind, VideoFrame,
};
use frames_media::clock::SystemClock;
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_platform_macos::capture::MacCapture;
use frames_platform_macos::encoder::MacEncoder;
use frames_platform_macos::system;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    let events = Arc::new(AtomicU64::new(0));
    let reader = {
        let (session, events) = (session.clone(), events.clone());
        tokio::spawn(async move {
            let Ok((_send, mut recv)) = session.accept_channel().await else {
                return;
            };
            let mut decoder = EventReader::default();
            let mut buffer = [0u8; 1024];
            while let Ok(Some(n)) = recv.read(&mut buffer).await {
                let count = decoder.push(&buffer[..n]).len() as u64;
                events.fetch_add(count, Ordering::Relaxed);
            }
        })
    };
    let result =
        tokio::task::spawn_blocking(move || stream(&handle, &session, &o, &events)).await?;
    reader.abort();
    result
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
    events: &AtomicU64,
) -> Result<()> {
    let frame_time = Duration::from_micros(1_000_000 / u64::from(o.fps.max(1)));
    let _ = system::set_realtime_thread(frame_time, frame_time / 4, frame_time / 2);
    let clock = SystemClock::new();
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
    let info = StreamInfo {
        codec: o.codec,
        width,
        height,
        fps: o.fps,
    };
    handle
        .block_on(send_info(session, info))
        .context("sending the stream description")?;
    // The viewer accepts a cursor stream after the description.
    let (mut cursor_stream, _) = handle.block_on(session.open_channel())?;
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
    let mut encoder = MacEncoder::new(o.codec.into(), params)?;
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
            if Pointer::decode(&datagram).is_some() {
                pointers += 1;
            } else {
                host.on_datagram(arrived, &datagram, rtt);
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
                "host t={:>3}s frames {frames:>5} encode p50 {p50:.2} ms target {:>5.1} Mbps keyframes {} long-term {} retx {} | input pointers {pointers} events {} (not applied on macOS yet) | {:?} rtt {:.1} ms",
                next_report.as_secs(),
                f64::from(host.target_bps()) / 1e6,
                stats.keyframes,
                stats.long_term_recoveries,
                stats.retransmissions,
                events.load(Ordering::Relaxed),
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
    session.close("host stopped");
    Ok(())
}
