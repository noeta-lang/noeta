//! `std.reactive` — server-side signals (`signal`/`computed`/`effect`), **fully migrated** onto
//! the extension ABI (higher-order-abi H5): the graph is per-run extension state over
//! [`Retained`] arena cells, the handles are generic extern types, and the flush/coalescing/
//! E0045 machinery is ordinary Rust in these dispatches. Neither backend knows reactivity exists.
//!
//! # The stable-cell design
//!
//! Every language value the system holds lives in a **fixed arena cell** whose id never changes:
//! a signal's content cell (allocated at creation; `set` replaces the cell's value in place), a
//! computed's memo cell (recomputes land in it via [`NativeCtx::call_thunk_into`]), and the
//! immutable body cells. The graph ([`ReactiveGraph<Retained>`]) therefore stores only ids and
//! never displaces a value — dirtying is [`ReactiveGraph::touch`] — and the extern boxes carry
//! the ids as plain data, which is what lets `get` be a **declared arena read**
//! ([`ExtType::arena_getter`]): while the read gate is open, the backend inlines
//! `signal.get()`/`computed.get()` to an arena load.
//!
//! # The gate discipline
//!
//! Both read gates are open exactly when the fast read is the whole truth:
//! **no body is running** (a read inside a `computed`/`effect` body must record a dependency
//! edge) **and no memo is stale** (a dirty computed's `get` must recompute first) **and no flush
//! is in progress**. [`sync_gates`] recomputes that predicate at every boundary where it can
//! change; the full ctx dispatch behaves identically when the gate happens to be open — the
//! tree-walker always takes it, so the differential proves the equivalence on every fixture.
//!
//! # Borrow discipline
//!
//! The [`ReactiveGraph`] API is `&self` (interior `RefCell`, borrows released around its `run`
//! callbacks — the crate's load-bearing design). The [`ExtState`] cell is therefore only ever
//! borrowed **shared** here, so a body re-entering these dispatches (an effect setting a signal,
//! a computed reading a computed) nests without conflict.

use std::any::Any;
use std::cmp::Ordering;

use noeta_ext_abi::registry::{ExtCapability, ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    ArenaGetter, AttrValue, CtxError, CtxOut, ErrorKind, ExtState, ExternValue, NativeCtx,
    Retained, Slot, SpanKind, SpanStatus, StdError, ctx_arity, no_function_error,
};
use noeta_reactive::{MAX_FLUSH_STEPS, NodeId, ReactiveGraph};
// The reactive extension **contract** this engine implements/serves: the `ReactiveSource`
// capability (provided below) and the foreign view-source extractors (`view.expose` consults them).
use noeta_reactive_abi::{ReactiveSource, ViewSource, ViewSourceExtract};

pub const SIGNAL_TYPE_NAME: &str = "Signal";
pub const COMPUTED_TYPE_NAME: &str = "Computed";
pub const EFFECT_TYPE_NAME: &str = "Effect";
pub const VIEW_TYPE_NAME: &str = "View";

/// The reactive types' qualified runtime identities — what
/// [`crate::ExternValue::type_identity`] returns, what the compiled-in ctx fast route matches
/// on, and the keys the read gates ([`NativeCtx::set_read_gate`]) open and close under.
pub const SIGNAL_TYPE_IDENTITY: &str = "std.reactive.Signal";
pub const COMPUTED_TYPE_IDENTITY: &str = "std.reactive.Computed";
pub const EFFECT_TYPE_IDENTITY: &str = "std.reactive.Effect";
pub const VIEW_TYPE_IDENTITY: &str = "std.reactive.View";

const VAR_A: SigType = SigType::Var(0);
const OPT_BOOL: SigType = SigType::Optional(&SigType::Bool);

pub const REACTIVE_CTX_FNS: &[ExtFn] = &[
    // `signal(v: A, dedupe?: bool) -> Signal<A>` — a reactive cell. With `dedupe = true` the signal
    // suppresses re-firing dependents when a `set`/`update` lands a value **equal to the current one
    // under the language's `==`** (opt-in; the default and an omitted flag keep the always-fire
    // behavior). See [`signal_ctx_method_dispatch`].
    ExtFn {
        name: "signal",
        params: &[VAR_A, OPT_BOOL],
        ret: RetTy::Concrete(SigType::Generic(SIGNAL_TYPE_NAME, &[VAR_A])),
    },
    // `computed(fn() -> A) -> Computed<A>` — a lazy memoized derivation.
    ExtFn {
        name: "computed",
        params: &[SigType::Fn(&[], &VAR_A)],
        ret: RetTy::Concrete(SigType::Generic(COMPUTED_TYPE_NAME, &[VAR_A])),
    },
    // `effect(fn) -> Effect` — an eager side effect; the body's return (if any) is discarded.
    ExtFn {
        name: "effect",
        params: &[SigType::Fn(&[], &SigType::Dyn)],
        ret: RetTy::Concrete(SigType::Named(EFFECT_TYPE_NAME)),
    },
    // `view() -> View` — a named window onto reactive state for the diff-push transport
    // (server-hmr L1); see [`view_ctx_method_dispatch`] for the protocol.
    ExtFn {
        name: "view",
        params: &[],
        ret: RetTy::Concrete(SigType::Named(VIEW_TYPE_NAME)),
    },
];

