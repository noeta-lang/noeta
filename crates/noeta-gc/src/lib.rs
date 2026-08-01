//! Garbage collection: the runtime-wide memory-management floor.
//!
//! M1's GC is **refcount + a cycle collector** (architecture §5). This crate owns the *policy*
//! — when to retain and when to release-and-free, and the trial-deletion cycle collector —
//! while the unsafe refcount/graph primitives live in `noeta-value`'s heap module. Keeping
//! policy here (safe) and mechanism there (unsafe, miri-covered under `cargo miri test`) lets the collector grow
//! without touching the value representation.
//!
//! The acyclic floor: `retain` bumps the count, `release` drops it and frees at zero (running
//! `__destruct` is the VM's job, since a destructor needs the interpreter). Reference cycles —
//! objects kept alive only by referencing each other (under value semantics, only a self-recursive
//! closure capturing itself) — escape refcounting and are reaped by a **cycle collector** (Phase 6).
//!
//! Two collectors are wired and selected by [`noeta_value::CollectorMode`]: the default
//! [`collect_trace`] (a backup mark-sweep over the live-object registry) and [`collect_trial_deletion`]
//! (Bacon–Rajan synchronous trial deletion, fed by the release path's candidate buffer). Each
//! *identifies* the garbage and hands it back as a [`Garbage`] set for the VM to reclaim — running
//! `__destruct` on the dead members that carry one before freeing. The dormant [`CycleCollector`]
//! struct below is the original trial-deletion prototype, retained for its unit tests of the shared
//! `mark_gray`/`scan`/`gather_white` primitives. (`plans/memory-management/phase-6-benchmarks.md` has
//! the trace-vs-trial head-to-head and the default-collector rationale.)

use std::collections::HashSet;

use noeta_value::{Color, Value};

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

/// The unreachable objects a collection identified, handed back to the interpreter to **reclaim**
/// (run `__destruct`, then free) — the collector never runs user code itself, since a destructor
/// needs the interpreter. See [`noeta_value`]'s reclaim contract and the VM's `reclaim_cycle_garbage`.
#[derive(Debug, Default)]
pub struct Garbage {
    /// Members reclaimed for the first time: their `__destruct` (if any) must run **before** they
    /// are freed, while the rest of the dead subgraph is still allocated (container-before-contained).
    pub fresh: Vec<Value>,
    /// Members whose `__destruct` already ran on the release path (trial-deletion's deferred-dealloc
    /// objects): free only, never re-run their destructor.
    pub already_destructed: Vec<Value>,
    /// Whether a `fresh` member's references to **live** (surviving) values must be released as it is
    /// freed. The trace needs this — `gc_free_shallow` does not release children, so a garbage object
    /// pointing at a live one would otherwise leave that live value over-counted. Trial deletion has
    /// already corrected those edges via its trial-decrement, so it sets this `false`.
    pub release_external: bool,
}

/// **Backup mark-sweep trace** (Phase 6, the LXR-principled backup collector): a stop-the-world
/// trace over the live-object registry that finds everything unreachable from `roots` — reference
/// cycles and floating garbage alike, the cases refcounting alone cannot. Run at a safepoint (the
/// root set must be fully walkable): the interpreter enumerates its roots (globals, live frame
/// registers, open upvalue cells) and hands them here. Marks reachable objects and **returns** the
/// unmarked for the interpreter to reclaim (so `__destruct` can run before the free).
///
/// Colors: `alloc` paints every object `Black`; this trace paints reachable objects `Gray` and
/// resets survivors to `Black`, so `Black`-after-mark == unreachable garbage.
pub fn collect_trace(roots: &[Value]) -> Garbage {
    for &root in roots {
        mark(root);
    }
    let live = noeta_value::live_objects();
    // Garbage is everything the mark did not reach; identify it *before* resetting survivors (which
    // also become `Black`), so the two are distinguishable.
    let fresh: Vec<Value> = live
        .iter()
        .copied()
        .filter(|v| v.gc_color() != Color::Gray)
        .collect();
    for &v in &live {
        if v.gc_color() == Color::Gray {
            v.gc_set_color(Color::Black);
        }
    }
    noeta_value::note_refcount_anomalies(count_refcount_anomalies(&fresh));
    Garbage {
        fresh,
        already_destructed: Vec::new(),
        release_external: true,
    }
}

