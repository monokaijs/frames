//! Unwrapping 32-bit wire counters into 64-bit values.
//!
//! The wire carries sequence numbers, frame numbers, and microsecond
//! timestamps as `u32` to keep headers small. Values that move forward by
//! less than 2^31 between observations unwrap unambiguously: each new value
//! is placed at the 64-bit position nearest to the previous one.

#[derive(Clone, Copy, Debug, Default)]
pub struct Unwrapper {
    last: Option<u64>,
}

impl Unwrapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts from a known 64-bit value (e.g. a local clock reading).
    pub fn starting_at(value: u64) -> Self {
        Self { last: Some(value) }
    }

    /// Maps `value` to the nearest 64-bit value with the same low 32 bits.
    /// Older values (reordering) map below the last one, never below zero.
    pub fn unwrap(&mut self, value: u32) -> u64 {
        let Some(last) = self.last else {
            self.last = Some(u64::from(value));
            return u64::from(value);
        };
        let delta = i64::from(value.wrapping_sub(last as u32) as i32);
        let unwrapped = (last as i64).saturating_add(delta).max(0) as u64;
        // Track the highest value seen, so a reordered packet does not move
        // the reference backwards.
        if unwrapped > last {
            self.last = Some(unwrapped);
        }
        unwrapped
    }

    /// Like [`Self::unwrap`] without updating the reference.
    pub fn peek(&self, value: u32) -> u64 {
        let mut copy = *self;
        copy.unwrap(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_across_the_32_bit_boundary_and_tolerates_reordering() {
        let mut unwrapper = Unwrapper::starting_at(u64::from(u32::MAX) - 2);
        assert_eq!(unwrapper.unwrap(u32::MAX - 1), u64::from(u32::MAX) - 1);
        assert_eq!(unwrapper.unwrap(1), u64::from(u32::MAX) + 2);
        assert_eq!(
            unwrapper.unwrap(u32::MAX),
            u64::from(u32::MAX),
            "a reordered older value maps just below"
        );
        assert_eq!(unwrapper.unwrap(2), u64::from(u32::MAX) + 3);
        assert_eq!(unwrapper.peek(10), u64::from(u32::MAX) + 11);
        assert_eq!(
            unwrapper.unwrap(3),
            u64::from(u32::MAX) + 4,
            "peek changes nothing"
        );
    }

    #[test]
    fn first_value_and_early_reordering_never_go_negative() {
        let mut unwrapper = Unwrapper::new();
        assert_eq!(unwrapper.unwrap(5), 5);
        assert_eq!(unwrapper.unwrap(u32::MAX), 0, "clamped instead of negative");
        assert_eq!(unwrapper.unwrap(6), 6);
    }

    #[test]
    fn long_monotonic_runs_unwrap_exactly() {
        let mut unwrapper = Unwrapper::new();
        let mut expected = 0u64;
        let step = 0x3FFF_FFFFu64;
        for _ in 0..64 {
            expected += step;
            assert_eq!(unwrapper.unwrap(expected as u32), expected);
        }
    }
}
