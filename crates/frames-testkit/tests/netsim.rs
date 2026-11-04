//! Phase 2 gates for the v2 media engine (docs/V2_PLAN.md), in virtual time.
//!
//! `cargo test --release -p frames-testkit --test netsim -- --nocapture`
//! also prints a summary line per scenario.

use frames_media::api::{EncodeParams, EncoderFeatures, Gop};
use frames_media::client::ClientConfig;
use frames_media::host::HostConfig;
use frames_media::rate::RateConfig;
use frames_testkit::codec::EncoderModel;
use frames_testkit::link::{Burst, LinkConfig};
use frames_testkit::netsim::{Report, Scenario, run};

fn scenario(link: LinkConfig, seconds: f64) -> Scenario {
    let rate = RateConfig {
        min_bps: 1_000_000,
        max_bps: 80_000_000,
        start_bps: 20_000_000,
    };
    Scenario {
        seconds,
        fps: 60,
        link,
        back: LinkConfig {
            loss: 0.0,
            burst: None,
            ..link
        },
        changes: Vec::new(),
        host: HostConfig::new(1, 1, rate),
        client: ClientConfig::new(1, 1),
        encoder: EncodeParams {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: rate.start_bps,
            vbv_frames: 1.0,
            gop: Gop::Infinite,
            intra_refresh_frames: Some(6),
            slices: 4,
        },
        model: EncoderModel::default(),
        features: None,
        seed: 7,
    }
}

fn internet(rtt_ms: u64) -> LinkConfig {
    LinkConfig {
        delay_us: rtt_ms * 500,
        jitter_us: 1_000,
        reorder: false,
        loss: 0.0,
        burst: None,
        rate_bps: Some(100_000_000),
        queue_bytes: 512 << 10,
    }
}

fn summary(name: &str, report: &Report) {
    let c = &report.client;
    let h = &report.host;
    println!(
        "NETSIM {name:<10} captured {:>4} clean {:>4} lost {:>3} skipped {:>3} broken {} keyframes {} \
         | latency p50 {:>5.1} p95 {:>5.1} p99 {:>5.1} ms | recoveries {:>3} p95 {:>5.1} ms \
         | invalidations {} long-term {} forced {} | nacks {} retx {} stale {} | target {:.1} Mbps",
        report.captured,
        c.frames_clean,
        c.frames_lost,
        c.frames_skipped,
        c.decoder_broken,
        h.keyframes,
        report.latency_percentile_us(50) as f64 / 1e3,
        report.latency_percentile_us(95) as f64 / 1e3,
        report.latency_percentile_us(99) as f64 / 1e3,
        c.recoveries,
        report.recovery_percentile_us(95) as f64 / 1e3,
        h.invalidations,
        h.long_term_recoveries,
        h.forced_keyframes,
        c.nacks,
        h.retransmissions,
        h.dropped_stale,
        report
            .trace
            .last()
            .map_or(0.0, |s| f64::from(s.target_bps) / 1e6),
    );
}

#[test]
#[ignore = "prints numbers; the gates below assert them"]
fn report_all_scenarios() {
    let lan = scenario(LinkConfig::lan(), 10.0);
    summary("lan", &run(&lan));

    let lossy = scenario(
        LinkConfig {
            loss: 0.01,
            ..internet(20)
        },
        10.0,
    );
    summary("lossy", &run(&lossy));

    let bursty = scenario(
        LinkConfig {
            burst: Some(Burst {
                enter: 0.003,
                exit: 0.3,
                loss: 0.9,
            }),
            ..internet(20)
        },
        10.0,
    );
    summary("bursty", &run(&bursty));

    let mut step = scenario(internet(20), 10.0);
    step.host.rate.start_bps = 50_000_000;
    step.changes = vec![(
        4_000_000,
        LinkConfig {
            rate_bps: Some(10_000_000),
            queue_bytes: 256 << 10,
            ..internet(20)
        },
    )];
    let report = run(&step);
    summary("stepdown", &report);
    for t in [
        3_900_000, 4_500_000, 5_000_000, 5_500_000, 6_000_000, 8_000_000, 9_900_000,
    ] {
        let sample = report
            .trace
            .iter()
            .take_while(|s| s.at_us <= t)
            .last()
            .unwrap();
        println!(
            "  t={:.1}s target {:.1} Mbps queue {} KB",
            t as f64 / 1e6,
            f64::from(sample.target_bps) / 1e6,
            sample.queued_bytes / 1024
        );
    }

    let mut blip = scenario(internet(20), 6.0);
    blip.changes = vec![
        (
            2_000_000,
            LinkConfig {
                loss: 1.0,
                ..internet(20)
            },
        ),
        (2_080_000, internet(20)),
    ];
    summary("blip", &run(&blip));

    let mut outage = scenario(internet(20), 6.0);
    outage.features =
        Some(EncoderFeatures::LONG_TERM_REFERENCES.with(EncoderFeatures::SLICE_OUTPUT));
    outage.client.invalidate = true;
    outage.changes = vec![
        (
            2_000_000,
            LinkConfig {
                loss: 1.0,
                ..internet(20)
            },
        ),
        (2_600_000, internet(20)),
    ];
    summary("outage", &run(&outage));
}