pub const SIGNAL_CTX_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "get",
        params: &[],
        ret: RetTy::Concrete(VAR_A),
    },
    ExtFn {
        name: "set",
        params: &[VAR_A],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "update",
        params: &[SigType::Fn(&[VAR_A], &VAR_A)],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

pub const COMPUTED_CTX_METHODS: &[ExtFn] = &[ExtFn {
    name: "get",
    params: &[],
    ret: RetTy::Concrete(VAR_A),
}];

pub const EFFECT_CTX_METHODS: &[ExtFn] = &[ExtFn {
    name: "dispose",
    params: &[],
    ret: RetTy::Concrete(SigType::Unit),
}];

const OPT_STR: SigType = SigType::Option(&SigType::String);

pub const VIEW_CTX_METHODS: &[ExtFn] = &[
    // `expose(name, handle)` — bind `name` to a `Signal` or `Computed` (anything else is a
    // runtime error). Re-exposing a name replaces its binding (the hot-swap re-run path).
    ExtFn {
        name: "expose",
        params: &[SigType::String, SigType::Dyn],
        ret: RetTy::Concrete(SigType::Unit),
    },
    // `unexpose(name)` — drop the binding `name` and **dispose its handle**, so a diff never pushes
    // it again and its backing cells reclaim (the keyed-list structural-change path: a row that
    // leaves the key set is unexposed, tearing down its per-row reactive scope). A no-op for an
    // unknown name.
    ExtFn {
        name: "unexpose",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    // `snapshot() -> string` — the full-state frame; also the baseline diffs are minimal against.
    ExtFn {
        name: "snapshot",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    },
    // `diff() -> ?string` — a patch frame of bindings whose value changed since the last
    // snapshot/diff, or `none` when nothing (observably) changed.
    ExtFn {
        name: "diff",
        params: &[],
        ret: RetTy::Concrete(OPT_STR),
    },
];

pub const SIGNAL_ARENA_GETTER: ArenaGetter = ("get", |e| signal_box(e).cell);
pub const COMPUTED_ARENA_GETTER: ArenaGetter = ("get", |e| computed_box(e).memo);

/// The per-run reactive engine state: the graph, over arena-cell ids, plus the last gate state
/// pushed to the backend (so a redundant sync is one branch, not two backend calls).
///
/// **Private engine detail** (`pub(crate)`). An out-of-`std` reactive node — `para.synced`, whose
/// synced signal *is* a node in this shared graph — never touches this type or the graph; it goes
/// through the [`ReactiveSource`] capability the engine provides instead, so this representation can
/// evolve without breaking it.
pub(crate) struct ReactiveExt {
    pub graph: ReactiveGraph<Retained>,
    gates_open: std::cell::Cell<bool>,
    /// Every live effect `(node, body)` in creation order — the hot-swap epoch registry
    /// (server-hmr H1). A swap that re-runs the top level disposes all of them first
    /// ([`hotswap_dispose_effects`]) and lets the re-run re-create them; a user-level
    /// `.dispose()` prunes its entry so the swap never double-releases a body cell.
    effects: std::cell::RefCell<Vec<(NodeId, Retained)>>,
    /// Every `view()` created this run (server-hmr L1) — the diff-push subscribers. Views are
    /// never removed (they live with the run, like the graph); a swap's top-level re-run builds
    /// fresh ones and the old, unreferenced entries just stop being polled. Borrowed **mutably
    /// only in short, non-reentrant windows** — never across a body run.
    views: std::cell::RefCell<Vec<ViewState>>,
    /// Reused drain buffer for [`distribute_changes`] — a hot set→flush loop allocates nothing.
    changed_scratch: std::cell::RefCell<Vec<NodeId>>,
    /// Reused drain buffer for [`release_reclaimed`] — the owner-tree cell reclaim (S4b) allocates
    /// nothing in the steady state.
    reclaimed_scratch: std::cell::RefCell<Vec<Retained>>,
    /// Whether the opt-in flush telemetry is on (server-hmr L4 / native-otel T5e): resolved once
    /// per run on first use — `tel_enabled()` AND `NOETA_TRACE_REACTIVE` truthy — then a plain
    /// bool read on the hot path. `None` until the first flush asks.
    trace: std::cell::Cell<Option<bool>>,
}

// Internal reactive-graph state; opaque in Debug (its `ViewState`/graph internals are not formatted).
impl std::fmt::Debug for ReactiveExt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReactiveExt").finish_non_exhaustive()
    }
}

pub const STATE_KEY: &str = "std.reactive";

/// Fresh reactive engine state — the `ExtState` initializer, shared by [`state_of`] (the module
/// dispatch path) and the `ReactiveSource` capability provider (the foreign-node path), so reaching
/// the engine either way yields the *same* per-run cell whichever happens first.
fn reactive_state_init() -> Box<dyn Any> {
    Box::new(ReactiveExt {
        graph: ReactiveGraph::new(),
        // The backend's gates start open — mirror that.
        gates_open: std::cell::Cell::new(true),
        effects: std::cell::RefCell::new(Vec::new()),
        views: std::cell::RefCell::new(Vec::new()),
        changed_scratch: std::cell::RefCell::new(Vec::new()),
        reclaimed_scratch: std::cell::RefCell::new(Vec::new()),
        trace: std::cell::Cell::new(None),
    })
}

pub(crate) fn state_of<C: NativeCtx + ?Sized>(ctx: &mut C) -> ExtState {
    ctx.state(STATE_KEY, reactive_state_init)
}

/// Recompute the read gates from the graph's state (see the module docs) and push a *change* to
/// the backend; an unchanged state is one branch.
pub(crate) fn sync_gates<C: NativeCtx + ?Sized>(ctx: &mut C, ext: &ReactiveExt) {
    let open =
        !ext.graph.is_flushing() && !ext.graph.tracking() && ext.graph.dirty_computed_count() == 0;
    if ext.gates_open.replace(open) != open {
        ctx.set_read_gate(SIGNAL_TYPE_IDENTITY, open);
        // A signal read is compromised only by tracking, but one shared predicate keeps the gate
        // reasoning one sentence long; refine per-type only if a bench demands it.
        ctx.set_read_gate(COMPUTED_TYPE_IDENTITY, open);
    }
}

