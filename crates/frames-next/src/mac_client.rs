//! macOS viewer: frames-net → v2 client engine → VideoToolbox → Metal.
//!
//! Input from the window goes to the host on the shared input lane, and the
//! host's cursor shape is shown over the video by the window itself.

use crate::Options;
use crate::lane::{Captured, InputLane};
use crate::stats::Samples;
use anyhow::Result;
use frames_input::{Button, Event, keys};
use frames_media::api::{MediaClock, MediaTransport, PresentMode, Presenter};
use frames_media::client::{ClientConfig, ClientSession};
use frames_media::clock::SystemClock;
use frames_platform_macos::decoder::MacDecoder;
use frames_platform_macos::presenter::{MacInput, MacPresenter};
use frames_platform_macos::system;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let info = crate::await_stream(&session).await?;
    // The host's cursor shape and visibility, applied on the viewer thread.
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel();
    let (sizes_tx, sizes) = std::sync::mpsc::channel();
    let (menus_tx, menus) = std::sync::mpsc::channel();
    let drain = {
        let session = session.clone();
        let (cursor_streams, mut cursor_incoming) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(crate::streams::route_viewer(
            session.clone(),
            cursor_streams,
            sizes_tx,
            menus_tx,
            o.adb_server,
        ));
        tokio::spawn(async move {
            let Some(mut recv) = cursor_incoming.recv().await else {
                return;
            };
            let mut reader = frames_input::cursor::CursorReader::default();
            let mut buffer = vec![0u8; 64 * 1024];
            while let Ok(Some(n)) = recv.read(&mut buffer).await {
                let Some(messages) = reader.push(&buffer[..n]) else {
                    return;
                };
                for message in messages {
                    if cursor_tx.send(message).is_err() {
                        return;
                    }
                }
            }
        })
    };
    let (mut lane, writer) = InputLane::open(&session).await?;
    // Cross-platform sessions swap Control and Command, as v1 does.
    // Control and Command trade places across platforms, unless the user
    // wants the same physical keys (`--physical-keys`, for terminals).
    lane.swap_modifiers =
        !o.physical_keys && !info.platform.is_empty() && info.platform != crate::platform();
    if lane.swap_modifiers {
        println!("keys: Control and Command swapped ({} host)", info.platform);
    }
    // Clipboard sharing: one two-way stream, opened after the input stream.
    let clipboard = if o.clipboard == crate::clip::Mode::Off {
        None
    } else {
        let streams = crate::streams::open(&session, crate::streams::CLIPBOARD).await?;
        Some(crate::clip::ClipboardLink::start(
            &tokio::runtime::Handle::current(),
            streams,
            o.clipboard,
            true,
        ))
    };
    let audio = crate::audio::Receiver::start(o.audio).await?;
    let result = tokio::task::spawn_blocking(move || {
        let updates = crate::streams::HostUpdates {
            cursor: cursor_rx,
            sizes,
            menus,
        };
        view(&session, info, &o, lane, updates, clipboard, audio)
    })
    .await?;
    let _ = writer.await;
    drain.abort();
    result
}

/// The connection menus' state last given to FramesMac (`control::version`).
static MENUS_SHOWN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Gives FramesMac the connection menus' state when it changed.
fn push_menus() {
    let version = crate::control::version();
    if MENUS_SHOWN.swap(version, std::sync::atomic::Ordering::Relaxed) != version {
        system::viewer_set_state(&crate::control::snapshot_json());
    }
}

/// `input` as what the lane sends: text longer than one event fits goes as
/// several.
fn captured_all(input: MacInput) -> Vec<Captured> {
    match captured(input) {
        Some(Captured::Event {
            event: Event::Text { text },
            at_us,
        }) => Event::texts(&text)
            .into_iter()
            .map(|event| Captured::Event { event, at_us })
            .collect(),
        other => other.into_iter().collect(),
    }
}