fn step_down() -> Scenario {
    let mut step = scenario(internet(20), 10.0);
    step.host.rate.start_bps = 50_000_000;
    step.changes = vec![(
        4_000_000,
        LinkConfig {
            rate_bps: Some(10_000_000),
            queue_bytes: 256 << 10,
            ..internet(20)
        },
    )];
    step
}

fn outage(at_us: u64, length_us: u64, features: Option<EncoderFeatures>) -> Scenario {
    let mut s = scenario(internet(20), 6.0);
    s.features = features;
    s.changes = vec![
        (
            at_us,
            LinkConfig {
                loss: 1.0,
                ..internet(20)
            },
        ),
        (at_us + length_us, internet(20)),
    ];
    s
}

fn assert_no_broken_submissions(report: &Report) {
    assert_eq!(
        report.client.decoder_broken, 0,
        "the client must never submit a frame whose reference is missing"
    );
}

#[test]
fn lan_delivers_every_frame_with_one_keyframe() {
    let report = run(&scenario(LinkConfig::lan(), 5.0));
    let c = &report.client;
    assert_eq!((c.frames_lost, c.frames_skipped), (0, 0));
    assert_eq!(report.host.keyframes, 1);
    assert!(c.frames_clean + 2 >= report.captured, "{c:?}");
    assert!(
        report.latency_percentile_us(95) <= 6_000,
        "{}",
        report.latency_percentile_us(95)
    );
    assert_no_broken_submissions(&report);
}

#[test]
fn random_loss_is_repaired_without_losing_frames() {
    let report = run(&scenario(
        LinkConfig {
            loss: 0.01,
            ..internet(20)
        },
        5.0,
    ));
    assert_eq!(report.client.frames_lost, 0);
    assert_eq!(report.host.keyframes, 1);
    assert!(report.host.retransmissions > 0);
    assert!(report.client.frames_clean + 3 >= report.captured);
    // One way 10 ms plus one retransmission round trip at worst.
    assert!(
        report.latency_percentile_us(99) <= 40_000,
        "{}",
        report.latency_percentile_us(99)
    );
    assert_no_broken_submissions(&report);
}

#[test]
fn burst_loss_is_repaired_without_keyframes() {
    let report = run(&scenario(
        LinkConfig {
            burst: Some(Burst {
                enter: 0.003,
                exit: 0.3,
                loss: 0.9,
            }),
            ..internet(20)
        },
        5.0,
    ));
    assert_eq!(report.host.keyframes, 1);
    assert!(
        report.client.frames_lost * 100 <= report.captured,
        "{:?}",
        report.client
    );
    assert!(
        report.latency_percentile_us(99) <= 80_000,
        "{}",
        report.latency_percentile_us(99)
    );
    assert_no_broken_submissions(&report);
}

#[test]
fn a_short_outage_recovers_by_invalidation_without_a_keyframe() {
    let report = run(&outage(2_000_000, 80_000, None));
    assert_eq!(report.host.keyframes, 1, "{:?}", report.host);
    assert!(report.host.invalidations >= 1);
    assert_eq!(report.host.forced_keyframes, 0);
    assert!(report.client.recoveries >= 1);
    assert!(
        report.recovery_percentile_us(95) <= 150_000,
        "{}",
        report.recovery_percentile_us(95)
    );
    assert_no_broken_submissions(&report);
}

#[test]
fn a_long_outage_recovers_from_a_long_term_reference() {
    // 600 ms is far past what invalidation can reach; without invalidation
    // support at all, the confirmed long-term slot must be used.
    let features = EncoderFeatures::LONG_TERM_REFERENCES.with(EncoderFeatures::SLICE_OUTPUT);
    let report = run(&outage(2_000_000, 600_000, Some(features)));
    assert_eq!(report.host.keyframes, 1, "{:?}", report.host);
    assert!(report.host.long_term_recoveries >= 1);
    assert_eq!(report.host.forced_keyframes, 0);
    // Detection happens once packets flow again; recovery takes about a
    // round trip plus a frame or two after that.
    assert!(
        report.recovery_percentile_us(95) <= 100_000,
        "{}",
        report.recovery_percentile_us(95)
    );
    assert_no_broken_submissions(&report);
}