/// Count the **refcount anomalies** in a trace collection's garbage set. Garbage is unreachable
/// from the live graph (a live object referencing it would have marked it), so its members can
/// only be referenced from *within* the set: in a refcount-correct program each member's count is
/// exactly its in-edges from other members — a dead cycle balances, and so do the acyclic values
/// it drags down. A mismatch is a refcount bug the reclaim below would otherwise silently absorb:
/// `count > in-edges` means a release was skipped (the object leaked until this collection),
/// `count < in-edges` means a retain was skipped (a double-release hazard). The leak oracles
/// assert the accumulated count ([`noeta_value::refcount_anomalies`]) is zero; a clean program's
/// garbage set is empty, so the check is free outside genuine cycle collections.
fn count_refcount_anomalies(garbage: &[Value]) -> usize {
    if garbage.is_empty() {
        return 0;
    }
    let members: HashSet<u64> = garbage.iter().map(|v| v.bits()).collect();
    let mut in_edges: std::collections::HashMap<u64, u32> =
        std::collections::HashMap::with_capacity(garbage.len());
    for &obj in garbage {
        for child in obj.gc_children() {
            if child.is_pointer() && members.contains(&child.bits()) {
                *in_edges.entry(child.bits()).or_insert(0) += 1;
            }
        }
    }
    garbage
        .iter()
        .filter(|obj| {
            let bad = obj.refcount() != in_edges.get(&obj.bits()).copied().unwrap_or(0);
            if bad && std::env::var_os("NOETA_ANOMALY_DEBUG").is_some() {
                eprintln!(
                    "anomaly: {} rc={} in_edges={}",
                    obj.type_name(),
                    obj.refcount(),
                    in_edges.get(&obj.bits()).copied().unwrap_or(0)
                );
            }
            bad
        })
        .count()
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
pub fn collect_trial_deletion() -> Garbage {
    let roots = noeta_value::take_candidates();
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
    let mut white = Vec::new();
    for &s in &gray_roots {
        gather_white(s, &mut white);
    }
    // Classify for the interpreter to reclaim, deduped by address so a member is never returned (and
    // so never freed or dereferenced) twice — a deferred object pulled into a cycle by `mark_gray` is
    // returned as `fresh` (the cycle path) and dropped from `already_destructed`. The trial-decrement
    // already corrected every external edge, so `release_external` is false. White members have not
    // had `__destruct` run (they died only now, inside the cycle); deferred ones already did, on the
    // release path that buffered them.
    let mut seen: HashSet<u64> = HashSet::with_capacity(white.len() + deferred.len());
    let fresh: Vec<Value> = white
        .into_iter()
        .filter(|v| seen.insert(v.bits()))
        .collect();
    let already_destructed: Vec<Value> = deferred
        .into_iter()
        .filter(|v| seen.insert(v.bits()))
        .collect();
    Garbage {
        fresh,
        already_destructed,
        release_external: false,
    }
}

// --- In-run safepoint collection (memory-management 6.x) -----------------------------------------
//
// The two collectors above run at clean exit, where the root set is trivially small and every
// destructor may fire. The safepoint variants below run DURING execution, bounding the peak
// residency of cycle-building loops, under one semantic rule: **a safepoint collection never runs
// a destructor**. The destructor spec (destructor-order-spec §1/§2/§7, in git history) makes a
// `destruct` block the only observable memory-management effect and ties its firing to the last
// owning release — an event cyclic garbage never produces, so cycle-destructor timing is
// collector-defined and today realized at exit on both backends. Reclaiming *destructor-free*
// garbage mid-run is therefore invisible; a dead component containing any destructor-bearing
// member is **deferred** intact to the exit collection, which reclaims it exactly as before
// (same members, same reverse-`seq` order, same output). This also frees the two backends from
// having to synchronize their collection points — unobservable work needs no differential.

/// **Safepoint trace collection** (`Trace` mode): mark from the interpreter-enumerated `roots`
/// (every live register window, frame upvalues, globals, channel buffers, extension arena, embed
/// handles, scheduler-held tasks, traced futures, promoted-argument pins), then reclaim the
/// unreachable registry members — except:
///
/// - **Anomaly abort.** If the garbage set's refcounts do not exactly balance its internal
///   in-edges, the whole collection is abandoned (empty [`Garbage`], colors restored, counts
///   untouched — the trace never mutates refcounts). An imbalance means either a refcount bug or
///   a **missed root**: a live object referenced only from state the safepoint could not
///   enumerate would show `refcount > in-edges`, so this check makes a missed root cost liveness
///   until exit — never a use-after-free. (Unlike the exit trace, nothing is added to the
///   anomaly oracle here: the exit collection still sees and reports a genuine refcount bug.)
/// - **Destructor deferral.** Weakly-connected components of the garbage containing any member
///   for which `defer` answers true (the VM asks "does this shape have an own `destruct`?") are
///   left allocated for the exit collection, preserving today's observable destructor timing and
///   ordering. Destructor-bearing values *captured* by a dead cycle are themselves members of the
///   dead set, so the per-member predicate covers them.
///
/// The returned garbage is destructor-free by construction; the VM reclaims it through the same
/// `reclaim_cycle_garbage` path (whose destructor phase is then vacuous), with
/// `release_external: true` exactly like the exit trace.
pub fn collect_trace_safepoint(roots: &[Value], defer: &dyn Fn(Value) -> bool) -> Garbage {
    for &root in roots {
        mark(root);
    }
    let live = noeta_value::live_objects();
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
    if garbage.is_empty() || count_refcount_anomalies(&garbage) != 0 {
        return Garbage::default();
    }
    let mut fresh = Vec::new();
    for component in weakly_connected_components(&garbage) {
        if component.iter().any(|&v| defer(v)) {
            continue;
        }
        fresh.extend(component);
    }
    // Deterministic free order (reverse creation), matching the exit reclaim's tie-break. Not
    // observable — the set is destructor-free — but it keeps the reclaim path's behavior uniform.
    fresh.sort_by_key(|g| std::cmp::Reverse(g.gc_seq()));
    Garbage {
        fresh,
        already_destructed: Vec::new(),
        release_external: true,
    }
}

/// **Safepoint trial-deletion collection** (`TrialDeletion` mode): the Bacon–Rajan collection
/// over the buffered candidate roots, with the safepoint destructor rule applied. Unlike the
/// trace it needs no root enumeration at all — deadness is proven by the trial decrement (every
/// external owner, including a VM register, holds a counted reference), so there is no
/// missed-root failure mode and no abort path. Dead components containing a destructor-bearing
/// member are **restored** (their trial decrements undone edge-for-edge) and re-buffered as
/// candidates, so the exit collection reclaims them exactly as it would have.
pub fn collect_trial_deletion_safepoint(defer: &dyn Fn(Value) -> bool) -> Garbage {
    let roots = noeta_value::take_candidates();
    let mut gray_roots = Vec::new();
    let mut deferred_dealloc = Vec::new();
    for &s in &roots {
        s.gc_set_buffered(false);
        match s.gc_color() {
            Color::Purple => {
                mark_gray(s);
                gray_roots.push(s);
            }
            // A deferred-dealloc object (refcount 0, children already released, `__destruct`
            // already run on the release path): free-only, exactly as the exit collection.
            _ => deferred_dealloc.push(s),
        }
    }
    for &s in &gray_roots {
        scan(s);
    }
    let mut white = Vec::new();
    for &s in &gray_roots {
        gather_white(s, &mut white);
    }
    let mut seen: HashSet<u64> = HashSet::with_capacity(white.len() + deferred_dealloc.len());
    let white: Vec<Value> = white
        .into_iter()
        .filter(|v| seen.insert(v.bits()))
        .collect();
    let already_destructed: Vec<Value> = deferred_dealloc
        .into_iter()
        .filter(|v| seen.insert(v.bits()))
        .collect();
    let mut fresh = Vec::new();
    for component in weakly_connected_components(&white) {
        if component.iter().any(|&v| defer(v)) {
            // Defer the destructor-bearing component to the exit collection: undo the trial
            // decrements edge-for-edge (each member's out-edges were decremented exactly once by
            // `mark_gray`; edges into the component all come from within it, since it is white =
            // externally unreferenced and weak connectivity keeps sibling components edge-free),
            // then re-buffer every member so the exit trial deletion finds the cycle again.
            for &m in &component {
                for child in m.gc_children() {
                    if !child.is_shared() {
                        child.gc_rc_inc();
                    }
                }
            }
            for &m in &component {
                noeta_value::rebuffer_candidate(m);
            }
            continue;
        }
        fresh.extend(component);
    }
    fresh.sort_by_key(|g| std::cmp::Reverse(g.gc_seq()));
    Garbage {
        fresh,
        already_destructed,
        release_external: false,
    }
}

/// Partition `garbage` into its weakly-connected components under the heap reference graph
/// restricted to the set — the granularity at which the safepoint collectors decide "reclaim now"
/// vs "defer to exit". Weak (undirected) connectivity is what makes deferral sound: no reclaimed
/// member can hold an edge to a deferred one (or vice versa), so freeing one component never
/// releases or dangles into another.
fn weakly_connected_components(garbage: &[Value]) -> Vec<Vec<Value>> {
    let index: std::collections::HashMap<u64, usize> = garbage
        .iter()
        .enumerate()
        .map(|(i, v)| (v.bits(), i))
        .collect();
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); garbage.len()];
    for (i, &v) in garbage.iter().enumerate() {
        for child in v.gc_children() {
            if let Some(&j) = index.get(&child.bits())
                && j != i
            {
                adjacency[i].push(j);
                adjacency[j].push(i);
            }
        }
    }
    let mut component_of = vec![usize::MAX; garbage.len()];
    let mut components = Vec::new();
    for start in 0..garbage.len() {
        if component_of[start] != usize::MAX {
            continue;
        }
        let id = components.len();
        let mut members = Vec::new();
        let mut queue = vec![start];
        component_of[start] = id;
        while let Some(i) = queue.pop() {
            members.push(garbage[i]);
            for &j in &adjacency[i] {
                if component_of[j] == usize::MAX {
                    component_of[j] = id;
                    queue.push(j);
                }
            }
        }
        components.push(members);
    }
    components
}

