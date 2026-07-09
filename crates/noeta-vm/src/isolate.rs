//! Real OS-thread isolates: copy-at-the-boundary marshalling (isolates I.4b).
//!
//! A real `isolate f(args)` runs on its own OS thread with its own VM and heap (out-of-oracle, CLI
//! only). Because runtime `Value`s are raw NaN-boxed heap pointers with non-atomic refcounts,
//! **no `Value` may cross a thread** (shape handles themselves are `Arc` since P-PAR S1 — the
//! prerequisite for shared-region borrow-share — but the object graph they hang off is not).
//! Instead the argument graph is copied into a `Send`, self-contained
//! [`Wire`] on the parent thread and rebuilt into fresh heap objects on the worker (and the result
//! copied back the same way). The one thing genuinely shared is `Arc<Module>` — the compiled module is
//! `Send + Sync` (fully index-based, no `Rc`) — so shapes are carried by their `Module.shapes` **index**,
//! identical across every VM that shares the module, needing no name lookup on rebuild.
//!
//! Cross-thread channels (isolates I.4c): shipping a `Sender`/`Receiver` into an isolate shares one
//! [`ChannelCore`] by `Arc`, so both isolates operate on a single queue. Send/recv stay **cooperative
//! polls** over that shared (mutex-guarded) queue — never a thread block — so a producer/consumer
//! split across isolate threads makes progress by each thread's scheduler re-polling, without the
//! block-stalls-siblings deadlock hazard. `marshal` ships an endpoint over a shared channel and rejects
//! one over a `Local` (cooperative-only) channel with the `"channel"` error (cooperative fallback).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use noeta_object::Shape;
use noeta_stdlib::TraceContext;
use noeta_value::{ChannelId, Value};

use crate::Channel;
use noeta_bytecode::Builtin;

/// A bounded, cross-thread channel (isolates I.4c): the `Shared` backing behind [`Channel::Shared`].
/// A `Mutex`-guarded FIFO of `Wire` messages reachable by `Arc` from every isolate holding an endpoint
/// — the one place a message crosses a thread. Operations are **non-blocking** (`try_send`/`try_recv`
/// under a short lock); the callers poll cooperatively (Pending on full/empty), so no thread ever
/// blocks *on the channel* and a producer/consumer split across isolate threads makes progress by
/// each thread's scheduler re-polling. Successful ops bump the process-wide [`WAKE`] eventcount
/// (P-PAR S3) so a scheduler parked at a stall re-polls immediately instead of sleeping a quantum.
#[derive(Debug)]
pub struct ChannelCore {
    inner: Mutex<ChannelInner>,
    capacity: usize,
}

#[derive(Debug)]
struct ChannelInner {
    // Each message carries the sender's trace context (native-otel T5d) — the automatic-
    // propagation envelope, crossing the thread with the payload.
    queue: VecDeque<(Wire, Option<TraceContext>)>,
    closed: bool,
}

/// The state of a send at a poll (isolates I.4c): the buffer has `Room`, is `Full` (suspend), or the
/// channel is `Closed` (a bug — the receiver would never see it).
pub enum SendState {
    Room,
    Full,
    Closed,
}

/// The outcome of a receive poll (isolates I.4c): a message `Got`, the buffer is `Empty` (suspend), or
/// the channel is closed and drained (`ClosedEmpty` → `none`).
pub enum RecvState {
    Got(Wire, Option<TraceContext>),
    Empty,
    ClosedEmpty,
}

impl ChannelCore {
    /// A fresh empty channel with the given buffer capacity, behind an `Arc` for cross-thread sharing.
    pub fn new(capacity: usize) -> Arc<ChannelCore> {
        Arc::new(ChannelCore {
            inner: Mutex::new(ChannelInner {
                queue: VecDeque::new(),
                closed: false,
            }),
            capacity,
        })
    }

