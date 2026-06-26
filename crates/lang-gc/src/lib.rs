//! Garbage collection: the runtime-wide memory-management floor.
//!
//! M1's GC is **refcount + a cycle collector** (architecture §5). This crate owns the *policy*
//! — when to retain and when to release-and-free, and the trial-deletion cycle collector —
//! while the unsafe refcount/graph primitives live in `lang-value`'s heap module. Keeping
//! policy here (safe) and mechanism there (unsafe, `miri`-gated) lets the collector grow
//! without touching the value representation.
//!
//! The acyclic floor: `retain` bumps the count, `release` drops it and frees at zero (running
//! `__destruct` is the VM's job, since a destructor needs the interpreter). Reference cycles —
//! objects kept alive only by referencing each other — escape refcounting; the
//! [`CycleCollector`] reclaims them by **Bacon–Rajan synchronous trial deletion**.
//!
//! The collector is correct and `miri`-tested, but **not yet wired into the VM's `release`
//! path**: the current language cannot form a cycle (objects are immutable after construction,
//! so no program can tie a knot), so there is no cyclic garbage to buffer. It activates once
//! field mutation lands and `release` begins buffering candidate roots.

use std::collections::HashSet;

use lang_value::{Color, Value};

/// Add an owning reference to `value` (no-op for immediates).
#[inline]
pub fn retain(value: Value) {
    value.inc_ref();
}

/// Drop an owning reference to `value`, reclaiming it through the **active cycle collector**
/// (Phase 6.4): a prompt refcount free in `Trace` mode, or the Bacon–Rajan `Decrement` (buffer a
/// surviving cycle root, defer a buffered object's dealloc) in `TrialDeletion` mode. No-op for
/// immediates. (`__destruct` is the VM's job — see [`crate`] docs.)
#[inline]
pub fn release(value: Value) {
    value.release();
}

/// **Backup mark-sweep trace** (Phase 6, the LXR-principled backup collector): a stop-the-world
/// trace over the live-object registry that reclaims everything unreachable from `roots` —
/// reference cycles and floating garbage alike, the cases refcounting alone cannot. Run at a
/// safepoint (the root set must be fully walkable): the interpreter enumerates its roots (globals,
/// live frame registers, open upvalue cells) and hands them here. Marks reachable objects, then
/// frees the unmarked via `gc_free_shallow` in a flat pass over a registry snapshot (so the graph is
/// intact during marking and no object is touched after it is freed).
///
/// Colors: `alloc` paints every object `Black`; this trace paints reachable objects `Gray` and
/// resets survivors to `Black`, so `Black`-after-mark == unreachable garbage. (`gc_free_shallow`
/// does not run `__destruct` — destructor-bearing cycles are a documented follow-up; the closure/
/// scope cycles this phase targets carry none.)
pub fn collect_trace(roots: &[Value]) {
    for &root in roots {
        mark(root);
    }
    let live = lang_value::live_objects();
    // Garbage is everything the mark did not reach; identify it *before* resetting survivors (which
    // also become `Black`), so the two are distinguishable.
    let garbage: Vec<Value> = live
        .iter()
        .copied()
        .filter(|v| v.gc_color() != Color::Gray)
        .collect();
    for &v in &live {
        if v.gc_color() == Color::Gray {
            v.gc_set_color(Color::Black);
        }
    }
    for v in garbage {
        v.gc_free_shallow();
    }
}

