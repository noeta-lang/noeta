//! `para.synced` — collaborative, peer-to-peer state (p2p P2, architecture §9.15.1): the point
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
//! A synced signal is a **signal node** in the shared reactive graph over an arena cell holding the
//! CRDT value — the identical machinery `signal` uses — plus a topic and a subscription id. It
//! participates through the [`ReactiveSource`] **capability** (obtained per-run via
//! `noeta_ext_abi::capability`), the stable trait ABI in `noeta-reactive-abi`: `create_source` mints
//! the node, `read_source` is the reactive `.get`, and `wake` is the external-change epilogue
//! `merge`/`sync` run after landing a new value in the cell. That the integration is *real* (one
//! graph, not a parallel system) is what makes a peer's merge rerun `computed`/`effect` exactly like
//! a local `set` — without this crate depending on `noeta-stdlib` or seeing the engine's internals.

use std::any::Any;
use std::cmp::Ordering;

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    Cap, CtxError, CtxOut, CtxResult, ErrorKind, ExternBox, ExternValue, NativeCtx, NativeValue,
    Retained, Slot, StdError, capability, ctx_arity, no_function_error, no_method_error,
    type_error,
};
use noeta_reactive::NodeId;
// `synced` shares the SAME reactive graph as core `std.reactive`: a synced signal IS a node in that
// graph, so a peer's merge propagates to `computed`/`effect` like a local `set`. It reaches the
// engine through the `ReactiveSource` **capability** (never the engine's internals), and depends on
// nothing of `noeta-stdlib` — only the tiny `noeta-reactive-abi` contract crate.
use noeta_ext_abi::registry::ExtCapability;
use noeta_reactive_abi::{ReactiveSource, ViewSource, ViewSourceExtract};

use crate::crdt::{from_bytes_like, merge_dyn, to_bytes_dyn};
use crate::provider::with_p2p;

/// Obtain the reactive engine's `ReactiveSource` capability for this run — the seam a synced signal
/// (a node over the shared graph) drives its create/read/wake through. Present whenever `std.reactive`
/// is installed, which it always is in any registry that also resolved `para.synced`.
fn reactive<C: NativeCtx + ?Sized>(ctx: &mut C) -> Cap<dyn ReactiveSource> {
    capability::<dyn ReactiveSource, C>(ctx)
        .expect("std.reactive capability (the engine para.synced extends)")
}

/// The `SyncedSignal` → reactive-`view` extractor (the reactive seam that lets core `view.expose`
/// accept a [`SyncedSignalBox`] — a signal node over the shared graph — without naming this
/// out-of-`std` type). Provided to the engine as the `dyn ViewSourceExtract` **capability**
/// declared on this unit ([`SYNCED_CAPABILITIES`]), the same broker `para.synced` already consumes
/// the engine's `ReactiveSource` through — replacing the process-global extractor list a dispatch
/// side effect used to fill (audit-2 Finding 12): registry-scoped, no first-use registration.
struct SyncedViewExtract;

impl ViewSourceExtract for SyncedViewExtract {
    fn extract(&self, any: &dyn Any) -> Option<(NodeId, ViewSource)> {
        any.downcast_ref::<SyncedSignalBox>()
            .map(|s| (s.node, ViewSource::Signal { cell: s.cell }))
    }
}

/// The capabilities `para.p2p` provides (declared on the unit's `Extension::capabilities`): the
/// [`ViewSourceExtract`] seam `view.expose` resolves a `SyncedSignal` through. The extractor is
/// stateless, so the backing state cell is an inert unit — the broker requires a state slot, and
/// sharing [`crate::provider::STATE_KEY`] would wrongly couple extractor resolution to the p2p
/// backend's lifecycle.
pub const SYNCED_CAPABILITIES: &[ExtCapability] = &[ExtCapability {
    id: || std::any::TypeId::of::<dyn ViewSourceExtract>(),
    state_key: "para.synced.view",
    init: || Box::new(()),
    build: |_state| {
        let handle: Box<dyn ViewSourceExtract> = Box::new(SyncedViewExtract);
        Box::new(handle)
    },
}];

pub const SYNCED_SIGNAL_TYPE_NAME: &str = "SyncedSignal";

