//! Windows viewer: frames-net → v2 client engine → D3D11VA → flip presenter.

use crate::lane::{Captured, InputLane};
use crate::stats::Samples;
use crate::{Options, win};
use anyhow::Result;
use frames_codec::ffmpeg::FfmpegDecoder;
use frames_media::api::{MediaClock, MediaTransport, PresentMode, Presenter};
use frames_media::client::{ClientConfig, ClientSession};
use frames_media::clock::SystemClock;
use frames_platform_windows::present::D3D11Presenter;
use frames_platform_windows::system::{Mmcss, TimerResolution};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::w;

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let info = crate::await_stream(&session).await?;
    // Files fetched from the host show in the received-files window.
    crate::win_files::start_shelf();
    // The host's cursor shape and visibility, applied on the viewer thread.
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel();
    let (sizes_tx, sizes) = std::sync::mpsc::channel();
    let (shapes_tx, shapes) = std::sync::mpsc::channel();
    let (menus_tx, menus) = std::sync::mpsc::channel();
    let cursor_reader = {
        let session = session.clone();
        let (cursor_streams, mut cursor_incoming) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(crate::streams::route_viewer(
            session.clone(),
            cursor_streams,
            sizes_tx,
            shapes_tx,
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
    // Buttons, keys, and wheel: one reliable stream, written by a task.
    let (mut lane, writer) = InputLane::open(&session).await?;
    // Control and Command trade places across platforms (as v1) unless
    // the user wants the same physical keys (`--physical-keys`, or the
    // window's options in the connection menu, applied by the view loop).
    lane.swap_modifiers =
        !o.physical_keys && !info.platform.is_empty() && info.platform != crate::platform();
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
            shapes,
            menus,
        };
        view(&session, info, &o, lane, updates, clipboard, audio)
    })
    .await?;
    let _ = writer.await;
    cursor_reader.abort();
    result
}

/// Generated mouse and key messages for unattended runs: a circling pointer
/// at 125 Hz, and a click, a wheel notch, and an "A" key press each second.
struct SyntheticInput {
    hwnd: windows::Win32::Foundation::HWND,
    started: Instant,
    moves: u64,
    seconds: u64,
}

impl SyntheticInput {
    fn post(&self, message: u32, wparam: usize, lparam: isize) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        let _ = unsafe { PostMessageW(Some(self.hwnd), message, WPARAM(wparam), LPARAM(lparam)) };
    }

    fn tick(&mut self) {
        use windows::Win32::UI::WindowsAndMessaging::*;
        let elapsed = self.started.elapsed();
        let due_moves = elapsed.as_millis() as u64 / 8;
        if due_moves > self.moves && win::relative() {
            // 3 right and 1 up per step, as the Mac viewer's generated input.
            win::synthetic_motion(
                3 * (due_moves - self.moves) as i32,
                -((due_moves - self.moves) as i32),
            );
            self.moves = due_moves;
        } else if due_moves > self.moves {
            self.moves = due_moves;
            let angle = self.moves as f64 * 0.05;
            let (x, y) = (
                (240.0 + 160.0 * angle.cos()) as i32,
                (135.0 + 90.0 * angle.sin()) as i32,
            );
            self.post(WM_MOUSEMOVE, 0, ((y as isize) << 16) | x as isize);
        }
        if elapsed.as_secs() > self.seconds {
            self.seconds = elapsed.as_secs();
            let at = (135isize << 16) | 240;
            self.post(WM_LBUTTONDOWN, 1, at);
            self.post(WM_LBUTTONUP, 0, at);
            self.post(WM_MOUSEWHEEL, (120usize) << 16, at);
            // Scan code 0x1e ("A") in bits 16-23.
            self.post(WM_KEYDOWN, 0x41, 1 | (0x1e << 16));
            self.post(WM_KEYUP, 0x41, 1 | (0x1e << 16) | (3 << 30));
            // Every other second an accented character, as an input method
            // or dead key delivers it (text input checks).
            if self.seconds.is_multiple_of(2) {
                self.post(WM_CHAR, 0xE9, 1);
            }
        }
    }
}

