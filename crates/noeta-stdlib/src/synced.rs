//! `std.synced` — collaborative, peer-to-peer state (p2p P2, architecture §9.15.1): the point
//! where the three layers meet. A `synced_signal(initial, topic)` is **a signal that happens to be
//! shared** — a node in the *same* reactive graph as `signal`/`computed`/`effect` ([`crate::
//! reactive`]), holding a CRDT ([`crate::crdt`]), whose changes cross the [`P2p`] transport
//! ([`crate::p2p`]). So a peer's edit, merged in, propagates through the reactivity graph to every
//! `computed`/`effect` exactly like a local `set` — reactivity does not care where a change came
//! from.
//!
//! # Surface
//!
//! - `synced_signal(initial: T, topic)` where `T: Mergeable` (a CRDT — enforced at compile time,
//!   p2p P2.M). Subscribes to the topic and announces its initial state.
//! - `.get() -> T` — the current merged value; a read inside a `computed`/`effect` subscribes to it.
//! - `.merge(delta)` — merge `delta` into the local state, wake the graph (dependents rerun), and
//!   publish the new state to peers. (A CRDT has no "set" — you converge, you do not overwrite.)
//! - `.sync()` — drain the topic: merge every peer message into the local state, and if anything
//!   changed, wake the graph once. **Explicit by design** — the network boundary stays visible
//!   (§9.15.1), so it is legible where peer state enters, rather than magic on every read.
//!
//! # What makes it deterministic
//!
//! The transport is the sandbox's in-process broadcast broker ([`crate::p2p`]) and the merge is a
//! pure CRDT join, so a publish/sync program is byte-identical across backends and terminates
//! in-oracle. Two `synced_signal`s on one topic *in the same program* are two replicas that
//! converge through the broker — the deterministic stand-in for two real peers (P3).
//!
//! # Sharing the reactive graph
//!
//! A synced signal is a [`ReactiveGraph`](noeta_reactive::ReactiveGraph) **signal node** over an
//! arena cell holding the CRDT value — the identical machinery `signal` uses — plus a topic and a
//! subscription id. `merge`/`sync` land the new value in the cell, `touch` the node, and
//! `drive_flush` the graph, reusing [`crate::reactive`]'s engine wholesale (shared `ExtState`, same
//! `STATE_KEY`), which is what makes the reactivity integration real rather than a parallel system.

use std::any::Any;
use std::cmp::Ordering;

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_native::{
    CtxError, CtxOut, CtxResult, ErrorKind, ExternBox, ExternValue, NativeCtx, NativeValue,
    Retained, Slot, StdError, ctx_arity, no_function_error, no_method_error, type_error,
};
use noeta_reactive::NodeId;

use crate::crdt::{from_bytes_like, merge_dyn, to_bytes_dyn};
use crate::reactive::{ReactiveExt, drive_flush, state_of, sync_gates};

pub const SYNCED_SIGNAL_TYPE_NAME: &str = "SyncedSignal";

const VAR_A: SigType = SigType::Var(0);

/// `synced_signal(initial: T, topic: string) -> SyncedSignal<T>` where `T: Mergeable` — the bound
/// is the compile-time guarantee that only a CRDT may be synced (p2p P2.M).
pub const SYNCED_CTX_FNS: &[ExtFn] = &[ExtFn {
    name: "synced_signal",
    params: &[SigType::BoundedVar(0, "Mergeable"), SigType::String],
    ret: RetTy::Concrete(SigType::Generic(SYNCED_SIGNAL_TYPE_NAME, &[VAR_A])),
}];

pub const SYNCED_CTX_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "get",
        params: &[],
        ret: RetTy::Concrete(VAR_A),
    },
    ExtFn {
        name: "merge",
        params: &[VAR_A],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "sync",
        params: &[],
        ret: RetTy::Concrete(SigType::Unit),
    },
];

/// The extern box: the reactive-graph node, the arena cell holding the CRDT value, the p2p
/// subscription, and the topic (for publishing). Plain `Send` data; copies alias the same node/cell
/// (reference semantics — the point of a signal). Equality is by these ids (two handles to one
/// synced signal are equal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedSignalBox {
    pub node: NodeId,
    pub cell: Retained,
    pub subscription: u64,
    pub topic: String,
}

impl ExternValue for SyncedSignalBox {
    fn type_name(&self) -> &'static str {
        SYNCED_SIGNAL_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<SyncedSignalBox>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<synced_signal {}>", self.topic)
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

pub fn synced_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "synced_signal" => {
            ctx_arity(func, args, 2)?;
            let topic = match ctx.view(args[1])? {
                NativeValue::Str(s) => s,
                _ => return Err(type_error("synced_signal", "string").into()),
            };
            // Serialize the initial state (and validate it really is a CRDT — the `Mergeable` bound
            // makes this hold statically, but a `dyn`-laundered value could still arrive).
            let bytes = clone_crdt(ctx, args[0])
                .and_then(|v| to_bytes_dyn(&*v))
                .ok_or_else(not_a_crdt)?;
            // Subscribe first (cursor at the log start), then announce the initial state, so another
            // replica that later subscribes still sees it and converges.
            let subscription = ctx.host().p2p_subscribe(&topic);
            ctx.host()
                .p2p_publish(&topic, bytes)
                .map_err(CtxError::from)?;
            // The CRDT lives in an arena cell; the node is a signal in the shared reactive graph.
            let cell = ctx.retain(args[0])?;
            let node = {
                let state = state_of(ctx);
                let ext = state.borrow();
                let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
                ext.graph.signal(cell)
            };
            Ok(CtxOut::Out(NativeOut::Extern(ExternBox::new(
                SyncedSignalBox {
                    node,
                    cell,
                    subscription,
                    topic,
                },
            ))))
        }
        _ => Err(no_function_error("synced", func).into()),
    }
}

