//! The M0 prelude support: the small set of built-in capabilities programs rely on
//! without importing anything.
//!
//! Deliberately value-agnostic for now. The runtime `Value` type lives in `noeta-eval`
//! during M0, so the value-returning builtins (`map`/`filter`/`sum`/`Ok`/`some`/...)
//! are wired up there as they land (Slice 2 onward). What lives here is the part that
//! does not depend on `Value`: deterministic identity generation and prelude metadata.
//! When the builtin set outgrows this split (M1), `Value` moves to a shared crate and
//! the functions move here.

/// A deterministic, seeded id source backing `next_id()`.
///
/// Determinism is a hard requirement: conformance output must not depend on wall
/// clock or address-space layout, or an agent cannot tell a real regression from a
/// flake. So `next_id` is a plain counter, not time- or random-based.
#[derive(Debug, Clone)]
pub struct IdGen {
    next: u64,
}

impl IdGen {
    /// Start from `seed` (use the same seed in tests for reproducible output).
    pub fn new(seed: u64) -> IdGen {
        IdGen { next: seed }
    }

    /// Return the current id and advance. Successive calls yield `seed`, `seed+1`, ...
    pub fn next_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }
}

impl Default for IdGen {
    fn default() -> IdGen {
        IdGen::new(1)
    }
}

/// The names reserved by the M0 prelude. Used by name resolution (Slice 8) and, later,
/// by the LSP to mark prelude identifiers. Grows as value-returning builtins land.
///
/// Keywords (`echo`, etc.) do NOT belong here: they are consumed by the parser as their own
/// statement forms and never reach identifier resolution, so listing them is dead weight and
/// misleadingly implies they are shadowable prelude bindings (they are not).
pub const PRELUDE_NAMES: &[&str] = &[
    "next_id",
    "len",
    "map",
    "filter",
    "sum",
    "Ok",
    "Err",
    "some",
    "none",
    "panic",
    "assert",
    "sleep",
    "all",
    "race",
    "map_bounded",
    // Reactivity S1/S2/S3: `signal(v)` creates a reactive cell; `computed(fn)` a lazy memoized
    // derivation; `effect(fn)` a side effect that reruns when a signal it reads changes.
    "signal",
    "computed",
    "effect",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_gen_is_deterministic() {
        let mut ids = IdGen::new(1);
        assert_eq!(ids.next_id(), 1);
        assert_eq!(ids.next_id(), 2);
        assert_eq!(ids.next_id(), 3);
    }
}