    /// Whether a send could proceed right now, without marshalling a message (so a full-buffer poll
    /// stays cheap). The caller marshals and [`try_send`](Self::try_send)s only on `Room`.
    pub fn send_state(&self) -> SendState {
        let inner = self.inner.lock().expect("channel mutex poisoned");
        if inner.closed {
            SendState::Closed
        } else if inner.queue.len() < self.capacity {
            SendState::Room
        } else {
            SendState::Full
        }
    }

    /// Push a marshalled message if there is still room and the channel is open (re-checked under the
    /// lock, so a race after [`send_state`](Self::send_state) is safe); returns whether it was pushed.
    /// A successful push is cross-thread progress, so it bumps [`WAKE`] — a consumer's scheduler
    /// parked in `isolate_in_flight_wait` re-polls immediately instead of sleeping out its quantum.
    pub fn try_send(&self, msg: Wire, context: Option<TraceContext>) -> bool {
        let mut inner = self.inner.lock().expect("channel mutex poisoned");
        if !inner.closed && inner.queue.len() < self.capacity {
            inner.queue.push_back((msg, context));
            drop(inner);
            WAKE.notify();
            true
        } else {
            false
        }
    }

    /// Dequeue the next message, or report the buffer empty / closed-and-drained. A dequeue frees a
    /// buffer slot — progress for a producer parked on send-full — so it bumps [`WAKE`].
    pub fn try_recv(&self) -> RecvState {
        let mut inner = self.inner.lock().expect("channel mutex poisoned");
        if let Some((msg, context)) = inner.queue.pop_front() {
            drop(inner);
            WAKE.notify();
            RecvState::Got(msg, context)
        } else if inner.closed {
            RecvState::ClosedEmpty
        } else {
            RecvState::Empty
        }
    }

    /// Close the channel (idempotent): no further sends, and a drained receiver reads `none`.
    /// Close is progress for a receiver parked on recv-empty (it now reads `none`), so bump [`WAKE`].
    pub fn close(&self) {
        self.inner.lock().expect("channel mutex poisoned").closed = true;
        WAKE.notify();
    }

    /// Whether the channel is still open — a stalled scheduler keeps polling while any open shared
    /// channel could yet be fed/drained by another isolate thread (rather than declaring a deadlock).
    pub fn is_open(&self) -> bool {
        !self.inner.lock().expect("channel mutex poisoned").closed
    }
}

/// Cross-thread progress wakeup (P-PAR S3): a process-wide eventcount replacing the stall wait's
/// 100 µs sleep-spin. Every cross-thread progress event — a worker's result landing, a shared
/// channel gaining a message, freeing a slot, or closing — bumps the generation and signals; a
/// scheduler that finished an unproductive poll round parks in [`wait_past`](WakeSignal::wait_past)
/// against the generation it read **before** the round, so progress made *during* the round returns
/// immediately (no missed-wakeup window). Process-wide rather than per-VM because one event source
/// (a shared `ChannelCore`) can unblock schedulers in several isolate trees; a spurious wake just
/// re-polls, which the cooperative model already tolerates. The deterministic sandbox never parks
/// (no real isolates, all channels `Local`), so in-oracle behaviour is untouched — its round loops
/// only pay one relaxed-ish atomic load per round for the generation snapshot.
pub struct WakeSignal {
    generation: std::sync::atomic::AtomicU64,
    lock: Mutex<()>,
    cv: std::sync::Condvar,
}

/// The one process-wide wake signal (see [`WakeSignal`]).
pub static WAKE: WakeSignal = WakeSignal::new();

impl WakeSignal {
    const fn new() -> WakeSignal {
        WakeSignal {
            generation: std::sync::atomic::AtomicU64::new(0),
            lock: Mutex::new(()),
            cv: std::sync::Condvar::new(),
        }
    }

