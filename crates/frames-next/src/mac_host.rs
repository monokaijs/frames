//! macOS host: ScreenCaptureKit → VideoToolbox → v2 host engine → frames-net.
//!
//! `--source qa` uses the generated source; screen capture falls back to it
//! too when this binary lacks Screen Recording permission. A viewer trusted
//! for control drives the real cursor and keyboard (Quartz events, which need
//! the Accessibility permission); QA runs only count its input, so they never
//! move the real cursor.

use crate::{Options, StreamInfo, send_info};
use anyhow::{Context, Result, bail};
use frames_input::{Motion, MotionReceiver, Pointer};
use frames_media::api::{
    CaptureConfig, CaptureEvent, CaptureSource, EncodeParams, EncoderControl, Gop, MediaClock,
    MediaTransport, PathKind, VideoEncoder, VideoFrame,
};
use frames_media::clock::SystemClock;
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_platform_macos::capture::MacCapture;
use frames_platform_macos::encoder::MacEncoder;
use frames_platform_macos::input::MacInjector;
use frames_platform_macos::pixel::PixelBuffer;
use frames_platform_macos::system;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    // Input, clipboard, and files arrive on the viewer's tagged streams.
    let services = crate::services::HostServices::start(&session, &o);
    tokio::task::spawn_blocking(move || stream(&handle, &session, &o, &services)).await?
}

/// One picture of the main display as packed BGRA (for device card
/// previews). Refuses the generated source shown without Screen Recording.
pub fn screenshot() -> Result<(Vec<u8>, u32, u32)> {
    let clock = SystemClock::new();
    let mut capture = MacCapture::start(10, false)?;
    capture.configure(CaptureConfig {
        fps: 10,
        separate_cursor: true,
    })?;
    let frame = first_frame(&mut capture, &clock)?;
    if capture.synthetic() == Some(true) {
        bail!("no Screen Recording permission");
    }
    PixelBuffer::from_surface(&frame.surface)
        .and_then(PixelBuffer::bgra)
        .context("cannot read the picture")
}

