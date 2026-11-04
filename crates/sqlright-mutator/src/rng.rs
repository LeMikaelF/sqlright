//! Random numbers with the ranges of the C++ helpers in `utils.h`.

use rand::rngs::SmallRng;
use rand::{Rng as _, SeedableRng};

/// A seeded random number generator. All random choices of the mutator and
/// the oracles come from here, so a run is reproducible from its seed.
#[derive(Debug, Clone)]
pub struct Rng {
    inner: SmallRng,
}

impl Rng {
    pub fn seeded(seed: u64) -> Rng {
        Rng {
            inner: SmallRng::seed_from_u64(seed),
        }
    }

    /// A random index in `0..range`, or 0 when `range` is 0.
    pub fn rand_int(&mut self, range: usize) -> usize {
        if range == 0 {
            0
        } else {
            self.inner.gen_range(0..range)
        }
    }

    /// A random value in `start..end`, or 0 when the range is empty
    /// (like the C++ `get_rand_int(start, end)`).
    pub fn rand_range(&mut self, start: i64, end: i64) -> i64 {
        if end > start {
            self.inner.gen_range(start..end)
        } else {
            0
        }
    }

    /// A random value in `0..range` for large ranges.
    pub fn rand_i64(&mut self, range: i64) -> i64 {
        if range > 0 {
            self.inner.gen_range(0..range)
        } else {
            0
        }
    }

    /// A random double in `min..max`. Returns 0 when `max < min` and `min`
    /// when they are equal, like the C++ `get_rand_double`.
    pub fn rand_double(&mut self, min: f64, max: f64) -> f64 {
        if max < min {
            0.0
        } else if max == min {
            min
        } else {
            let unit: f64 = self.inner.gen_range(0.0..1.0);
            (max - min) * unit + min
        }
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: usize, den: usize) -> bool {
        self.rand_int(den) < num
    }

    /// A random element of a slice.
    pub fn choose<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            let i = self.rand_int(items.len());
            Some(&items[i])
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.inner.gen()
    }
}
