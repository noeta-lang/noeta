//! The **reactive DB source** (aether DB5): `db.watch(conn, channel) -> Watch`, a node in the *same*
//! `std.reactive` graph as a plain `signal`, whose value is a **revision counter**.
//!
//! - `watch.get()` — a reactive read: subscribes the running computation, so a `computed` that reads
//!   it (and re-queries the DB) re-runs whenever the watch wakes.
//! - `watch.pump()` — poll the connection's pending change notifications **non-blocking** (Postgres
//!   `LISTEN`/`NOTIFY`); if any arrived on this channel, bump the revision and **wake** the graph, so
//!   every dependent (an `effect`, a LiveView `view.expose`) updates. Returns whether it woke.
//!
//! This makes an **external** write — another process/connection changing the table — flow to the UI:
//! a DB trigger (or the writer) issues `NOTIFY channel`, the app pumps the watch from its loop, and
//! the reactive query re-runs. Pull-pumped like `para.synced`'s `.sync()`, so `wake` only ever fires
//! on the reactive thread — no cross-thread reactivity. The reactive engine is reached purely through
//! the `ReactiveSource` capability contract; this module never sees the engine's internals.
//!
//! Driver-agnostic: **Postgres** uses real `LISTEN`/`NOTIFY` (cross-process, cross-worker).
//! **SQLite** — a library with no server notify — uses its per-connection update hook plus a
//! process-global bus, so a write wakes watchers in this isolate and any sibling *isolate* of the
//! same process (the parallel server's workers), but not a separate OS process writing the same file.
//! A driver that supports neither leaves the `listen`/`notifications` defaults and `watch` degrades to
//! an in-process revision signal (`pump` never wakes).

use std::any::Any;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use noeta_native::registry::{ExtFn, NativeOut, RetTy, Scalar, SigType};
use noeta_native::{
    Cap, CtxError, CtxOut, CtxResult, ExternBox, ExternValue, NativeCtx, NativeValue, Retained,
    Slot, capability, ctx_arity, no_function_error, no_method_error, type_error,
};
use noeta_reactive::NodeId;
use noeta_reactive_abi::{ReactiveSource, ViewSource, register_view_source_extractor};

use crate::conn::{CONNECTION_TYPE_NAME, ConnectionBox};
use crate::driver::SqlDriver;

/// The registered type's short name — its qualified identity is `para.db.Watch`.
pub const WATCH_TYPE_NAME: &str = "Watch";

/// Obtain the reactive engine's `ReactiveSource` capability for this run — how the watch node drives
/// its create/read/wake. Present whenever `std.reactive` is installed (always, in any registry that
/// resolved `para.db`).
fn reactive(ctx: &mut dyn NativeCtx) -> Cap<dyn ReactiveSource> {
    capability::<dyn ReactiveSource, dyn NativeCtx>(ctx)
        .expect("std.reactive capability (the engine para.db.watch extends)")
}

/// Register the `Watch` → reactive-`view` extractor once, so core `view.expose` accepts a `Watch` (a
/// signal node over the shared graph) without naming this out-of-`std` type. A `Watch` can never be
/// exposed before it is first constructed, so registration always precedes use.
fn ensure_view_seam_registered() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_view_source_extractor(|any| {
            any.downcast_ref::<WatchBox>()
                .map(|w| (w.node, ViewSource::Signal { cell: w.cell }))
        });
    });
}

const CONNECTION_SIG: SigType = SigType::Named(CONNECTION_TYPE_NAME);

/// `db.watch(conn, channel) -> Watch` — a higher-order (ctx) function: it reaches the reactive engine.
pub const WATCH_FNS: &[ExtFn] = &[ExtFn {
    name: "watch",
    params: &[CONNECTION_SIG, SigType::String],
    ret: RetTy::Concrete(SigType::Named(WATCH_TYPE_NAME)),
}];

pub const WATCH_METHODS: &[ExtFn] = &[
    // `.get() -> int` — a reactive read of the revision (subscribes the running computation).
    ExtFn {
        name: "get",
        params: &[],
        ret: RetTy::Concrete(SigType::Int),
    },
    // `.pump() -> bool` — poll pending notifications; wake if any fired on this channel. Returns
    // whether it woke. Call it from the app's loop (e.g. the serve loop, alongside background tasks).
    ExtFn {
        name: "pump",
        params: &[],
        ret: RetTy::Concrete(SigType::Bool),
    },
];

/// The extern box: the reactive-graph node, the arena cell holding the revision `int`, the shared
/// driver handle to poll notifications on, and the channel it listens to. Copies alias the same
/// node/cell (reference semantics — the point of a reactive value).
#[derive(Clone)]
pub struct WatchBox {
    pub node: NodeId,
    pub cell: Retained,
    conn: Arc<Mutex<Box<dyn SqlDriver>>>,
    channel: String,
}