/// Paint `s` and everything reachable from it `Gray` (reachable/live). Idempotent via the color
/// check, so shared substructure and cycles terminate. A **borrow-shared** object (isolates I.3)
/// is skipped outright: it is owned by its region — never registered, never swept — and writing
/// its color from a safepoint collection while worker isolates hold the graph would be a data
/// race; its children are all shared too (promotion deep-copies whole graphs), so the skip loses
/// no reachability.
fn mark(s: Value) {
    if !s.is_pointer() || s.is_shared() || s.gc_color() == Color::Gray {
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
/// the candidate subgraph no longer count toward keeping a node alive. Borrow-shared children
/// are skipped (never refcounted, never collected — see [`mark`]).
fn mark_gray(s: Value) {
    if s.gc_color() == Color::Gray {
        return;
    }
    s.gc_set_color(Color::Gray);
    for child in s.gc_children() {
        if child.is_shared() {
            continue;
        }
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

/// Restore `s` (and its still-gray subgraph) to black, undoing the trial decrements. Shared
/// children are skipped, mirroring [`mark_gray`]'s skip (their counts were never decremented).
fn scan_black(s: Value) {
    s.gc_set_color(Color::Black);
    for child in s.gc_children() {
        if child.is_shared() {
            continue;
        }
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

    use noeta_object::intern_shape;
    use noeta_object::{Shape, ShapeKind};

    /// A one-slot object whose slot starts as unit — the building block for a heap cycle.
    fn cell() -> Value {
        let shape = intern_shape(Shape::object(ShapeKind::Class, "Cell", vec!["next".into()]));
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

    /// Reclaim a [`Garbage`] set the way the VM's `reclaim_cycle_garbage` does, minus destructors
    /// (safepoint garbage is destructor-free by construction): release each fresh member's edges
    /// to surviving values, then free every member shallowly.
    fn reclaim(garbage: Garbage) {
        let dead: HashSet<u64> = garbage
            .fresh
            .iter()
            .chain(&garbage.already_destructed)
            .map(|v| v.bits())
            .collect();
        if garbage.release_external {
            for &g in &garbage.fresh {
                for child in g.gc_children() {
                    if !dead.contains(&child.bits()) {
                        release(child);
                    }
                }
            }
        }
        for g in garbage.fresh.into_iter().chain(garbage.already_destructed) {
            g.gc_free_shallow();
        }
    }

    #[test]
    fn safepoint_trace_reclaims_an_unreachable_cycle_and_spares_the_rooted() {
        // Two A<->B cycles; one stays rooted, one is released. The safepoint trace must reclaim
        // exactly the unrooted one.
        let (a, b) = (cell(), cell());
        a.set_slot(0, b);
        b.set_slot(0, a);
        let (c, d) = (cell(), cell());
        c.set_slot(0, d);
        d.set_slot(0, c);
        release(a);
        release(b); // a<->b now unreachable garbage
        let before = noeta_value::live_count();
        let garbage = collect_trace_safepoint(&[c], &|_| false);
        assert_eq!(
            garbage.fresh.len(),
            2,
            "exactly the dead cycle is reclaimed"
        );
        reclaim(garbage);
        assert_eq!(noeta_value::live_count(), before - 2);
        // c<->d survived intact and rooted; tear it down through the safepoint path too.
        release(c);
        release(d);
        let garbage = collect_trace_safepoint(&[], &|_| false);
        assert_eq!(garbage.fresh.len(), 2);
        reclaim(garbage);
    }

    #[test]
    fn safepoint_trace_defers_a_destructor_bearing_component() {
        // Cycle 1 (a<->b) is "destructor-bearing" (the predicate defers `a`); cycle 2 (c<->d) is
        // destructor-free. Only cycle 2 may be reclaimed mid-run; cycle 1 stays allocated for the
        // exit collection, its refcounts untouched.
        let (a, b) = (cell(), cell());
        a.set_slot(0, b);
        b.set_slot(0, a);
        let (c, d) = (cell(), cell());
        c.set_slot(0, d);
        d.set_slot(0, c);
        release(a);
        release(b);
        release(c);
        release(d);
        let deferred = a;
        let garbage = collect_trace_safepoint(&[], &|v| v.bits() == deferred.bits());
        assert_eq!(garbage.fresh.len(), 2, "only the destructor-free cycle");
        assert!(
            garbage
                .fresh
                .iter()
                .all(|v| v.bits() != a.bits() && v.bits() != b.bits())
        );
        reclaim(garbage);
        // The deferred cycle survived with exact counts; the exit-style trace reclaims it.
        assert_eq!(a.refcount(), 1);
        assert_eq!(b.refcount(), 1);
        let garbage = collect_trace(&[]);
        assert_eq!(garbage.fresh.len(), 2);
        reclaim(garbage);
    }

    #[test]
    fn safepoint_trace_aborts_on_a_missed_root() {
        // `x` is live, held ONLY by a root the enumeration "missed" (we simply do not pass it).
        // Its refcount then exceeds its in-edges from the garbage set, so the whole collection
        // must abort — reclaiming nothing — rather than free a live object.
        let x = cell();
        let garbage = collect_trace_safepoint(&[], &|_| false);
        assert!(
            garbage.fresh.is_empty(),
            "imbalance must abort the collection"
        );
        assert_eq!(x.refcount(), 1, "the live object is untouched");
        release(x); // frees promptly (Trace mode)
    }

    #[test]
    fn safepoint_trial_deletion_reclaims_free_cycles_and_defers_destructor_bearing() {
        noeta_value::set_collector_mode(noeta_value::CollectorMode::TrialDeletion);
        // Destructor-free cycle a<->b and "destructor-bearing" cycle c<->d, both released so the
        // release path buffers candidates.
        let (a, b) = (cell(), cell());
        a.set_slot(0, b);
        b.set_slot(0, a);
        let (c, d) = (cell(), cell());
        c.set_slot(0, d);
        d.set_slot(0, c);
        release(a);
        release(b);
        release(c);
        release(d);
        let deferred = c;
        let before = noeta_value::live_count();
        let garbage = collect_trial_deletion_safepoint(&|v| v.bits() == deferred.bits());
        assert_eq!(garbage.fresh.len(), 2, "only the destructor-free cycle");
        assert!(
            garbage
                .fresh
                .iter()
                .all(|v| v.bits() != c.bits() && v.bits() != d.bits())
        );
        assert!(!garbage.release_external);
        reclaim(garbage);
        assert_eq!(noeta_value::live_count(), before - 2);
        // The deferred cycle's trial decrements were restored and it was re-buffered: the exit
        // collection reclaims it exactly as it would have without the safepoint.
        assert_eq!(c.refcount(), 1);
        assert_eq!(d.refcount(), 1);
        let garbage = collect_trial_deletion();
        assert_eq!(garbage.fresh.len(), 2);
        reclaim(garbage);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    }

    /// A two-slot object — a cycle partner in `next`, a payload in `data`.
    fn pair() -> Value {
        let shape = intern_shape(Shape::object(
            ShapeKind::Class,
            "Pair",
            vec!["next".into(), "data".into()],
        ));
        Value::object(shape, vec![Value::unit(), Value::unit()])
    }

    /// The acyclic exclusion's load-bearing case, on **both** collectors: a dead cycle that owns a
    /// *leaf* (a string). The leaf is no longer in the live-object registry, so the trace cannot
    /// hand it back as garbage — and it must not need to: `release_external` drops the dead nodes'
    /// edge to it, taking its count to zero. Trial deletion never consulted the registry to begin
    /// with; it reaches the leaf through `gc_children`'s trial decrement. Either way residency
    /// returns to its pre-cycle value, which is the whole claim the leak oracle makes.
    #[test]
    fn a_dead_cycle_still_reclaims_the_leaf_it_holds() {
        for mode in [
            noeta_value::CollectorMode::Trace,
            noeta_value::CollectorMode::TrialDeletion,
        ] {
            noeta_value::set_collector_mode(mode);
            let before = noeta_value::live_count();
            let (a, b) = (pair(), pair());
            a.set_slot(0, b); // b retained by a (rc 2)
            b.set_slot(0, a); // a retained by b (rc 2)
            let text = Value::string("dragged down by the cycle");
            a.set_slot(1, text); // text retained by a (rc 2)
            release(text); // …and `a` is now its only owner (rc 1)
            release(a); // drop the external handles: the pair survives only on its own edges
            release(b);
            assert_eq!(
                noeta_value::live_count(),
                before + 3,
                "the cycle keeps all three alive with no external reference ({mode:?})"
            );
            let garbage = match mode {
                noeta_value::CollectorMode::Trace => {
                    let garbage = collect_trace(&[]);
                    assert!(
                        garbage.fresh.iter().all(|v| v.bits() != text.bits()),
                        "an unregistered leaf is never handed back by the sweep — refcounting \
                         reclaims it when the dead nodes' edges are released"
                    );
                    garbage
                }
                noeta_value::CollectorMode::TrialDeletion => collect_trial_deletion(),
            };
            reclaim(garbage);
            assert_eq!(
                noeta_value::live_count(),
                before,
                "including the string the dead cycle held ({mode:?})"
            );
        }
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
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