/// Release the arena cells of owner-tree children the graph disposed since the last drain
/// (reactivity S4b) — the client half of the value-generic core's reclaim buffer. Cheap when empty
/// (the steady state); called after every read/flush/dispose so a body that creates-then-drops
/// reactive nodes each run reclaims their backing cells on the spot rather than at scope end.
pub(crate) fn release_reclaimed<C: NativeCtx + ?Sized>(ctx: &mut C, ext: &ReactiveExt) {
    let mut buf = ext.reclaimed_scratch.borrow_mut();
    buf.clear();
    ext.graph.drain_reclaimed_into(&mut buf);
    if buf.is_empty() {
        return;
    }
    // Take ownership out of the borrow so `release_retained` (which needs `&mut ctx`) cannot alias
    // the scratch cell, then hand the buffer back for reuse.
    let cells: Vec<Retained> = std::mem::take(&mut buf);
    drop(buf);
    for cell in &cells {
        ctx.release_retained(*cell);
    }
    let mut buf = ext.reclaimed_scratch.borrow_mut();
    if buf.is_empty() {
        *buf = cells;
        buf.clear();
    }
}

/// Run every queued effect to a fixpoint (the ordinary Rust that used to be each backend's
/// `drive_flush`): bodies run via the fused [`NativeCtx::run_thunk`]; an abort inside a body is
/// stashed and re-raised (the flush stops), and a non-converging update is the E0045
/// reactive-cycle diagnostic. Gates are re-synced when the dust settles.
pub(crate) fn drive_flush<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    ext: &ReactiveExt,
) -> Result<(), CtxError> {
    // Opt-in flush telemetry (server-hmr L4 / native-otel T5e): a span only when the flag is on
    // AND the flush will actually run something — a no-op flush (a `set` with no subscribers)
    // emits nothing. The span is pushed as the active context so spans the effect bodies create
    // nest under it, connecting reactive propagation into the request/session trace.
    let span = if reactive_tracing(ctx, ext) && ext.graph.pending_effects() > 0 {
        let parent = crate::tracing::current_parent(ctx);
        let id = ctx
            .host()
            .tel_span_start("reactive.flush", SpanKind::Internal, parent);
        crate::tracing::push_active(ctx, id);
        Some(id)
    } else {
        None
    };
    let mut effects_run: i64 = 0;
    let mut aborted: Option<CtxError> = None;
    let overflowed = ext
        .graph
        .flush(&mut |body: Retained| -> Retained {
            if aborted.is_none() {
                effects_run += 1;
                sync_gates(ctx, ext);
                if let Err(e) = ctx.run_thunk(body) {
                    aborted = Some(e);
                }
            }
            body
        })
        .is_err();
    sync_gates(ctx, ext);
    // Release the cells of any owner-tree children the reruns disposed (reactivity S4b).
    release_reclaimed(ctx, ext);
    // The flush subscriber (server-hmr L1): every change path funnels through here (a top-level
    // `set`/`update`/effect-creation/synced-merge drives a flush even when no effect is queued;
    // a set *inside* a flush lands in the graph's change log and is drained by this outer call),
    // so distributing once per flush marks every view binding whose node changed.
    let changed = distribute_changes(ext);
    if let Some(id) = span {
        crate::tracing::pop_active(ctx, id);
        let host = ctx.host();
        host.tel_span_set_attr(id, "reactive.effects", AttrValue::Int(effects_run));
        host.tel_span_set_attr(id, "reactive.changed", AttrValue::Int(changed as i64));
        if aborted.is_some() {
            host.tel_span_set_status(id, SpanStatus::Error("effect body aborted".into()));
        } else if overflowed {
            host.tel_span_set_status(
                id,
                SpanStatus::Error("reactive update did not converge".into()),
            );
        }
        host.tel_span_end(id);
    }
    if let Some(e) = aborted {
        return Err(e);
    }
    if overflowed {
        return Err(StdError {
            kind: ErrorKind::ReactiveCycle,
            message: format!(
                "reactive update did not converge after {MAX_FLUSH_STEPS} steps — an effect \
                 keeps changing a signal it depends on"
            ),
        }
        .into());
    }
    Ok(())
}

/// Resolve (once per run, then a plain bool read) the opt-in flush-telemetry flag (server-hmr
/// L4): spans are emitted only when telemetry is active AND `NOETA_TRACE_REACTIVE` is `1`/`true`
/// — per-flush tracing is far too noisy for default-on, and the flush is a perf-gated hot path,
/// so the off state must cost one cached-bool branch.
fn reactive_tracing<C: NativeCtx + ?Sized>(ctx: &mut C, ext: &ReactiveExt) -> bool {
    if let Some(on) = ext.trace.get() {
        return on;
    }
    let host = ctx.host();
    let on = host.tel_enabled()
        && host
            .env_get("NOETA_TRACE_REACTIVE")
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    ext.trace.set(Some(on));
    on
}

/// Hot-swap hook (server-hmr H1): dispose **every live effect** and release its body cell — the
/// swap's re-run of the (new) top level re-creates the ones that still exist. Exactly the
/// user-level `.dispose()` per effect, driven off the epoch registry. A program that never touched
/// reactivity gets an empty state and this is a no-op.
pub fn hotswap_dispose_effects<C: NativeCtx + ?Sized>(ctx: &mut C) {
    let state = state_of(ctx);
    let ext = state.borrow();
    let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
    let drained: Vec<(NodeId, Retained)> = ext.effects.borrow_mut().drain(..).collect();
    for (node, body) in drained {
        ext.graph.dispose(node);
        ctx.release_retained(body);
    }
    // Each root effect's dispose cascades to its owner-tree children (S4b) — release their cells.
    release_reclaimed(ctx, ext);
    sync_gates(ctx, ext);
}