impl std::fmt::Debug for WatchBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<watch {}>", self.channel)
    }
}

impl ExternValue for WatchBox {
    fn type_name(&self) -> &'static str {
        WATCH_TYPE_NAME
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        // Two handles to one watch are equal iff they share the reactive node.
        other
            .as_any()
            .downcast_ref::<WatchBox>()
            .is_some_and(|o| o.node == self.node)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<watch {}>", self.channel)
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

/// The `db.watch` ctx dispatch (paired with [`WATCH_FNS`]).
pub fn watch_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    ensure_view_seam_registered();
    match func {
        "watch" => {
            ctx_arity(func, args, 2)?;
            let channel = match ctx.view(args[1])? {
                NativeValue::Str(s) => s,
                _ => return Err(type_error("watch", "string").into()),
            };
            let conn = extern_conn(ctx, args[0])?;
            // Subscribe the connection to the channel. An unsupported driver (SQLite) leaves the
            // watch as an in-process revision signal — its `pump` simply never finds notifications.
            if let Ok(mut driver) = conn.lock() {
                let _ = driver.listen(&channel);
            }
            // The revision cell starts at 0; the node is a signal over it in the shared graph.
            let zero = ctx.intern(NativeOut::Scalar(Scalar::Int(0)))?;
            let cell = ctx.retain(zero)?;
            ctx.free(zero);
            let rx = reactive(ctx);
            let node = rx.create_source(ctx, cell);
            Ok(CtxOut::Out(NativeOut::Extern(ExternBox::new(WatchBox {
                node,
                cell,
                conn,
                channel,
            }))))
        }
        _ => Err(no_function_error("db", func).into()),
    }
}

/// The `Watch` method ctx dispatch (paired with [`WATCH_METHODS`]).
pub fn watch_ctx_method_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let handle = handle_of(ctx, recv)?;
    match method {
        "get" => {
            ctx_arity(method, args, 0)?;
            let rx = reactive(ctx);
            let read_cell = rx.read_source(ctx, handle.node);
            Ok(CtxOut::Retained(read_cell))
        }
        "pump" => {
            ctx_arity(method, args, 0)?;
            // Poll pending notifications on the shared driver; did any fire on our channel?
            let fired = {
                let mut driver = handle
                    .conn
                    .lock()
                    .map_err(|_| type_error("pump", "a live connection"))?;
                match driver.notifications() {
                    Ok(channels) => channels.iter().any(|c| c == &handle.channel),
                    Err(_) => false, // a driver without notifications never wakes (in-process only)
                }
            };
            if fired {
                bump(ctx, handle.cell)?;
                let rx = reactive(ctx);
                rx.wake(ctx, handle.node)?;
            }
            Ok(CtxOut::Out(NativeOut::Scalar(Scalar::Bool(fired))))
        }
        _ => Err(no_method_error(WATCH_TYPE_NAME, method).into()),
    }
}

/// Increment the revision `int` held in the watch's arena cell.
fn bump(ctx: &mut dyn NativeCtx, cell: Retained) -> CtxResult<()> {
    let current = ctx.retained_get(cell)?;
    let n = match ctx.view(current)? {
        NativeValue::Scalar(Scalar::Int(n)) => n,
        _ => 0,
    };
    ctx.free(current);
    let next = ctx.intern(NativeOut::Scalar(Scalar::Int(n + 1)))?;
    ctx.retained_set(cell, next)?;
    ctx.free(next);
    Ok(())
}

/// Clone the `WatchBox` a receiver slot wraps.
fn handle_of(ctx: &mut dyn NativeCtx, recv: Slot) -> CtxResult<WatchBox> {
    let mut handle = None;
    ctx.with_extern(recv, &mut |e| {
        handle = e.as_any().downcast_ref::<WatchBox>().cloned();
    })?;
    Ok(handle.expect("a Watch receiver wraps a WatchBox"))
}

/// Clone the shared driver handle out of a `Connection` argument.
fn extern_conn(ctx: &mut dyn NativeCtx, slot: Slot) -> CtxResult<Arc<Mutex<Box<dyn SqlDriver>>>> {
    let mut arc = None;
    ctx.with_extern(slot, &mut |e| {
        arc = e
            .as_any()
            .downcast_ref::<ConnectionBox>()
            .map(|c| c.0.clone());
    })?;
    match arc {
        Some(arc) => Ok(arc),
        None => Err(type_error("watch", "a Connection").into()),
    }
}
