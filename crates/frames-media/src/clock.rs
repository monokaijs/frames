//! Real-time [`MediaClock`] for running the engine on real transports.

use crate::api::MediaClock;
use std::sync::OnceLock;
use std::time::Instant;

fn process_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Monotonic microseconds since the first clock in this process. Every
/// instance shares that epoch, so timestamps taken by different pipeline
/// stages and threads compare directly.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            epoch: process_epoch(),
        }
    }
}

impl SystemClock {
    /// The [`MediaClock::now_us`] value for `at`.
    pub fn micros_at(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_micros() as u64
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaClock for SystemClock {
    fn now_us(&self) -> u64 {
        self.epoch.elapsed().as_micros() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instances_share_one_epoch() {
        let a = SystemClock::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = SystemClock::new();
        let (ta, tb) = (a.now_us(), b.now_us());
        assert!(tb >= ta && tb - ta < 1_000, "{ta} {tb}");
    }

    #[test]
    fn advances_monotonically() {
        let clock = SystemClock::new();
        let a = clock.now_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(clock.now_us() >= a + 2_000);
    }
}