/// Hot-swap hook (server-hmr H1): the given slots hold globals a swap fragment is about to
/// re-bind. Any that carry a `Signal`/`Computed` handle gets its graph node disposed, so the
/// replaced node stops participating in flushes (a preserved subscriber re-subscribes to the
/// replacement on its next run — dependency edges are rebuilt per run). The content/memo arena
/// cells are deliberately **not** released: an alias captured elsewhere may still read them;
/// they reclaim at teardown (bounded by replaced bindings per swap). Effects are not handled
/// here — the epoch registry ([`hotswap_dispose_effects`]) owns them.
pub fn hotswap_dispose_handles<C: NativeCtx + ?Sized>(ctx: &mut C, handles: &[Slot]) {
    let mut nodes: Vec<NodeId> = Vec::new();
    for &handle in handles {
        let _ = ctx.with_extern(handle, &mut |e| {
            if let Some(s) = e.as_any().downcast_ref::<SignalBox>() {
                nodes.push(s.node);
            } else if let Some(c) = e.as_any().downcast_ref::<ComputedBox>() {
                nodes.push(c.node);
            }
        });
    }
    if nodes.is_empty() {
        return;
    }
    let state = state_of(ctx);
    let ext = state.borrow();
    let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
    for node in nodes {
        ext.graph.dispose(node);
    }
    // A disposed handle may own owner-tree children (S4b) — release their cells.
    release_reclaimed(ctx, ext);
    sync_gates(ctx, ext);
}

// ----- views: the diff-push transport's flush subscriber (server-hmr L1) -----
//
// A `View` is a set of named bindings onto `Signal`/`Computed` (and `SyncedSignal`) handles plus
// per-binding dirt and a serialized baseline. The graph's change log (drained once per flush by
// [`distribute_changes`]) marks candidate bindings dirty; `diff()` serializes only the dirty ones
// and drops any whose JSON equals the baseline — so a `set` to an equal value, or a recompute
// that lands on the same result, pushes nothing. The wire protocol (shared with the L3 HMR
// events, one channel two event kinds):
//
//   snapshot:  {"type":"snapshot","values":{"count":0,"double":0}}
//   patch:     {"type":"patch","changes":{"count":1}}
//
// Entries are sorted by binding name and serialized by the exact `json.stringify` walk, so frames
// are deterministic and differential-pinned.

/// One named binding in a view: the graph node to watch and where its value lives.
struct ViewBinding {
    name: String,
    node: NodeId,
    source: ViewSource,
    /// The serialization last pushed for this binding (set at expose/snapshot, updated by each
    /// emitting diff) — the baseline that makes a patch minimal by *value*, not just by node.
    last: String,
}

// `ViewSource` (where a view binding reads its value — a signal cell or a computed body+memo) and
// the foreign view-source extractor contract (`ViewSourceExtract`) live in `noeta-reactive-abi`,
// the reactive extension contract. `view.expose` recognizes a foreign node type (e.g.
// `para.synced`'s `SyncedSignal`) by consuming the foreign extension's `ViewSourceExtract`
// **capability** after its own built-in `Signal`/`Computed` handles — without naming or depending
// on that type, and scoped to the run's registry (audit-2 Finding 12: this replaced a
// process-global extractor list).

// ===== The `ReactiveSource` capability provider (the capability-broker seam) =====================
//
// The engine implements `noeta_reactive_abi::ReactiveSource` so a foreign source node — a value that
// is a *node in this same reactive graph* as core `signal`/`computed`/`effect` — can create its
// node, subscribe a reader, and wake dependents, without ever seeing the engine's representation
// (`ReactiveExt`, the `ReactiveGraph`, the gate/flush/telemetry machinery). `para.synced`'s
// CRDT-backed signal is the first client: a shared value that `computed`/`effect` track, and that a
// peer merge wakes so the graph reruns those dependents.
//
// The consumer obtains the capability per-run via `noeta_ext_abi::capability::<dyn ReactiveSource>`.
// The handle owns a clone of the engine's `ExtState`, so — exactly like the module dispatches below —
// each method borrows the cell for its own work and releases before any re-entry into user code
// (the flush runs effect bodies, which may re-enter reactive and take their own shared borrow of the
// same cell; two shared borrows coexist, a `borrow_mut` across re-entry would not, hence the
// discipline). This is the same `state_of + borrow + downcast + drive` shape the whole module uses,
// now behind the trait rather than sprayed across consumer call sites.

/// The `ReactiveSource` handle vended to a foreign node. Holds a clone of the engine `ExtState`; each
/// method downcasts it to [`ReactiveExt`] for the duration of that operation only.
struct ReactiveSourceHandle {
    state: ExtState,
}

impl ReactiveSource for ReactiveSourceHandle {
    fn create_source(&self, _ctx: &mut dyn NativeCtx, cell: Retained) -> NodeId {
        let ext = self.state.borrow();
        let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
        // A foreign source is a ROOT node: the extension owns its cell and lifetime, so it must never
        // be adopted into the owner tree and torn down by a rerun of whatever body constructed it.
        ext.graph.signal_root(cell)
    }

    fn read_source(&self, _ctx: &mut dyn NativeCtx, node: NodeId) -> Retained {
        let ext = self.state.borrow();
        let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
        ext.graph.read(node, &mut |body| body)
    }

    fn wake(&self, ctx: &mut dyn NativeCtx, node: NodeId) -> Result<(), CtxError> {
        let ext = self.state.borrow();
        let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
        ext.graph.touch(node);
        sync_gates(ctx, ext);
        if !ext.graph.is_flushing() {
            drive_flush(ctx, ext)?;
        }
        Ok(())
    }
}

/// Build the erased `ReactiveSource` handle from the engine's backing state — the [`ExtCapability`]
/// `build` thunk. Boxes a `Box<dyn ReactiveSource>` (a sized fat pointer) as `Box<dyn Any>`, which
/// `noeta_ext_abi::capability` recovers by a safe downcast.
fn build_reactive_source(state: ExtState) -> Box<dyn Any> {
    let handle: Box<dyn ReactiveSource> = Box::new(ReactiveSourceHandle { state });
    Box::new(handle)
}

