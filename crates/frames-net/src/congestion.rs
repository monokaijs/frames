//! QUIC congestion control for a Frames connection.
//!
//! While a media stream runs, the media engine's delay-based controller owns
//! the sending rate (spike 0-A showed QUIC's RTT does not see relay queueing),
//! and QUIC's window and pacing follow its target. With no media target set,
//! an inner Cubic controller governs as usual, so bulk transfers and control
//! traffic stay congestion-safe on their own.

use noq_proto::RttEstimator;
use noq_proto::congestion::{Controller, ControllerFactory, ControllerMetrics, Cubic, CubicConfig};
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Shared media target in bits per second; 0 = no media, Cubic governs.
#[derive(Clone, Debug, Default)]
pub struct MediaTarget(Arc<AtomicU64>);

impl MediaTarget {
    pub fn set(&self, bits_per_second: u64) {
        self.0.store(bits_per_second, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// The engine paces its own packets at a multiple of the target; QUIC's
/// pacer must never be the slower one.
const PACING_HEADROOM: u64 = 3;

#[derive(Debug, Clone)]
pub struct MediaController {
    target: MediaTarget,
    cubic: Cubic,
    rtt: Duration,
    mtu: u16,
}

impl MediaController {
    fn media_window(&self, bits_per_second: u64) -> u64 {
        let rtt = self.rtt.max(Duration::from_millis(5)).as_secs_f64();
        let window = (bits_per_second as f64 / 8.0 * rtt * 2.0) as u64;
        window.max(32 * u64::from(self.mtu))
    }
}

impl Controller for MediaController {
    fn on_sent(&mut self, now: Instant, bytes: u64, largest_pn: u64) {
        self.cubic.on_sent(now, bytes, largest_pn);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        pn: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.rtt = rtt.get();
        self.cubic.on_ack(now, sent, bytes, pn, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.cubic
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        is_ecn: bool,
        lost_bytes: u64,
        largest_lost_pn: u64,
    ) {
        // Cubic always learns; with media running, loss reaches the media
        // controller through the receiver's reports instead.
        self.cubic.on_congestion_event(
            now,
            sent,
            is_persistent_congestion,
            is_ecn,
            lost_bytes,
            largest_lost_pn,
        );
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu;
        self.cubic.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        match self.target.get() {
            0 => self.cubic.window(),
            target => self.media_window(target),
        }
    }

    fn metrics(&self) -> ControllerMetrics {
        match self.target.get() {
            0 => self.cubic.metrics(),
            target => {
                let mut metrics = ControllerMetrics::default();
                metrics.congestion_window = self.media_window(target);
                metrics.pacing_rate = Some(target / 8 * PACING_HEADROOM);
                metrics
            }
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.cubic.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Builds one [`MediaController`] per path, sharing the connection's target.
#[derive(Debug)]
pub struct MediaControllerFactory {
    pub target: MediaTarget,
}

impl ControllerFactory for MediaControllerFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        let cubic = Arc::new(CubicConfig::default()).build(now, current_mtu);
        let cubic = *cubic
            .into_any()
            .downcast::<Cubic>()
            .expect("CubicConfig builds Cubic");
        Box::new(MediaController {
            target: self.target.clone(),
            cubic,
            rtt: Duration::from_millis(50),
            mtu: current_mtu,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_media_target_and_falls_back_to_cubic() {
        let target = MediaTarget::default();
        let factory = Arc::new(MediaControllerFactory {
            target: target.clone(),
        });
        let controller = factory.build(Instant::now(), 1200);
        let cubic_window = controller.window();
        assert!(
            controller.metrics().pacing_rate.is_none(),
            "Cubic paces itself"
        );
        target.set(40_000_000);
        // 40 Mbps for 2 × the 50 ms initial RTT.
        assert_eq!(controller.window(), 500_000);
        assert_eq!(controller.metrics().pacing_rate, Some(15_000_000));
        target.set(0);
        assert_eq!(controller.window(), cubic_window);
    }
}
