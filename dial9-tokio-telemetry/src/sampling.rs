//! Shared geometric/Poisson sampling primitives.
//!
//! Used by the task-dump idle sampler (sampling on nanoseconds) and by the
//! memory profiler (sampling on bytes). The unit is opaque to the math —
//! callers pass the mean and treat the returned u64 as a counter in their
//! native unit.

/// Minimal splitmix64 PRNG. Fast, no dependencies, good enough for sampling.
pub(crate) struct SplitMix64(u64);

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Draw from an exponential distribution with the given mean.
    /// Returns at least 1 to avoid immediate re-trigger when the
    /// counter goes to zero.
    ///
    /// The unit is whatever the caller treats `mean` as (nanoseconds,
    /// bytes, etc.).
    pub(crate) fn draw_exponential(&mut self, mean: u64) -> u64 {
        // Generate a uniform float in (0, 1] — avoid exact 0 to prevent ln(0).
        let u = (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64);
        let u = if u == 0.0 { f64::MIN_POSITIVE } else { u };
        let sample = -u.ln() * (mean as f64);
        (sample as u64).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::SplitMix64;

    #[test]
    fn splitmix_deterministic_with_fixed_seed() {
        let mut rng = SplitMix64::new(42);
        let a = rng.next_u64();
        let b = rng.next_u64();

        let mut rng2 = SplitMix64::new(42);
        assert_eq!(a, rng2.next_u64());
        assert_eq!(b, rng2.next_u64());
    }

    #[test]
    fn draw_exponential_returns_at_least_1() {
        let mut rng = SplitMix64::new(0);
        for _ in 0..1000 {
            assert!(rng.draw_exponential(1) >= 1);
        }
    }

    #[test]
    fn draw_exponential_mean_approximates_target() {
        let mut rng = SplitMix64::new(123);
        let mean: u64 = 1024;
        let n = 100_000;
        let sum: f64 = (0..n).map(|_| rng.draw_exponential(mean) as f64).sum();
        let observed_mean = sum / n as f64;
        // Within ±5% of the configured mean.
        assert!(
            (observed_mean - mean as f64).abs() < mean as f64 * 0.05,
            "observed mean {observed_mean} too far from expected {mean}"
        );
    }

    #[test]
    fn draw_exponential_handles_large_mean() {
        let mut rng = SplitMix64::new(999);
        let mean: u64 = 1_000_000_000;
        let mut saw_large = false;
        for _ in 0..1000 {
            let v = rng.draw_exponential(mean);
            assert!(v >= 1);
            if v > 1_000_000 {
                saw_large = true;
            }
        }
        assert!(saw_large, "expected some values much larger than 1");
    }
}