#[test]
fn bitrate_follows_a_step_down_within_two_seconds_without_a_standing_queue() {
    let report = run(&step_down());
    assert!(
        report.target_at(3_900_000) > 40_000_000,
        "ramped up before the step"
    );
    assert!(
        report.target_at(6_000_000) <= 10_000_000,
        "{}",
        report.target_at(6_000_000)
    );
    assert!(
        report.target_at(9_900_000) >= 7_000_000,
        "{}",
        report.target_at(9_900_000)
    );
    let late_queue = report
        .trace
        .iter()
        .filter(|s| s.at_us >= 7_000_000)
        .map(|s| s.queued_bytes)
        .max()
        .unwrap();
    assert!(late_queue <= 48 << 10, "standing queue {late_queue} bytes");
    assert_eq!(report.host.keyframes, 1);
    assert_no_broken_submissions(&report);
}

#[test]
fn runs_replay_exactly_from_a_seed() {
    let lossy = scenario(
        LinkConfig {
            loss: 0.02,
            ..internet(30)
        },
        3.0,
    );
    assert_eq!(run(&lossy), run(&lossy));
    let other = Scenario {
        seed: 8,
        ..lossy.clone()
    };
    assert_ne!(
        run(&lossy).link,
        run(&other).link,
        "the seed drives the losses"
    );
}

/// Phase 5: probing at the start finds the path's capacity instead of
/// climbing to it from a low guess.
#[test]
fn startup_probing_reaches_capacity_quickly() {
    let link = LinkConfig {
        rate_bps: Some(50_000_000),
        ..internet(20)
    };
    let rate = RateConfig {
        min_bps: 1_000_000,
        max_bps: 80_000_000,
        start_bps: 5_000_000,
    };
    let run_with = |probe: bool| {
        let mut s = scenario(link, 3.0);
        s.host = HostConfig::new(1, 1, rate);
        s.host.probe_at_start = probe;
        s.encoder.bitrate_bps = rate.start_bps;
        run(&s)
    };
    let probed = run_with(true);
    let plain = run_with(false);
    summary("probed", &probed);
    summary("unprobed", &plain);
    let (with, without) = (probed.target_at(1_000_000), plain.target_at(1_000_000));
    println!("target at 1 s: probed {with} unprobed {without}");
    assert!(with >= 25_000_000, "probed target at 1 s: {with}");
    assert!(with > without, "probing must help: {with} vs {without}");
    assert!(with <= 50_000_000, "never above the link: {with}");
    assert_eq!(probed.client.frames_lost, 0);
    assert_eq!(probed.host.keyframes, 1);
}

/// Phase 5 gate (docs/V2_PLAN.md): on an internet path with 50 ms RTT and
/// 1% loss, p95 frame latency at most RTT/2 + 20 ms, under 0.5% of frames
/// frozen (lost or skipped), and no keyframe storms. Starts low (5 Mbps), as
/// a real session does, so probing and the delay-based controller both count.
#[test]
fn internet_path_meets_the_phase_5_gate() {
    let rate = RateConfig {
        min_bps: 1_000_000,
        max_bps: 80_000_000,
        start_bps: 5_000_000,
    };
    let mut s = scenario(
        LinkConfig {
            loss: 0.01,
            rate_bps: Some(50_000_000),
            ..internet(50)
        },
        10.0,
    );
    s.host = HostConfig::new(1, 1, rate);
    s.encoder.bitrate_bps = rate.start_bps;
    let report = run(&s);
    summary("phase5", &report);
    let frozen = report.client.frames_lost + report.client.frames_skipped;
    let p95 = report.latency_percentile_us(95);
    assert!(p95 <= 25_000 + 20_000, "p95 {p95} µs");
    assert!(
        frozen * 200 < report.captured,
        "{frozen} frozen of {}",
        report.captured
    );
    assert!(
        report.host.keyframes <= 2,
        "keyframes {}",
        report.host.keyframes
    );
    assert!(
        report.target_at(9_000_000) >= 25_000_000,
        "the stream should still use the path: {}",
        report.target_at(9_000_000)
    );
    assert_no_broken_submissions(&report);
}
