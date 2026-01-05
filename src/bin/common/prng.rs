//! Tiny deterministic PRNG utilities for CLI tools.
//!
//! We intentionally avoid OS randomness here so that tools remain reproducible (particularly
//! important when debugging nondeterminism).

/// A small, fast deterministic PRNG based on SplitMix64.
///
/// This is not cryptographically secure; it is intended only for deterministic shuffling and
/// similar harness-level use.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
  state: u64,
}

impl SplitMix64 {
  pub fn new(seed: u64) -> Self {
    Self { state: seed }
  }

  pub fn next_u64(&mut self) -> u64 {
    // https://prng.di.unimi.it/splitmix64.c
    self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = self.state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
  }

  pub fn next_usize(&mut self, upper_exclusive: usize) -> usize {
    if upper_exclusive <= 1 {
      return 0;
    }
    (self.next_u64() % upper_exclusive as u64) as usize
  }
}

/// In-place Fisher–Yates shuffle driven by [`SplitMix64`].
pub fn shuffle<T>(items: &mut [T], seed: u64) {
  let mut rng = SplitMix64::new(seed);
  for i in (1..items.len()).rev() {
    let j = rng.next_usize(i + 1);
    items.swap(i, j);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn splitmix64_is_deterministic() {
    let mut a = SplitMix64::new(123);
    let mut b = SplitMix64::new(123);
    for _ in 0..32 {
      assert_eq!(a.next_u64(), b.next_u64());
    }
  }

  #[test]
  fn shuffle_is_deterministic() {
    let mut a = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let mut b = a.clone();
    shuffle(&mut a, 999);
    shuffle(&mut b, 999);
    assert_eq!(a, b);
  }
}