/// The reactive engine's provided capabilities — the `ReactiveSource` seam, backed by the same
/// `"std.reactive"` `ExtState` slot the module dispatches use. Declared on `CoreExtension`.
pub const REACTIVE_CAPABILITIES: &[ExtCapability] = &[ExtCapability {
    id: || std::any::TypeId::of::<dyn ReactiveSource>(),
    state_key: STATE_KEY,
    init: reactive_state_init,
    build: build_reactive_source,
}];

/// One `view()`'s state: bindings in expose order plus the dirty set the flush subscriber fills.
#[derive(Default)]
struct ViewState {
    bindings: Vec<ViewBinding>,
    dirty: std::collections::BTreeSet<usize>,
}

/// Drain the graph's change log and mark every matching binding dirty, in every view. Runs once
/// per [`drive_flush`]; alloc-free in the steady state (the drain buffer is reused, and with no
/// views the log is empty because observation is only switched on by `view()`). Returns the
/// number of distinct changed nodes (the `reactive.changed` span attribute, server-hmr L4).
fn distribute_changes(ext: &ReactiveExt) -> usize {
    let mut changed = ext.changed_scratch.borrow_mut();
    changed.clear();
    ext.graph.drain_changed_into(&mut changed);
    if changed.is_empty() {
        return 0;
    }
    changed.sort_unstable();
    changed.dedup();
    let mut views = ext.views.borrow_mut();
    for view in views.iter_mut() {
        for (idx, binding) in view.bindings.iter().enumerate() {
            if changed.binary_search(&binding.node).is_ok() {
                view.dirty.insert(idx);
            }
        }
    }
    changed.len()
}

/// Serialize a binding's *current* value as JSON — the shared `json.stringify` walk, so both
/// backends produce identical bytes. A dirty computed recomputes first (the `.get()` semantics);
/// a node a hot swap disposed returns `None` (the binding is silently skipped — its replacement
/// was exposed by the swap's re-run). Must be called with **no view borrow held**: a computed
/// body re-enters the backend.
fn binding_value_json<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    ext: &ReactiveExt,
    node: NodeId,
    source: &ViewSource,
) -> Result<Option<String>, CtxError> {
    if !ext.graph.is_live(node) {
        return Ok(None);
    }
    let slot = match *source {
        ViewSource::Signal { cell } => ctx.retained_get(cell)?,
        ViewSource::Computed { memo, .. } => {
            // Mirror `computed.get`: recompute-if-dirty into the stable memo cell (the graph
            // hands the body cell to the callback, exactly like the method dispatch).
            let mut aborted: Option<CtxError> = None;
            ext.graph.read(node, &mut |body: Retained| -> Retained {
                if aborted.is_none() {
                    sync_gates(ctx, ext);
                    if let Err(e) = ctx.call_thunk_into(body, memo) {
                        aborted = Some(e);
                    }
                }
                memo
            });
            sync_gates(ctx, ext);
            // The recompute may have torn down owner-tree children (S4b) — release their cells.
            release_reclaimed(ctx, ext);
            if let Some(e) = aborted {
                return Err(e);
            }
            ctx.retained_get(memo)?
        }
    };
    let value = ctx.view(slot)?;
    ctx.free(slot);
    Ok(Some(crate::json::stringify(&value)))
}

/// Render a frame: `{"type":<kind>,<field>:{"name":<json>,…}}` with entries sorted by name.
fn view_frame(
    kind: &str,
    field: &str,
    entries: &std::collections::BTreeMap<String, String>,
) -> String {
    let body: Vec<String> = entries
        .iter()
        .map(|(name, json)| format!("{}:{}", crate::json::json_string(name), json))
        .collect();
    format!("{{\"type\":\"{kind}\",\"{field}\":{{{}}}}}", body.join(","))
}