/// Relative mouse is on in a viewer window of this process.
static RELATIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn captured(input: MacInput) -> Option<Captured> {
    let button = |code: u8| match code {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::Back,
        _ => Button::Forward,
    };
    Some(match input {
        MacInput::Move { x, y, at_us } => Captured::Move { x, y, at_us },
        MacInput::Button {
            button: code,
            down,
            x,
            y,
            at_us,
        } => Captured::Event {
            event: Event::Button {
                button: button(code),
                down,
                x,
                y,
            },
            at_us,
        },
        MacInput::Wheel { dx, dy, at_us } => Captured::Event {
            event: Event::Wheel { dx, dy },
            at_us,
        },
        MacInput::Key { code, down, at_us } => Captured::Event {
            event: Event::Key {
                usage: keys::mac_to_usage(code)?,
                down,
            },
            at_us,
        },
        MacInput::FocusLost { at_us, window } => {
            // Gamepads drive the host only while a viewer window has focus.
            if window {
                crate::control::set_focused(false);
            }
            Captured::Event {
                event: Event::ReleaseAll,
                at_us,
            }
        }
        MacInput::Motion { dx, dy, at_us } => Captured::Motion { dx, dy, at_us },
        MacInput::Mode { relative, at_us } => {
            println!("relative mouse {}", if relative { "on" } else { "off" });
            RELATIVE.store(relative, std::sync::atomic::Ordering::Relaxed);
            Captured::Event {
                event: Event::Mode { relative },
                at_us,
            }
        }
        MacInput::ToggleStats | MacInput::Command(4) => {
            crate::control::send(crate::control::Command::Stats);
            return None;
        }
        // Image quality: what every window of this viewer receives at most
        // (`control::WindowLoop` tells the host).
        // Longer text (a paste through an input method) goes as several
        // events (`captured_all`).
        MacInput::Text { text, at_us } => Captured::Event {
            event: Event::Text { text },
            at_us,
        },
        // The window was minimized, covered or shown again, or another
        // window came to the front: the host paces its frames (v1).
        MacInput::Visibility {
            shown,
            focused,
            at_us,
        } => Captured::Event {
            event: Event::Visibility { shown, focused },
            at_us,
        },
        MacInput::Command(command @ 1..=3) => {
            crate::control::send(crate::control::Command::Quality {
                quality: command as u8 - 1,
            });
            return None;
        }
        MacInput::Command(6) => {
            crate::control::send(crate::control::Command::Disconnect);
            return None;
        }
        MacInput::Command(_) | MacInput::Size { .. } => return None,
        // The window came to the front: the host raises its window and
        // reads its menus again (v1's focus action).
        MacInput::FocusGained { at_us } => {
            crate::control::set_focused(true);
            Captured::Event {
                event: Event::RefreshMenus,
                at_us,
            }
        }
        MacInput::Menu(id) => Captured::Event {
            event: Event::Menu { id },
            at_us: frames_media::clock::SystemClock::new().now_us(),
        },
    })
}

