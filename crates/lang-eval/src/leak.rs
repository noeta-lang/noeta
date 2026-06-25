//! The tree-walker's live-heap accounting — the eval-side half of the leak oracle
//! (architecture §0/§5).
//!
//! The VM counts every [`lang_value`] heap object directly (`lang_value::live_count`). The
//! tree-walker reclaims through Rust `Rc`, so there is no single allocation chokepoint to
//! instrument; instead we count the heap-bearing aggregates that can *leak* — the ones that
//! participate in reference cycles: [`Scope`](crate::Scope), [`Closure`](crate::Closure), and
//! [`ObjectValue`](crate::ObjectValue). Each bumps this per-isolate (thread-local) counter when
//! constructed and drops it in its `Drop` impl. A program that reclaims cleanly returns the count
//! to its starting value; the known global-function ↔ global-scope cycle keeps a `Scope` (and its
//! `Closure`) alive past exit, so the oracle reports a positive residual for programs with
//! top-level functions — the exact debt Phase 6 drives to zero.
//!
//! The count is a signed integer so that a *missing* increment (a construction site that forgot to
//! route through the counted constructor) surfaces as a negative count rather than an unsigned
//! underflow — an under-count is self-revealing, while the `Drop` side is centralized and cannot be
//! missed.

use std::cell::Cell;

thread_local! {
    static LIVE: Cell<i64> = const { Cell::new(0) };
}

/// The net number of live counted aggregates (`Scope` + `Closure` + `ObjectValue`) on this
/// isolate's thread. The leak oracle compares the per-program delta against zero.
pub fn live_count() -> i64 {
    LIVE.with(|c| c.get())
}

/// Record the construction of a counted aggregate.
pub(crate) fn inc() {
    LIVE.with(|c| c.set(c.get() + 1));
}

/// Record the reclamation of a counted aggregate (called from its `Drop`).
pub(crate) fn dec() {
    LIVE.with(|c| c.set(c.get() - 1));
}
