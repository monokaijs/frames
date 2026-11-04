//! Two-process v2 media stream over iroh with synthetic capture and codec.
//!
//!   v2_stream host [--relay-only] [--seconds 20]
//!   v2_stream connect <endpoint-id> [--relay-only] [--seconds 20]
//!
//! The host prints its endpoint id; the client dials it by id (relay and
//! discovery find the path). Every second the client prints delivery,
//! latency, recovery, and path; the host prints its target bitrate.

use anyhow::{Context, Result, bail};
use frames_media::api::{CaptureEvent, CaptureSource, EncodeParams, Gop, MediaTransport, PathKind};
use frames_media::client::{ClientConfig, ClientSession};
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_net::{DeviceKey, MediaChannel, Node, NodeConfig};
use frames_testkit::capture::SyntheticCapture;
use frames_testkit::clock::VirtualClock;
use frames_testkit::codec::{EncoderModel, SyntheticDecoder, SyntheticEncoder};
use frames_testkit::netsim::percentile;
use std::sync::Arc;
use std::time::{Duration, Instant};

const RATE: RateConfig = RateConfig {
    min_bps: 1_000_000,
    max_bps: 50_000_000,
    start_bps: 5_000_000,
};

fn path_label(media: &MediaChannel) -> String {
    let path = media.path();
    let kind = match path.kind {
        PathKind::Direct => "direct",
        PathKind::Relay => "relay",
    };
    format!("{kind} rtt {:.1} ms", path.rtt_us as f64 / 1e3)
}

async fn host(seconds: u64, relay_only: bool) -> Result<()> {
    let node = Node::bind(&DeviceKey::generate(), config(relay_only)).await?;
    node.online(Duration::from_secs(10)).await?;
    println!("endpoint id: {}", node.id());
    let session = node.accept().await.context("node closed")??;
    println!(
        "connected: {} pairing code {}",
        session.remote(),
        session.pairing_code()?
    );
    let media = session.media();
    let mut config = HostConfig::new(1, 1, RATE);
    config.max_datagram = media.path().max_datagram.min(1200);
    let mut host = HostSession::new(config);
    if media.path().kind == PathKind::Relay {
        host.set_max_bps(frames_net::N0_RELAY_MAX_BPS);
    }
    let clock = VirtualClock::new(0);
    let started = Instant::now();
    let now_us = || started.elapsed().as_micros() as u64;
    let mut capture = SyntheticCapture::new(&clock, 1920, 1080, 60);
    let params = EncodeParams {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: RATE.start_bps,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: None,
        slices: 2,
    };
    let mut encoder = SyntheticEncoder::new(&clock, params, EncoderModel::default());
    let mut next_report = Duration::from_secs(1);
    let mut path_kind = media.path().kind;
    while started.elapsed() < Duration::from_secs(seconds) {
        clock.advance_to(now_us());
        let now = now_us();
        let path = media.path();
        let rtt = path.rtt_us;
        if path.kind != path_kind {
            path_kind = path.kind;
            host.path_changed();
            host.set_max_bps(match path.kind {
                PathKind::Relay => frames_net::N0_RELAY_MAX_BPS,
                PathKind::Direct => RATE.max_bps,
            });
        }
        if capture.next_due_us() <= now
            && let Ok(CaptureEvent::Frame(frame)) = capture.next(now)
        {
            host.encode(now, &mut encoder, &frame)?;
            media.set_target_rate(u64::from(host.target_bps()));
        }
        while let Some(datagram) = host.poll_transmit(now) {
            let _ = media.try_send(datagram);
        }
        while let Some(datagram) = media.try_recv() {
            host.on_datagram(now_us(), &datagram, rtt);
        }
        if started.elapsed() >= next_report {
            let stats = host.stats();
            println!(
                "host t={:>2}s target {:>5.1} Mbps keyframes {} invalidations {} long-term {} retx {} | {}",
                next_report.as_secs(),
                f64::from(host.target_bps()) / 1e6,
                stats.keyframes,
                stats.invalidations,
                stats.long_term_recoveries,
                stats.retransmissions,
                path_label(&media),
            );
            next_report += Duration::from_secs(1);
        }
        tokio::time::sleep(Duration::from_micros(300)).await;
    }
    session.close("done");
    Ok(())
}

async fn client(id: &str, seconds: u64, relay_only: bool) -> Result<()> {
    let node = Node::bind(&DeviceKey::generate(), config(relay_only)).await?;
    let started_connect = Instant::now();
    let session = node.connect(id.parse::<iroh::EndpointId>()?).await?;
    println!(
        "connected in {:.0} ms, pairing code {}",
        started_connect.elapsed().as_secs_f64() * 1e3,
        session.pairing_code()?
    );
    let media: Arc<MediaChannel> = session.media();
    let mut config = ClientConfig::new(1, 1);
    config.max_datagram = media.path().max_datagram.min(1200);
    let mut client = ClientSession::new(config);
    let mut decoder = SyntheticDecoder::new();
    let started = Instant::now();
    let now_us = || started.elapsed().as_micros() as u64;
    let mut next_report = Duration::from_secs(1);
    let mut reported = (0u64, 0usize);
    while started.elapsed() < Duration::from_secs(seconds) {
        client.set_rtt(media.path().rtt_us);
        while let Some(datagram) = media.try_recv() {
            client.on_datagram(now_us(), &datagram);
        }
        client.poll(now_us(), &mut decoder);
        if let Some(feedback) = client.poll_feedback(now_us()) {
            let _ = media.try_send(feedback);
        }
        if started.elapsed() >= next_report {
            let stats = client.stats();
            let latency = &stats.latency_us[reported.1..];
            println!(
                "client t={:>2}s clean {:>3} fps lost {:>3} | latency p50 {:>6.1} p95 {:>6.1} ms | recovering {} | {}",
                next_report.as_secs(),
                stats.frames_clean - reported.0,
                stats.frames_lost,
                percentile(latency, 50) as f64 / 1e3,
                percentile(latency, 95) as f64 / 1e3,
                client.recovering(),
                path_label(&media),
            );
            reported = (stats.frames_clean, stats.latency_us.len());
            next_report += Duration::from_secs(1);
        }
        tokio::time::sleep(Duration::from_micros(300)).await;
    }
    Ok(())
}

fn config(relay_only: bool) -> NodeConfig {
    NodeConfig {
        relay_only,
        ..NodeConfig::default()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let seconds = args
        .iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .map_or(Ok(20), |s| s.parse())?;
    let relay_only = flag("--relay-only");
    match args.first().map(String::as_str) {
        Some("host") => host(seconds, relay_only).await,
        Some("connect") => {
            client(
                args.get(1).context("connect needs an endpoint id")?,
                seconds,
                relay_only,
            )
            .await
        }
        _ => bail!("usage: v2_stream host|connect <id> [--relay-only] [--seconds N]"),
    }
}
