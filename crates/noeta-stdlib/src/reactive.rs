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

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_native::{
    ctx_arity, no_function_error, ArenaGetter, CtxError, CtxOut, ErrorKind, ExtState, ExternValue,
    NativeCtx, Retained, Slot, StdError,
};
use noeta_reactive::{NodeId, ReactiveGraph, MAX_FLUSH_STEPS};

pub const SIGNAL_TYPE_NAME: &str = "Signal";
pub const COMPUTED_TYPE_NAME: &str = "Computed";
pub const EFFECT_TYPE_NAME: &str = "Effect";

const VAR_A: SigType = SigType::Var(0);

pub const REACTIVE_CTX_FNS: &[ExtFn] = &[
    // `signal(v: A) -> Signal<A>` — a reactive cell.
    ExtFn {
        name: "signal",
        params: &[VAR_A],
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

pub const SIGNAL_ARENA_GETTER: ArenaGetter = ("get", |e| signal_box(e).cell);
pub const COMPUTED_ARENA_GETTER: ArenaGetter = ("get", |e| computed_box(e).memo);

/// The per-run extension state: the graph, over arena-cell ids, plus the last gate state pushed
/// to the backend (so a redundant sync is one branch, not two backend calls).
///
/// `pub(crate)` because [`crate::synced`] (p2p P2) shares this exact graph: a synced signal is a
/// signal node here plus a topic, so a peer's merge propagates to `computed`/`effect` like any
/// local `set`. It reaches the graph, `sync_gates`, and `drive_flush` through the items below.
pub(crate) struct ReactiveExt {
    pub(crate) graph: ReactiveGraph<Retained>,
    gates_open: std::cell::Cell<bool>,
}

pub(crate) const STATE_KEY: &str = "std.reactive";

pub(crate) fn state_of<C: NativeCtx + ?Sized>(ctx: &mut C) -> ExtState {
    ctx.state(STATE_KEY, || {
        Box::new(ReactiveExt {
            graph: ReactiveGraph::new(),
            // The backend's gates start open — mirror that.
            gates_open: std::cell::Cell::new(true),
        })
    })
}

/// Recompute the read gates from the graph's state (see the module docs) and push a *change* to
/// the backend; an unchanged state is one branch.
pub(crate) fn sync_gates<C: NativeCtx + ?Sized>(ctx: &mut C, ext: &ReactiveExt) {
    let open =
        !ext.graph.is_flushing() && !ext.graph.tracking() && ext.graph.dirty_computed_count() == 0;
    if ext.gates_open.replace(open) != open {
        ctx.set_read_gate(SIGNAL_TYPE_NAME, open);
        // A signal read is compromised only by tracking, but one shared predicate keeps the gate
        // reasoning one sentence long; refine per-type only if a bench demands it.
        ctx.set_read_gate(COMPUTED_TYPE_NAME, open);
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
    let mut aborted: Option<CtxError> = None;
    let overflowed = ext
        .graph
        .flush(&mut |body: Retained| -> Retained {
            if aborted.is_none() {
                sync_gates(ctx, ext);
                if let Err(e) = ctx.run_thunk(body) {
                    aborted = Some(e);
                }
            }
            body
        })
        .is_err();
    sync_gates(ctx, ext);
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

pub fn reactive_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "signal" => {
            ctx_arity(func, args, 1)?;
            let cell = ctx.retain(args[0])?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            let node = ext.graph.signal(cell);
            Ok(CtxOut::Out(NativeOut::Extern(noeta_native::ExternBox::new(
                SignalBox { node, cell },
            ))))
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
            let node = ext.graph.computed(body);
            // Created dirty — the memo gate is now closed until the first read.
            sync_gates(ctx, ext);
            Ok(CtxOut::Out(NativeOut::Extern(noeta_native::ExternBox::new(
                ComputedBox { node, body, memo },
            ))))
        }
        "effect" => {
            ctx_arity(func, args, 1)?;
            let body = ctx.retain(args[0])?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            let node = ext.graph.effect(body);
            // Run it once now (subscribing it to the signals it reads) — unless we are already
            // inside a flush, which will drain it (no nested flush; reactivity S4).
            if !ext.graph.is_flushing() {
                drive_flush(ctx, ext)?;
            }
            Ok(CtxOut::Out(NativeOut::Extern(noeta_native::ExternBox::new(
                EffectBox { node, body },
            ))))
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
    let (node, cell) = {
        let mut parts = None;
        ctx.with_extern(recv, &mut |e| {
            let b = signal_box(e);
            parts = Some((b.node, b.cell));
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
            ctx.free(current);
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
        _ => Err(noeta_native::no_method_error(SIGNAL_TYPE_NAME, method).into()),
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
            if let Some(e) = aborted {
                return Err(e);
            }
            Ok(CtxOut::Retained(memo))
        }
        _ => Err(noeta_native::no_method_error(COMPUTED_TYPE_NAME, method).into()),
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
            sync_gates(ctx, ext);
            // The node held only ids; the body's arena cell is ours to release (its closure —
            // and whatever it captured — drops at its last reference, destructor-aware).
            ctx.release_retained(body);
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(noeta_native::no_method_error(EFFECT_TYPE_NAME, method).into()),
    }
}

// ----- the extern boxes: plain ids, reference semantics (copies alias the node) -----

macro_rules! reactive_box {
    ($name:ident, $type_name:expr, $display:expr, { $($field:ident: $ty:ty),+ }) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name {
            $(pub $field: $ty,)+
        }

        impl ExternValue for $name {
            fn type_name(&self) -> &'static str {
                $type_name
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

reactive_box!(SignalBox, SIGNAL_TYPE_NAME, "<signal>", { node: NodeId, cell: Retained });
reactive_box!(ComputedBox, COMPUTED_TYPE_NAME, "<computed>", {
    node: NodeId, body: Retained, memo: Retained
});
reactive_box!(EffectBox, EFFECT_TYPE_NAME, "<effect>", { node: NodeId, body: Retained });

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