pub fn view_ctx_method_dispatch<C: NativeCtx + ?Sized>(
    method: &str,
    ctx: &mut C,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let id = {
        let mut id = None;
        ctx.with_extern(recv, &mut |e| id = Some(view_box(e).id))?;
        id.expect("a View receiver wraps a ViewBox")
    };
    let state = state_of(ctx);
    let ext = state.borrow();
    let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
    match method {
        "expose" => {
            ctx_arity(method, args, 2)?;
            let noeta_ext_abi::registry::NativeValue::Str(name) = ctx.view(args[0])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: "view.expose: the binding name must be a string".to_string(),
                }
                .into());
            };
            // Accept any handle that is a node over the shared graph: Signal, Computed, or a
            // foreign node reached through the reactive seam — `para.synced`'s SyncedSignal and
            // `para.db`'s Watch ARE signal nodes (LiveView over synced/DB state), recognized via
            // each foreign extension's `ViewSourceExtract` capability (the PLURAL broker lookup:
            // one provider per foreign reactive-node extension). Resolved up front because the
            // broker needs `ctx`, which `with_extern` borrows; empty just means no installed
            // extension provides one.
            let foreign = noeta_ext_abi::capabilities::<dyn ViewSourceExtract, C>(ctx);
            let mut found: Option<(NodeId, ViewSource)> = None;
            let _ = ctx.with_extern(args[1], &mut |e| {
                if let Some(s) = e.as_any().downcast_ref::<SignalBox>() {
                    found = Some((s.node, ViewSource::Signal { cell: s.cell }));
                } else if let Some(c) = e.as_any().downcast_ref::<ComputedBox>() {
                    found = Some((
                        c.node,
                        ViewSource::Computed {
                            body: c.body,
                            memo: c.memo,
                        },
                    ));
                } else if let Some(hit) = foreign.iter().find_map(|f| f.extract(e.as_any())) {
                    found = Some(hit);
                }
            });
            let Some((node, source)) = found else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: format!(
                        "view.expose: `{name}` must be bound to a Signal, Computed, or \
                         SyncedSignal handle"
                    ),
                }
                .into());
            };
            // Baseline now (may recompute a dirty computed — no view borrow is held yet).
            let Some(json) = binding_value_json(ctx, ext, node, &source)? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: format!("view.expose: `{name}` is bound to a disposed handle"),
                }
                .into());
            };
            let mut views = ext.views.borrow_mut();
            let view = &mut views[id];
            let binding = ViewBinding {
                name: name.clone(),
                node,
                source,
                last: json,
            };
            if let Some(idx) = view.bindings.iter().position(|b| b.name == name) {
                // Re-exposing a name replaces the binding and resets its baseline — the hot-swap
                // re-run path (a preserved signal re-exposed is a no-op change-wise).
                view.bindings[idx] = binding;
                view.dirty.remove(&idx);
            } else {
                view.bindings.push(binding);
            }
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        "unexpose" => {
            ctx_arity(method, args, 1)?;
            let noeta_ext_abi::registry::NativeValue::Str(name) = ctx.view(args[0])? else {
                return Err(StdError {
                    kind: ErrorKind::ArgType,
                    message: "view.unexpose: the binding name must be a string".to_string(),
                }
                .into());
            };
            // Drop the binding and dispose its graph node, then remap the dirty set to the shifted
            // indices (removing the binding shifts every later index down by one). Disposing the
            // node reclaims its owner-tree scope; `release_reclaimed` returns the cells on the spot,
            // so a churning keyed list (rows in and out) leaves residency flat.
            let node = {
                let mut views = ext.views.borrow_mut();
                let view = &mut views[id];
                let Some(idx) = view.bindings.iter().position(|b| b.name == name) else {
                    return Ok(CtxOut::Out(NativeOut::Unit));
                };
                let node = view.bindings[idx].node;
                view.bindings.remove(idx);
                let old = std::mem::take(&mut view.dirty);
                view.dirty = old
                    .into_iter()
                    .filter(|&d| d != idx)
                    .map(|d| if d > idx { d - 1 } else { d })
                    .collect();
                node
            };
            ext.graph.dispose(node);
            release_reclaimed(ctx, ext);
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        "snapshot" => {
            ctx_arity(method, args, 0)?;
            // Take the work list under a transient borrow (serialization re-enters the backend).
            let work: Vec<(usize, String, NodeId)> = {
                let views = ext.views.borrow();
                views[id]
                    .bindings
                    .iter()
                    .enumerate()
                    .map(|(i, b)| (i, b.name.clone(), b.node))
                    .collect()
            };
            let mut entries = std::collections::BTreeMap::new();
            let mut fresh: Vec<(usize, String)> = Vec::new();
            for (idx, name, node) in work {
                let source = {
                    let views = ext.views.borrow();
                    view_source_copy(&views[id].bindings[idx].source)
                };
                if let Some(json) = binding_value_json(ctx, ext, node, &source)? {
                    entries.insert(name, json.clone());
                    fresh.push((idx, json));
                }
            }
            let mut views = ext.views.borrow_mut();
            let view = &mut views[id];
            for (idx, json) in fresh {
                view.bindings[idx].last = json;
                view.dirty.remove(&idx);
            }
            Ok(CtxOut::Out(NativeOut::Str(view_frame(
                "snapshot", "values", &entries,
            ))))
        }
        "diff" => {
            ctx_arity(method, args, 0)?;
            let work: Vec<(usize, String, NodeId, ViewSource, String)> = {
                let views = ext.views.borrow();
                let view = &views[id];
                view.dirty
                    .iter()
                    .map(|&idx| {
                        let b = &view.bindings[idx];
                        (
                            idx,
                            b.name.clone(),
                            b.node,
                            view_source_copy(&b.source),
                            b.last.clone(),
                        )
                    })
                    .collect()
            };
            // Opt-in diff telemetry (server-hmr L4): a span only when there is dirt to inspect,
            // active while serializing (a recompute's own spans nest under it). Aborts are
            // captured, not propagated mid-span, so the context stack cannot be left imbalanced.
            let dirty_count = work.len() as i64;
            let span = if !work.is_empty() && reactive_tracing(ctx, ext) {
                let parent = crate::tracing::current_parent(ctx);
                let id = ctx
                    .host()
                    .tel_span_start("view.diff", SpanKind::Internal, parent);
                crate::tracing::push_active(ctx, id);
                Some(id)
            } else {
                None
            };
            let mut entries = std::collections::BTreeMap::new();
            let mut fresh: Vec<(usize, Option<String>)> = Vec::new();
            let mut failure: Option<CtxError> = None;
            for (idx, name, node, source, last) in work {
                match binding_value_json(ctx, ext, node, &source) {
                    Ok(Some(json)) if json != last => {
                        entries.insert(name, json.clone());
                        fresh.push((idx, Some(json)));
                    }
                    // Equal to the baseline (a no-op set / a recompute landing on the same
                    // value), or the node was disposed — either way, nothing to push.
                    Ok(_) => fresh.push((idx, None)),
                    Err(e) => {
                        failure = Some(e);
                        break;
                    }
                }
            }
            if let Some(id) = span {
                crate::tracing::pop_active(ctx, id);
                let host = ctx.host();
                host.tel_span_set_attr(id, "view.dirty", AttrValue::Int(dirty_count));
                host.tel_span_set_attr(id, "view.pushed", AttrValue::Int(entries.len() as i64));
                if failure.is_some() {
                    host.tel_span_set_status(
                        id,
                        SpanStatus::Error("binding serialization aborted".into()),
                    );
                }
                host.tel_span_end(id);
            }
            if let Some(e) = failure {
                return Err(e);
            }
            let mut views = ext.views.borrow_mut();
            let view = &mut views[id];
            for (idx, json) in fresh {
                if let Some(json) = json {
                    view.bindings[idx].last = json;
                }
                // Exactly the taken indices — dirt added by a reentrant set during our own
                // serialization stays for the next diff.
                view.dirty.remove(&idx);
            }
            if entries.is_empty() {
                return Ok(CtxOut::Out(NativeOut::None));
            }
            Ok(CtxOut::Out(NativeOut::Some(Box::new(NativeOut::Str(
                view_frame("patch", "changes", &entries),
            )))))
        }
        _ => Err(noeta_ext_abi::no_method_error(VIEW_TYPE_NAME, method).into()),
    }
}