    /// Snapshot the generation — read at the top of a poll round, before any polling.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record cross-thread progress: bump the generation (under the lock, so a parked waiter's
    /// re-check cannot miss it) and wake every parked scheduler.
    pub fn notify(&self) {
        {
            let _guard = self.lock.lock().expect("wake mutex poisoned");
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
        self.cv.notify_all();
    }

    /// Park until the generation moves past `seen` (progress since the caller's snapshot) or the
    /// safety timeout elapses. Returns immediately if progress already happened; a timeout or a
    /// spurious wake is harmless — the caller loops and re-polls either way, so the timeout exists
    /// only to keep liveness under a missed-notify bug, never for correctness.
    pub fn wait_past(&self, seen: u64, timeout: std::time::Duration) {
        let guard = self.lock.lock().expect("wake mutex poisoned");
        if self.generation.load(std::sync::atomic::Ordering::Acquire) != seen {
            return;
        }
        let _ = self
            .cv
            .wait_timeout(guard, timeout)
            .expect("wake mutex poisoned");
    }
}

/// One isolate argument as shipped to a worker thread (P-PAR S2): either a [`Wire`] deep copy
/// (the I.4b path — kept for immediates, channel endpoints, function values, and any graph that
/// is not [promotable](noeta_value::Value::is_promotable_graph)), or a **borrowed** root into the
/// parent's [`noeta_value::SharedRegion`] — promoted once, read zero-copy by every worker.
pub enum IsoArg {
    Copied(Wire),
    Borrowed(SharedRoot),
}

pub use noeta_value::SharedRoot;

/// A `Send`, self-contained serialization of a value graph crossing an isolate boundary by copy. No
/// `Value`/heap pointer or `Rc` is inside it, so it moves freely between threads; the worker rebuilds
/// its own heap objects from it. Shapes are the `Module.shapes` index (stable across VMs sharing the
/// one `Arc<Module>`).
#[derive(Debug, Clone)]
pub enum Wire {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    F32(f32),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Wire>),
    Tuple(Vec<Wire>),
    Set(Vec<Wire>),
    Map(Vec<(String, Wire)>),
    /// A struct/class object: its `Module.shapes` index and its fields in slot order.
    Object {
        shape: u32,
        fields: Vec<Wire>,
    },
    /// An enum value: its `Module.shapes` index (which encodes name + variant) and its payload.
    Enum {
        shape: u32,
        data: Vec<Wire>,
    },
    /// A top-level function value (empty upvalues): its prototype index. Lets a marshalled global
    /// function be reconstructed on the worker so an isolate body can call it.
    Function(u32),
    /// A first-class builtin (`use std.task.{sleep}` binds one, prelude-redesign P2) — plain `Send`
    /// data, so an import-bound global reaches the worker and the isolate body can call it.
    NativeFn(Builtin),
    /// A Ring 2 native module binding (`use std.{math}`), by surface name — plain data; the worker's
    /// own registry/host serve its calls.
    NativeModule(String),
    /// A selectively-imported module function (`use std.math.sqrt`) — the `(module, func)` pair.
    ModuleFn(String, String),
    /// An unbound method handle (`Type.method` as a value, MH) — `(ty, method, associated)`.
    MethodHandle(String, String, bool),
    /// A bound method handle (`value.method`, EX.2b) — the captured receiver ships recursively.
    BoundMethod(Box<Wire>, String),
    /// A channel sender endpoint (isolates I.4c): the shared cross-thread channel it names. Shipping an
    /// endpoint into an isolate clones the `Arc`, so both isolates operate on one queue.
    Sender(Arc<ChannelCore>),
    /// A channel receiver endpoint (isolates I.4c).
    Receiver(Arc<ChannelCore>),
}

