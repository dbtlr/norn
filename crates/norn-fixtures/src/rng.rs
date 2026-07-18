//! SplitMix64 — a small, dependency-free deterministic PRNG.
//!
//! Same algorithm as the one used by `retired/tests/eav_parity_property.rs`
//! (see that file for the wave-2 acceptance property-test precedent). Copied
//! rather than shared because `retired/` is documentation, not source — see
//! `retired/CLAUDE.md`.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..n`. Panics if `n == 0`.
    pub fn range(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Uniform value in `0..n` as u64. Panics if `n == 0`.
    pub fn range_u64(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// True with probability `num`/`denom` (per-mille style: `chance(37, 1000)`).
    pub fn chance(&mut self, num: u32, denom: u32) -> bool {
        self.range_u64(denom as u64) < num as u64
    }

    /// Pick a uniformly random element from a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }
}