pub fn synced_ctx_method_dispatch<C: NativeCtx + ?Sized>(
    method: &str,
    ctx: &mut C,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let handle = handle_of(ctx, recv)?;
    match method {
        // A reactive read of the content cell — subscribes the running body, exactly like a signal.
        "get" => {
            ctx_arity(method, args, 0)?;
            let state = state_of(ctx);
            let ext = state.borrow();
            let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
            let read_cell = ext.graph.read(handle.node, &mut |body| body);
            Ok(CtxOut::Retained(read_cell))
        }
        // Local converge + publish: merge `delta` into the current state, wake dependents, and
        // broadcast the new state to the topic.
        "merge" => {
            ctx_arity(method, args, 1)?;
            let current_slot = ctx.retained_get(handle.cell)?;
            let current = clone_crdt(ctx, current_slot).ok_or_else(not_a_crdt)?;
            let delta =
                clone_crdt(ctx, args[0]).ok_or_else(|| type_error("merge", "a CRDT delta"))?;
            let merged = merge_dyn(&*current, &*delta).ok_or_else(mismatched_merge)?;
            let bytes = to_bytes_dyn(&*merged).ok_or_else(not_a_crdt)?;
            let merged_slot = ctx.intern(NativeOut::Extern(ExternBox(merged)))?;
            ctx.retained_set(handle.cell, merged_slot)?;
            ctx.free(merged_slot);
            ctx.free(current_slot);
            self_wake(ctx, handle.node)?;
            ctx.host()
                .p2p_publish(&handle.topic, bytes)
                .map_err(CtxError::from)?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        // Drain the subscription and merge every peer message; wake dependents once if the value
        // actually changed (merging a state already reflected — including this node's own echoes —
        // is a CRDT no-op, so it does not spuriously rerun effects).
        "sync" => {
            ctx_arity(method, args, 0)?;
            let mut changed = false;
            while let Some(bytes) = ctx
                .host()
                .p2p_poll_sub(handle.subscription)
                .map_err(CtxError::from)?
            {
                let current_slot = ctx.retained_get(handle.cell)?;
                let current = clone_crdt(ctx, current_slot).ok_or_else(not_a_crdt)?;
                // A malformed / cross-type message is untrusted input — skip it, do not abort.
                if let Some(peer) = from_bytes_like(&*current, &bytes) {
                    let merged = merge_dyn(&*current, &*peer).ok_or_else(mismatched_merge)?;
                    if !merged.eq_value(&*current) {
                        let merged_slot = ctx.intern(NativeOut::Extern(ExternBox(merged)))?;
                        ctx.retained_set(handle.cell, merged_slot)?;
                        ctx.free(merged_slot);
                        changed = true;
                    }
                }
                ctx.free(current_slot);
            }
            if changed {
                self_wake(ctx, handle.node)?;
            }
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        _ => Err(no_method_error(SYNCED_SIGNAL_TYPE_NAME, method).into()),
    }
}

/// The receiver's ids, read out of its extern box.
fn handle_of<C: NativeCtx + ?Sized>(ctx: &mut C, recv: Slot) -> CtxResult<SyncedSignalBox> {
    let mut handle = None;
    ctx.with_extern(recv, &mut |e| {
        handle = e.as_any().downcast_ref::<SyncedSignalBox>().cloned();
    })?;
    Ok(handle.expect("a SyncedSignal receiver wraps a SyncedSignalBox"))
}

/// Clone a slot's value out as a boxed CRDT extern value, or `None` if it is not an extern CRDT.
fn clone_crdt<C: NativeCtx + ?Sized>(ctx: &mut C, slot: Slot) -> Option<Box<dyn ExternValue>> {
    let mut cloned = None;
    // `with_extern` errs on a non-extern slot; treat that as "not a CRDT" (None).
    let _ = ctx.with_extern(slot, &mut |e| {
        if to_bytes_dyn(e).is_some() {
            cloned = Some(e.clone_box());
        }
    });
    cloned
}

/// Touch the node and flush the shared reactive graph (unless a flush is already running) — the
/// signal `set` epilogue, reused so a synced change propagates identically.
fn self_wake<C: NativeCtx + ?Sized>(ctx: &mut C, node: NodeId) -> Result<(), CtxError> {
    let state = state_of(ctx);
    let ext = state.borrow();
    let ext: &ReactiveExt = ext.downcast_ref().expect("std.reactive state");
    ext.graph.touch(node);
    sync_gates(ctx, ext);
    if !ext.graph.is_flushing() {
        drive_flush(ctx, ext)?;
    }
    Ok(())
}

fn not_a_crdt() -> CtxError {
    StdError {
        kind: ErrorKind::ArgType,
        message: "a synced value must be a CRDT (`GCounter`/`PnCounter`/`GSet`)".to_string(),
    }
    .into()
}

fn mismatched_merge() -> CtxError {
    StdError {
        kind: ErrorKind::ArgType,
        message: "cannot merge CRDT values of different types".to_string(),
    }
    .into()
}
