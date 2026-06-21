//! Garbage collection: the runtime-wide memory-management floor.
//!
//! M1's GC is **refcount + (later) a cycle collector** (architecture §5). This crate owns
//! the *policy* — when to retain and when to release-and-free — while the unsafe refcount
//! primitives live in `lang-value`'s heap module. Keeping policy here (safe) and mechanism
//! there (unsafe, `miri`-gated) is what lets the cycle collector and `__destruct` ordering
//! (slice M1.6) grow in this crate without touching the value representation.
//!
//! In M1.0 the policy is the acyclic floor: `retain` bumps the count, `release` drops it and
//! frees at zero. Reference cycles are not yet collected (no cyclic types are reachable in
//! the M1.0 subset); the trial-deletion cycle collector arrives in M1.6.

use lang_value::Value;

/// Add an owning reference to `value` (no-op for immediates).
#[inline]
pub fn retain(value: Value) {
    value.inc_ref();
}

/// Drop an owning reference to `value`, freeing it (and, later, running `__destruct`) when
/// the last reference goes away. No-op for immediates.
#[inline]
pub fn release(value: Value) {
    if value.dec_ref() {
        value.free();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_frees_at_zero_and_retain_keeps_alive() {
        let v = Value::string("data");
        retain(v); // count 2
        release(v); // count 1, still alive
        assert_eq!(v.as_string().as_deref(), Some("data"));
        release(v); // count 0, freed — miri verifies no leak and no use-after-free
    }

    #[test]
    fn immediates_are_inert() {
        let v = Value::int(7);
        retain(v);
        release(v);
        assert_eq!(v.as_int(), Some(7));
    }
}
