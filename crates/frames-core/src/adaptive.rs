//! Conservative, deterministic adaptation. Parity is charged to the same wire budget.
use crate::media::{DeliveryStats, FecProfile};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// User-selected balance between text clarity, motion, and bandwidth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityPreset {
    #[default]
    High,
    Medium,
    Low,
}
impl QualityPreset {
    pub fn index(self) -> i32 {
        match self {
            Self::High => 0,
            Self::Medium => 1,
            Self::Low => 2,
        }
    }
    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::High),
            1 => Some(Self::Medium),
            2 => Some(Self::Low),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
    fn bitrate(self, width: u32, height: u32) -> u32 {
        let (base, ceiling) = match self {
            Self::High => (50_000_000u64, 200_000_000),
            Self::Medium => (12_000_000, 64_000_000),
            Self::Low => (4_000_000, 24_000_000),
        };
        (base * width as u64 * height as u64 / (1920 * 1080)).clamp(base, ceiling) as u32
    }
    fn scale(self) -> u32 {
        if self == Self::Low { 75 } else { 100 }
    }
    fn fps(self) -> u32 {
        if self == Self::Low { 30 } else { 60 }
    }
    fn scale_floor(self) -> u32 {
        if self == Self::High { 100 } else { 50 }
    }
}
impl std::str::FromStr for QualityPreset {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "high" => Ok(Self::High),
            "medium" => Ok(Self::Medium),
            "low" => Ok(Self::Low),
            _ => Err("quality must be high, medium, or low"),
        }
    }
}
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
    clean_since: Instant,
    /// Last feedback window with more than 2% shard loss or an unrecoverable
    /// frame; parity stays at its highest level for PARITY_HOLD after it.
    heavy_loss_at: Option<Instant>,
    overloaded_since: Option<Instant>,
    last_step: Instant,
    last_increase: Instant,
    base_fps: u32,
    fps_cap: u32,
    enabled: bool,
    local_pressure_until: Option<Instant>,
    base_bitrate: u32,
    manual_bitrate: Option<u32>,
    preset: QualityPreset,
    source: (u32, u32),
}
/// Queueing delay above the propagation round trip that counts as overload
/// by itself; jitter alone stays below it.
const QUEUEING_DELAY: Duration = Duration::from_millis(30);
/// Share of the transport's current sending rate (congestion window per round
/// trip) that video may use while overloaded, leaving room for parity
/// rounding, audio and control traffic.
const TRANSPORT_SHARE: f64 = 0.8;
/// FEC protection rises at once but steps down only after this long without
/// the loss that raised it. Deciding per 100 ms window flipped bursty Wi-Fi
/// between 8 and 4 parity several times a second; each flip rescaled the
/// bitrate and each rise forced a keyframe, freezing per-window sessions.
const PARITY_HOLD: Duration = Duration::from_secs(5);
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
            clean_since: now,
            heavy_loss_at: None,
            overloaded_since: None,
            last_step: now,
            last_increase: now,
            base_fps: fps,
            fps_cap: 60,
            enabled: std::env::var("FRAMES_ADAPTATION").as_deref() != Ok("0"),
            local_pressure_until: None,
            base_bitrate: 12_000_000,
            manual_bitrate: None,
            preset: QualityPreset::Medium,
            source: (1920, 1080),
        }
    }
    pub fn quality(&self) -> Quality {
        self.quality
    }
    pub fn set_fps(&mut self, fps: u32) {
        self.base_fps = fps;
        self.quality.fps = fps.min(self.fps_cap);
    }
    /// Retina/4K captures need a larger clean-link budget than 1080p. Keep
    /// congestion reductions proportional when the source itself is resized.
    pub fn set_source_size(&mut self, width: u32, height: u32) {
        self.source = (width, height);
        let requested = self.preset.bitrate(width, height);
        let budget = self
            .manual_bitrate
            .map_or(requested, |limit| requested.min(limit));
        self.quality.bitrate = (self.quality.bitrate as u64 * budget as u64
            / self.base_bitrate as u64)
            .min(budget as u64) as u32;
        self.base_bitrate = budget;
    }
    pub fn set_bandwidth_mbps(&mut self, mbps: u32) {
        self.manual_bitrate = (mbps != 0).then_some(mbps.clamp(1, 200) * 1_000_000);
        self.set_source_size(self.source.0, self.source.1);
    }
    pub fn set_preset(&mut self, preset: QualityPreset, now: Instant) {
        self.preset = preset;
        self.base_bitrate = preset.bitrate(self.source.0, self.source.1);
        if let Some(limit) = self.manual_bitrate {
            self.base_bitrate = self.base_bitrate.min(limit);
        }
        self.fps_cap = preset.fps();
        self.quality = Quality {
            bitrate: self.base_bitrate,
            scale_percent: preset.scale(),
            fps: self.base_fps.min(self.fps_cap),
            fec: FecProfile::default(),
        };
        self.stable_since = now;
        self.clean_since = now;
        self.overloaded_since = None;
        self.local_pressure_until = None;
        self.last_step = now;
        self.last_increase = now;
    }
    pub fn congestion(&mut self, now: Instant) -> Quality {
        self.local_pressure_until = Some(now + Duration::from_millis(250));
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
        // A standing queue at a bottleneck: RTT well above the propagation
        // delay. QUIC on such a path often reports no loss at all, because
        // frames miss their deadlines before the network can drop them.
        let queueing = !rtt.is_zero()
            && !min_rtt.is_zero()
            && rtt.saturating_sub(min_rtt) > QUEUEING_DELAY.max(min_rtt);
        let overloaded = congested
            || queueing
            || self.local_pressure_until.is_some_and(|until| now < until)
            || (feedback.congestion_events > self.previous.congestion_events
                && feedback.transport_lost_packets > self.previous.transport_lost_packets
                && feedback.queue_micros > 4_000
                && rtt.saturating_sub(min_rtt) > Duration::from_millis(15))
            || feedback.queue_micros > 8_000
            || (feedback.decoder_backlog >= 2 && feedback.decode_p95_micros > 16_667);
        self.previous = feedback;
        if !self.enabled {
            return self.quality;
        }
        if overloaded {
            self.stable_since = now;
            self.clean_since = now;
            let floor = (self.base_bitrate / 12).clamp(128_000, 1_000_000);
            if feedback.congestion_window_bytes > 0 && !rtt.is_zero() {
                // While overloaded the sender fills the congestion window, so
                // window/RTT is what the path carries now. Under random loss
                // Cubic holds it near 3 Mbit/s and a capped uplink holds it at
                // the cap; 20% steps every 300 ms took seconds to get there from
                // a 50 Mbit/s start, and every frame sent meanwhile failed.
                let parity = 1.0 + f64::from(self.quality.fec.parity_per_32) / 32.0;
                let path = feedback.congestion_window_bytes as f64 * 8.0 / rtt.as_secs_f64();
                let ceiling = (path * TRANSPORT_SHARE / parity) as u32;
                if ceiling < self.quality.bitrate {
                    self.quality.bitrate = ceiling.max(floor);
                    self.last_step = now;
                }
            }
            let since = *self.overloaded_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= Duration::from_millis(300)
                && now.saturating_duration_since(self.last_step) >= Duration::from_millis(300)
            {
                self.quality.bitrate = (self.quality.bitrate * 4 / 5).max(floor);
                self.last_step = now;
            }
            if now.saturating_duration_since(since) >= Duration::from_secs(1) {
                if self.quality.scale_percent > self.preset.scale_floor() {
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
                self.heavy_loss_at = Some(now);
                self.clean_since = now;
            } else if loss > 0.001 {
                self.clean_since = now;
            }
            self.quality.fec.parity_per_32 = if self
                .heavy_loss_at
                .is_some_and(|at| now.saturating_duration_since(at) < PARITY_HOLD)
            {
                8
            } else if now.saturating_duration_since(self.clean_since) < PARITY_HOLD {
                // Light loss within the hold, or the hold after an overload.
                4
            } else {
                2
            };
            if failed {
                self.stable_since = now;
            }
            if self.quality.fec.parity_per_32 != previous_parity {
                self.quality.bitrate = (self.quality.bitrate as u64 * (32 + previous_parity as u64)
                    / (32 + self.quality.fec.parity_per_32 as u64))
                    .min(self.base_bitrate as u64) as u32;
            }
            let protected_ceiling = (self.base_bitrate as u64 * 36
                / (32 + self.quality.fec.parity_per_32 as u64))
                .min(self.base_bitrate as u64);
            if now.saturating_duration_since(self.stable_since) >= Duration::from_secs(3)
                && now.saturating_duration_since(self.last_increase) >= Duration::from_secs(1)
            {
                self.quality.bitrate =
                    (self.quality.bitrate as u64 * 105 / 100).min(protected_ceiling) as u32;
                self.last_increase = now;
            }
            if now.saturating_duration_since(self.stable_since) >= Duration::from_secs(10) {
                if self.fps_cap < self.preset.fps() {
                    self.fps_cap = if self.fps_cap < 45 { 45 } else { 60 };
                    self.quality.fps = self.base_fps.min(self.fps_cap);
                } else {
                    self.quality.scale_percent =
                        (self.quality.scale_percent + 25).min(self.preset.scale());
                }
                self.stable_since = now;
            }
        }
        self.quality
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_control_clarity_and_respect_a_custom_bandwidth_cap() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        c.set_preset(QualityPreset::High, now);
        assert_eq!(c.quality().bitrate, 50_000_000);
        for n in 1..=60 {
            c.congestion(now + Duration::from_millis(n * 100));
        }
        assert_eq!(c.quality().scale_percent, 100);
        assert_eq!(c.quality().fps, 30);
        c.set_preset(QualityPreset::Medium, now + Duration::from_secs(7));
        assert_eq!(c.quality().bitrate, 12_000_000);
        assert_eq!(c.quality().scale_percent, 100);
        assert_eq!(c.quality().fps, 60);
        c.set_preset(QualityPreset::Low, now + Duration::from_secs(8));
        assert_eq!(c.quality().bitrate, 4_000_000);
        assert_eq!(c.quality().scale_percent, 75);
        assert_eq!(c.quality().fps, 30);
        c.set_bandwidth_mbps(2);
        assert_eq!(c.quality().bitrate, 2_000_000);
        c.set_preset(QualityPreset::High, now + Duration::from_secs(9));
        assert_eq!(c.quality().bitrate, 2_000_000);
        c.set_bandwidth_mbps(0);
        assert_eq!(c.quality().bitrate, 50_000_000);
        c.set_source_size(3840, 2160);
        assert_eq!(c.quality().bitrate, 200_000_000);
    }
    #[test]
    fn recoverable_loss_does_not_lock_the_desktop_at_reduced_quality() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        for n in 0..=15 {
            c.congestion(now + Duration::from_millis(n * 100));
        }
        assert_eq!(c.quality.scale_percent, 75);
        let mut feedback = Feedback::default();
        for n in 1..=400 {
            feedback.delivery.received_shards += 199;
            feedback.delivery.missing_shards += 1;
            c.update(
                feedback,
                Duration::from_millis(10),
                Duration::from_millis(2),
                false,
                now + Duration::from_millis(2000 + n * 100),
            );
        }
        assert_eq!(c.quality.scale_percent, 100);
        assert_eq!(c.quality.fps, 60);
        assert_eq!(c.quality.bitrate, 12_000_000);
        assert_eq!(
            c.update(
                feedback,
                Duration::ZERO,
                Duration::ZERO,
                false,
                now + Duration::from_millis(42100)
            )
            .fec
            .parity_per_32,
            4
        );
        assert_eq!(
            c.update(
                feedback,
                Duration::ZERO,
                Duration::ZERO,
                false,
                now + Duration::from_secs(48)
            )
            .fec
            .parity_per_32,
            2
        );
    }
    #[test]
    fn rtt_jitter_without_real_queue_pressure_does_not_destroy_desktop_quality() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        for n in 1..=300 {
            let q = c.update(
                Feedback {
                    queue_micros: 5056,
                    congestion_events: n / 20,
                    transport_lost_packets: n / 20,
                    ..Default::default()
                },
                Duration::from_micros([26_262, 9_652, 17_189][n as usize % 3]),
                Duration::from_millis(2),
                false,
                now + Duration::from_millis(n * 100),
            );
            assert_eq!(q.scale_percent, 100);
            assert_eq!(q.fps, 60);
            assert!(q.bitrate > 10_000_000);
        }
    }
    #[test]
    fn manual_bandwidth_raises_the_ceiling_and_still_adapts_to_congestion() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        c.set_preset(QualityPreset::High, now);
        c.set_bandwidth_mbps(50);
        c.set_source_size(1920, 1080);
        assert_eq!(c.quality().bitrate, 50_000_000);
        c.congestion(now);
        c.congestion(now + Duration::from_millis(400));
        assert_eq!(c.quality().bitrate, 40_000_000);
        c.set_source_size(3840, 2160);
        assert_eq!(c.quality().bitrate, 40_000_000);
        let q = c.update(
            Feedback::default(),
            Duration::ZERO,
            Duration::ZERO,
            false,
            now + Duration::from_secs(4),
        );
        assert!(q.bitrate > 40_000_000 && q.bitrate <= 50_000_000);
        c.set_bandwidth_mbps(1);
        c.congestion(now + Duration::from_secs(5));
        c.congestion(now + Duration::from_millis(5400));
        assert!(c.quality().bitrate < 1_000_000);
    }
    #[test]
    fn clean_network_late_pictures_and_isolated_encoder_spikes_preserve_resolution() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        for n in 1..=300 {
            let at = now + Duration::from_millis(n * 100);
            if n % 10 == 0 {
                c.congestion(at);
            }
            let q = c.update(
                Feedback {
                    late_frames: n,
                    ..Default::default()
                },
                Duration::from_millis(2),
                Duration::from_millis(2),
                false,
                at,
            );
            assert_eq!(q.scale_percent, 100);
            assert_eq!(q.fps, 60);
            assert_eq!(
                q.bitrate, 12_000_000,
                "a brief local spike must not collapse a clean-link bitrate"
            );
        }
    }
    #[test]
    fn retina_budget_scales_with_source_pixels_and_preserves_congestion_reductions() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        c.set_source_size(3840, 2160);
        assert_eq!(c.quality().bitrate, 48_000_000);
        c.congestion(now);
        c.congestion(now + Duration::from_millis(400));
        assert_eq!(c.quality().bitrate, 38_400_000);
        c.set_source_size(1920, 1080);
        assert_eq!(c.quality().bitrate, 9_600_000);
        c.set_source_size(1920, 1080);
        assert_eq!(c.quality().bitrate, 9_600_000);
    }
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
    fn overload_settles_at_the_transport_rate_at_once_but_clean_links_ignore_it() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        c.set_preset(QualityPreset::High, now);
        // Clean link with a small (application-limited) window: no change.
        let q = c.update(
            Feedback {
                congestion_window_bytes: 12_000,
                ..Default::default()
            },
            Duration::from_millis(22),
            Duration::from_millis(18),
            false,
            now + Duration::from_millis(100),
        );
        assert_eq!(q.bitrate, 50_000_000);
        // The sender queue backs up behind a 10 KB window at 22 ms RTT
        // (about 3.6 Mbit/s): settle below it in one report.
        let q = c.update(
            Feedback {
                congestion_window_bytes: 10_000,
                queue_micros: 20_000,
                ..Default::default()
            },
            Duration::from_millis(22),
            Duration::from_millis(18),
            false,
            now + Duration::from_millis(200),
        );
        assert!(
            q.bitrate < 3_000_000 && q.bitrate >= 1_000_000,
            "{}",
            q.bitrate
        );
    }
    #[test]
    fn sustained_queueing_delay_is_overload_even_without_losses() {
        let now = Instant::now();
        let mut c = Controller::new(60, now);
        for n in 1..=10 {
            c.update(
                Feedback::default(),
                Duration::from_millis(100),
                Duration::from_millis(31),
                false,
                now + Duration::from_millis(100 * n),
            );
        }
        assert!(c.quality().bitrate < 12_000_000);
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
    fn bursty_loss_holds_parity_instead_of_flipping_every_window() {
        let start = Instant::now();
        let mut c = Controller::new(60, start);
        let mut received = 0;
        let mut missing = 0;
        let mut changes = 0;
        let mut last = None;
        // Wi-Fi bursts: heavy loss in every other 100 ms window for 3 s.
        for step in 0..30u64 {
            received += 100;
            missing += if step % 2 == 0 { 10 } else { 0 };
            let feedback = Feedback {
                delivery: DeliveryStats {
                    received_shards: received,
                    missing_shards: missing,
                    ..Default::default()
                },
                ..Default::default()
            };
            let now = start + Duration::from_millis(100 * step);
            let parity = c
                .update(feedback, Duration::ZERO, Duration::ZERO, false, now)
                .fec
                .parity_per_32;
            if last.is_some_and(|old| old != parity) {
                changes += 1;
            }
            last = Some(parity);
        }
        assert_eq!(last, Some(8));
        assert_eq!(changes, 0, "parity rose once at the start and then held");
        let mut window = |c: &mut Controller, at_ms: u64, lost: u64| {
            received += 1000;
            missing += lost;
            let feedback = Feedback {
                delivery: DeliveryStats {
                    received_shards: received,
                    missing_shards: missing,
                    ..Default::default()
                },
                ..Default::default()
            };
            c.update(
                feedback,
                Duration::ZERO,
                Duration::ZERO,
                false,
                start + Duration::from_millis(at_ms),
            )
            .fec
            .parity_per_32
        };
        assert_eq!(window(&mut c, 4_500, 0), 8, "held after the last burst");
        // Light loss (0.5%) once heavy loss is 5 s old: one step down, not two.
        assert_eq!(window(&mut c, 8_100, 5), 4);
        assert_eq!(window(&mut c, 9_000, 0), 4, "light loss holds too");
        assert_eq!(
            window(&mut c, 13_200, 0),
            2,
            "clean for the hold: lowest level"
        );
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
