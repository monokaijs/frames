//! Real-time [`MediaClock`] for running the engine on real transports.

use crate::api::MediaClock;
use std::time::Instant;

/// Microseconds since the clock was created.
#[derive(Clone, Copy, Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
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
    fn advances_monotonically() {
        let clock = SystemClock::new();
        let a = clock.now_us();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(clock.now_us() >= a + 2_000);
    }
}