/// **Trial-deletion collection** (Bacon–Rajan synchronous, Phase 6.4): reclaim the unreachable
/// cycles among the buffered candidate roots — the objects whose refcount was decremented without
/// reaching zero since the last collection. Unlike [`collect_trace`] it never scans the whole heap;
/// it examines only the candidate subgraph, the trade-off the 6.4 benchmark weighs against the
/// trace's per-allocation registry cost.
///
/// `MarkRoots` trial-decrements each purple candidate's subgraph (so edges *internal* to it stop
/// counting) and disposes of any candidate that is no longer purple — a deferred-dealloc object
/// (black, refcount 0, its children already released when it hit zero) is freed here. `ScanRoots`
/// restores any subgraph still externally referenced; `CollectRoots` frees what stays white (the
/// genuine cycles). Reuses the same `mark_gray`/`scan`/`gather_white` primitives as the dormant
/// [`CycleCollector`].
pub fn collect_trial_deletion() {
    let roots = lang_value::take_candidates();
    let mut gray_roots = Vec::new();
    let mut deferred = Vec::new();
    for &s in &roots {
        // Each candidate is consumed by this collection — unbuffer it up front so the deferral guard
        // in `free_shallow` won't fire while we reclaim it below.
        s.gc_set_buffered(false);
        match s.gc_color() {
            // A live possible root: trial-decrement its subgraph.
            Color::Purple => {
                mark_gray(s);
                gray_roots.push(s);
            }
            // A **deferred-dealloc** object: it hit its last reference while buffered (children already
            // released, only its box held back so the buffer never dangled). Reclaimed in the final
            // pass — but `mark_gray` above may recolor it gray if it sits inside a candidate's
            // subgraph, in which case the cycle collection frees it and the dedup set skips it here.
            _ => deferred.push(s),
        }
    }
    for &s in &gray_roots {
        scan(s);
    }
    let mut garbage = Vec::new();
    for &s in &gray_roots {
        gather_white(s, &mut garbage);
    }
    // All traversal is done; reclaim. `freed` dedups by address so nothing is freed — or even
    // dereferenced — twice (a deferred object pulled into a cycle is reclaimed here and skipped below).
    let mut freed: HashSet<u64> = HashSet::with_capacity(garbage.len() + deferred.len());
    for v in garbage {
        if freed.insert(v.bits()) {
            v.gc_free_shallow();
        }
    }
    for v in deferred {
        if freed.insert(v.bits()) {
            v.gc_free_shallow();
        }
    }
}

/// Paint `s` and everything reachable from it `Gray` (reachable/live). Idempotent via the color
/// check, so shared substructure and cycles terminate.
fn mark(s: Value) {
    if !s.is_pointer() || s.gc_color() == Color::Gray {
        return;
    }
    s.gc_set_color(Color::Gray);
    for child in s.gc_children() {
        mark(child);
    }
}

/// A trial-deletion cycle collector (Bacon–Rajan synchronous collection, architecture §5).
///
/// When a reference is dropped without the count reaching zero, the object is a *possible*
/// cycle root — [`add_candidate`](CycleCollector::add_candidate) buffers it. [`collect`](
/// CycleCollector::collect) then trial-deletes: it follows internal references decrementing
/// counts, and any object whose count falls to zero is reachable only from within the buffered
/// subgraph — a cycle — and is freed. Objects reachable from outside survive (their counts are
/// restored). Best-effort destruction order for cycles, matching the spec's weaker guarantee
/// for the cyclic case.
#[derive(Debug, Default)]
pub struct CycleCollector {
    roots: Vec<Value>,
}

impl CycleCollector {
    pub fn new() -> CycleCollector {
        CycleCollector { roots: Vec::new() }
    }

    /// Buffer an object as a possible cycle root (colored `Purple`, recorded once). Call this
    /// from `release` when a decrement leaves the count above zero.
    pub fn add_candidate(&mut self, value: Value) {
        if !value.is_pointer() {
            return;
        }
        value.gc_set_color(Color::Purple);
        if !value.gc_buffered() {
            value.gc_set_buffered(true);
            self.roots.push(value);
        }
    }

    /// Run a collection over the buffered roots, freeing any unreachable cycles.
    pub fn collect(&mut self) {
        let roots = std::mem::take(&mut self.roots);
        // Mark: trial-decrement internal references, painting reachable-within-subgraph gray.
        for &root in &roots {
            if root.gc_color() == Color::Purple {
                mark_gray(root);
            }
        }
        // Scan: restore objects still externally referenced (count > 0) to black; the rest
        // are provisionally white (garbage).
        for &root in &roots {
            scan(root);
        }
        // Collect in two phases so a member is never dereferenced after it is freed: first
        // *gather* every white object (depth-first, recoloring black to avoid revisits) while
        // the graph is still intact, then free the gathered set in a flat pass.
        let mut garbage = Vec::new();
        for &root in &roots {
            gather_white(root, &mut garbage);
        }
        for &root in &roots {
            root.gc_set_buffered(false);
        }
        for value in garbage {
            value.gc_free_shallow();
        }
    }
}

/// Paint `s` gray and trial-decrement each child's count, recursing — so edges *internal* to
/// the candidate subgraph no longer count toward keeping a node alive.
fn mark_gray(s: Value) {
    if s.gc_color() == Color::Gray {
        return;
    }
    s.gc_set_color(Color::Gray);
    for child in s.gc_children() {
        child.gc_rc_dec();
        mark_gray(child);
    }
}

