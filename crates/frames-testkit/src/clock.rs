//! Virtual microsecond clock shared by every simulated component.

use frames_media::api::MediaClock;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Default)]
pub struct VirtualClock(Arc<AtomicU64>);

impl VirtualClock {
    pub fn new(start_us: u64) -> Self {
        Self(Arc::new(AtomicU64::new(start_us)))
    }

    pub fn advance(&self, us: u64) -> u64 {
        self.0.fetch_add(us, Ordering::SeqCst) + us
    }

    /// Moves the clock to `us`; never backwards.
    pub fn advance_to(&self, us: u64) {
        self.0.fetch_max(us, Ordering::SeqCst);
    }
}

impl MediaClock for VirtualClock {
    fn now_us(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Real time, for hardware runs. A [`VirtualClock`] can follow it with
/// `advance_to(system.now_us())`, so simulated links run in real time.
pub use frames_media::clock::SystemClock;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_time_and_never_go_back() {
        let clock = VirtualClock::new(10);
        let other = clock.clone();
        assert_eq!(clock.advance(5), 15);
        assert_eq!(other.now_us(), 15);
        other.advance_to(12);
        assert_eq!(clock.now_us(), 15);
        other.advance_to(40);
        assert_eq!(clock.now_us(), 40);
    }
}
