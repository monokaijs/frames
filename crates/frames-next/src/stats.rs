//! Per-second latency summaries.

/// The viewer window's title: the session's live state, refreshed once a
/// second (the v2 stand-in for v1's debug overlay).
pub fn window_title(
    fps: usize,
    p50_ms: f64,
    p95_ms: f64,
    path: &frames_media::api::PathInfo,
    lost: u64,
) -> String {
    let kind = match path.kind {
        frames_media::api::PathKind::Direct => "direct",
        frames_media::api::PathKind::Relay => "relayed",
    };
    format!(
        "Frames Next — {fps} fps · {p50_ms:.1} ms (p95 {p95_ms:.1}) · {kind} {:.1} ms RTT · {lost} lost",
        path.rtt_us as f64 / 1e3
    )
}

/// The `p`th percentile (0-100) in milliseconds; 0 when empty.
pub fn percentile_ms(values: &mut [u64], p: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * p / 100] as f64 / 1000.0
}

/// Collects samples over a reporting window and over the whole run, skipping
/// the first `warmup` samples of the run (connection setup, first keyframe).
pub struct Samples {
    window: Vec<u64>,
    all: Vec<u64>,
    seen: usize,
    warmup: usize,
}

impl Samples {
    pub fn new(warmup: usize) -> Self {
        Self {
            window: Vec::new(),
            all: Vec::new(),
            seen: 0,
            warmup,
        }
    }

    pub fn push(&mut self, value: u64) {
        self.seen += 1;
        if self.seen > self.warmup {
            self.window.push(value);
            self.all.push(value);
        }
    }

    /// `(count, p50, p95)` of the window, then clears it.
    pub fn take_window(&mut self) -> (usize, f64, f64) {
        let n = self.window.len();
        let p50 = percentile_ms(&mut self.window, 50);
        let p95 = percentile_ms(&mut self.window, 95);
        self.window.clear();
        (n, p50, p95)
    }

    /// `(count, p50, p95, p99)` over the run.
    pub fn summary(&mut self) -> (usize, f64, f64, f64) {
        let n = self.all.len();
        (
            n,
            percentile_ms(&mut self.all, 50),
            percentile_ms(&mut self.all, 95),
            percentile_ms(&mut self.all, 99),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_samples_are_skipped_and_windows_reset() {
        let mut samples = Samples::new(2);
        for v in [100_000, 100_000, 1_000, 2_000, 3_000] {
            samples.push(v);
        }
        assert_eq!(samples.take_window(), (3, 2.0, 2.0));
        assert_eq!(samples.take_window().0, 0);
        assert_eq!(samples.summary(), (3, 2.0, 2.0, 2.0));
    }
}