/// The `Module.shapes` index of `value`'s shape, found by pointer identity — every shaped value shares
/// the one interned `&'static Shape` per table entry, so a `ptr_eq` scan resolves the index. `None` if the
/// value has no shape or its shape is somehow not in the table (defensive).
fn shape_index(value: Value, shapes: &[&'static Shape]) -> Option<u32> {
    let shape = value.shape()?;
    shapes
        .iter()
        .position(|s| std::ptr::eq(*s, shape))
        .map(|i| i as u32)
}

/// Copy a value graph into a `Send` [`Wire`] on the source thread (isolates I.4b/I.4c). Value-type
/// (`Send`) payloads, top-level functions, and channel endpoints (shared cross-thread channels, I.4c)
/// are representable; any other non-`Send` payload is a bug (the checker's E0042 classifier keeps it
/// away from a boundary). `channels` resolves a `Sender`/`Receiver` id to its shared channel — a
/// `Local` (cooperative-only) channel cannot cross a thread, and returns the `"channel"` error so the
/// caller falls back to a cooperative task.
pub fn marshal(
    value: Value,
    shapes: &[&'static Shape],
    channels: &[Channel],
) -> Result<Wire, String> {
    // A channel endpoint ships the shared cross-thread channel (I.4c) by cloning its `Arc`.
    if let Some(id) = value.sender_id() {
        return match channels.get(id.index()) {
            Some(Channel::Shared(core)) => Ok(Wire::Sender(Arc::clone(core))),
            _ => Err("channel".to_string()),
        };
    }
    if let Some(id) = value.receiver_id() {
        return match channels.get(id.index()) {
            Some(Channel::Shared(core)) => Ok(Wire::Receiver(Arc::clone(core))),
            _ => Err("channel".to_string()),
        };
    }
    if value.is_unit() {
        return Ok(Wire::Unit);
    }
    if let Some(b) = value.as_bool() {
        return Ok(Wire::Bool(b));
    }
    if let Some(i) = value.as_int() {
        return Ok(Wire::Int(i));
    }
    if let Some(f) = value.as_f32() {
        return Ok(Wire::F32(f));
    }
    if let Some(f) = value.as_float() {
        return Ok(Wire::Float(f));
    }
    if let Some(s) = value.as_string() {
        return Ok(Wire::Str(s));
    }
    if let Some(b) = value.bytes_data() {
        return Ok(Wire::Bytes(b));
    }
    if let Some(items) = value.list_items() {
        return Ok(Wire::List(marshal_each(&items, shapes, channels)?));
    }
    if let Some(items) = value.tuple_items() {
        return Ok(Wire::Tuple(marshal_each(&items, shapes, channels)?));
    }
    if let Some(items) = value.set_items() {
        return Ok(Wire::Set(marshal_each(&items, shapes, channels)?));
    }
    if let Some(entries) = value.map_entries() {
        let mut out = Vec::with_capacity(entries.len());
        for (k, v) in entries {
            out.push((k, marshal(v, shapes, channels)?));
        }
        return Ok(Wire::Map(out));
    }
    if value.is_object() {
        let shape = shape_index(value, shapes).ok_or("unknown object shape")?;
        let fields = value.slots().unwrap_or_default();
        return Ok(Wire::Object {
            shape,
            fields: marshal_each(&fields, shapes, channels)?,
        });
    }
    if value.is_enum() {
        let shape = shape_index(value, shapes).ok_or("unknown enum shape")?;
        let data = value.enum_data().unwrap_or_default();
        return Ok(Wire::Enum {
            shape,
            data: marshal_each(&data, shapes, channels)?,
        });
    }
    // A top-level function (no captured upvalues) marshals to its prototype index; a closure with
    // captures is not shippable (it holds heap cells) — but the Send classifier already rejects
    // closures at a boundary, so this only fires for a marshalled global function.
    if let Some(proto) = value.as_closure() {
        if value.closure_upvalue_count() == 0 {
            return Ok(Wire::Function(proto));
        }
        return Err("closure with captures".to_string());
    }
    // Import-bound callables (prelude-redesign P2/MH) are plain `Send` data: a first-class builtin,
    // a native-module binding, a selectively-imported module function, or a method handle. Shipping
    // them keeps `use`-bound globals usable inside a real isolate body.
    if let Some(builtin) = value.as_native_fn() {
        return Ok(Wire::NativeFn(builtin));
    }
    if let Some(name) = value.native_module_name() {
        return Ok(Wire::NativeModule(name));
    }
    if let Some((module, func)) = value.module_fn_parts() {
        return Ok(Wire::ModuleFn(module, func));
    }
    if let Some((ty, method, associated)) = value.method_handle_parts() {
        return Ok(Wire::MethodHandle(ty, method, associated));
    }
    if let Some((recv, method)) = value.bound_method_parts() {
        return Ok(Wire::BoundMethod(
            Box::new(marshal(recv, shapes, channels)?),
            method,
        ));
    }
    Err(format!(
        "value of type `{}` is not shippable",
        value.type_name()
    ))
}

fn marshal_each(
    values: &[Value],
    shapes: &[&'static Shape],
    channels: &[Channel],
) -> Result<Vec<Wire>, String> {
    values
        .iter()
        .map(|&v| marshal(v, shapes, channels))
        .collect()
}

/// Rebuild a [`Wire`] into fresh heap objects on the current (worker) thread (isolates I.4b/I.4c),
/// using the worker's own interned `&'static Shape` table (indices match the source's — same `Module`). A
/// channel endpoint registers its shared [`ChannelCore`] into this VM's `channels` table and yields an
/// endpoint value indexing it, so both isolates share one queue. Every returned `Value` owns one
/// reference, exactly like a directly-constructed value.
pub fn rebuild(wire: &Wire, shapes: &[&'static Shape], channels: &mut Vec<Channel>) -> Value {
    match wire {
        Wire::Unit => Value::unit(),
        Wire::Bool(b) => Value::bool(*b),
        Wire::Int(i) => Value::int(*i),
        Wire::Float(f) => Value::float(*f),
        Wire::F32(f) => Value::f32(*f),
        Wire::Str(s) => Value::string(s),
        Wire::Bytes(b) => Value::bytes(b.clone()),
        Wire::List(items) => Value::list(rebuild_each(items, shapes, channels)),
        Wire::Tuple(items) => Value::tuple(rebuild_each(items, shapes, channels)),
        Wire::Set(items) => Value::set(rebuild_each(items, shapes, channels)),
        Wire::Map(entries) => {
            let map = entries
                .iter()
                .map(|(k, v)| (k.clone(), rebuild(v, shapes, channels)))
                .collect();
            Value::map(map)
        }
        Wire::Object { shape, fields } => Value::object(
            shapes[*shape as usize],
            rebuild_each(fields, shapes, channels),
        ),
        Wire::Enum { shape, data } => Value::enum_value(
            shapes[*shape as usize],
            rebuild_each(data, shapes, channels),
        ),
        Wire::Function(proto) => Value::closure(*proto, Vec::new()),
        Wire::NativeFn(builtin) => Value::native_fn(*builtin),
        Wire::NativeModule(name) => Value::native_module(name),
        Wire::ModuleFn(module, func) => Value::module_fn(module, func),
        Wire::MethodHandle(ty, method, associated) => Value::method_handle(ty, method, *associated),
        Wire::BoundMethod(recv, method) => {
            Value::bound_method(rebuild(recv, shapes, channels), method)
        }
        Wire::Sender(core) => {
            let id = ChannelId::from_index(channels.len());
            channels.push(Channel::Shared(Arc::clone(core)));
            Value::make_sender(id)
        }
        Wire::Receiver(core) => {
            let id = ChannelId::from_index(channels.len());
            channels.push(Channel::Shared(Arc::clone(core)));
            Value::make_receiver(id)
        }
    }
}

fn rebuild_each(
    wires: &[Wire],
    shapes: &[&'static Shape],
    channels: &mut Vec<Channel>,
) -> Vec<Value> {
    wires.iter().map(|w| rebuild(w, shapes, channels)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_object::ShapeKind;
    use noeta_value::apply_binary;

    fn eq(a: Value, b: Value) -> bool {
        apply_binary(noeta_ast::BinaryOp::Eq, a, b)
            .unwrap()
            .as_bool()
            .unwrap()
    }

    #[test]
    fn round_trips_a_nested_value_graph() {
        // A struct shape plus the built-in-free primitives/collections a `Send` graph is made of.
        let shapes = vec![noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "Point",
            vec!["x".into(), "y".into()],
        ))];
        // list[ (1, "two"), Point{3,4}, [true, 3.5] ] — tuples, strings, an object, a nested list, f64.
        let original = Value::list(vec![
            Value::tuple(vec![Value::int(1), Value::string("two")]),
            Value::object(shapes[0], vec![Value::int(3), Value::int(4)]),
            Value::list(vec![Value::bool(true), Value::float(3.5)]),
        ]);

        // Marshal to the Send wire form, then rebuild — round-trips structurally, and the rebuilt graph
        // is a distinct allocation (marshalling copies).
        let wire = marshal(original, &shapes, &[]).expect("a Send value graph marshals");
        let rebuilt = rebuild(&wire, &shapes, &mut Vec::new());
        assert_ne!(original.bits(), rebuilt.bits());
        assert!(
            eq(original, rebuilt),
            "the rebuilt graph equals the original"
        );
        assert_eq!(
            rebuilt.display(),
            "[(1, \"two\"), Point {x: 3, y: 4}, [true, 3.5]]"
        );

        original.release();
        rebuilt.release();
    }

    #[test]
    fn ships_a_shared_channel_endpoint() {
        // Isolates I.4c: a `Sender`/`Receiver` over a *shared* channel marshals to the cloned `Arc`, and
        // rebuilding registers it into the destination's channel table (both isolates share one queue).
        let core = ChannelCore::new(4);
        let src_channels = vec![Channel::Shared(Arc::clone(&core))];
        let tx = Value::make_sender(ChannelId::from_index(0));
        let wire = marshal(tx, &[], &src_channels).expect("a shared endpoint marshals");
        assert!(matches!(&wire, Wire::Sender(c) if Arc::ptr_eq(c, &core)));
        tx.release();

        let mut dst_channels: Vec<Channel> = Vec::new();
        let rebuilt = rebuild(&wire, &[], &mut dst_channels);
        assert_eq!(rebuilt.sender_id(), Some(ChannelId::from_index(0)));
        assert_eq!(dst_channels.len(), 1);
        assert!(matches!(&dst_channels[0], Channel::Shared(c) if Arc::ptr_eq(c, &core)));
        rebuilt.release();
    }

    #[test]
    fn rejects_a_non_shared_endpoint() {
        // A `Local` (cooperative-only) channel — or an unknown id — cannot cross a thread: the distinct
        // "channel" error lets the spawn site fall back to a cooperative task.
        let local = vec![Channel::Local {
            buffer: VecDeque::new(),
            capacity: 1,
            closed: false,
        }];
        let tx = Value::make_sender(ChannelId::from_index(0));
        assert_eq!(marshal(tx, &[], &local).unwrap_err(), "channel");
        tx.release();
        let rx = Value::make_receiver(ChannelId::from_index(5)); // out of range
        assert_eq!(marshal(rx, &[], &local).unwrap_err(), "channel");
        rx.release();
    }

    #[test]
    fn round_trips_f32_and_bytes_and_enum() {
        // f32 (distinct immediate), bytes, and an enum value (shape carries name + variant).
        let shapes = vec![noeta_object::intern_shape(Shape::enum_variant(
            "Color",
            "rgb",
            vec!["r".into()],
            false,
        ))];
        let original = Value::enum_value(shapes[0], vec![Value::int(255)]);
        let wire = marshal(original, &shapes, &[]).unwrap();
        let rebuilt = rebuild(&wire, &shapes, &mut Vec::new());
        assert!(eq(original, rebuilt));
        original.release();
        rebuilt.release();

        let f = Value::f32(1.5);
        assert!(matches!(marshal(f, &[], &[]).unwrap(), Wire::F32(x) if x == 1.5));
        let b = Value::bytes(vec![1, 2, 3]);
        let wb = marshal(b, &[], &[]).unwrap();
        let rb = rebuild(&wb, &[], &mut Vec::new());
        assert_eq!(rb.bytes_data(), Some(vec![1, 2, 3]));
        b.release();
        rb.release();
    }
}