fn view(
    session: &frames_net::Session,
    info: crate::StreamInfo,
    o: &Options,
    mut lane: InputLane,
    updates: crate::streams::HostUpdates,
    mut clipboard: Option<crate::clip::ClipboardLink>,
    audio: Option<crate::audio::Receiver>,
) -> Result<()> {
    win::dpi_aware();
    let _mmcss = Mmcss::games();
    let _timer = TimerResolution::one_millisecond();
    let clock = SystemClock::new();
    let shared: Arc<dyn MediaClock> = Arc::new(clock);
    // The picture at its own size when it fits, else as large as fits on
    // the screen, always in its own shape.
    let mut stream_size = (info.width, info.height);
    // A host menu, tooltip or owned dialog goes where its owner's picture
    // shows it, at the owner's scale (v1); other windows open at their own
    // size within v1's limits.
    let window_info = o.window_info.clone();
    let popup = window_info.as_ref().is_some_and(|w| w.is_popup());
    let placement_log = std::cell::RefCell::new(String::new());
    // Where the parent's viewer shows this window now, if it is there.
    // Both as the host lists them now (they move and resize on the host),
    // else as they were when this window opened.
    let parent_placement = |stream_size: (u32, u32)| match (&window_info, &o.parent_info) {
        (Some(window), Some(parent)) => {
            let window = crate::catalog::latest(window.id).unwrap_or_else(|| window.clone());
            let parent = crate::catalog::latest(parent.id).unwrap_or_else(|| parent.clone());
            win::viewer_placement(parent.id).map(|(owner, x, y, scale, parent_stream)| {
                let (at, size) = crate::resize::popup_place(
                    (window.x, window.y, window.width, window.height),
                    (parent.x, parent.y, parent.width, parent.height),
                    parent_stream,
                    (x, y, scale),
                    stream_size,
                );
                // Diagnostics: every input of a placement that differs from
                // the last one (popups that move after opening).
                let inputs = format!(
                    "window {:.0},{:.0} {:.0}x{:.0}, parent {:.0},{:.0} {:.0}x{:.0}, parent picture {}x{} at {},{} scale {:.3}, picture {}x{} -> at {},{} size {}x{}",
                    window.x, window.y, window.width, window.height,
                    parent.x, parent.y, parent.width, parent.height,
                    parent_stream.0, parent_stream.1, x, y, scale,
                    stream_size.0, stream_size.1, at.0, at.1, size.0, size.1
                );
                if placement_log.replace(inputs.clone()) != inputs {
                    println!("placement: {inputs}");
                }
                (owner, at, size)
            })
        }
        _ => None,
    };
    let until = Instant::now() + Duration::from_secs(1);
    let mut placement = parent_placement(stream_size);
    while placement.is_none() && o.parent_info.is_some() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
        placement = parent_placement(stream_size);
    }
    // A popup follows its parent's picture (`crate::resize::Follow`): it
    // waits hidden until the parent's viewer is placed, then moves with it.
    let mut follow = crate::resize::Follow::default();
    // An owned dialog whose parent's viewer was not there yet is placed
    // over it once it is (then the user may move it).
    let mut place_dialog = placement.is_none() && o.parent_info.is_some() && !popup;
    let viewer = match placement {
        Some((owner, at, size)) if popup => {
            follow.update(Some((at, size)));
            win::popup_window(Some(owner), at.0, at.1, size.0, size.1)?
        }
        // Always frameless, even before its parent's viewer exists.
        None if popup => {
            let (width, height) = stream_size;
            let hwnd = win::popup_window(None, 0, 0, width.max(1), height.max(1))?;
            win::hide(hwnd);
            println!("popup waits for its window's viewer to place it");
            hwnd
        }
        _ => {
            let (width, height) = match placement {
                Some((_, _, size)) => size,
                None => win::initial_size(stream_size),
            };
            let viewer = win::window(w!("Frames Next"), win::Kind::Viewer, 200, width, height)?;
            if let Some((owner, at, _)) = placement {
                win::own_and_place(viewer, Some(owner), Some(at));
            }
            viewer
        }
    };
    if popup {
        // The picture is exactly the window.
        win::set_layout(true, false);
    }
    // The host's measured outline of this popup's picture (transparent
    // corners), applied to its window whenever it or the window changes.
    let mut shape: Option<crate::resize::Shape> = None;
    let mut shaped_area = ((0, 0), (0, 0));
    if let Some(window) = &window_info {
        if let Some(icon) = &window.icon {
            win::set_icon(viewer, icon);
        }
        if !popup && !window.app.is_empty() {
            win::set_app_id(viewer, &info.name, &window.app);
        }
    }
    win::set_aspect(info.width, info.height);
    win::set_title(viewer, &crate::viewer_title(o, &info));
    win::set_menu_context(o.menu_id, &info.platform);
    win::set_hide_button(viewer, o.window_id.is_some() && o.menu_id != 0 && !popup);
    win::session::start_connection_window();
    win::pump();
    // The connection menu's choices for this window (`crate::control`).
    let mut menus = crate::control::WindowLoop::new(o, &info);
    menus.resize.streaming(stream_size);
    let mut menu_version = 0;
    let mut entry = crate::control::WindowEntry::default();
    let mut notice: Option<(String, Instant)> = None;
    let mut title = String::new();
    let mut stats_on = false;
    // The debug overlay (Ctrl+Alt+F10, "Debug overlay"): v1's panel over the
    // picture's top left.
    let mut overlay = crate::overlay::Recorder::default();
    let mut panel: Option<crate::win_overlay::DebugPanel> = None;
    let mut fill = false;
    let mut user_resize_until: Option<Instant> = None;
    let mut last_placement = Instant::now() - Duration::from_secs(1);
    // Whether the decoder in use is the hardware one (the debug overlay).
    let hardware_decode = std::cell::Cell::new(true);
    let open_decoder = |(width, height): (u32, u32)| -> Result<FfmpegDecoder> {
        Ok(
            match FfmpegDecoder::new(info.codec.into(), width, height, true) {
                Ok(decoder) => {
                    hardware_decode.set(true);
                    decoder
                }
                Err(error) => {
                    println!("hardware decode unavailable ({error}); decoding in software");
                    hardware_decode.set(false);
                    FfmpegDecoder::new(info.codec.into(), width, height, false)?
                }
            },
        )
    };
    let mut decoder = open_decoder(stream_size)?;
    let mut presenter = D3D11Presenter::new(viewer, shared);
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
    if o.relative_qa {
        win::set_relative(viewer, true, false);
    }
    let started = Instant::now();
    let mut path_key: Option<String> = None;
    let mut synthetic = o.synthetic_input.then_some(SyntheticInput {
        hwnd: viewer,
        started,
        moves: 0,
        seconds: 0,
    });
    let mut cursor_shapes = 0u64;
    let mut next_report = Duration::from_secs(1);
    let mut latency = Samples::new(30);
    let mut presented = 0u64;
    let mut redraw = crate::resize::Redraw::<frames_media::api::DecodedFrame>::new();
    let mut superseded = 0u64;
    let mut errors = 0u64;
    let mut reported_clean = 0u64;
    let mut close_asked = false;
    let mut showing_stats = false;
    let mut stats_line = String::new();
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) || crate::control::ending() {
            break;
        }
        // A window of a host application: the first close asks the host
        // window to close (its going away ends this session); a second one
        // closes here.
        let dropped = win::take_dropped(viewer);
        if !dropped.is_empty() {
            // Over the picture: the host drops the files there too.
            let picture = frames_platform_windows::present::picture_rect(
                stream_size,
                win::client_size(viewer),
                fill,
            );
            let point = win::take_drop_point().map(|(x, y)| {
                (
                    frames_input::normalize(
                        x - picture.left,
                        (picture.right - picture.left).max(1) as u32,
                    ),
                    frames_input::normalize(
                        y - picture.top,
                        (picture.bottom - picture.top).max(1) as u32,
                    ),
                )
            });
            for path in dropped {
                crate::transfer::send_dropped(session, path, point);
            }
        }
        if win::close_requested() {
            if o.window_id.is_none() || close_asked {
                break;
            }
            close_asked = true;
            win::clear_close();
            println!("asking the host to close its window");
            let at_us = clock.now_us();
            lane.send(
                &media,
                &clock,
                vec![Captured::Event {
                    event: frames_input::Event::CloseWindow,
                    at_us,
                }],
            );
        }
        if session.is_closed() {
            println!("the host left");
            break;
        }
        if let Some(synthetic) = &mut synthetic {
            synthetic.tick();
        }
        let mut cursor_updates = 0u64;
        // The host window resized: a decoder for the new size, and this
        // window in the new shape at its current scale.
        while let Ok(items) = updates.menus.try_recv() {
            println!("host menus: {}", crate::menus::summary(&items));
            crate::control::set_window_details(o.menu_id, None, Some(!items.is_empty()));
            // A popup (menu, tooltip, sheet) is the picture alone: a menu
            // row on it would push the picture down (its app's menus are
            // on its main window).
            if !popup {
                win::set_host_menu(viewer, &items);
            }
        }
        // A popup's outline: clipped again when the host measures a new one
        // or the window or picture changes size.
        let mut reshape = false;
        while let Ok(new) = updates.shapes.try_recv() {
            // Diagnostics: the bands as received (rows, left, right).
            match &new {
                Some(s) => println!(
                    "picture shape: transparent corners, top {:?}, bottom {:?} (picture {}x{})",
                    s.top, s.bottom, stream_size.0, stream_size.1
                ),
                None => println!("picture shape: rectangle"),
            }
            // A/B check: FRAMES_SHAPE=off keeps popups rectangular.
            if std::env::var("FRAMES_SHAPE").is_ok_and(|v| v == "off") {
                println!("picture shape not applied (FRAMES_SHAPE=off)");
                continue;
            }
            shape = new;
            reshape = true;
        }
        let shaping = (win::client_size(viewer), stream_size);
        if popup && (reshape || (shape.is_some() && shaping != shaped_area)) {
            win::shape_window(viewer, shape.as_ref(), stream_size);
            shaped_area = shaping;
        }
        // The menus changed: this window's entry, the statistics, a notice.
        if crate::control::version() != menu_version {
            menu_version = crate::control::version();
            let snapshot = crate::control::snapshot();
            if let Some(found) = snapshot.windows.iter().find(|w| w.id == o.menu_id) {
                entry = found.clone();
            }
            stats_on = snapshot.stats;
            if let Some(text) = &snapshot.notice
                && notice.as_ref().is_none_or(|(old, _)| old != text)
            {
                notice = Some((text.clone(), Instant::now()));
            }
        }
        let resize_remote = entry.prefs.resize_remote && entry.resizable && !entry.desktop;
        let mut host_events = Vec::new();
        let changes = menus.tick(session, |event| host_events.push(event));
        if changes.toggle_relative {
            win::set_relative(viewer, !win::relative(), true);
        }
        if let Some(prefs) = &changes.prefs
            && let Some(swap) = changes.swap
        {
            // Settings' physical keys hold until this window's own choice.
            let swap = swap && !(o.physical_keys && prefs.swap.is_none());
            lane.swap_modifiers = swap;
            println!(
                "keys: Control and Command {}",
                if swap { "swapped" } else { "as they are" }
            );
        }
        if let Some(prefs) = changes.prefs {
            presenter.set_fill(prefs.fill);
            fill = prefs.fill;
            // Physical keys (the window's option or Settings') send every
            // key as a key; otherwise typed characters go as text.
            win::set_physical(prefs.physical || o.physical_keys);
            println!(
                "window options: {}{}{}",
                if prefs.resize_remote {
                    "resize remote"
                } else {
                    "stretch locally"
                },
                if prefs.fill { ", fill" } else { "" },
                if prefs.header { ", header" } else { "" },
            );
        }
        if !popup {
            win::set_layout(entry.prefs.fill, !resize_remote);
        }
        if last_placement.elapsed() >= Duration::from_millis(100) {
            last_placement = Instant::now();
            win::register_viewer(o.menu_id, viewer, stream_size);
            let now = parent_placement(stream_size);
            if popup {
                let owner = now.map(|(owner, _, _)| owner);
                if let (Some(owner), Some((at, size))) =
                    (owner, follow.update(now.map(|(_, at, size)| (at, size))))
                {
                    win::place_popup(viewer, owner, at.0, at.1, size.0, size.1);
                    redraw.invalidate();
                }
            } else if place_dialog && let Some((owner, at, _)) = now {
                win::own_and_place(viewer, Some(owner), Some(at));
                place_dialog = false;
            }
        }
        while let Ok(size) = updates.sizes.try_recv() {
            println!("stream resized to {}x{}", size.0, size.1);
            decoder = open_decoder(size)?;
            let screen = win::work_area();
            let current = win::client_size(viewer);
            // "Resize remote app": the host followed this window (or its
            // header changed); pixels stay 1:1 unless that does not fit.
            let (width, height) = if resize_remote {
                crate::resize::window_size(size, size, size, screen)
            } else {
                crate::resize::window_size(current, stream_size, size, screen)
            };
            win::set_aspect(size.0, size.1);
            if (width, height) != current {
                win::set_client_size(viewer, width, height);
            }
            stream_size = size;
            menus.resize = crate::control::RemoteResize::default();
            menus.resize.streaming(win::client_size(viewer));
            client.request_keyframe();
        }
        // Only the user's own resizing resizes the host window (v1).
        if win::take_user_resize() {
            user_resize_until = Some(Instant::now() + Duration::from_secs(1));
        }
        if resize_remote && user_resize_until.is_some_and(|until| Instant::now() < until) {
            menus.local_size(win::client_size(viewer), |event| host_events.push(event));
        }
        if !host_events.is_empty() {
            let at_us = clock.now_us();
            lane.send(
                &media,
                &clock,
                host_events
                    .into_iter()
                    .map(|event| Captured::Event { event, at_us })
                    .collect(),
            );
        }
        // v1's title: the window, its sizing mode, keys and any notice.
        let wanted = if showing_stats {
            stats_line.clone()
        } else {
            let mut wanted = format!(
                "{} — {}",
                crate::viewer_title(o, &info),
                if resize_remote {
                    "Resize remote"
                } else {
                    "Stretch locally"
                }
            );
            if entry.prefs.physical {
                wanted.push_str(" / Physical keys");
            }
            // v1's statuses, then any recent notice.
            let status = if close_asked {
                Some("Closing remote window…")
            } else if presented == 0 {
                Some("Waiting for video…")
            } else {
                None
            };
            if let Some(status) = status {
                wanted.push_str(" — ");
                wanted.push_str(status);
            } else if let Some((text, at)) = &notice
                && at.elapsed() < Duration::from_secs(8)
            {
                wanted.push_str(" — ");
                wanted.push_str(text);
            }
            wanted
        };
        if wanted != title {
            win::set_title(viewer, &wanted);
            title = wanted;
        }
        while let Ok(message) = updates.cursor.try_recv() {
            win::apply_cursor(viewer, &message);
            cursor_updates += 1;
        }
        if cursor_updates > 0 && cursor_shapes == 0 {
            println!("client cursor: host cursor shown");
        }
        cursor_shapes += cursor_updates;
        win::pump();
        lane.send(&media, &clock, win::take_input());
        lane.tick(&media, clock.now_us());
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
        // Pointer mapping, the overlay and the window's shape follow the
        // pictures themselves when they differ from the announced size.
        if let Some(size) = newest.as_ref().map(|f| f.surface.size())
            && size != stream_size
            && size.0 > 0
            && size.1 > 0
        {
            println!(
                "pictures are {}x{}, not the announced {}x{}: following them",
                size.0, size.1, stream_size.0, stream_size.1
            );
            stream_size = size;
            win::set_aspect(size.0, size.1);
            win::register_viewer(o.menu_id, viewer, stream_size);
        }
        // No new frame, but the drawing area changed: show the last one
        // again (a still picture would otherwise stay blank).
        let area = win::client_size(viewer);
        let again = if newest.is_none() {
            redraw.due(area)
        } else {
            None
        };
        if let Some(frame) = again
            && presenter.present(frame.clone(), mode).is_ok()
        {
            redraw.shown(frame, area);
        }
        if let Some(frame) = newest {
            let captured = frame.captured_us;
            let kept = frame.clone();
            match presenter.present(frame, mode) {
                Ok(stamp) => {
                    redraw.shown(kept, area);
                    presented += 1;
                    let measured = stamp.presented_us.saturating_sub(captured);
                    latency.push(measured);
                    overlay.presented(measured);
                }
                Err(error) => {
                    errors += 1;
                    if errors <= 3 {
                        eprintln!("present failed: {error}");
                    }
                }
            }
        }
        if let Some(feedback) = client.poll_feedback(clock.now_us()) {
            let _ = media.try_send(feedback);
        }
        // Not on popups, and only where it fits (the panel checks).
        if stats_on && !popup && overlay.due() {
            let audio_counts = audio.as_ref().map(|audio| {
                use std::sync::atomic::Ordering::Relaxed;
                (
                    audio.stats.packets.load(Relaxed),
                    audio.stats.concealed.load(Relaxed),
                )
            });
            let decoder_label = if hardware_decode.get() {
                "FFmpeg D3D11VA · hardware"
            } else {
                "FFmpeg · software"
            };
            if let Some(data) = overlay.snapshot(&crate::overlay::Input {
                codec: info.codec.label(),
                width: stream_size.0,
                height: stream_size.1,
                decoder: decoder_label,
                path: &path,
                stats: client.stats(),
                superseded,
                audio: audio_counts,
                shortcut: "Ctrl+Alt+F10",
            }) {
                if panel.is_none() {
                    match unsafe { crate::win_overlay::DebugPanel::new(viewer) } {
                        Ok(created) => panel = Some(created),
                        Err(error) => println!("debug overlay unavailable: {error:#}"),
                    }
                }
                if let Some(panel) = &panel {
                    let content = frames_platform_windows::present::picture_rect(
                        stream_size,
                        win::client_size(viewer),
                        fill,
                    );
                    if let Err(error) = unsafe { panel.show(&data, content) } {
                        println!("debug overlay: {error:#}");
                    }
                }
            }
        } else if !stats_on && let Some(shown) = panel.take() {
            unsafe { shown.hide() };
        }
        if started.elapsed() >= next_report {
            let stats = client.stats();
            let (n, p50, p95) = latency.take_window();
            // The overlay shows them; the title only for diagnostics.
            showing_stats = o.stats_title;
            if showing_stats {
                stats_line = crate::stats::window_title(n, p50, p95, &path, stats.frames_lost);
            }
            println!(
                "client t={:>3}s presented {n:>3}/s clean {:>3} lost {} | capture to present p50 {p50:>5.2} p95 {p95:>5.2} ms | recoveries {} nacks {} | {:?} rtt {:.1} ms",
                next_report.as_secs(),
                stats.frames_clean - reported_clean,
                stats.frames_lost,
                stats.recoveries,
                stats.nacks,
                path.kind,
                path.rtt_us as f64 / 1e3,
            );
            reported_clean = stats.frames_clean;
            lane.report_window();
            next_report += Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    // Give the pointer back before the window goes.
    win::set_relative(viewer, false, true);
    let _ = win::take_input();
    win::unregister_viewer(o.menu_id);
    win::destroy(viewer);
    lane.release_all();
    lane.print_summary(cursor_shapes);
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
        "SUMMARY presented={presented} superseded={superseded} measured={n} lost={} keyframes={} recoveries={} capture_to_present p50={p50:.2} p95={p95:.2} p99={p99:.2} ms",
        stats.frames_lost, stats.keyframes, stats.recoveries
    );
    session.close("viewer closed");
    Ok(())
}
