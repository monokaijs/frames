//! The viewer's debug overlay (Control+Option+F10 / Ctrl+Alt+F10, "Show
//! debug overlay"), as v1's: a panel of the stream's live state over the
//! top left of the picture, with a frame graph (each frame's interval as a
//! bar, its capture-to-present latency as a line, the frame budget dashed)
//! and a bitrate graph. The view loop records what happens; the platform
//! draws [`Overlay`] (JSON to FramesMac).

use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Frames in the frame graph.
pub const FRAMES: usize = 240;
/// Bitrate samples, one per [`SAMPLE`].
pub const BITRATE_SAMPLES: usize = 150;
pub const SAMPLE: Duration = Duration::from_millis(200);
/// How often the panel is redrawn.
pub const REFRESH: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Normal,
    Warn,
    Bad,
    Dim,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Line {
    pub text: String,
    pub level: Level,
}

/// What the platform draws.
#[derive(Clone, Debug, Serialize)]
pub struct Overlay {
    pub lines: Vec<Line>,
    /// Oldest first: [interval since the previous frame, capture to
    /// present], in microseconds (0: not measured).
    pub frames: Vec<[u32; 2]>,
    pub frame_budget_us: u32,
    /// Oldest first, one per `sample_ms`.
    pub bitrate_kbps: Vec<u32>,
    pub sample_ms: u32,
}

/// The stream's state for one redraw.
pub struct Input<'a> {
    pub codec: &'a str,
    pub width: u32,
    pub height: u32,
    pub decoder: &'a str,
    pub path: &'a frames_media::api::PathInfo,
    pub stats: &'a frames_media::client::ClientStats,
    /// Decoded frames not shown because a newer one was ready.
    pub superseded: u64,
    /// Audio packets received and concealed, when audio is on.
    pub audio: Option<(u64, u64)>,
    pub shortcut: &'a str,
}

/// Records the stream for the overlay.
pub struct Recorder {
    frames: VecDeque<[u32; 2]>,
    last_presented: Option<Instant>,
    bitrate: VecDeque<u32>,
    sample_bytes: u64,
    sample_started: Instant,
    /// Counts over the last second, for rates.
    second_started: Instant,
    presented: u64,
    decoded: u64,
    rates: (f64, f64),
    last_decoded_total: u64,
    redrawn: Option<Instant>,
}

impl Default for Recorder {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            frames: VecDeque::with_capacity(FRAMES),
            last_presented: None,
            bitrate: VecDeque::with_capacity(BITRATE_SAMPLES),
            sample_bytes: 0,
            sample_started: now,
            second_started: now,
            presented: 0,
            decoded: 0,
            rates: (0.0, 0.0),
            last_decoded_total: 0,
            redrawn: None,
        }
    }
}

fn ms(us: u64) -> String {
    format!("{:.1} ms", us as f64 / 1000.0)
}

fn percentile(values: &[u32], p: usize) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() - 1) * p / 100]
}

impl Recorder {
    /// A frame went to the glass, `latency_us` after its capture (0: not
    /// known, no clock sync yet).
    pub fn presented(&mut self, latency_us: u64) {
        let now = Instant::now();
        let interval = self
            .last_presented
            .map_or(0, |last| now.duration_since(last).as_micros() as u32);
        self.last_presented = Some(now);
        if self.frames.len() == FRAMES {
            self.frames.pop_front();
        }
        self.frames
            .push_back([interval, latency_us.min(u64::from(u32::MAX)) as u32]);
        self.presented += 1;
    }

    /// `bytes` of video arrived.
    pub fn received(&mut self, bytes: usize) {
        self.roll(Instant::now());
        self.sample_bytes += bytes as u64;
    }

    /// Whether a redraw is due ([`Self::snapshot`] would return one).
    pub fn due(&self) -> bool {
        self.redrawn.is_none_or(|at| at.elapsed() >= REFRESH)
    }

    /// Closes the bitrate samples that ended by `now`.
    fn roll(&mut self, now: Instant) {
        while now.duration_since(self.sample_started) >= SAMPLE {
            let kbps = self.sample_bytes * 8 * 1000 / SAMPLE.as_millis() as u64 / 1000;
            if self.bitrate.len() == BITRATE_SAMPLES {
                self.bitrate.pop_front();
            }
            self.bitrate.push_back(kbps.min(u64::from(u32::MAX)) as u32);
            self.sample_bytes = 0;
            self.sample_started += SAMPLE;
        }
    }

    fn advance(&mut self, now: Instant, decoded_total: u64) {
        self.roll(now);
        self.decoded += decoded_total.saturating_sub(self.last_decoded_total);
        self.last_decoded_total = decoded_total;
        let elapsed = now.duration_since(self.second_started);
        if elapsed >= Duration::from_secs(1) {
            let seconds = elapsed.as_secs_f64();
            self.rates = (
                self.presented as f64 / seconds,
                self.decoded as f64 / seconds,
            );
            (self.presented, self.decoded) = (0, 0);
            self.second_started = now;
        }
    }

    /// The panel, at most every [`REFRESH`] (`None`: not due yet).
    pub fn snapshot(&mut self, input: &Input) -> Option<Overlay> {
        let now = Instant::now();
        self.advance(now, input.stats.frames_decoded);
        if self
            .redrawn
            .is_some_and(|at| now.duration_since(at) < REFRESH)
        {
            return None;
        }
        self.redrawn = Some(now);
        Some(self.build(input))
    }

