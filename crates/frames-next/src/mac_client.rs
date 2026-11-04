//! macOS viewer: frames-net → v2 client engine → VideoToolbox → Metal.
//!
//! View only for now: the input lane and the host cursor are Windows-only in
//! the viewer (the host still accepts a viewer that sends neither).

use crate::stats::Samples;
use crate::{Options, receive_info};
use anyhow::{Context, Result};
use frames_media::api::{MediaClock, MediaTransport, PresentMode, Presenter};
use frames_media::client::{ClientConfig, ClientSession};
use frames_media::clock::SystemClock;
use frames_platform_macos::decoder::MacDecoder;
use frames_platform_macos::presenter::MacPresenter;
use frames_platform_macos::system;
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let info = tokio::time::timeout(Duration::from_secs(10), receive_info(&session))
        .await
        .context("the host did not describe its stream within 10 s")??;
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
    let result = tokio::task::spawn_blocking(move || view(&session, info, &o)).await?;
    drain.abort();
    result
}

fn view(session: &frames_net::Session, info: crate::StreamInfo, o: &Options) -> Result<()> {
    let frame = Duration::from_micros(1_000_000 / u64::from(info.fps.max(1)));
    let _ = system::set_realtime_thread(frame, frame / 4, frame / 2);
    let clock = SystemClock::new();
    let mut decoder = MacDecoder::new(info.codec.into())?;
    // The window needs NSApplication on the main thread (frames-next runs it).
    let mut presenter = MacPresenter::new(info.width / 2, info.height / 2, true)?;
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
            next_report += Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let stats = client.stats();
    let (n, p50, p95, p99) = latency.summary();
    println!(
        "SUMMARY presented={presented} measured={n} lost={} keyframes={} recoveries={} capture_to_present p50={p50:.2} p95={p95:.2} p99={p99:.2} ms (cross-machine: includes clock offset error up to rtt/2)",
        stats.frames_lost, stats.keyframes, stats.recoveries
    );
    session.close("viewer closed");
    Ok(())
}
