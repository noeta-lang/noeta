//! Use-after-drop audit (memory-management migration): the runtime half of the
//! **static-≤-dynamic last-use property** — a machine check that no *computed* death (an inserted
//! [`noeta_ir::Stmt::DropVar`]) ever precedes the *real* dynamic last use of its binding.
//!
//! The drop-insertion pass releases a function-local's value at the program point where the
//! liveness analysis says it dies. If that point were ever *earlier* than the binding's last
//! actual read on some path, the value would be reclaimed while still needed — the one way drop
//! placement can be unsound. This audit detects exactly that, dynamically: while active, the IR
//! interpreter's [`Scope`](crate::Scope) reports every drop, rebind, and read; a read of a binding
//! whose value has been dropped (and not since rebound) is a **violation**.
//!
//! It is **always compiled but inert** (one thread-local bool check on the read/drop/bind paths)
//! until [`begin`] turns it on — matching the house style of the always-on leak oracle. The
//! conformance corpus runs the whole sweep under [`begin`]/[`end`] and asserts zero violations, so
//! the property is checked against ground-truth execution over every program, independent of the
//! static liveness reasoning that placed the drops.
//!
//! Poison is keyed by `(scope pointer, name)`: a drop poisons that binding in that exact scope, a
//! rebind (a fresh `declare`, or a reassignment) clears it, and a read of a poisoned binding is the
//! violation. Per-scope keying means a shadow or a fresh per-iteration loop scope is a distinct
//! binding and never a false positive.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

thread_local! {
    /// Whether the audit is recording. Off by default; the read/drop/bind hooks short-circuit on it
    /// so production (and any run outside [`begin`]/[`end`]) pays only this one bool check.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// `(scope pointer, binding name)` pairs whose value has been dropped and not since rebound.
    static POISON: RefCell<HashSet<(usize, String)>> = RefCell::new(HashSet::new());
    /// Count of reads of a poisoned binding observed since [`begin`].
    static VIOLATIONS: Cell<usize> = const { Cell::new(0) };
}

/// Start auditing on this thread: clear prior state and begin recording. Pair with [`end`].
pub fn begin() {
    POISON.with(|p| p.borrow_mut().clear());
    VIOLATIONS.with(|v| v.set(0));
    ACTIVE.with(|a| a.set(true));
}

/// Stop auditing and return the number of use-after-drop violations observed since [`begin`].
pub fn end() -> usize {
    ACTIVE.with(|a| a.set(false));
    VIOLATIONS.with(|v| v.get())
}

#[inline]
fn active() -> bool {
    ACTIVE.with(|a| a.get())
}

/// Record that `name`'s value was dropped in the scope identified by `scope` (a `DropVar`).
#[inline]
pub(crate) fn on_drop(scope: usize, name: &str) {
    if active() {
        POISON.with(|p| p.borrow_mut().insert((scope, name.to_string())));
    }
}

/// Record that `name` was (re)bound in `scope` — clears any poison, so a later read is legitimate.
#[inline]
pub(crate) fn on_bind(scope: usize, name: &str) {
    if active() {
        POISON.with(|p| p.borrow_mut().remove(&(scope, name.to_string())));
    }
}

/// Record a read of `name` in `scope`; a hit on a poisoned binding is a use-after-drop violation.
#[inline]
pub(crate) fn on_read(scope: usize, name: &str) {
    if active() && POISON.with(|p| p.borrow().contains(&(scope, name.to_string()))) {
        VIOLATIONS.with(|v| v.set(v.get() + 1));
    }
}

/// Drop every poison entry for a scope as it is freed. This keeps the poison set small (only
/// live scopes' dropped-not-rebound bindings) and — crucially — prevents an ABA false positive: a
/// later scope reusing this freed address can never inherit a stale poison.
#[inline]
pub(crate) fn on_scope_drop(scope: usize) {
    if active() {
        POISON.with(|p| p.borrow_mut().retain(|(s, _)| *s != scope));
    }
}
