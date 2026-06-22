//! The `random` Ring 2 module: a deterministic, seedable PRNG. Imported with
//! `use std.{random}` and called `random.int(1, 6)`, `random.float()`, `random.seed(42)`.
//!
//! Determinism is load-bearing for the project (the agent feedback loop, and the differential
//! oracle). So the generator is a *pure stepper* defined here once: given the current 64-bit
//! state it returns the next state and an output, with no hidden entropy. Each backend holds
//! the state (a single `u64`) and threads it through these functions, so a given seed yields
//! the identical sequence in both runtimes — by construction, not by test.
//!
//! The state defaults to [`DEFAULT_SEED`] (a fixed constant), so even un-seeded programs are
//! reproducible and backend-identical; a program calls `random.seed(n)` to pick a different
//! stream. The algorithm is SplitMix64 — tiny, dependency-free, well-distributed.

use crate::{Arg, ErrorKind, StdError};

/// The PRNG state both backends start from, so un-seeded `random` use is still deterministic
/// and identical across runtimes. (SplitMix64's standard increment constant — an arbitrary odd
/// 64-bit value.)
pub const DEFAULT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Advance the state and return `(next_state, output)`. SplitMix64.
pub fn next(state: u64) -> (u64, u64) {
    let next_state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = next_state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (next_state, z)
}

/// Re-seed: the new state *is* the seed (SplitMix64 mixes any state), so `seed(n)` makes the
/// stream a pure function of `n`. A negative `i64` seed reuses its two's-complement bit pattern.
pub fn seed_state(seed: i64) -> u64 {
    seed as u64
}

/// Draw an int uniformly in the inclusive range `[lo, hi]`, returning `(next_state, value)`.
/// `lo > hi` is an empty range and an error. Uses 128-bit math so the range width never
/// overflows and the modulo reduction is computed identically on every platform.
pub fn int(state: u64, lo: i64, hi: i64) -> Result<(u64, i64), StdError> {
    if lo > hi {
        return Err(range_error(lo, hi));
    }
    let (next_state, output) = next(state);
    let width = (hi as i128 - lo as i128 + 1) as u128;
    let value = lo as i128 + (output as u128 % width) as i128;
    Ok((next_state, value as i64))
}

/// Draw a float uniformly in `[0, 1)`, returning `(next_state, value)`. Uses the top 53 bits of
/// the output (the f64 mantissa width), so every representable value is reachable and the result
/// is bit-identical across backends.
pub fn float(state: u64) -> (u64, f64) {
    let (next_state, output) = next(state);
    let value = (output >> 11) as f64 / (1u64 << 53) as f64;
    (next_state, value)
}

/// The canonical "empty range" error for `random.int(lo, hi)` with `lo > hi` (→ `E0007`).
pub fn range_error(lo: i64, hi: i64) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("random.int range [{lo}, {hi}] is empty (lo must not exceed hi)"),
    }
}

/// Read an int argument for a `random` function (used by both backends' dispatch glue).
pub fn want_int(func: &str, args: &[Arg], index: usize) -> Result<i64, StdError> {
    match args[index] {
        Arg::Int(value) => Ok(value),
        _ => Err(StdError {
            kind: ErrorKind::ArgType,
            message: format!("function `{func}` expects an int argument"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_is_deterministic() {
        // The same seed yields the same stream — the whole point.
        let (s1, a) = float(DEFAULT_SEED);
        let (_s2, b) = float(s1);
        let (t1, a2) = float(DEFAULT_SEED);
        let (_t2, b2) = float(t1);
        assert_eq!(a, a2);
        assert_eq!(b, b2);
        // ...and successive draws differ (not a stuck generator).
        assert_ne!(a, b);
    }

    #[test]
    fn float_stays_in_unit_interval() {
        let mut state = DEFAULT_SEED;
        for _ in 0..1000 {
            let (next_state, value) = float(state);
            assert!((0.0..1.0).contains(&value), "{value} out of [0, 1)");
            state = next_state;
        }
    }

    #[test]
    fn int_stays_in_inclusive_range() {
        let mut state = seed_state(7);
        let mut saw_lo = false;
        let mut saw_hi = false;
        for _ in 0..1000 {
            let (next_state, value) = int(state, 1, 6).expect("non-empty range");
            assert!((1..=6).contains(&value), "{value} out of [1, 6]");
            saw_lo |= value == 1;
            saw_hi |= value == 6;
            state = next_state;
        }
        // Over 1000 draws a die hits both ends.
        assert!(saw_lo && saw_hi);
    }

    #[test]
    fn negative_and_wide_ranges() {
        let (_s, value) = int(seed_state(1), -5, -5).expect("singleton range");
        assert_eq!(value, -5);
        let (_s, value) = int(seed_state(1), i64::MIN, i64::MAX).expect("full range");
        let _ = value; // Must not overflow; any value is in range.
    }

    #[test]
    fn empty_range_is_an_error() {
        match int(0, 6, 1) {
            Err(error) => assert_eq!(error.kind, ErrorKind::ArgType),
            Ok(_) => panic!("expected an error for lo > hi"),
        }
    }

    #[test]
    fn seed_makes_the_stream_a_function_of_the_seed() {
        let (_a, x) = int(seed_state(42), 0, 1_000_000).unwrap();
        let (_b, y) = int(seed_state(42), 0, 1_000_000).unwrap();
        let (_c, z) = int(seed_state(43), 0, 1_000_000).unwrap();
        assert_eq!(x, y);
        assert_ne!(x, z);
    }
}