/// Copy a source's ids out (both variants are plain `Retained` ids) so no view borrow is held
/// while serializing.
fn view_source_copy(source: &ViewSource) -> ViewSource {
    match *source {
        ViewSource::Signal { cell } => ViewSource::Signal { cell },
        ViewSource::Computed { body, memo } => ViewSource::Computed { body, memo },
    }
}

pub fn reactive_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "signal" => {
            // `signal(v)` or `signal(v, dedupe: bool)` — the second arg is the opt-in
            // change-suppression flag (trailing-optional, default off).
            if args.is_empty() || args.len() > 2 {
                return Err(noeta_ext_abi::arity_error(func, 1, args.len()).into());
            }
            let dedupe = match args.get(1) {
                Some(&flag) => match ctx.view(flag)? {
                    noeta_ext_abi::registry::NativeValue::Scalar(
                        noeta_ext_abi::registry::Scalar::Bool(b),
                    ) => b,
                    _ => {
                        return Err(StdError {
                            kind: ErrorKind::ArgType,
                            message: "signal: the dedupe flag must be a bool".to_string(),
                        }
                        .into());
                    }
                },
                None => false,
            };
            let cell = ctx.retain(args[0])?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            let node = ext.graph.signal(cell);
            Ok(CtxOut::Out(NativeOut::Extern(
                noeta_ext_abi::ExternBox::new(SignalBox { node, cell, dedupe }),
            )))
        }
        "computed" => {
            ctx_arity(func, args, 1)?;
            let body = ctx.retain(args[0])?;
            // The memo cell starts as a unit placeholder; the first (dirty) read lands the real
            // memo in it in place, so the cell id the box carries stays stable.
            let unit = ctx.intern(NativeOut::Unit)?;
            let memo = ctx.retain(unit)?;
            ctx.free(unit);
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            // The memo cell is seeded into the node too, so an owner-tree teardown (S4b) can reclaim
            // it even for a computed that is disposed before it is ever read.
            let node = ext.graph.computed(body, memo);
            // Created dirty — the memo gate is now closed until the first read.
            sync_gates(ctx, ext);
            Ok(CtxOut::Out(NativeOut::Extern(
                noeta_ext_abi::ExternBox::new(ComputedBox { node, body, memo }),
            )))
        }
        "effect" => {
            ctx_arity(func, args, 1)?;
            let body = ctx.retain(args[0])?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            // A *root* effect (created at top level, not inside a running body) joins the hot-swap
            // epoch registry, which owns its body-cell release. A *child* effect (created inside a
            // `computed`/`effect` body — `tracking()` is true) is owned by the enclosing node in the
            // S4b owner tree: its node and body cell are reclaimed when that owner reruns/disposes, so
            // it must NOT also be in the registry (a stale entry over a reused slot would misfire).
            let is_root = !ext.graph.tracking();
            let node = ext.graph.effect(body);
            if is_root {
                ext.effects.borrow_mut().push((node, body));
            }
            // Run it once now (subscribing it to the signals it reads) — unless we are already
            // inside a flush, which will drain it (no nested flush; reactivity S4).
            if !ext.graph.is_flushing() {
                drive_flush(ctx, ext)?;
            }
            Ok(CtxOut::Out(NativeOut::Extern(
                noeta_ext_abi::ExternBox::new(EffectBox { node, body }),
            )))
        }
        "view" => {
            ctx_arity(func, args, 0)?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            // The first view switches the graph's change log on; until then a hot `set` loop
            // records nothing (the L1 hook is pay-for-use).
            ext.graph.set_observed(true);
            let id = {
                let mut views = ext.views.borrow_mut();
                views.push(ViewState::default());
                views.len() - 1
            };
            Ok(CtxOut::Out(NativeOut::Extern(
                noeta_ext_abi::ExternBox::new(ViewBox { id }),
            )))
        }
        _ => Err(no_function_error("reactive", func).into()),
    }
}