fn view(
    session: &frames_net::Session,
    info: crate::StreamInfo,
    o: &Options,
    lane: InputLane,
    updates: crate::streams::HostUpdates,
    mut clipboard: Option<crate::clip::ClipboardLink>,
    audio: Option<crate::audio::Receiver>,
) -> Result<()> {
    let frame = Duration::from_micros(1_000_000 / u64::from(info.fps.max(1)));
    let _ = system::set_realtime_thread(frame, frame / 4, frame / 2);
    let clock = SystemClock::new();
    let mut decoder = MacDecoder::new(info.codec.into())?;
    // The window needs NSApplication on the main thread (frames-next runs it).
    // The picture at its own size when it fits (FramesMac fits it to the
    // screen), in its own shape.
    let mut presenter = match &o.popup {
        Some(_) => MacPresenter::new_popup(info.width, info.height)?,
        None => MacPresenter::new(info.width, info.height, true)?,
    };
    // A host menu, tooltip or sheet: over its window's picture.
    if let Some((popup, parent)) = &o.popup {
        use frames_platform_macos::presenter::PopupRole;
        let role = match popup.role.as_str() {
            "menu" => PopupRole::Menu,
            "tooltip" => PopupRole::Tooltip,
            "sheet" => PopupRole::Sheet,
            _ => PopupRole::Popover,
        };
        let f = |v: f64| v;
        presenter.attach(
            parent.id,
            (
                f(popup.x - parent.x),
                f(popup.y - parent.y),
                f(popup.width),
                f(popup.height),
            ),
            (f(parent.width), f(parent.height)),
            role,
        );
    }
    let base_title = crate::viewer_title(o, &info);
    presenter.set_title(&format!("{base_title} — Waiting for video…"));
    presenter.set_menu_id(o.menu_id);
    system::viewer_set_command_handler(crate::control::send_json);
    MENUS_SHOWN.store(0, std::sync::atomic::Ordering::Relaxed);
    push_menus();
    system::viewer_app();
    // The connection menus' choices for this window (`crate::control`).
    let mut window_loop = crate::control::WindowLoop::new(o, &info);
    window_loop.resize.streaming((info.width, info.height));
    // The size the user last gave the window, in pixels.
    let user_size = Arc::new(Mutex::new(None::<(u32, u32)>));
    // A window of a host application: closing it asks the host window to
    // close (its going away ends this session); a second close closes here.
    presenter.intercept_close(o.window_id.is_some());
    // Events are sent from the main thread as they arrive: the viewer loop
    // can wait up to a frame for a drawable, which would delay input.
    let lane = Arc::new(Mutex::new(lane));
    {
        let (lane, media, user_size) = (lane.clone(), session.media(), user_size.clone());
        presenter.set_input(move |input| {
            if let MacInput::Size { width, height } = input {
                *user_size.lock().unwrap() = Some((width, height));
            }
            let items = captured_all(input);
            if !items.is_empty() {
                lane.lock()
                    .unwrap()
                    .send(&media, &SystemClock::new(), items);
            }
        });
    }
    if o.relative_qa {
        presenter.set_relative(true, false);
    }
    let media = session.media();
    let mut config = ClientConfig::new(1, 1);
    config.max_datagram = media.path().max_datagram.min(1200);
    let mut client = ClientSession::new(config);
    let mode = if o.vsync {
        PresentMode::Vsync
    } else {
        PresentMode::Immediate
    };

    // Media sent while the window and decoder were being created is stale:
    // drop it and let the engine recover from the newest frames instead of
    // presenting a second-old backlog.
    let mut stale = 0;
    while media.try_recv().is_some() {
        stale += 1;
    }
    if stale > 0 {
        println!("dropped {stale} datagrams queued during start-up");
    }
    // Whatever keyframe came first was just discarded (or is still to come);
    // ask for one, since a still picture would never send another.
    client.request_keyframe();
    let started = Instant::now();
    let mut path_key: Option<String> = None;
    let mut synthetic_step = 0u64;
    let mut cursor_updates = 0u64;
    let mut next_report = Duration::from_secs(1);
    let mut latency = Samples::new(30);
    let mut presented = 0u64;
    let mut superseded = 0u64;
    let mut reported_clean = 0u64;
    let mut menus_version = 0u64;
    let mut stats_on = false;
    // The debug overlay (Control+Option+F10, "Show debug overlay").
    let mut overlay = crate::overlay::Recorder::default();
    let mut overlay_shown = false;
    let codec_name = info.codec.label();
    let mut stream_size = (info.width, info.height);
    let mut resize_remote = false;
    let mut title_detail = String::new();
    let mut notice: Option<String> = None;
    let mut notice_until: Option<Instant> = None;
    let mut status: Option<&str> = Some("Waiting for video…");
    let mut shown_title = String::new();
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d)
            || presenter.closed()
            || crate::control::ending()
        {
            break;
        }
        if let Some(json) = presenter.take_dropped_json() {
            // Paths, or {"path", "x", "y"} for files dropped on the picture
            // (normalized over it): the host drops them there too.
            for dropped in serde_json::from_str::<Vec<serde_json::Value>>(&json).unwrap_or_default()
            {
                let (path, point) = match &dropped {
                    serde_json::Value::String(path) => (Some(path.as_str()), None),
                    entry => (
                        entry["path"].as_str(),
                        entry["x"]
                            .as_u64()
                            .zip(entry["y"].as_u64())
                            .map(|(x, y)| (x.min(65535) as u16, y.min(65535) as u16)),
                    ),
                };
                if let Some(path) = path {
                    crate::transfer::send_dropped(session, path.into(), point);
                }
            }
        }
        if presenter.take_close_request() {
            status = Some("Closing remote window…");
            println!("asking the host to close its window");
            let at_us = clock.now_us();
            lane.lock().unwrap().send(
                &session.media(),
                &clock,
                vec![Captured::Event {
                    event: Event::CloseWindow,
                    at_us,
                }],
            );
        }
        if session.is_closed() {
            println!("the host left");
            break;
        }
        if o.synthetic_input {
            // 125 Hz, as the Windows viewer's generated input.
            let due = started.elapsed().as_millis() as u64 / 8;
            while synthetic_step < due {
                synthetic_step += 1;
                presenter.post_synthetic_input(synthetic_step);
            }
        }
        lane.lock().unwrap().tick(&media, clock.now_us());
        // The connection menus: what changed for this window.
        push_menus();
        let send = |event: Event| {
            let at_us = SystemClock::new().now_us();
            lane.lock().unwrap().send(
                &media,
                &SystemClock::new(),
                vec![Captured::Event { event, at_us }],
            );
        };
        let changes = window_loop.tick(session, send);
        if changes.toggle_relative {
            presenter.set_relative(!RELATIVE.load(std::sync::atomic::Ordering::Relaxed), true);
        }
        if let Some(swap) = changes.swap {
            let swap = swap && !o.physical_keys;
            let mut lane = lane.lock().unwrap();
            if lane.swap_modifiers != swap {
                lane.release_all();
                lane.swap_modifiers = swap;
                println!(
                    "keys: Control and Command {}",
                    if swap { "swapped" } else { "as they are" }
                );
            }
        }
        let version = crate::control::version();
        if version != menus_version || changes.prefs.is_some() {
            menus_version = version;
            let snapshot = crate::control::snapshot();
            stats_on = snapshot.stats;
            let prefs = window_loop.prefs();
            let entry = snapshot.windows.iter().find(|w| w.id == o.menu_id);
            let desktop = entry.is_none_or(|w| w.desktop);
            let resizable = entry.is_some_and(|w| w.resizable) && !desktop;
            resize_remote = prefs.resize_remote && resizable && o.popup.is_none();
            // Popups fill their window, which has the popup's shape.
            let fill = prefs.fill || o.popup.is_some();
            let keep_aspect = !resize_remote && o.popup.is_none();
            presenter.set_view_options(fill, keep_aspect, prefs.physical || o.physical_keys);
            title_detail = if desktop {
                String::new()
            } else {
                format!(
                    " — {}{}",
                    if resize_remote {
                        "Resize remote"
                    } else {
                        "Stretch locally"
                    },
                    if prefs.physical || o.physical_keys {
                        " / Physical keys"
                    } else {
                        ""
                    }
                )
            };
            if snapshot.notice != notice {
                notice = snapshot.notice.clone();
                notice_until = notice
                    .as_ref()
                    .map(|_| Instant::now() + Duration::from_secs(6));
            }
        }
        if resize_remote && let Some(size) = *user_size.lock().unwrap() {
            window_loop.local_size(size, send);
        }
        // The host window resized: a decoder for the new size, and the
        // window in the new shape at its current scale.
        while let Ok(items) = updates.menus.try_recv() {
            println!("host menus: {}", crate::menus::summary(&items));
            crate::control::set_window_details(o.menu_id, None, Some(!items.is_empty()));
            presenter.set_menus(&serde_json::to_string(&items).unwrap_or_default());
        }
        while let Ok((width, height)) = updates.sizes.try_recv() {
            println!("stream resized to {width}x{height}");
            stream_size = (width, height);
            decoder = MacDecoder::new(info.codec.into())?;
            presenter.resize(width, height);
            client.request_keyframe();
        }
        while let Ok(message) = updates.cursor.try_recv() {
            match message {
                frames_input::cursor::CursorMessage::Shape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    bgra,
                } => presenter.set_cursor(
                    u32::from(width),
                    u32::from(height),
                    (u32::from(hotspot_x), u32::from(hotspot_y)),
                    &bgra,
                ),
                frames_input::cursor::CursorMessage::Visible(visible) => {
                    presenter.set_cursor_visible(visible)
                }
            }
            cursor_updates += 1;
        }
        if let Some(clipboard) = &mut clipboard {
            clipboard.tick();
        }
        let path = media.path();
        // Which path carries the stream, when it changes (diagnostics).
        let key = media.selected_path_key();
        if key != path_key {
            println!("path: {}", media.describe_paths().join(" | "));
            path_key = key;
        }
        client.set_rtt(path.rtt_us);
        while let Some((datagram, arrived)) = media.try_recv_timed() {
            if datagram.first() == Some(&crate::audio::TAG) {
                if let Some(audio) = &audio {
                    audio.push(&datagram);
                }
                continue;
            }
            overlay.received(datagram.len());
            client.on_datagram(arrived.unwrap_or_else(|| clock.now_us()), &datagram);
        }
        // Every ready frame is decoded (later ones reference it), but only
        // the newest is shown: after a stall (the window opening, a slow
        // drawable) the older ones are already late.
        // Older frames of a burst go back to the decoder's surface pool as
        // soon as a newer one decodes.
        let (newest, skipped) = client.poll_latest(clock.now_us(), &mut decoder);
        superseded += skipped;
        if let Some(frame) = newest {
            let captured = frame.captured_us;
            match presenter.present(frame, mode) {
                Ok(stamp) => {
                    presented += 1;
                    if status == Some("Waiting for video…") {
                        status = None;
                    }
                    let measured = stamp.presented_us.saturating_sub(captured);
                    latency.push(measured);
                    overlay.presented(measured);
                }
                Err(error) => eprintln!("present failed: {error}"),
            }
        }
        if let Some(feedback) = client.poll_feedback(clock.now_us()) {
            let _ = media.try_send(feedback);
        }
        // The title: the window's name and sizing mode, then a status or
        // a notice (a fetched file), as v1; statistics replace it.
        if notice_until.is_some_and(|until| Instant::now() >= until) {
            notice_until = None;
        }
        // The overlay over the picture; the title keeps the window's name
        // (`--stats-title` puts statistics there for diagnostics).
        if stats_on && overlay.due() {
            let stats = client.stats();
            let audio_counts = audio.as_ref().map(|audio| {
                use std::sync::atomic::Ordering::Relaxed;
                (
                    audio.stats.packets.load(Relaxed),
                    audio.stats.concealed.load(Relaxed),
                )
            });
            if let Some(data) = overlay.snapshot(&crate::overlay::Input {
                codec: codec_name,
                width: stream_size.0,
                height: stream_size.1,
                decoder: "VideoToolbox · hardware",
                path: &path,
                stats,
                superseded,
                audio: audio_counts,
                shortcut: "Control+Option+F10",
            }) {
                presenter.set_overlay(serde_json::to_string(&data).ok().as_deref());
                overlay_shown = true;
            }
        } else if !stats_on && overlay_shown {
            presenter.set_overlay(None);
            overlay_shown = false;
        }
        if !o.stats_title {
            let detail = match (status, notice_until.and(notice.as_deref())) {
                (Some(status), _) => format!(" — {status}"),
                (None, Some(notice)) => format!(" — {notice}"),
                (None, None) => String::new(),
            };
            let title = format!("{base_title}{title_detail}{detail}");
            if title != shown_title {
                presenter.set_title(&title);
                shown_title = title;
            }
        }
        if started.elapsed() >= next_report {
            let stats = client.stats();
            let (n, p50, p95) = latency.take_window();
            if o.stats_title {
                let title = crate::stats::window_title(n, p50, p95, &path, stats.frames_lost);
                presenter.set_title(&title);
                shown_title.clear();
            }
            println!(
                "client t={:>3}s presented {n:>3}/s clean {:>3} lost {} | capture to present p50 {p50:>5.2} p95 {p95:>5.2} ms | recoveries {} nacks {} | {:?} rtt {:.1} ms offset {:.1} ms",
                next_report.as_secs(),
                stats.frames_clean - reported_clean,
                stats.frames_lost,
                stats.recoveries,
                stats.nacks,
                path.kind,
                path.rtt_us as f64 / 1e3,
                client.clock_offset_us() as f64 / 1e3,
            );
            reported_clean = stats.frames_clean;
            lane.lock().unwrap().report_window();
            next_report += Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    drop(presenter);
    let mut lane = lane.lock().unwrap();
    lane.release_all();
    lane.print_summary(cursor_updates);
    if let Some(audio) = &audio {
        use std::sync::atomic::Ordering::Relaxed;
        let stats = &audio.stats;
        println!(
            "AUDIO packets={} concealed={} pcm_chunks={} zero_crossings_per_s={:.1}",
            stats.packets.load(Relaxed),
            stats.concealed.load(Relaxed),
            stats.pcm_chunks.load(Relaxed),
            stats.crossing_rate_x10.load(Relaxed) as f64 / 10.0
        );
    }
    if let Some(clipboard) = &clipboard {
        println!(
            "CLIPBOARD sent={} received={} now={:?}",
            clipboard.sent,
            clipboard.received,
            clipboard.current()
        );
    }
    let stats = client.stats();
    let (n, p50, p95, p99) = latency.summary();
    println!(
        "SUMMARY presented={presented} superseded={superseded} measured={n} lost={} keyframes={} recoveries={} capture_to_present p50={p50:.2} p95={p95:.2} p99={p99:.2} ms (cross-machine: includes clock offset error up to rtt/2)",
        stats.frames_lost, stats.keyframes, stats.recoveries
    );
    session.close("viewer closed");
    Ok(())
}
