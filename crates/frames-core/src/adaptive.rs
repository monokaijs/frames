//! Conservative, deterministic adaptation. Parity is charged to the same wire budget.
use crate::media::{DeliveryStats, FecProfile};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Feedback {
    pub delivery: DeliveryStats,
    pub decoded: u64,
    pub presented: u64,
    pub late_frames: u64,
    pub decoder_backlog: u32,
    pub presentation_gaps: u64,
    pub audio_underruns: u64,
    pub queue_micros: u32,
    pub decode_p95_micros: u32,
    pub present_gap_p95_micros: u32,
    pub congestion_events: u64,
    pub congestion_window_bytes: u64,
    pub transport_lost_packets: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quality {
    pub bitrate: u32,
    pub scale_percent: u32,
    pub fps: u32,
    pub fec: FecProfile,
}
pub struct Controller {
    quality: Quality,
    previous: Feedback,
    stable_since: Instant,
    overloaded_since: Option<Instant>,
    last_step: Instant,
    last_increase: Instant,
    base_fps: u32,
    fps_cap: u32,
    enabled: bool,
}
impl Controller {
    pub fn new(fps: u32, now: Instant) -> Self {
        Self {
            quality: Quality {
                bitrate: 12_000_000,
                scale_percent: 100,
                fps,
                fec: FecProfile::default(),
            },
            previous: Feedback::default(),
            stable_since: now,
            overloaded_since: None,
            last_step: now,
            last_increase: now,
            base_fps: fps,
            fps_cap: 60,
            enabled: std::env::var("FRAMES_ADAPTATION").as_deref() != Ok("0"),
        }
    }
    pub fn quality(&self) -> Quality {
        self.quality
    }
    pub fn set_fps(&mut self, fps: u32) {
        self.base_fps = fps;
        self.quality.fps = fps.min(self.fps_cap);
    }
    pub fn congestion(&mut self, now: Instant) -> Quality {
        self.update(self.previous, Duration::ZERO, Duration::ZERO, true, now)
    }
    pub fn update(
        &mut self,
        feedback: Feedback,
        rtt: Duration,
        min_rtt: Duration,
        congested: bool,
        now: Instant,
    ) -> Quality {
        let previous_parity = self.quality.fec.parity_per_32;
        let received = feedback
            .delivery
            .received_shards
            .saturating_sub(self.previous.delivery.received_shards);
        let missing = feedback
            .delivery
            .missing_shards
            .saturating_sub(self.previous.delivery.missing_shards);
        let failed =
            feedback.delivery.unrecoverable_frames > self.previous.delivery.unrecoverable_frames;
        let loss = missing as f64 / (received + missing).max(1) as f64;
        let overloaded = congested
            || (feedback.congestion_events > self.previous.congestion_events
                && (feedback.queue_micros > 4_000
                    || rtt.saturating_sub(min_rtt) > Duration::from_millis(5)))
            || feedback.queue_micros > 8_000
            || (feedback.decoder_backlog >= 2 && feedback.decode_p95_micros > 16_667)
            || rtt.saturating_sub(min_rtt) > Duration::from_millis(10)
            || (feedback.late_frames > self.previous.late_frames && missing == 0 && !failed);
        self.previous = feedback;
        if !self.enabled {
            return self.quality;
        }
        if overloaded {
            self.stable_since = now;
            let since = *self.overloaded_since.get_or_insert(now);
            if now.saturating_duration_since(self.last_step) >= Duration::from_millis(300) {
                self.quality.bitrate = (self.quality.bitrate * 4 / 5).max(1_000_000);
                self.last_step = now;
            }
            if now.saturating_duration_since(since) >= Duration::from_secs(1) {
                if self.quality.scale_percent > 50 {
                    self.quality.scale_percent -= 25;
                } else {
                    self.fps_cap = if self.fps_cap > 45 { 45 } else { 30 };
                    self.quality.fps = self.base_fps.min(self.fps_cap);
                }
                self.overloaded_since = Some(now);
            }
        } else {
            self.overloaded_since = None;
            if loss > 0.02 || failed {
                self.quality.fec.parity_per_32 = 8;
                self.stable_since = now;
            } else if loss > 0.001 {
                self.quality.fec.parity_per_32 = 4;
                self.stable_since = now;
            } else if now.saturating_duration_since(self.stable_since) >= Duration::from_secs(5) {
                self.quality.fec.parity_per_32 = 2;
            }
            if now.saturating_duration_since(self.stable_since) >= Duration::from_secs(3)
                && now.saturating_duration_since(self.last_increase) >= Duration::from_secs(1)
            {
                self.quality.bitrate =
                    (self.quality.bitrate as u64 * 105 / 100).min(12_000_000) as u32;
                self.last_increase = now;
            }
            if now.saturating_duration_since(self.stable_since) >= Duration::from_secs(10) {
                if self.fps_cap < 60 {
                    self.fps_cap = if self.fps_cap < 45 { 45 } else { 60 };
                    self.quality.fps = self.base_fps.min(self.fps_cap);
                } else {
                    self.quality.scale_percent = (self.quality.scale_percent + 25).min(100);
                }
                self.stable_since = now;
            }
        }
        if self.quality.fec.parity_per_32 != previous_parity {
            self.quality.bitrate = (self.quality.bitrate as u64 * (32 + previous_parity as u64)
                / (32 + self.quality.fec.parity_per_32 as u64))
                .min(12_000_000) as u32;
        }
        self.quality
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistent_congestion_reduces_rate_then_resolution_then_fps_and_recovers_slowly() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        for n in 1..=30 {
            c.update(
                Feedback::default(),
                Duration::from_millis(2),
                Duration::from_millis(2),
                true,
                now + Duration::from_millis(n * 100),
            );
        }
        assert_eq!(c.quality.scale_percent, 50);
        assert_eq!(c.quality.fps, 60);
        assert!(c.quality.bitrate < 12_000_000);
        c.update(
            Feedback::default(),
            Duration::ZERO,
            Duration::ZERO,
            true,
            now + Duration::from_secs(5),
        );
        assert_eq!(c.quality.fps, 45);
        let q = c.update(
            Feedback::default(),
            Duration::ZERO,
            Duration::ZERO,
            false,
            now + Duration::from_secs(16),
        );
        assert_eq!(q.fps, 60);
        assert_eq!(q.scale_percent, 50);
    }
    #[test]
    fn focusing_restores_base_rate_but_preserves_an_overload_cap() {
        let now = Instant::now();
        let mut controller = Controller::new(24, now);
        controller.set_fps(60);
        assert_eq!(controller.quality().fps, 60);
        for second in 0..4 {
            controller.congestion(now + Duration::from_secs(second));
        }
        assert_eq!(controller.quality().fps, 45);
        controller.set_fps(24);
        assert_eq!(controller.quality().fps, 24);
        controller.set_fps(60);
        assert_eq!(controller.quality().fps, 45);
    }
    #[test]
    fn extra_protection_consumes_the_existing_wire_budget() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        let before = c.quality();
        let after = c.update(
            Feedback {
                delivery: DeliveryStats {
                    received_shards: 97,
                    missing_shards: 3,
                    ..Default::default()
                },
                ..Default::default()
            },
            Duration::ZERO,
            Duration::ZERO,
            false,
            now,
        );
        assert_eq!(after.fec.parity_per_32, 8);
        assert!(after.bitrate < before.bitrate);
        assert!((after.bitrate as u64 * 40).abs_diff(before.bitrate as u64 * 36) < 40);
    }
    #[test]
    fn parity_responds_to_raw_loss_and_clean_link() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        let feedback = Feedback {
            delivery: DeliveryStats {
                received_shards: 90,
                missing_shards: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            c.update(feedback, Duration::ZERO, Duration::ZERO, false, now)
                .fec
                .parity_per_32,
            8
        );
        assert_eq!(
            c.update(
                feedback,
                Duration::ZERO,
                Duration::ZERO,
                false,
                now + Duration::from_secs(6)
            )
            .fec
            .parity_per_32,
            2
        );
    }
}