    fn build(&self, input: &Input) -> Overlay {
        let (shown, decoded) = self.rates;
        let fps = shown.round().max(1.0) as u32;
        let latencies: Vec<u32> = self
            .frames
            .iter()
            .rev()
            .take(60)
            .map(|[_, latency]| *latency)
            .filter(|latency| *latency > 0)
            .collect();
        let (p50, p95) = (percentile(&latencies, 50), percentile(&latencies, 95));
        let mbps = self
            .bitrate
            .iter()
            .rev()
            .take(5)
            .map(|&k| u64::from(k))
            .sum::<u64>() as f64
            / self.bitrate.len().clamp(1, 5) as f64
            / 1000.0;
        let stats = input.stats;
        let total = stats.frames_clean + stats.frames_lost;
        let lost = if total == 0 {
            0.0
        } else {
            stats.frames_lost as f64 * 100.0 / total as f64
        };
        let kind = match input.path.kind {
            frames_media::api::PathKind::Direct => "direct",
            frames_media::api::PathKind::Relay => "relayed",
        };
        let mut lines = Vec::new();
        let mut line = |level, text: String| lines.push(Line { text, level });
        line(
            Level::Normal,
            format!(
                "{} {}×{} · {fps} fps",
                input.codec, input.width, input.height
            ),
        );
        line(Level::Normal, format!("Decoder  {}", input.decoder));
        line(
            Level::Normal,
            format!("FPS      {shown:.1} shown · {decoded:.1} decoded"),
        );
        line(Level::Normal, format!("Bitrate  {mbps:.1} Mb/s"));
        line(
            if lost >= 5.0 {
                Level::Bad
            } else if lost >= 1.0 {
                Level::Warn
            } else {
                Level::Normal
            },
            format!(
                "Network  {kind} · RTT {} · lost frames {lost:.1}% · NACKs {}",
                ms(input.path.rtt_us),
                stats.nacks
            ),
        );
        line(
            if p95 >= 100_000 {
                Level::Bad
            } else if p95 >= 50_000 {
                Level::Warn
            } else {
                Level::Normal
            },
            if latencies.is_empty() {
                "Latency  — (no clock sync yet)".into()
            } else {
                format!(
                    "Latency  p50 {} · p95 {}",
                    ms(u64::from(p50)),
                    ms(u64::from(p95))
                )
            },
        );
        line(
            if stats.frames_lost > 0 || stats.frames_skipped > 0 {
                Level::Warn
            } else {
                Level::Normal
            },
            format!(
                "Drops    late {} · lost {} · skipped {} · keyframes {}",
                input.superseded, stats.frames_lost, stats.frames_skipped, stats.keyframes
            ),
        );
        line(
            Level::Normal,
            format!("Recovery {} recoveries", stats.recoveries),
        );
        if let Some((packets, concealed)) = input.audio {
            let total = packets + concealed;
            let share = if total == 0 {
                0.0
            } else {
                concealed as f64 * 100.0 / total as f64
            };
            line(
                if share >= 1.0 {
                    Level::Warn
                } else {
                    Level::Normal
                },
                format!("Audio    concealed {share:.1}%"),
            );
        }
        line(
            Level::Dim,
            format!("▮ frame  ─ latency  ┄ budget · {} hides", input.shortcut),
        );
        Overlay {
            lines,
            frames: self.frames.iter().copied().collect(),
            frame_budget_us: 1_000_000 / fps.max(1),
            bitrate_kbps: self.bitrate.iter().copied().collect(),
            sample_ms: SAMPLE.as_millis() as u32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_lists_v1s_lines_and_keeps_bounded_graphs() {
        let mut recorder = Recorder::default();
        for _ in 0..300 {
            recorder.presented(12_000);
            recorder.received(1200);
        }
        assert_eq!(recorder.frames.len(), FRAMES);
        let path = frames_media::api::PathInfo {
            kind: frames_media::api::PathKind::Direct,
            rtt_us: 3_000,
            max_datagram: 1200,
        };
        let stats = frames_media::client::ClientStats::default();
        let overlay = recorder
            .snapshot(&Input {
                codec: "HEVC",
                width: 1920,
                height: 1080,
                decoder: "VideoToolbox · hardware",
                path: &path,
                stats: &stats,
                superseded: 2,
                audio: Some((100, 0)),
                shortcut: "Control+Option+F10",
            })
            .expect("first redraw is due");
        let texts: Vec<&str> = overlay.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(texts[0].starts_with("HEVC 1920×1080"));
        assert!(texts.iter().any(|t| t.starts_with("Latency  p50 12.0 ms")));
        assert!(texts.iter().any(|t| t.starts_with("Audio")));
        assert_eq!(overlay.lines.last().unwrap().level, Level::Dim);
        // Not due again right away.
        assert!(
            recorder
                .snapshot(&Input {
                    codec: "HEVC",
                    width: 1920,
                    height: 1080,
                    decoder: "",
                    path: &path,
                    stats: &stats,
                    superseded: 0,
                    audio: None,
                    shortcut: "",
                })
                .is_none()
        );
    }
}