/// If `s` still has external references (count > 0) restore its subgraph to black; otherwise
/// paint it white (provisional garbage) and scan its children.
fn scan(s: Value) {
    if s.gc_color() != Color::Gray {
        return;
    }
    if s.refcount() > 0 {
        scan_black(s);
    } else {
        s.gc_set_color(Color::White);
        for child in s.gc_children() {
            scan(child);
        }
    }
}

/// Restore `s` (and its still-gray subgraph) to black, undoing the trial decrements.
fn scan_black(s: Value) {
    s.gc_set_color(Color::Black);
    for child in s.gc_children() {
        child.gc_rc_inc();
        if child.gc_color() != Color::Black {
            scan_black(child);
        }
    }
}

/// Gather every white object reachable from `s` into `garbage` (the cycle to reclaim),
/// recoloring black so each is collected exactly once. Nothing is freed here, so every
/// `children` read sees an intact graph — the actual frees happen in a flat pass afterward.
fn gather_white(s: Value, garbage: &mut Vec<Value>) {
    if s.gc_color() == Color::White {
        s.gc_set_color(Color::Black);
        garbage.push(s);
        for child in s.gc_children() {
            gather_white(child, garbage);
        }
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

    use lang_object::{Shape, ShapeKind};
    use std::rc::Rc;

    /// A one-slot object whose slot starts as unit — the building block for a heap cycle.
    fn cell() -> Value {
        let shape = Rc::new(Shape::object(ShapeKind::Class, "Cell", vec!["next".into()]));
        Value::object(shape, vec![Value::unit()])
    }

    #[test]
    fn cycle_collector_reclaims_an_unreachable_cycle() {
        // Build A <-> B (each slot points at the other), then drop the external handles. The
        // two objects now reference only each other — a cycle refcounting cannot reclaim. The
        // collector frees them; miri verifies no leak and no use-after-free.
        let a = cell();
        let b = cell();
        a.set_slot(0, b); // b retained by a's slot (b refcount 2)
        b.set_slot(0, a); // a retained by b's slot (a refcount 2)
        // Drop the external references (as if the bindings holding a and b went out of scope).
        release(a); // a: 2 -> 1 (still held by b's slot)
        release(b); // b: 2 -> 1 (still held by a's slot)
        assert_eq!(
            a.refcount(),
            1,
            "the cycle keeps a alive despite no external ref"
        );
        assert_eq!(b.refcount(), 1);

        let mut gc = CycleCollector::new();
        gc.add_candidate(a);
        gc.add_candidate(b);
        gc.collect(); // frees both members of the cycle
    }

    #[test]
    fn cycle_collector_reclaims_a_closure_capturing_its_own_cell() {
        // The self-recursive nested `fn` shape: a cell holds a closure, and that closure captures
        // the cell as its upvalue (cell -> closure -> cell). Once the external handle is dropped
        // the pair is an unreachable cycle that only the collector can free.
        let cell = Value::cell(Value::unit());
        let closure = Value::closure(0, vec![cell]); // closure owns one ref to the cell
        cell.cell_set(closure); // cell now holds the closure (closure refcount 2)
        release(closure); // drop the external handle: closure 2 -> 1, kept alive only by the cell
        assert_eq!(closure.refcount(), 1, "the cycle keeps the closure alive");

        let mut gc = CycleCollector::new();
        gc.add_candidate(cell);
        gc.add_candidate(closure);
        gc.collect(); // frees both members of the cycle; miri verifies no leak
    }

    #[test]
    fn cycle_collector_spares_an_externally_referenced_object() {
        // a -> b, and b is also held by an outside reference. The collector must NOT free b
        // (or a, which b would drag down): only genuine garbage is reclaimed.
        let a = cell();
        let b = cell();
        a.set_slot(0, b); // b refcount 2 (alloc + a's slot)
        b.set_slot(0, a); // a refcount 2
        retain(b); // an external owner of b (b refcount 3)
        release(a); // a: 2 -> 1
        release(b); // b: 3 -> 2

        let mut gc = CycleCollector::new();
        gc.add_candidate(a);
        gc.add_candidate(b);
        gc.collect(); // b is externally reachable, so nothing is collected

        // Both survive with their counts restored; the objects are still usable.
        assert_eq!(b.gc_children().len(), 1);
        assert_eq!(a.gc_children().len(), 1);
        // Tear down by hand: drop the external owner, breaking the cycle's refcounts to zero.
        release(b); // b: 2 -> 1
        // Now a(1) <-> b(1) is again an unreachable cycle; collect it to satisfy miri.
        let mut gc = CycleCollector::new();
        gc.add_candidate(a);
        gc.add_candidate(b);
        gc.collect();
    }
}
