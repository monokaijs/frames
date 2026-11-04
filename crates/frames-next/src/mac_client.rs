//! macOS viewer: frames-net → v2 client engine → VideoToolbox → Metal.
//!
//! Input from the window goes to the host on the shared input lane. The
//! host's cursor shape is not shown yet (the Mac's own cursor stays).

use crate::lane::{Captured, InputLane};
use crate::stats::Samples;
use crate::{Options, receive_info};
use anyhow::{Context, Result};
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
    // A host seeing this device for the first time asks its user (60 s).
    println!("waiting for the host (it may be asking to approve this device)");
    let info = tokio::time::timeout(Duration::from_secs(90), receive_info(&session))
        .await
        .context("the host did not start the stream within 90 s")??;
    println!(
        "stream: {:?} {}x{} at {} fps",
        info.codec, info.width, info.height, info.fps
    );
    // Accept (and drain) the host's cursor stream so its writes never block.
    let drain = {
        let session = session.clone();
        tokio::spawn(async move {
            if let Ok((_send, mut recv)) = session.accept_channel().await {
                let mut buffer = vec![0u8; 64 * 1024];
                while let Ok(Some(_)) = recv.read(&mut buffer).await {}
            }
        })
    };
    let (lane, writer) = InputLane::open(&session).await?;
    let result = tokio::task::spawn_blocking(move || view(&session, info, &o, lane)).await?;
    let _ = writer.await;
    drain.abort();
    result
}

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
        MacInput::FocusLost { at_us } => Captured::Event {
            event: Event::ReleaseAll,
            at_us,
        },
    })
}

fn view(
    session: &frames_net::Session,
    info: crate::StreamInfo,
    o: &Options,
    lane: InputLane,
) -> Result<()> {
    let frame = Duration::from_micros(1_000_000 / u64::from(info.fps.max(1)));
    let _ = system::set_realtime_thread(frame, frame / 4, frame / 2);
    let clock = SystemClock::new();
    let mut decoder = MacDecoder::new(info.codec.into())?;
    // The window needs NSApplication on the main thread (frames-next runs it).
    let mut presenter = MacPresenter::new(info.width / 2, info.height / 2, true)?;
    // Events are sent from the main thread as they arrive: the viewer loop
    // can wait up to a frame for a drawable, which would delay input.
    let lane = Arc::new(Mutex::new(lane));
    {
        let (lane, media) = (lane.clone(), session.media());
        presenter.set_input(move |input| {
            if let Some(item) = captured(input) {
                lane.lock()
                    .unwrap()
                    .send(&media, &SystemClock::new(), vec![item]);
            }
        });
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
    let started = Instant::now();
    let mut synthetic_step = 0u64;
    let mut next_report = Duration::from_secs(1);
    let mut latency = Samples::new(30);
    let mut presented = 0u64;
    let mut reported_clean = 0u64;
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) {
            break;
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
                Err(error) => eprintln!("present failed: {error}"),
            }
        }
        if let Some(feedback) = client.poll_feedback(clock.now_us()) {
            let _ = media.try_send(feedback);
        }
        if started.elapsed() >= next_report {
            let stats = client.stats();
            let (n, p50, p95) = latency.take_window();
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
    lane.print_summary(0);
    let stats = client.stats();
    let (n, p50, p95, p99) = latency.summary();
    println!(
        "SUMMARY presented={presented} measured={n} lost={} keyframes={} recoveries={} capture_to_present p50={p50:.2} p95={p95:.2} p99={p99:.2} ms (cross-machine: includes clock offset error up to rtt/2)",
        stats.frames_lost, stats.keyframes, stats.recoveries
    );
    session.close("viewer closed");
    Ok(())
}