/// Clears a flag when dropped, so a helper thread stops with the session.
struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
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
    let _awake = system::KeepAwake::display();
    let clock = SystemClock::new();
    // QA runs never touch the real cursor or keyboard.
    if o.isolated {
        println!("isolated input is Windows-only; this Mac uses its normal input for the viewer");
    }
    let mut injector = (o.control && !o.qa_source).then(MacInjector::new);
    // Why input does not reach the Mac, for the viewer's title.
    let mut notices = crate::streams::Notices::new(session.clone(), handle.clone());
    if injector.is_some() && !MacInjector::trusted() {
        println!(
            "input: this program lacks the Accessibility permission; macOS will ignore the viewer's input"
        );
    }
    let mut capture = match o.window_id {
        Some(id) => MacCapture::window(u32::try_from(id).context("not a window id")?, o.fps)?,
        None => MacCapture::start(o.fps, o.qa_source)?,
    };
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
        name: crate::device_name(o),
        compatibility: None,
    };
    handle
        .block_on(send_info(session, info))
        .context("sending the stream description")?;
    // The viewer accepts a cursor stream after the description.
    let (mut cursor_stream, _) =
        handle.block_on(crate::streams::open(session, crate::streams::CURSOR))?;
    if o.adb && o.control && o.windows.is_empty() {
        handle.spawn(crate::streams::adb_listener(session.clone(), o.adb_listen));
    }
    handle.spawn({
        let session = session.clone();
        async move {
            let mut bytes = Vec::new();
            frames_input::cursor::CursorMessage::Visible(true).encode(&mut bytes);
            let _ = cursor_stream.write_all(&bytes).await;
            // Keeps the stream open for the session, and no longer.
            session.closed().await;
        }
    });

    let params = EncodeParams {
        width,
        height,
        fps: o.fps,
        bitrate_bps: o.start_mbps() * 1_000_000,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: None,
        slices: 1,
    };
    let mut encoder = MacEncoder::new(codec.into(), params)?;
    let media = session.media();
    let rate = RateConfig {
        min_bps: 2_000_000,
        max_bps: o.max_mbps() * 1_000_000,
        start_bps: o.start_mbps() * 1_000_000,
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
    // The viewer's image quality caps the rate; it can change it during the
    // session (`Event::MaxBitrate`). Relays cap it further.
    let ceiling_bps = (o.mbps * 2).max(20) * 1_000_000;
    let mut viewer_cap = o.max_mbps_cap;
    let max_bps = |kind: PathKind, cap: Option<u32>| {
        let max = cap.map_or(ceiling_bps, |mbps| ceiling_bps.min(mbps * 1_000_000));
        match kind {
            PathKind::Relay => max.min(relay_max_bps),
            PathKind::Direct => max,
        }
    };
    let mut path_kind = media.path().kind;
    let mut path_key = media.selected_path_key();
    if path_kind == PathKind::Relay {
        host.set_max_bps(relay_max_bps);
    }

    let mut clipboard: Option<crate::clip::ClipboardLink> = None;
    let started = Instant::now();
    let mut next_report = Duration::from_secs(1);
    let mut last_frame: Option<VideoFrame> = None;
    let mut last_encode_us = 0u64;
    let frame_interval_us = 1_000_000 / u64::from(o.fps.max(1));
    let mut pending = Some(first);
    let mut follower = crate::resize::Follower::new((width, height));
    // A shared application window's menus, for the viewer's window.
    let mut host_menus = o.window_id.map(crate::menus::HostMenus::start);
    let mut resize_stream = None;
    // The shared window's transparent-corner shape, measured from its
    // captured alpha on its first frame and at each new size (a sheet's or
    // menu's rounded corners); the viewer clips its popup window to it.
    let mut shape_sent: Option<crate::resize::Shape> = None;
    let mut logged_area = None;
    let mut shape_size: Option<(u32, u32)> = None;
    let (mut frames, mut pointers) = (0u64, 0u64);
    // QA runs have no injector: relative motion is summed here instead, so
    // a run can check that all of it arrived once.
    let (mut qa_relative, mut qa_motion, mut qa_total) =
        (false, MotionReceiver::default(), (0i64, 0i64));
    let mut encode_us = Vec::new();
    // A shared window moves: input follows its current frame, looked up off
    // this thread (listing windows takes milliseconds).
    let bounds = std::sync::Arc::new(std::sync::Mutex::new(None));
    let watching = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let _watcher = o.window_id.filter(|_| injector.is_some()).map(|id| {
        let (bounds, watching) = (bounds.clone(), watching.clone());
        std::thread::spawn(move || {
            while watching.load(Ordering::Relaxed) {
                *bounds.lock().unwrap() = crate::catalog::mac_bounds(id);
                std::thread::sleep(Duration::from_millis(500));
            }
        })
    });
    let _stop_watching = StopOnDrop(watching);
    // The viewer's choices for this window's picture (`Event::Header`,
    // `Event::RemoteCursor`): title bar cropped, cursor not drawn in.
    let (mut show_header, mut show_cursor) = (false, false);
    // The viewer's window hidden or behind others (`Event::Visibility`).
    let mut visibility = crate::pacing::Visibility::default();
    let mut held = None;
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        if session.is_closed() {
            println!("the viewer left");
            break;
        }
        // Input maps to what is captured: the window without its title bar,
        // as the capture reports it, else the window's frame.
        let area = capture.geometry().or(*bounds.lock().unwrap());
        if area != logged_area {
            // Diagnostics: where the captured picture is, each time it moves
            // or changes size (popup placement on the viewer follows it).
            if let Some((x, y, width, height)) = area {
                println!("captured area {x:.0},{y:.0} {width:.0}x{height:.0}");
            }
            logged_area = area;
        }
        if let Some(injector) = &mut injector
            && let Some((x, y, width, height)) = area
        {
            injector.set_area(x, y, width, height);
        }
        if let (Some(id), Some(area)) = (o.window_id, area) {
            CAPTURED.lock().unwrap().insert(id, area);
        }
        let path = media.path();
        let key = media.selected_path_key();
        if path.kind != path_kind || (key.is_some() && key != path_key) {
            path_kind = path.kind;
            path_key = key;
            // Rate control restarts on the new path and probes it.
            host.path_changed();
            host.set_max_bps(max_bps(path.kind, viewer_cap));
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
                // Frames follow at the new size (`crate::resize`).
                Ok(CaptureEvent::Resized { .. }) => None,
                Ok(CaptureEvent::Timeout) => None,
                Err(error) => bail!("capture failed: {error}"),
            },
        };
        // A still picture sends no new frames; when the viewer needs a
        // recovery, encode the last one again.
        let frame = match frame {
            Some(frame) => {
                last_frame = Some(frame.clone());
                Some(frame)
            }
            None if host.wants_frame()
                && clock.now_us().saturating_sub(last_encode_us) >= frame_interval_us =>
            {
                last_frame.clone().map(|mut frame| {
                    frame.captured_us = clock.now_us();
                    frame
                })
            }
            None => None,
        };
        let frame = visibility.admit(frame, &mut held, clock.now_us(), last_encode_us);
        if frame.is_some() && visibility.take_keyframe() {
            encoder.control(EncoderControl::ForceKeyframe)?;
        }
        let sized = frame.map(|frame| {
            let size = frame.surface.size();
            (frame, size)
        });
        if let crate::resize::Step::Encode(frame, resized) = follower.step(sized, Instant::now()) {
            if let Some(size) = resized {
                let params = EncodeParams {
                    width: size.0,
                    height: size.1,
                    bitrate_bps: host.target_bps(),
                    ..params
                };
                encoder.control(EncoderControl::Reconfigure(params))?;
                encoder.control(EncoderControl::ForceKeyframe)?;
                println!("source resized to {}x{}", size.0, size.1);
                crate::resize::announce(handle, session, &mut resize_stream, size)?;
            }
            let size = frame.surface.size();
            if o.window_id.is_some() && shape_size != Some(size) {
                shape_size = Some(size);
                let shape = PixelBuffer::from_surface(&frame.surface)
                    .and_then(PixelBuffer::bgra)
                    .and_then(|(bgra, width, height)| {
                        crate::resize::Shape::measure(&bgra, width, height)
                    });
                if shape != shape_sent {
                    crate::resize::announce_shape(
                        handle,
                        session,
                        &mut resize_stream,
                        shape.as_ref(),
                    )?;
                    shape_sent = shape;
                }
            }
            let before = clock.now_us();
            last_encode_us = before;
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
            } else if let Some(motion) = Motion::decode(&datagram) {
                match &mut injector {
                    Some(injector) => {
                        injector.motion(&motion);
                    }
                    None if qa_relative => {
                        if let Some((dx, dy)) = qa_motion.accept(&motion) {
                            qa_total.0 += i64::from(dx);
                            qa_total.1 += i64::from(dy);
                        }
                    }
                    None => {}
                }
            } else {
                host.on_datagram(arrived, &datagram, rtt);
            }
        }
        if let Some(menus) = &mut host_menus
            && let Err(error) = menus.send_changes(handle, session)
        {
            println!("application menus stopped: {error:#}");
            host_menus = None;
        }
        while let Ok(event) = inputs.try_recv() {
            if let frames_input::Event::Menu { id } = event {
                if o.control
                    && let Some(menus) = &host_menus
                {
                    menus.invoke(id);
                }
                continue;
            }
            if let frames_input::Event::MaxBitrate { mbps } = event {
                viewer_cap = Some(u32::from(mbps).max(1));
                host.set_max_bps(max_bps(media.path().kind, viewer_cap));
                println!("the viewer's image quality: at most {mbps} Mbps");
                continue;
            }
            match event {
                frames_input::Event::Visibility { shown, focused } => {
                    if visibility.set(shown, focused) {
                        println!("the viewer's window: {}", visibility.describe());
                    }
                    continue;
                }
                frames_input::Event::Header { show } => {
                    show_header = show;
                    capture.set_options(show_header, show_cursor);
                    println!("title bar {}", if show { "shown" } else { "cropped" });
                    continue;
                }
                frames_input::Event::RemoteCursor { show } => {
                    show_cursor = show;
                    capture.set_options(show_header, show_cursor);
                    println!(
                        "cursor {} the picture",
                        if show { "drawn into" } else { "left out of" }
                    );
                    continue;
                }
                frames_input::Event::RefreshMenus => {
                    if let Some(menus) = &mut host_menus {
                        menus.refresh();
                    }
                    // The viewer's window came to the front (or asked):
                    // so does this one, as v1 did.
                    if o.control
                        && !o.qa_source
                        && let Some(id) = o.window_id.and_then(|id| u32::try_from(id).ok())
                    {
                        frames_platform_macos::capture::activate_window(id, true);
                    }
                    continue;
                }
                frames_input::Event::ResizeWindow { width, height } => {
                    if o.control && o.window_id.is_some() {
                        let resized = capture.resize_window(u32::from(width), u32::from(height));
                        println!(
                            "the viewer asked for {width}x{height}: {}",
                            if resized {
                                "resizing"
                            } else {
                                "this window cannot be resized"
                            }
                        );
                    }
                    continue;
                }
                _ => {}
            }
            if event == frames_input::Event::CloseWindow {
                // As the window's close button would; it may ask to save.
                if o.control
                    && let Some(id) = o.window_id
                {
                    let closed =
                        u32::try_from(id).is_ok_and(frames_platform_macos::capture::close_window);
                    println!(
                        "the viewer asked to close the window: {}",
                        if closed {
                            "closing"
                        } else {
                            "it has no close button here"
                        }
                    );
                }
                continue;
            }
            if let frames_input::Event::Mode { relative } = event {
                println!(
                    "input: relative mouse {}",
                    if relative { "on" } else { "off" }
                );
                qa_relative = relative;
                qa_motion.restart();
            }
            // A click or typing reaches a window whose application is not
            // in front: bring it forward first (v1), or macOS sends the
            // keys elsewhere.
            if injector.is_some()
                && matches!(
                    event,
                    frames_input::Event::Button { down: true, .. }
                        | frames_input::Event::Key { down: true, .. }
                        | frames_input::Event::Text { .. }
                )
                && let Some(id) = o.window_id.and_then(|id| u32::try_from(id).ok())
            {
                let click = matches!(event, frames_input::Event::Button { .. });
                frames_platform_macos::capture::activate_window(id, click);
            }
            if let Some(injector) = &mut injector {
                injector.event(event);
                if !MacInjector::trusted() {
                    notices.send(
                        "Input blocked: allow Frames under Privacy & Security → Accessibility on the Mac",
                    );
                }
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
    if injector.is_none() && qa_total != (0, 0) {
        println!("MOTION total_x={} total_y={}", qa_total.0, qa_total.1);
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

/// A rectangle on screen in global points: x, y, width, height.
type ScreenRect = (f64, f64, f64, f64);

/// Where each shared window's picture is on screen, for files dropped on
/// it (`crate::services`).
static CAPTURED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<u64, ScreenRect>>> =
    std::sync::LazyLock::new(Default::default);

/// The shared window `id`'s picture on screen, while a session shows it.
pub fn captured_rect(id: u64) -> Option<ScreenRect> {
    CAPTURED.lock().unwrap().get(&id).copied()
}
