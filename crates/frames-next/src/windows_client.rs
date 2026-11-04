//! Windows viewer: frames-net → v2 client engine → D3D11VA → flip presenter.

use crate::lane::InputLane;
use crate::stats::Samples;
use crate::{Options, receive_info, win};
use anyhow::{Context, Result};
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
    // A host seeing this device for the first time asks its user (60 s).
    println!("waiting for the host (it may be asking to approve this device)");
    let info = tokio::time::timeout(Duration::from_secs(90), receive_info(&session))
        .await
        .context("the host did not start the stream within 90 s")??;
    println!(
        "stream: {:?} {}x{} at {} fps",
        info.codec, info.width, info.height, info.fps
    );
    // The host's cursor shape and visibility, applied on the viewer thread.
    let (cursor_tx, cursor_rx) = std::sync::mpsc::channel();
    let cursor_reader = {
        let session = session.clone();
        tokio::spawn(async move {
            let Ok((_send, mut recv)) = session.accept_channel().await else {
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
    let (lane, writer) = InputLane::open(&session).await?;
    let result =
        tokio::task::spawn_blocking(move || view(&session, info, &o, lane, cursor_rx)).await?;
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
        if due_moves > self.moves {
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
        }
    }
}

fn view(
    session: &frames_net::Session,
    info: crate::StreamInfo,
    o: &Options,
    mut lane: InputLane,
    cursor_rx: std::sync::mpsc::Receiver<frames_input::cursor::CursorMessage>,
) -> Result<()> {
    win::dpi_aware();
    let _mmcss = Mmcss::games();
    let _timer = TimerResolution::one_millisecond();
    let clock = SystemClock::new();
    let shared: Arc<dyn MediaClock> = Arc::new(clock);
    // Half size keeps the viewer beside the source on one screen.
    let viewer = win::window(
        w!("Frames Next"),
        win::Kind::Viewer,
        200,
        (info.width / 2).max(320),
        (info.height / 2).max(180),
    )?;
    win::pump();
    let mut decoder = match FfmpegDecoder::new(info.codec.into(), info.width, info.height, true) {
        Ok(decoder) => decoder,
        Err(error) => {
            println!("hardware decode unavailable ({error}); decoding in software");
            FfmpegDecoder::new(info.codec.into(), info.width, info.height, false)?
        }
    };
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
    let started = Instant::now();
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
    let mut errors = 0u64;
    let mut reported_clean = 0u64;
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) || win::close_requested() {
            break;
        }
        if session.is_closed() {
            println!("the host left");
            break;
        }
        if let Some(synthetic) = &mut synthetic {
            synthetic.tick();
        }
        let mut cursor_updates = 0u64;
        while let Ok(message) = cursor_rx.try_recv() {
            win::apply_cursor(&message);
            cursor_updates += 1;
        }
        if cursor_updates > 0 && cursor_shapes == 0 {
            println!("client cursor: host cursor shown");
        }
        cursor_shapes += cursor_updates;
        win::pump();
        lane.send(&media, &clock, win::take_input());
        let path = media.path();
        client.set_rtt(path.rtt_us);
        while let Some(datagram) = media.try_recv() {
            client.on_datagram(clock.now_us(), &datagram);
        }
        for frame in client.poll(clock.now_us(), &mut decoder) {
            let captured = frame.captured_us;
            match presenter.present(frame, mode) {
                Ok(stamp) => {
                    presented += 1;
                    latency.push(stamp.presented_us.saturating_sub(captured));
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
        if started.elapsed() >= next_report {
            let stats = client.stats();
            let (n, p50, p95) = latency.take_window();
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
    lane.release_all();
    lane.print_summary(cursor_shapes);
    let stats = client.stats();
    let (n, p50, p95, p99) = latency.summary();
    println!(
        "SUMMARY presented={presented} measured={n} lost={} keyframes={} recoveries={} capture_to_present p50={p50:.2} p95={p95:.2} p99={p99:.2} ms",
        stats.frames_lost, stats.keyframes, stats.recoveries
    );
    session.close("viewer closed");
    Ok(())
}
