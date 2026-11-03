//! Virtual-time end-to-end runs of the v2 media engine.
//!
//! Synthetic capture → [`HostSession`] → simulated links → [`ClientSession`]
//! → synthetic decoder, with feedback flowing back. The loop jumps the clock
//! straight to the next event, so seconds of media run in milliseconds and a
//! seed replays exactly.

use crate::capture::SyntheticCapture;
use crate::clock::VirtualClock;
use crate::codec::{EncoderModel, SyntheticDecoder, SyntheticEncoder};
use crate::link::{LinkConfig, LinkStats};
use crate::transport;
use frames_media::api::{
    CaptureEvent, CaptureSource, EncodeParams, EncoderFeatures, MediaClock, MediaTransport,
};
use frames_media::client::{ClientConfig, ClientSession, ClientStats};
use frames_media::host::{HostConfig, HostSession, HostStats};

#[derive(Clone, Debug)]
pub struct Scenario {
    pub seconds: f64,
    pub fps: u32,
    /// Host → client.
    pub link: LinkConfig,
    /// Client → host.
    pub back: LinkConfig,
    /// `(at_us, config)`: host → client conditions change at `at_us`.
    pub changes: Vec<(u64, LinkConfig)>,
    pub host: HostConfig,
    pub client: ClientConfig,
    pub encoder: EncodeParams,
    pub model: EncoderModel,
    /// Encoder features to advertise (`None` = everything the synthetic
    /// encoder implements).
    pub features: Option<EncoderFeatures>,
    pub seed: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub at_us: u64,
    pub target_bps: u32,
    pub queued_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub captured: u64,
    pub host: HostStats,
    pub client: ClientStats,
    pub link: LinkStats,
    pub trace: Vec<Sample>,
}

impl Report {
    pub fn latency_percentile_us(&self, percent: usize) -> u64 {
        percentile(&self.client.latency_us, percent)
    }

    pub fn recovery_percentile_us(&self, percent: usize) -> u64 {
        percentile(&self.client.recovery_us, percent)
    }

    /// Clean frames per second over `[from_us, to_us)` is not tracked per
    /// frame; this is the overall clean fraction of captured frames.
    pub fn clean_fraction(&self) -> f64 {
        self.client.frames_clean as f64 / self.captured.max(1) as f64
    }

    /// The target bitrate at `at_us` (last sample at or before it).
    pub fn target_at(&self, at_us: u64) -> u32 {
        self.trace
            .iter()
            .take_while(|s| s.at_us <= at_us)
            .last()
            .map_or(0, |s| s.target_bps)
    }
}

pub fn percentile(values: &[u64], percent: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() - 1) * percent.min(100) / 100]
}

const SAMPLE_US: u64 = 50_000;

pub fn run(scenario: &Scenario) -> Report {
    let clock = VirtualClock::new(0);
    let mut capture = SyntheticCapture::new(
        &clock,
        scenario.encoder.width,
        scenario.encoder.height,
        scenario.fps,
    );
    let mut encoder = SyntheticEncoder::new(&clock, scenario.encoder, scenario.model);
    if let Some(features) = scenario.features {
        encoder.set_features(features);
    }
    let mut decoder = SyntheticDecoder::new();
    let (host_end, client_end) =
        transport::pair(&clock, scenario.link, scenario.back, scenario.seed);
    let mut host = HostSession::new(scenario.host);
    let mut client = ClientSession::new(scenario.client);
    let mut changes = scenario.changes.clone();
    changes.sort_by_key(|(at, _)| *at);
    let mut changes = changes.into_iter().peekable();
    let end_us = (scenario.seconds * 1e6) as u64;
    let mut captured = 0;
    let mut trace = Vec::new();
    let mut next_sample = 0;

    loop {
        let now = clock.now_us();
        if now >= end_us {
            break;
        }
        while let Some((_, config)) = changes.next_if(|(at, _)| *at <= now) {
            host_end.set_send_config(config);
        }
        let rtt = host_end.path().rtt_us;
        client.set_rtt(rtt);
        if capture.next_due_us() <= now
            && let Ok(CaptureEvent::Frame(frame)) = capture.next(now)
        {
            captured += 1;
            host.encode(now, &mut encoder, &frame)
                .expect("synthetic encoder accepts every frame");
        }
        while let Some(datagram) = host.poll_transmit(now) {
            let _ = host_end.try_send(datagram);
        }
        while let Some(datagram) = client_end.try_recv() {
            client.on_datagram(now, &datagram);
        }
        client.poll(now, &mut decoder);
        if let Some(feedback) = client.poll_feedback(now) {
            let _ = client_end.try_send(feedback);
        }
        while let Some(datagram) = host_end.try_recv() {
            host.on_datagram(now, &datagram, rtt);
        }
        if now >= next_sample {
            trace.push(Sample {
                at_us: now,
                target_bps: host.target_bps(),
                queued_bytes: host_end.send_queued_bytes(),
            });
            next_sample = now + SAMPLE_US;
        }

        let mut next = capture.next_due_us().min(next_sample).min(end_us);
        for candidate in [
            host.next_transmit_us(now),
            host_end.next_arrival_us(),
            client_end.next_arrival_us(),
            Some(client.next_timeout_us(now)),
            changes.peek().map(|(at, _)| *at),
        ]
        .into_iter()
        .flatten()
        {
            next = next.min(candidate);
        }
        clock.advance_to(next.max(now + 1));
    }

    Report {
        captured,
        host: host.stats(),
        client: client.stats().clone(),
        link: host_end.send_stats(),
        trace,
    }
}
