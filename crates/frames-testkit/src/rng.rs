//! SplitMix64: tiny, seeded, and good enough for simulation decisions.

#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn chance(&mut self, probability: f64) -> bool {
        probability > 0.0 && self.unit() < probability
    }

    /// Uniform in `[0, n)`; `n == 0` gives 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_roughly_uniform() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let draws: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        assert_eq!(draws, (0..8).map(|_| b.next_u64()).collect::<Vec<_>>());
        let mut rng = Rng::new(1);
        let hits = (0..100_000).filter(|_| rng.chance(0.25)).count();
        assert!((23_000..27_000).contains(&hits), "{hits}");
        assert!(!rng.chance(0.0));
        assert_eq!(rng.below(0), 0);
    }
}