/// `SyncedSignal`'s qualified runtime identity — registered under `para.synced`.
pub const SYNCED_SIGNAL_TYPE_IDENTITY: &str = "para.synced.SyncedSignal";

const VAR_A: SigType = SigType::Var(0);
/// The optional third argument: the member set of an **encrypted** group (p2p P3.4b). A list of
/// peer-id (hex) strings. Present ⇒ the signal is end-to-end encrypted to exactly those members;
/// absent ⇒ the plaintext transport (unchanged). Members-imply-encryption is the safe default:
/// there is no way to declare members without encryption, and no meaningful encryption without a
/// membership set.
const MEMBERS: SigType = SigType::List(&SigType::String);

/// `synced_signal(initial: T, topic: string, members?: List<string>) -> SyncedSignal<T>` where
/// `T: Mergeable` — the bound is the compile-time guarantee that only a CRDT may be synced (p2p
/// P2.M). The optional `members` list opts the signal into an encrypted group (p2p P3.4b); given at
/// construction so the very first state announced to the topic is already encrypted.
pub const SYNCED_CTX_FNS: &[ExtFn] = &[ExtFn {
    name: "synced_signal",
    params: &[
        SigType::BoundedVar(0, "Mergeable"),
        SigType::String,
        SigType::Optional(&MEMBERS),
    ],
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
    // `.status() -> string` — this replica's convergence state for its topic (p2p P3.3): one of
    // `"synced"` / `"syncing"` / `"offline"`. Always `"synced"` under the deterministic sandbox
    // broker (a single node never lags), so a program that reads it stays oracle-safe; meaningful
    // over a real network, where an `effect` can render "working offline".
    ExtFn {
        name: "status",
        params: &[],
        ret: RetTy::Concrete(SigType::String),
    },
    // `.add_member(peer_id)` / `.remove_member(peer_id)` — runtime membership changes for an
    // encrypted group (p2p P3.4b). `remove` rotates the group key so the removed peer stops
    // decrypting new state. Only meaningful on an encrypted signal (one created with a members
    // list); the group creator is authoritative. No-ops under the deterministic sandbox.
    ExtFn {
        name: "add_member",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Unit),
    },
    ExtFn {
        name: "remove_member",
        params: &[SigType::String],
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
    /// The encrypted group's member set (p2p P3.4b), empty for a plaintext signal. When non-empty
    /// the signal's state crosses the wire encrypted to exactly these peer ids; the deterministic
    /// sandbox treats it as a transparent pass-through (the decrypted value equals the plaintext
    /// value), so an encrypted program stays oracle byte-identical.
    pub members: Vec<String>,
}

impl SyncedSignalBox {
    /// Whether this signal is an encrypted group (has a declared membership).
    pub fn is_encrypted(&self) -> bool {
        !self.members.is_empty()
    }
}

impl ExternValue for SyncedSignalBox {
    fn type_identity(&self) -> &'static str {
        SYNCED_SIGNAL_TYPE_IDENTITY
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

pub fn synced_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "synced_signal" => {
            // 2 required args (initial, topic); an optional 3rd (members) opts into encryption.
            if args.len() < 2 || args.len() > 3 {
                return Err(StdError {
                    kind: ErrorKind::Arity,
                    message: format!(
                        "`synced_signal` takes 2 or 3 arguments but {} were supplied",
                        args.len()
                    ),
                }
                .into());
            }
            let topic = match ctx.view(args[1])? {
                NativeValue::Str(s) => s,
                _ => return Err(type_error("synced_signal", "string").into()),
            };
            // The encrypted group's members (peer-id hex strings), or empty for the plaintext path.
            let members = match args.get(2) {
                Some(&slot) => match ctx.view(slot)? {
                    NativeValue::List(items) => items
                        .into_iter()
                        .map(|v| match v {
                            NativeValue::Str(s) => Ok(s),
                            _ => Err(type_error("synced_signal", "a list of peer-id strings")),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => {
                        return Err(type_error("synced_signal", "a list of peer-id strings").into());
                    }
                },
                None => Vec::new(),
            };
            // Serialize the initial state (and validate it really is a CRDT — the `Mergeable` bound
            // makes this hold statically, but a `dyn`-laundered value could still arrive).
            let bytes = clone_crdt(ctx, args[0])
                .and_then(|v| to_bytes_dyn(&*v))
                .ok_or_else(not_a_crdt)?;
            // Subscribe first (cursor at the log start), then announce the initial state, so another
            // replica that later subscribes still sees it and converges. An encrypted group
            // (`members` given) routes through the group transport, which encrypts the announce to
            // the declared members; a plaintext signal uses the durable transport directly.
            let subscription = if members.is_empty() {
                with_p2p(ctx, |p| p.p2p_subscribe_durable(&topic))?
            } else {
                with_p2p(ctx, |p| p.p2p_group_open(&topic, &members))?
            };
            if members.is_empty() {
                with_p2p(ctx, |p| p.p2p_publish_durable(&topic, bytes))?;
            } else {
                with_p2p(ctx, |p| p.p2p_group_publish(&topic, bytes))?;
            }
            // The CRDT lives in an arena cell; the node is a signal in the shared reactive graph.
            let cell = ctx.retain(args[0])?;
            // A signal node in the shared reactive graph over the arena cell holding the CRDT.
            let rx = reactive(ctx);
            let node = rx.create_source(ctx, cell);
            Ok(CtxOut::Out(NativeOut::Extern(ExternBox::new(
                SyncedSignalBox {
                    node,
                    cell,
                    subscription,
                    topic,
                    members,
                },
            ))))
        }
        _ => Err(no_function_error("synced", func).into()),
    }
}

pub fn synced_ctx_method_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let handle = handle_of(ctx, recv)?;
    match method {
        // A reactive read of the content cell — subscribes the running body, exactly like a signal.
        "get" => {
            ctx_arity(method, args, 0)?;
            let rx = reactive(ctx);
            let read_cell = rx.read_source(ctx, handle.node);
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
            let rx = reactive(ctx);
            rx.wake(ctx, handle.node)?;
            if handle.is_encrypted() {
                with_p2p(ctx, |p| p.p2p_group_publish(&handle.topic, bytes))?;
            } else {
                with_p2p(ctx, |p| p.p2p_publish_durable(&handle.topic, bytes))?;
            }
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        // Drain the subscription and merge every peer message; wake dependents once if the value
        // actually changed (merging a state already reflected — including this node's own echoes —
        // is a CRDT no-op, so it does not spuriously rerun effects).
        "sync" => {
            ctx_arity(method, args, 0)?;
            let mut changed = false;
            let encrypted = handle.is_encrypted();
            while let Some(bytes) = with_p2p(ctx, |p| {
                if encrypted {
                    p.p2p_group_poll(handle.subscription)
                } else {
                    p.p2p_poll_sub(handle.subscription)
                }
            })? {
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
                let rx = reactive(ctx);
                rx.wake(ctx, handle.node)?;
            }
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        // This replica's convergence state for its topic — a plain lowercase word (p2p P3.3).
        "status" => {
            ctx_arity(method, args, 0)?;
            let status = with_p2p(ctx, |p| Ok(p.p2p_sync_status(&handle.topic)))?;
            Ok(CtxOut::Out(NativeOut::Str(status.as_str().to_string())))
        }
        // Runtime membership changes for an encrypted group (p2p P3.4b). Only valid on an encrypted
        // signal; `remove_member` revokes (the group key is rotated on a real host).
        "add_member" | "remove_member" => {
            ctx_arity(method, args, 1)?;
            if !handle.is_encrypted() {
                return Err(StdError {
                    kind: ErrorKind::Panic,
                    message: format!(
                        "`{method}` is only valid on an encrypted synced_signal (one created with a members list)"
                    ),
                }
                .into());
            }
            let member = match ctx.view(args[0])? {
                NativeValue::Str(s) => s,
                _ => return Err(type_error(method, "a peer-id string").into()),
            };
            if method == "add_member" {
                with_p2p(ctx, |p| p.p2p_group_add(&handle.topic, &member))?;
            } else {
                with_p2p(ctx, |p| p.p2p_group_remove(&handle.topic, &member))?;
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
