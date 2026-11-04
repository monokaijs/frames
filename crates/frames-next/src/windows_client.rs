//! Windows viewer: frames-net → v2 client engine → D3D11VA → flip presenter.

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
    let info = tokio::time::timeout(Duration::from_secs(10), receive_info(&session))
        .await
        .context("the host did not describe its stream within 10 s")??;
    println!(
        "stream: {:?} {}x{} at {} fps",
        info.codec, info.width, info.height, info.fps
    );
    tokio::task::spawn_blocking(move || view(&session, info, &o)).await?
}

fn view(session: &frames_net::Session, info: crate::StreamInfo, o: &Options) -> Result<()> {
    win::dpi_aware();
    let _mmcss = Mmcss::games();
    let _timer = TimerResolution::one_millisecond();
    let clock = SystemClock::new();
    let shared: Arc<dyn MediaClock> = Arc::new(clock);
    // Half size keeps the viewer beside the source on one screen.
    let viewer = win::window(
        w!("Frames Next"),
        200,
        (info.width / 2).max(320),
        (info.height / 2).max(180),
    )?;
    win::pump();
    let mut decoder = FfmpegDecoder::new(info.codec.into(), info.width, info.height, true)?;
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

    let started = Instant::now();
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
        win::pump();
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
            next_report += Duration::from_secs(1);
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let stats = client.stats();
    let (n, p50, p95, p99) = latency.summary();
    println!(
        "SUMMARY presented={presented} measured={n} lost={} keyframes={} recoveries={} capture_to_present p50={p50:.2} p95={p95:.2} p99={p99:.2} ms",
        stats.frames_lost, stats.keyframes, stats.recoveries
    );
    session.close("viewer closed");
    Ok(())
}
