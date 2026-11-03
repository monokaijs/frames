//! Client-side media diagnostics for the debug overlay.
//!
//! Collection only runs while the overlay is enabled. Each window keeps a
//! short per-frame history for the graphs and counter samples for one-second
//! rates; snapshots are formatted here so the Mac and Windows UIs draw the
//! same text.
use frames_core::{
    codec::Codec,
    media::{DeliveryStats, LatencyHistogram},
    protocol::StreamConfig,
};
use frames_native::{DebugLevel, DebugLine, DebugOverlay};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// How often the media worker publishes a snapshot while the overlay is on.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Presented frames kept for the frame-time graph (~4 s at 60 fps).
const FRAME_HISTORY: usize = 240;
/// Bitrate samples kept for the bitrate graph.
const RATE_HISTORY: usize = 150;
/// Network-age samples used for the stage percentiles.
const NETWORK_HISTORY: usize = 120;
/// Window over which rates and loss are computed.
const RATE_WINDOW: Duration = Duration::from_secs(1);
/// Frame latency percentiles use the most recent frames only.
const LATENCY_FRAMES: usize = 120;

#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub delivery: DeliveryStats,
    pub decoded: u64,
    pub presented: u64,
    pub late: u64,
    pub gaps: u64,
    pub keyframe_requests: u64,
}
impl Counters {
    /// A new stream or decoder restarts some counters; rates across that
    /// boundary would be meaningless.
    fn restarted_since(&self, earlier: &Self) -> bool {
        let (a, b) = (&self.delivery, &earlier.delivery);
        a.received_bytes < b.received_bytes
            || a.received_shards < b.received_shards
            || a.missing_shards < b.missing_shards
            || a.completed_frames < b.completed_frames
            || self.decoded < earlier.decoded
            || self.presented < earlier.presented
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecoderKind {
    /// Pictures are decoded by the platform UI (Mac VideoToolbox session).
    Native,
    Hardware,
    Software,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStatus {
    pub received: u64,
    pub concealed: u64,
    pub underruns: u64,
}

pub struct Inputs<'a> {
    pub config: &'a StreamConfig,
    pub counters: Counters,
    /// Cumulative decode-time histogram for the current decoder.
    pub decode: &'a LatencyHistogram,
    pub decoder: DecoderKind,
    pub backlog: usize,
    pub recovering: bool,
    pub rtt: Duration,
    pub clock_uncertainty_us: u64,
    pub audio: Option<AudioStatus>,
    pub now: Instant,
}

#[derive(Default)]
pub struct Window {
    frames: VecDeque<[u32; 2]>,
    network: VecDeque<u32>,
    bitrate: VecDeque<u32>,
    samples: VecDeque<(Instant, Counters)>,
    decode_base: Option<(Instant, LatencyHistogram)>,
    decode_next: Option<(Instant, LatencyHistogram)>,
}

fn clamp_us(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}

impl Window {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.network.is_empty() && self.samples.is_empty()
    }
    pub fn presented(&mut self, interval: Option<Duration>, latency: Option<Duration>) {
        if self.frames.len() >= FRAME_HISTORY {
            self.frames.pop_front();
        }
        self.frames.push_back([
            interval.map_or(0, |d| clamp_us(d.as_micros() as u64)),
            latency.map_or(0, |d| clamp_us(d.as_micros() as u64)),
        ]);
    }
    pub fn received(&mut self, network: Duration) {
        if self.network.len() >= NETWORK_HISTORY {
            self.network.pop_front();
        }
        self.network.push_back(clamp_us(network.as_micros() as u64));
    }

    /// Decode-time histogram covering the last one to two seconds.
    fn recent_decode(&mut self, current: &LatencyHistogram, now: Instant) -> LatencyHistogram {
        if current.count() < self.decode_next.as_ref().map_or(0, |(_, h)| h.count()) {
            // A new decoder started; its histogram began from zero.
            self.decode_base = None;
            self.decode_next = None;
        }
        if self
            .decode_next
            .as_ref()
            .is_none_or(|(at, _)| now.saturating_duration_since(*at) >= RATE_WINDOW)
        {
            self.decode_base = self.decode_next.take();
            self.decode_next = Some((now, current.clone()));
        }
        match &self.decode_base {
            Some((_, base)) => current.since(base),
            None => current.clone(),
        }
    }

    pub fn snapshot(&mut self, input: Inputs<'_>) -> DebugOverlay {
        let now = input.now;
        let counters = input.counters;
        if self
            .samples
            .back()
            .is_some_and(|(_, earlier)| counters.restarted_since(earlier))
        {
            self.samples.clear();
        }
        // Bitrate graph: one point per snapshot, against the previous snapshot.
        if let Some((at, earlier)) = self.samples.back() {
            let elapsed = now.saturating_duration_since(*at).as_secs_f64();
            if elapsed > 0.0 {
                let bytes = counters
                    .delivery
                    .received_bytes
                    .saturating_sub(earlier.delivery.received_bytes);
                if self.bitrate.len() >= RATE_HISTORY {
                    self.bitrate.pop_front();
                }
                self.bitrate
                    .push_back((bytes as f64 * 8.0 / 1000.0 / elapsed).round() as u32);
            }
        }
        self.samples.push_back((now, counters));
        // Keep the newest sample that is at least one window old as the base.
        while self.samples.len() > 2
            && now.saturating_duration_since(self.samples[1].0) >= RATE_WINDOW
        {
            self.samples.pop_front();
        }
        let (base_at, base) = self.samples.front().copied().unwrap();
        let elapsed = now.saturating_duration_since(base_at).as_secs_f64();
        let rate = |value: u64, earlier: u64| {
            (elapsed > 0.0).then(|| value.saturating_sub(earlier) as f64 / elapsed)
        };
        let decode = self.recent_decode(input.decode, now);

        let config = input.config;
        let fps = config.fps.max(1);
        let delivery = counters.delivery;
        let received = rate(delivery.completed_frames, base.delivery.completed_frames);
        let decoded = rate(counters.decoded, base.decoded);
        let presented = rate(counters.presented, base.presented);
        let bitrate = rate(delivery.received_bytes, base.delivery.received_bytes);
        let shards = delivery
            .received_shards
            .saturating_sub(base.delivery.received_shards);
        let missing = delivery
            .missing_shards
            .saturating_sub(base.delivery.missing_shards);
        let loss = if shards + missing == 0 {
            0.0
        } else {
            missing as f64 * 100.0 / (shards + missing) as f64
        };

        let recent: Vec<u32> = self
            .frames
            .iter()
            .rev()
            .take(LATENCY_FRAMES)
            .map(|[_, latency]| *latency)
            .filter(|latency| *latency > 0)
            .collect();
        let latency_p50 = percentile(&recent, 50);
        let latency_p95 = percentile(&recent, 95);
        let network: Vec<u32> = self.network.iter().copied().collect();
        let network_p95 = percentile(&network, 95);

        let mut lines = Vec::new();
        let mut line = |level: DebugLevel, text: String| lines.push(DebugLine { text, level });

        let mut header = format!(
            "{} {}×{}",
            codec_name(config.codec),
            config.width,
            config.height
        );
        if (config.source_width, config.source_height) != (config.width, config.height)
            && config.source_width > 0
        {
            header += &format!(" ← {}×{}", config.source_width, config.source_height);
        }
        header += &format!(" · {fps} fps · gen {}", config.generation);
        line(DebugLevel::Normal, header);
        line(
            DebugLevel::Normal,
            format!(
                "Encoder  {} · {}{} · FEC {}/32",
                if config.encoder.is_empty() {
                    "unknown"
                } else {
                    &config.encoder
                },
                if config.hardware {
                    "hardware"
                } else {
                    "software"
                },
                if config.cpu_readback {
                    " · readback"
                } else {
                    ""
                },
                config.fec.parity_per_32
            ),
        );
        line(
            DebugLevel::Normal,
            format!("Decoder  {}", decoder_name(input.decoder)),
        );
        line(
            DebugLevel::Normal,
            format!(
                "FPS      {} shown · {} decoded · {} recv",
                one_decimal(presented),
                one_decimal(decoded),
                one_decimal(received)
            ),
        );
        line(
            DebugLevel::Normal,
            format!(
                "Bitrate  {} · start target {}",
                bitrate.map_or("—".into(), |bytes| megabits(bytes * 8.0)),
                megabits(config.target_bitrate as f64)
            ),
        );
        line(
            if loss >= 5.0 {
                DebugLevel::Bad
            } else if loss >= 1.0 {
                DebugLevel::Warn
            } else {
                DebugLevel::Normal
            },
            format!(
                "Network  RTT {} · loss {loss:.1}% · FEC fixed {}",
                millis(input.rtt.as_micros() as u64),
                delivery.recovered_frames
            ),
        );
        line(
            if latency_p95 >= 100_000 {
                DebugLevel::Bad
            } else if latency_p95 >= 50_000 {
                DebugLevel::Warn
            } else {
                DebugLevel::Normal
            },
            if recent.is_empty() {
                "Latency  — (no clock sync yet)".into()
            } else {
                format!(
                    "Latency  p50 {} · p95 {} (±{})",
                    millis(latency_p50 as u64),
                    millis(latency_p95 as u64),
                    millis(input.clock_uncertainty_us)
                )
            },
        );
        line(
            DebugLevel::Normal,
            format!(
                "Stages   net p95 {} · decode p95 {}",
                if network.is_empty() {
                    "—".into()
                } else {
                    millis(network_p95 as u64)
                },
                if decode.count() == 0 {
                    "—".into()
                } else {
                    millis(decode.percentile(95))
                }
            ),
        );
        let dropped = counters.late > base.late
            || delivery.unrecoverable_frames > base.delivery.unrecoverable_frames
            || counters.gaps > base.gaps
            || counters.keyframe_requests > base.keyframe_requests;
        line(
            if dropped {
                DebugLevel::Warn
            } else {
                DebugLevel::Normal
            },
            format!(
                "Drops    late {} · lost {} · stalls {} · IDR req {}",
                counters.late,
                delivery.unrecoverable_frames,
                counters.gaps,
                counters.keyframe_requests
            ),
        );
        line(
            if input.recovering {
                DebugLevel::Bad
            } else if input.backlog >= 2 {
                DebugLevel::Warn
            } else {
                DebugLevel::Normal
            },
            format!(
                "Queue    decoder {}{}",
                input.backlog,
                if input.recovering {
                    " · RECOVERING"
                } else {
                    ""
                }
            ),
        );
        if let Some(audio) = input.audio {
            let total = audio.received + audio.concealed;
            let concealed = if total == 0 {
                0.0
            } else {
                audio.concealed as f64 * 100.0 / total as f64
            };
            line(
                if audio.underruns > 0 || concealed >= 1.0 {
                    DebugLevel::Warn
                } else {
                    DebugLevel::Normal
                },
                format!(
                    "Audio    underruns {} · concealed {concealed:.1}%",
                    audio.underruns
                ),
            );
        }
        line(
            DebugLevel::Dim,
            format!(
                "▮ frame  ─ latency  ┄ budget · {} hides",
                frames_native::DEBUG_OVERLAY_SHORTCUT
            ),
        );

        DebugOverlay {
            lines,
            frames: self.frames.iter().copied().collect(),
            frame_budget_us: 1_000_000 / fps,
            bitrate_kbps: self.bitrate.iter().copied().collect(),
            sample_ms: SNAPSHOT_INTERVAL.as_millis() as u32,
        }
    }
}

fn percentile(values: &[u32], percent: usize) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}
fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
        Codec::Av1 => "AV1",
        Codec::Vp9 => "VP9",
    }
}
fn decoder_name(kind: DecoderKind) -> &'static str {
    match kind {
        DecoderKind::Native => "VideoToolbox · native",
        DecoderKind::Hardware if cfg!(windows) => "D3D11VA · hardware",
        DecoderKind::Hardware => "VideoToolbox · hardware",
        DecoderKind::Software => "software",
    }
}
fn one_decimal(value: Option<f64>) -> String {
    value.map_or("—".into(), |v| format!("{v:.1}"))
}
fn megabits(bits_per_second: f64) -> String {
    format!("{:.1} Mb/s", bits_per_second / 1_000_000.0)
}
fn millis(micros: u64) -> String {
    format!("{:.1} ms", micros as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> StreamConfig {
        StreamConfig {
            window: frames_core::model::WindowId {
                id: 1,
                generation: 1,
            },
            generation: 3,
            width: 1280,
            height: 720,
            codec: Codec::H264,
            encoder: "h264_nvenc".into(),
            hardware: true,
            fps: 60,
            cpu_readback: false,
            fec: Default::default(),
            target_bitrate: 12_000_000,
            source_width: 2560,
            source_height: 1440,
        }
    }
    fn inputs<'a>(
        config: &'a StreamConfig,
        decode: &'a LatencyHistogram,
        counters: Counters,
        now: Instant,
    ) -> Inputs<'a> {
        Inputs {
            config,
            counters,
            decode,
            decoder: DecoderKind::Hardware,
            backlog: 0,
            recovering: false,
            rtt: Duration::from_millis(2),
            clock_uncertainty_us: 300,
            audio: None,
            now,
        }
    }
    fn text(overlay: &DebugOverlay, prefix: &str) -> DebugLine {
        overlay
            .lines
            .iter()
            .find(|line| line.text.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix} line in {:?}", overlay.lines))
            .clone()
    }

    #[test]
    fn rates_and_loss_cover_the_last_second() {
        let (config, decode) = (config(), LatencyHistogram::default());
        let mut window = Window::default();
        let start = Instant::now();
        let mut counters = Counters::default();
        window.snapshot(inputs(&config, &decode, counters, start));
        // One second later: 60 frames, 1.5 MB, 2 of 100 shards missing.
        counters.delivery.completed_frames = 60;
        counters.decoded = 60;
        counters.presented = 30;
        counters.delivery.received_bytes = 1_500_000;
        counters.delivery.received_shards = 98;
        counters.delivery.missing_shards = 2;
        let overlay = window.snapshot(inputs(
            &config,
            &decode,
            counters,
            start + Duration::from_secs(1),
        ));
        assert_eq!(
            text(&overlay, "FPS").text,
            "FPS      30.0 shown · 60.0 decoded · 60.0 recv"
        );
        assert!(
            text(&overlay, "Bitrate")
                .text
                .starts_with("Bitrate  12.0 Mb/s")
        );
        let network = text(&overlay, "Network");
        assert!(network.text.contains("loss 2.0%"), "{}", network.text);
        assert_eq!(network.level, DebugLevel::Warn);
        assert_eq!(overlay.bitrate_kbps, vec![12_000]);
        assert_eq!(overlay.frame_budget_us, 16_666);
        assert!(
            text(&overlay, "H.264")
                .text
                .contains("1280×720 ← 2560×1440")
        );
    }

    #[test]
    fn a_restarted_stream_does_not_report_negative_or_huge_rates() {
        let (config, decode) = (config(), LatencyHistogram::default());
        let mut window = Window::default();
        let start = Instant::now();
        let mut counters = Counters {
            decoded: 600,
            presented: 600,
            ..Default::default()
        };
        counters.delivery.received_bytes = 50_000_000;
        window.snapshot(inputs(&config, &decode, counters, start));
        let restarted = Counters::default();
        let overlay = window.snapshot(inputs(
            &config,
            &decode,
            restarted,
            start + Duration::from_millis(200),
        ));
        assert!(text(&overlay, "FPS").text.contains("— shown"));
        assert!(overlay.bitrate_kbps.is_empty());
    }

    #[test]
    fn frame_history_feeds_latency_percentiles_and_is_bounded() {
        let (config, decode) = (config(), LatencyHistogram::default());
        let mut window = Window::default();
        for i in 0..(FRAME_HISTORY + 10) {
            window.presented(
                Some(Duration::from_micros(16_667)),
                Some(Duration::from_millis(if i % 10 == 0 { 120 } else { 20 })),
            );
        }
        let overlay = window.snapshot(inputs(
            &config,
            &decode,
            Counters::default(),
            Instant::now(),
        ));
        assert_eq!(overlay.frames.len(), FRAME_HISTORY);
        let latency = text(&overlay, "Latency");
        assert!(latency.text.contains("p50 20.0 ms"), "{}", latency.text);
        assert!(latency.text.contains("p95 120.0 ms"), "{}", latency.text);
        assert_eq!(latency.level, DebugLevel::Bad);
    }

    #[test]
    fn recovery_and_new_drops_are_highlighted() {
        let (config, decode) = (config(), LatencyHistogram::default());
        let mut window = Window::default();
        let start = Instant::now();
        window.snapshot(inputs(&config, &decode, Counters::default(), start));
        let counters = Counters {
            late: 1,
            keyframe_requests: 1,
            ..Default::default()
        };
        let mut input = inputs(
            &config,
            &decode,
            counters,
            start + Duration::from_millis(200),
        );
        input.recovering = true;
        let overlay = window.snapshot(input);
        assert_eq!(text(&overlay, "Drops").level, DebugLevel::Warn);
        let queue = text(&overlay, "Queue");
        assert_eq!(queue.level, DebugLevel::Bad);
        assert!(queue.text.contains("RECOVERING"));
    }

    #[test]
    fn decode_percentile_tracks_recent_pictures_only() {
        let config = config();
        let mut decode = LatencyHistogram::default();
        for _ in 0..1000 {
            decode.record(40_000);
        }
        let mut window = Window::default();
        let start = Instant::now();
        window.snapshot(inputs(&config, &decode, Counters::default(), start));
        window.snapshot(inputs(
            &config,
            &decode,
            Counters::default(),
            start + Duration::from_secs(1),
        ));
        for _ in 0..60 {
            decode.record(2_000);
        }
        let overlay = window.snapshot(inputs(
            &config,
            &decode,
            Counters::default(),
            start + Duration::from_millis(1200),
        ));
        let stages = text(&overlay, "Stages");
        assert!(stages.text.contains("decode p95 2.0 ms"), "{}", stages.text);
    }
}