pub fn signal_ctx_method_dispatch<C: NativeCtx + ?Sized>(
    method: &str,
    ctx: &mut C,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let (node, cell, dedupe) = {
        let mut parts = None;
        ctx.with_extern(recv, &mut |e| {
            let b = signal_box(e);
            parts = Some((b.node, b.cell, b.dedupe));
        })?;
        parts.expect("a Signal receiver wraps a SignalBox")
    };
    match method {
        // The full-dispatch `get` — taken while a gate is closed (a body is running, so the read
        // must record its dependency edge) and always on the tree-walker. Must behave exactly
        // like the inlined arena read when the gate is open — it does: read returns the cell id.
        "get" => {
            ctx_arity(method, args, 0)?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            // A signal read never runs a body; the callback is unreachable.
            let read_cell = ext.graph.read(node, &mut |body| body);
            debug_assert_eq!(read_cell, cell, "a signal's content cell is stable");
            Ok(CtxOut::Retained(cell))
        }
        "set" => {
            ctx_arity(method, args, 1)?;
            // Opt-in value-equality suppression (reactivity S0 note): a dedupe signal whose new value
            // is `==` its current one changes nothing — no store, no dirty, no flush, so dependents do
            // not re-fire. Default (non-dedupe) signals always fire, preserving the prior behavior.
            if dedupe {
                let current = ctx.retained_get(cell)?;
                let unchanged = ctx.values_equal(current, args[0])?;
                ctx.free(current);
                if unchanged {
                    return Ok(CtxOut::Out(NativeOut::Unit));
                }
            }
            ctx.retained_set(cell, args[0])?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            ext.graph.touch(node);
            sync_gates(ctx, ext);
            if !ext.graph.is_flushing() {
                drive_flush(ctx, ext)?;
            }
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        "update" => {
            ctx_arity(method, args, 1)?;
            // Read-modify-write: the read records a dependency edge if a body is running,
            // exactly as the old backend arms did (they read via `read_reactive`).
            let state = state_of(ctx);
            {
                let ext = state.borrow();
                let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
                ext.graph.read(node, &mut |body| body);
            }
            let current = ctx.retained_get(cell)?;
            let updated = ctx.call(args[0], &[current])?;
            // Opt-in value-equality suppression, exactly as `set`: if `update` lands a value `==` the
            // current one, do nothing (the updater's own side effects, if any, already ran).
            if dedupe {
                let unchanged = ctx.values_equal(current, updated)?;
                ctx.free(current);
                if unchanged {
                    ctx.free(updated);
                    return Ok(CtxOut::Out(NativeOut::Unit));
                }
            } else {
                ctx.free(current);
            }
            ctx.retained_set(cell, updated)?;
            ctx.free(updated);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            ext.graph.touch(node);
            sync_gates(ctx, ext);
            if !ext.graph.is_flushing() {
                drive_flush(ctx, ext)?;
            }
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(noeta_ext_abi::no_method_error(SIGNAL_TYPE_NAME, method).into()),
    }
}

pub fn computed_ctx_method_dispatch<C: NativeCtx + ?Sized>(
    method: &str,
    ctx: &mut C,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let (node, memo) = {
        let mut parts = None;
        ctx.with_extern(recv, &mut |e| {
            let b = computed_box(e);
            parts = Some((b.node, b.memo));
        })?;
        parts.expect("a Computed receiver wraps a ComputedBox")
    };
    match method {
        "get" => {
            ctx_arity(method, args, 0)?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            // A dirty computed recomputes here: the graph hands the body cell to the callback,
            // which runs it (fused; the result lands in the stable memo cell) — reentrant reads
            // inside it subscribe this node. A clean computed returns the memo cell untouched.
            let mut aborted: Option<CtxError> = None;
            ext.graph.read(node, &mut |body: Retained| -> Retained {
                if aborted.is_none() {
                    sync_gates(ctx, ext);
                    if let Err(e) = ctx.call_thunk_into(body, memo) {
                        aborted = Some(e);
                    }
                }
                memo
            });
            sync_gates(ctx, ext);
            // A recompute reruns the computed's body, which may have disposed owned children (S4b).
            release_reclaimed(ctx, ext);
            if let Some(e) = aborted {
                return Err(e);
            }
            Ok(CtxOut::Retained(memo))
        }
        _ => Err(noeta_ext_abi::no_method_error(COMPUTED_TYPE_NAME, method).into()),
    }
}

pub fn effect_ctx_method_dispatch<C: NativeCtx + ?Sized>(
    method: &str,
    ctx: &mut C,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let (node, body) = {
        let mut parts = None;
        ctx.with_extern(recv, &mut |e| {
            let b = effect_box(e);
            parts = Some((b.node, b.body));
        })?;
        parts.expect("an Effect receiver wraps an EffectBox")
    };
    match method {
        "dispose" => {
            ctx_arity(method, args, 0)?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            ext.graph.dispose(node);
            // Prune the hot-swap epoch registry so a later swap never double-releases this body.
            ext.effects.borrow_mut().retain(|(n, _)| *n != node);
            sync_gates(ctx, ext);
            // Disposing an effect cascades to its owner-tree children (S4b) — release their cells.
            release_reclaimed(ctx, ext);
            // The node held only ids; the body's arena cell is ours to release (its closure —
            // and whatever it captured — drops at its last reference, destructor-aware).
            ctx.release_retained(body);
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(noeta_ext_abi::no_method_error(EFFECT_TYPE_NAME, method).into()),
    }
}

// ----- the extern boxes: plain ids, reference semantics (copies alias the node) -----

macro_rules! reactive_box {
    ($name:ident, $identity:expr, $display:expr, { $($field:ident: $ty:ty),+ }) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            $(pub $field: $ty,)+
        }

        impl ExternValue for $name {
            fn type_identity(&self) -> &'static str {
                $identity
            }
            fn eq_value(&self, other: &dyn ExternValue) -> bool {
                other.as_any().downcast_ref::<$name>() == Some(self)
            }
            fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
                None
            }
            fn hash_value(&self) -> u64 {
                0 // not key-capable
            }
            fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
                write!(out, $display)
            }
            fn clone_box(&self) -> Box<dyn ExternValue> {
                Box::new(self.clone())
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn Any {
                self
            }
        }
    };
}

reactive_box!(SignalBox, SIGNAL_TYPE_IDENTITY, "<signal>", {
    node: NodeId, cell: Retained, dedupe: bool
});
reactive_box!(ComputedBox, COMPUTED_TYPE_IDENTITY, "<computed>", {
    node: NodeId, body: Retained, memo: Retained
});
reactive_box!(EffectBox, EFFECT_TYPE_IDENTITY, "<effect>", { node: NodeId, body: Retained });
reactive_box!(ViewBox, VIEW_TYPE_IDENTITY, "<view>", { id: usize });

fn signal_box(e: &dyn ExternValue) -> &SignalBox {
    e.as_any()
        .downcast_ref()
        .expect("a Signal receiver wraps a SignalBox")
}
fn computed_box(e: &dyn ExternValue) -> &ComputedBox {
    e.as_any()
        .downcast_ref()
        .expect("a Computed receiver wraps a ComputedBox")
}
fn effect_box(e: &dyn ExternValue) -> &EffectBox {
    e.as_any()
        .downcast_ref()
        .expect("an Effect receiver wraps an EffectBox")
}
fn view_box(e: &dyn ExternValue) -> &ViewBox {
    e.as_any()
        .downcast_ref()
        .expect("a View receiver wraps a ViewBox")
}
