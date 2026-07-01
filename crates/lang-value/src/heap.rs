//! The heap: manually-allocated, manually-refcounted objects behind a NaN-boxed pointer.
//!
//! This is the **only** module in the workspace that uses `unsafe`. It is kept small and
//! pure on purpose so `miri` can cover it: every `unsafe` here is either the int↔pointer
//! round-trip (via the exposed-provenance API, which is the `miri`-sound way to stash a
//! pointer in an integer) or a deref of an object this module itself allocated.
//!
//! An [`Obj`] is `Box`-allocated, its raw address stashed in a [`Value`] by the codec in
//! `lib.rs`, and freed by reconstructing the `Box`. Refcounts are non-atomic — the runtime
//! is shared-nothing per isolate, so no value crosses a thread boundary.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::ptr;
use std::rc::Rc;

use lang_bytecode::Builtin;
use lang_object::{PackedSchema, Shape};
use lang_stdlib::FileHandle;

use crate::Value;

// --- Live-heap accounting (the leak oracle, architecture §0/§5) ---
//
// A per-isolate (thread-local) count of live [`Obj`] allocations: bumped in [`alloc`], dropped in
// every reclamation path ([`free`], [`free_shallow`]). The runtime is shared-nothing per isolate so
// a thread-local is exactly per-isolate. The counter is the measuring stick for the leak oracle —
// `live_count()` must return to its pre-run value once a program's globals and frames are released
// (residency 0 at clean exit). It is a single integer, so it is always on (release builds pay one
// non-atomic increment per allocation).

thread_local! {
    static LIVE: Cell<usize> = const { Cell::new(0) };
    /// High-water mark of [`LIVE`] since the last [`reset_peak`] — the peak-residency meter
    /// (architecture §0.3). Doubles the leak counter as a memory-footprint gauge: prompt last-use
    /// reclamation (Phase 3) should cut this materially vs the reclaim-at-teardown baseline.
    static PEAK: Cell<usize> = const { Cell::new(0) };
    /// The set of every live heap object on this thread, keyed by its NaN-boxed word — the
    /// **object registry** the Phase-6 backup mark-sweep collector ([`lang_gc`]) sweeps. Updated on
    /// every [`alloc`] (insert) and every free ([`free`]/[`free_shallow`], remove). A cycle escapes
    /// refcounting but never the registry, so a trace from the live roots can find and reclaim it.
    /// (Always-on, like [`LIVE`]; an intrusive object-list is the perf option Phase 6.4 weighs.)
    static REGISTRY: RefCell<HashSet<u64>> = RefCell::new(HashSet::new());
    /// Monotonic object-creation counter (object-model slice 2c) — stamps each allocation's
    /// `ObjHeader::seq` so the cycle collector can finalize in a deterministic age order.
    static NEXT_SEQ: Cell<u32> = const { Cell::new(0) };
}

/// The number of live heap objects on this isolate's thread. Zero at a clean program exit; the
/// leak oracle asserts the per-program delta is zero (a cycle leak or missed release shows up as a
/// positive residual).
pub fn live_count() -> usize {
    LIVE.with(|c| c.get())
}

/// The peak live-object count since the last [`reset_peak`] — the peak-residency metric.
pub fn live_peak() -> usize {
    PEAK.with(|c| c.get())
}

/// Reset the peak high-water mark to the current live count, so the next run's peak is measured in
/// isolation. Call before a measured run; read [`live_peak`] after.
pub fn reset_peak() {
    PEAK.with(|p| LIVE.with(|l| p.set(l.get())));
}

fn live_inc() {
    LIVE.with(|c| {
        let n = c.get() + 1;
        c.set(n);
        PEAK.with(|p| {
            if n > p.get() {
                p.set(n);
            }
        });
    });
}

fn live_dec() {
    LIVE.with(|c| c.set(c.get() - 1));
}

/// A heap object: a refcount header followed by its payload. `repr(C)` so the header is
/// always first, though we only ever reach the payload through the typed `Box`.
#[repr(C)]
pub(crate) struct Obj {
    header: ObjHeader,
    pub(crate) payload: Payload,
}

#[repr(C)]
struct ObjHeader {
    /// Non-atomic: per-isolate single-threaded ownership (architecture §5, §7).
    refcount: u32,
    /// A monotonic per-isolate **creation sequence** (object-model slice 2c): assigned at
    /// allocation, it gives every object a stable, deterministic age. The cycle collector finalizes
    /// reclaimed members in reverse-creation order (newest-first), matching the language's
    /// reverse-declaration teardown — so cyclic `destruct` order is deterministic and agrees with the
    /// tree-walker (the live-object registry is a `HashSet`, whose iteration order is otherwise
    /// arbitrary).
    seq: u32,
    /// The trial-deletion cycle collector's per-object color (architecture §5). `Black` in
    /// normal use; the other colors are transient bookkeeping during a `collect`.
    color: Color,
    /// Whether the object is currently in the collector's candidate-root buffer.
    buffered: bool,
}

/// The cycle collector's object colors (Bacon–Rajan synchronous trial deletion). `Black` =
/// in use; `Gray` = under trial deletion; `White` = provisionally garbage; `Purple` = a
/// possible cycle root (an object whose count was decremented without reaching zero).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Black,
    Gray,
    White,
    Purple,
}

/// Which cycle collector the release path feeds (Phase 6.4 — the two are benchmarked head to head).
/// `Trace` (the default) keeps the live-object [`REGISTRY`] up to date for the backup mark-sweep and
/// frees promptly on every reclamation. `TrialDeletion` instead **buffers candidate roots** on a
/// surviving decrement and **defers the deallocation** of a buffered object that reaches refcount 0
/// (Bacon–Rajan), so the candidate buffer never dangles; it pays nothing per allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CollectorMode {
    Trace,
    TrialDeletion,
}

thread_local! {
    /// The active collector for this isolate's release path. `Trace` by default (the proven Phase-6
    /// path); a benchmark / opt-in flips it to `TrialDeletion`.
    static MODE: Cell<CollectorMode> = const { Cell::new(CollectorMode::Trace) };
    /// The Bacon–Rajan candidate-root buffer (used only in `TrialDeletion` mode): objects whose
    /// refcount was decremented without reaching zero, hence possible cycle roots. Drained by
    /// `lang_gc`'s trial-deletion collector.
    static CANDIDATES: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

/// Select the cycle collector the release path feeds. Set once before a run (the VM does this from
/// its configured mode); switching mid-run is not supported (the two keep different invariants).
pub fn set_collector_mode(mode: CollectorMode) {
    MODE.with(|m| m.set(mode));
}

/// The active collector mode.
pub fn collector_mode() -> CollectorMode {
    MODE.with(|m| m.get())
}

/// The heap payloads. Strings are the heap string type; `Int` boxes an `i64` that does
/// not fit the 48-bit immediate small-int range, so full i64 wrapping semantics are kept
/// (the differential oracle checks `i64::MAX + 1`). `Closure` holds a function-prototype
/// index into the compiled module's proto table plus the captured upvalue cells (see the
/// variant doc); a top-level function captures only globals (read live) and so has none.
///
/// `List` and `Map` are the M1.3 heap collections. A collection **owns one reference to
/// each value it holds** (the list's elements, the map's values; map keys are plain owned
/// `String`s, not values). When the collection is freed those owned references are released
/// first (see [`free`]), so dropping a list of strings frees the strings too. `BTreeMap`
/// gives the map deterministic, sorted-key iteration, matching the M0 tree-walker exactly.
/// `Object` and `Enum` are the M1.4 shaped aggregates. Each pairs a shared [`Shape`] handle
/// (the layout — same shape for same-built aggregates, so identity is a cheap `Rc` pointer
/// comparison) with a flat slot array it **owns one reference to each of**. An `Object`'s slots
/// are its field values in the shape's declared order; an `Enum`'s slots are its variant's
/// positional data. Freeing either releases its slots first (see [`free`]).
pub(crate) enum Payload {
    Str(String),
    /// A raw immutable byte buffer (`bytes`, P-PACK 4.4) — a GC leaf like `Str`; owns no child
    /// references, freeing it just drops the `Vec<u8>`.
    Bytes(Vec<u8>),
    Int(i64),
    /// A function value: a prototype index plus the captured upvalue cells (empty for a
    /// top-level `fn`/closure, which captures only globals). The closure **owns one reference
    /// to each upvalue cell**, so it is a GC node — a closure capturing a cell that captures
    /// the closure forms a cycle the trial-deletion collector reclaims.
    Closure {
        proto: u32,
        upvalues: Vec<Value>,
    },
    /// A mutable single-slot box: the shared storage for a local that an inner closure
    /// captures. It **owns one reference** to the value it holds (released in [`free`], traced
    /// by [`children`]), so the defining frame and every capturing closure see one live binding.
    Cell(Value),
    List(Vec<Value>),
    /// A tuple: a fixed-arity, heterogeneous, value-semantic positional aggregate (object-model
    /// slice 4). Stored as its element vector, owning one reference to each element exactly like a
    /// list — equality is structural and there is no shape (arity/positions are static, checked at
    /// compile time, so no per-value metadata is needed).
    Tuple(Vec<Value>),
    /// A set, stored as its canonical (sorted, de-duplicated) element vector — so iteration,
    /// display, and equality are deterministic and identical to the tree-walker. It owns one
    /// reference to each element, freed like a list's.
    Set(Vec<Value>),
    Map(BTreeMap<String, Value>),
    /// A flat `List<packed>` (P-PACK 2.4, byte-addressed since 3.2b): the elements packed as raw
    /// primitive bytes, one contiguous `Vec<u8>` of `schema.byte_size` bytes per element (an `f32`
    /// field is 4 bytes, the others 8), interpreted through the shared `schema`. A GC **leaf** — it
    /// owns no child `Value`s (only primitive bytes), so freeing it just drops the buffer. Elements
    /// are materialized to/from `Payload::Object` on demand, so the layout is invisible to `RunResult`.
    PackedList {
        schema: Rc<PackedSchema>,
        bytes: Vec<u8>,
    },
    Object {
        shape: Rc<Shape>,
        slots: Vec<Value>,
    },
    Enum {
        shape: Rc<Shape>,
        data: Vec<Value>,
    },
    /// A Ring 2 native module (`use std.{json}`), identified by its surface name. A leaf with
    /// no child values; dispatched by `lang-vm` (which maps the name to the module).
    NativeModule(String),
    /// A first-class prelude builtin (`len`/`map`/`filter`/`sum`) used as a value. A leaf (the
    /// `Builtin` id is plain data); `lang-vm` dispatches it at an indirect call site.
    NativeFn(Builtin),
    /// An `fs.open` file handle (M2.5): a mutable cursor over a content snapshot (read) or a
    /// pending write buffer. The whole state machine lives in `lang_stdlib::FileHandle` so it is
    /// byte-identical to the tree-walker's. Holds no child `Value`s (only owned `String`s), so it
    /// is a GC leaf like `Str`.
    FileHandle(FileHandle),
    /// A lazy iterator (Track I): a reference-semantic pull cursor. The base (`iter()`) is a cursor
    /// over a list; adapters (`take`/`drop`/`chain`/…) wrap one or two **source** iterators and pull
    /// from them on demand, so a pipeline fuses with no intermediate list. It **owns one reference**
    /// to each source it holds (a GC node like [`Self::Cell`]); the cursor mutation through `next()`
    /// is shared by every alias, exactly like a file handle. See [`IterState`].
    Iter(IterState),
    /// An async future (Track A): a reference-semantic deferred computation. In A.1 it wraps a **lazy
    /// thunk** — a zero-argument closure that runs the `async fn` body and returns the completion value
    /// — which is not invoked until the future is awaited/run (Rust-style laziness). Owns **one
    /// reference** to that closure (a GC node like [`Self::Cell`]). A.2 replaces the thunk with the
    /// async state machine and this becomes a pollable future.
    Future(Value),
    /// A **leaf timer future** (Track A.2): `sleep(ms)` produces one, carrying the absolute logical
    /// deadline (ms) at which it becomes ready. It holds no heap children — the deadline is a plain
    /// integer — so it needs no `release`/`children` handling beyond freeing its own node. Polling it
    /// consults the injected [`lang_stdlib::SandboxExecutor`]'s clock; it reports `Pending` until the
    /// clock reaches the deadline. This is the first future that can actually suspend.
    Timer(u64),
    /// A **task handle** (Track A.3b): the `Future<T>` `spawn e` returns. It references a task by its
    /// `(scope index, task index)` position in the backend's concurrency-scope stack; polling it reads
    /// the task's stored result (ready) or reports pending. A GC leaf — the two indices are plain
    /// integers; the task's future/result are owned by the scope, not the handle.
    Handle(u32, u32),
    /// A **leaf async-read future** (Track A.4c): the `Future<string>` `fs.read_async(path)` returns.
    /// It carries an id ticketing the read in the injected [`lang_stdlib::Executor`] (the sandbox
    /// executor resolves it synchronously; the real executor spawns it on tokio and harvests it in
    /// `advance`). A GC leaf — the id is a plain integer; the pending read lives in the executor.
    AsyncIo(u64),
}

/// The state machine behind a [`Payload::Iter`] (Track I). The base case cursors a list; each adapter
/// holds the source iterator(s) it pulls from. `lang-eval` mirrors this enum over its own `Value`.
pub(crate) enum IterState {
    /// Cursor over a backing list — the base iterator from `iter()`.
    List { list: Value, cursor: usize },
    /// Yield at most `remaining` more elements from `source` (`take(n)`).
    Take { source: Value, remaining: usize },
    /// Skip `pending` elements from `source`, then yield the rest (`drop(n)`).
    Drop { source: Value, pending: usize },
    /// Yield all of `first`, then all of `second` (`chain(other)`).
    Chain { first: Value, second: Value },
    /// Yield `(index, element)` tuples from `source`, indexing from `index` (`enumerate()`).
    Enumerate { source: Value, index: usize },
    /// Yield `(a_elem, b_elem)` tuples, stopping when either runs dry (`zip(other)`).
    Zip { a: Value, b: Value },
    /// Yield `func(element)` for each element of `source` (`map(f)`, Track I.1c). Owns a reference to
    /// the closure `func` so it stays alive for the iterator's lifetime.
    Map { source: Value, func: Value },
    /// Yield the elements of `source` for which `pred(element)` is true (`filter(f)`, Track I.1c).
    Filter { source: Value, pred: Value },
    /// A generator (Track G): `step` is a closure (a state machine over `mut`-captured cells) called
    /// once per element with one resume argument, returning `?T` (`some(x)` → element, `none` → end).
    /// Owns one reference to the closure.
    Gen { step: Value },
}

/// A snapshot of an [`IterState`]'s shape, with its child [`Value`]s (Copy) and counters copied out.
/// Reading this under a *short* borrow lets the pull driver recurse into a source — or run a user
/// closure — with **no** borrow held on the node, so a re-entrant access to the same iterator cannot
/// alias the live `&mut` (which would be undefined behavior). See [`crate::Value::iter_next_apply`].
pub(crate) enum IterShape {
    List,
    Take { source: Value, remaining: usize },
    Drop { source: Value, pending: usize },
    Chain { first: Value, second: Value },
    Enumerate { source: Value, index: usize },
    Zip { a: Value, b: Value },
    Map { source: Value, func: Value },
    Filter { source: Value, pred: Value },
    Gen { step: Value },
}

impl IterState {
    /// The child iterator/list/closure values this state owns one reference to (for GC trace/free).
    fn children(&self) -> [Option<Value>; 2] {
        match self {
            IterState::List { list, .. } => [Some(*list), None],
            IterState::Take { source, .. }
            | IterState::Drop { source, .. }
            | IterState::Enumerate { source, .. } => [Some(*source), None],
            IterState::Chain { first, second } => [Some(*first), Some(*second)],
            IterState::Zip { a, b } => [Some(*a), Some(*b)],
            IterState::Map { source, func } => [Some(*source), Some(*func)],
            IterState::Filter { source, pred } => [Some(*source), Some(*pred)],
            IterState::Gen { step } => [Some(*step), None],
        }
    }

    /// Copy this state's shape out (child values + counters) so the caller can act without holding a
    /// borrow on the node. `List` carries no copy — it has no recursion or user code, so its cursor is
    /// advanced under its own short borrow.
    pub(crate) fn shape(&self) -> IterShape {
        match self {
            IterState::List { .. } => IterShape::List,
            IterState::Take { source, remaining } => IterShape::Take {
                source: *source,
                remaining: *remaining,
            },
            IterState::Drop { source, pending } => IterShape::Drop {
                source: *source,
                pending: *pending,
            },
            IterState::Chain { first, second } => IterShape::Chain {
                first: *first,
                second: *second,
            },
            IterState::Enumerate { source, index } => IterShape::Enumerate {
                source: *source,
                index: *index,
            },
            IterState::Zip { a, b } => IterShape::Zip { a: *a, b: *b },
            IterState::Map { source, func } => IterShape::Map {
                source: *source,
                func: *func,
            },
            IterState::Filter { source, pred } => IterShape::Filter {
                source: *source,
                pred: *pred,
            },
            IterState::Gen { step } => IterShape::Gen { step: *step },
        }
    }
}

/// Allocate an object and return a NaN-boxed pointer [`Value`] owning one reference.
pub(crate) fn alloc(payload: Payload) -> Value {
    live_inc();
    let seq = NEXT_SEQ.with(|c| {
        let s = c.get();
        c.set(s.wrapping_add(1));
        s
    });
    let raw = Box::into_raw(Box::new(Obj {
        header: ObjHeader {
            refcount: 1,
            seq,
            color: Color::Black,
            buffered: false,
        },
        payload,
    }));
    let addr = raw.expose_provenance();
    debug_assert!(
        addr & !Value::PTR_MASK as usize == 0,
        "heap address does not fit the 48-bit NaN-box payload"
    );
    let value = Value(Value::SIGN_BIT | Value::QNAN | (addr as u64 & Value::PTR_MASK));
    // The registry is the backup mark-sweep's sweep set; trial-deletion works from buffered
    // candidates instead, so it pays no per-allocation registry cost (the Phase-6.4 trade-off).
    if MODE.with(|m| m.get()) == CollectorMode::Trace {
        REGISTRY.with(|r| r.borrow_mut().insert(value.0));
    }
    value
}

/// Drop `value` from the live-object registry — called by every free path so the registry tracks
/// exactly the live heap. Separate from the `live_dec` counter so both stay in lock-step. A no-op
/// outside `Trace` mode (the registry is only maintained there).
fn registry_remove(value: Value) {
    if MODE.with(|m| m.get()) == CollectorMode::Trace {
        REGISTRY.with(|r| r.borrow_mut().remove(&value.0));
    }
}

/// A snapshot of every live heap object, for the backup mark-sweep collector. Reconstructs a
/// [`Value`] from each registered word (a pure bit-reinterpret; the provenance was exposed at
/// [`alloc`], so recovering the pointer later is sound). The borrow is released before the caller
/// sweeps, so freeing (which mutates the registry) cannot alias it.
pub fn live_objects() -> Vec<Value> {
    REGISTRY.with(|r| r.borrow().iter().map(|&w| Value(w)).collect())
}

/// Recover the typed pointer from a NaN-boxed pointer value. The caller must have checked
/// `value.is_pointer()`.
fn obj_ptr(value: Value) -> *mut Obj {
    let addr = (value.0 & Value::PTR_MASK) as usize;
    ptr::with_exposed_provenance_mut::<Obj>(addr)
}

/// Read the payload of a pointer value under a closure, so no reference outlives the
/// object. The caller must have checked `value.is_pointer()`.
pub(crate) fn with_payload<R>(value: Value, f: impl FnOnce(&Payload) -> R) -> R {
    // SAFETY: `value` is a pointer this module allocated and has not freed (refcount > 0),
    // so the object is live and the reference does not escape the closure.
    let obj = unsafe { &*obj_ptr(value) };
    f(&obj.payload)
}

/// Mutate the payload of a pointer value under a closure, so no reference outlives the object.
/// The caller must have checked `value.is_pointer()` **and** must hold the only owning reference
/// (refcount == 1) when the mutation observably changes the value — this is the in-place
/// copy-on-write path. Single-threaded, so the `&mut` is unaliased.
pub(crate) fn with_payload_mut<R>(value: Value, f: impl FnOnce(&mut Payload) -> R) -> R {
    // SAFETY: `value` is a live pointer this module allocated; single-threaded, and the caller
    // guarantees uniqueness for the COW case, so the `&mut` does not alias another live reference.
    let obj = unsafe { &mut *obj_ptr(value) };
    f(&mut obj.payload)
}

/// Read the current refcount of a pointer value. Used to detect the last reference (so a
/// destructor can run on the about-to-be-final release). The caller must have checked
/// `value.is_pointer()`.
pub(crate) fn refcount(value: Value) -> u32 {
    // SAFETY: live object allocated by this module; single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.refcount
}

/// The object's creation sequence (object-model slice 2c) — its allocation age, for the cycle
/// collector's deterministic reverse-creation finalization order.
pub(crate) fn seq(value: Value) -> u32 {
    // SAFETY: live object allocated by this module; single-threaded read.
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.seq
}

/// Increment the refcount of a pointer value. No-op enforced by the caller for immediates.
pub(crate) fn inc_ref(value: Value) {
    // SAFETY: live object allocated by this module; single-threaded so the read-modify-write
    // is not racy.
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.refcount += 1;
}

/// Decrement the refcount; return `true` when it reaches zero (the caller then [`free`]s).
pub(crate) fn dec_ref(value: Value) -> bool {
    // SAFETY: as `inc_ref`.
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.refcount -= 1;
    obj.header.refcount == 0
}

/// Free a pointer value whose refcount has reached zero, running the payload's destructor
/// (e.g. the `String`'s) by reconstructing and dropping the `Box`. A collection owns one
/// reference to each value it holds, so those are released first (which recursively frees
/// any that reach zero) before the container's own allocation is dropped.
pub(crate) fn free(value: Value) {
    // SAFETY: `value` is a pointer this module allocated, its refcount is zero (so no other
    // owner exists), and it is freed exactly once.
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
    registry_remove(value);
    live_dec();
    match &boxed.payload {
        Payload::List(items)
        | Payload::Tuple(items)
        | Payload::Set(items)
        | Payload::Object { slots: items, .. }
        | Payload::Enum { data: items, .. } => {
            for &element in items {
                release_child(element);
            }
        }
        Payload::Map(entries) => {
            for &element in entries.values() {
                release_child(element);
            }
        }
        Payload::Closure { upvalues, .. } => {
            for &cell in upvalues {
                release_child(cell);
            }
        }
        Payload::Cell(inner) => release_child(*inner),
        // An iterator owns one reference to each source it holds (a node like `Cell`).
        Payload::Iter(state) => {
            for child in state.children().into_iter().flatten() {
                release_child(child);
            }
        }
        // A future owns one reference to its thunk/step closure (a node like `Cell`).
        Payload::Future(step) => release_child(*step),
        // A packed list (P-PACK 2.4) owns only primitive words — no child references — so freeing it
        // just drops the buffer (and its shared `Rc<PackedSchema>`), like any other leaf.
        Payload::Str(_)
        | Payload::Bytes(_)
        | Payload::Int(_)
        | Payload::NativeModule(_)
        | Payload::NativeFn(_)
        | Payload::PackedList { .. }
        | Payload::Timer(_)
        | Payload::Handle(..)
        | Payload::AsyncIo(_)
        | Payload::FileHandle(_) => {}
    }
    drop(boxed);
}

/// Drop one owned reference to a value a freed container held, reclaiming it at zero. The child is a
/// distinct object from the container being freed, so there is no aliasing. Routes through the
/// mode-aware [`release`] so a freed container's children feed the active collector (buffering cycle
/// roots in `TrialDeletion` mode just as the top-level release does).
fn release_child(value: Value) {
    release(value);
}

/// Drop one owning reference to `value`, reclaiming it through the **active collector** (Phase 6.4).
/// In `Trace` mode this is the prompt refcount free; in `TrialDeletion` mode it is the Bacon–Rajan
/// `Decrement` — a surviving decrement buffers a possible cycle root, and an object reaching zero
/// releases its children immediately but **defers its own deallocation if it is buffered** (so the
/// candidate buffer never holds a freed pointer). A no-op for immediates.
pub(crate) fn release(value: Value) {
    if !value.is_pointer() {
        return;
    }
    match MODE.with(|m| m.get()) {
        CollectorMode::Trace => {
            if dec_ref(value) {
                free(value);
            }
        }
        CollectorMode::TrialDeletion => {
            if dec_ref(value) {
                // Bacon–Rajan `Release`: release children now (their own decrements may buffer them
                // as roots), then reclaim this object's allocation. `free_shallow` defers it if it is
                // buffered, so the buffer's reference stays valid until the collector frees it.
                for child in children(value) {
                    release(child);
                }
                free_shallow(value);
            } else if can_be_cyclic(value) {
                possible_root(value);
            }
        }
    }
}

/// Whether a value's type can participate in a cycle — i.e. it can hold references to other heap
/// objects. Only these are buffered as candidate roots; a leaf (string, boxed int, native handle)
/// can never close a cycle, so buffering it would be wasted work.
fn can_be_cyclic(value: Value) -> bool {
    if !value.is_pointer() {
        return false;
    }
    let obj = unsafe { &*obj_ptr(value) };
    matches!(
        obj.payload,
        Payload::List(_)
            | Payload::Tuple(_)
            | Payload::Set(_)
            | Payload::Map(_)
            | Payload::Object { .. }
            | Payload::Enum { .. }
            | Payload::Closure { .. }
            | Payload::Cell(_)
    )
}

/// Buffer `value` as a Bacon–Rajan possible cycle root (`PossibleRoot`): paint it purple and, if it
/// is not already buffered, record it for the next trial-deletion collection.
fn possible_root(value: Value) {
    if color(value) != Color::Purple {
        set_color(value, Color::Purple);
        if !buffered(value) {
            set_buffered(value, true);
            CANDIDATES.with(|c| c.borrow_mut().push(value));
        }
    }
}

/// Take the buffered candidate roots for a trial-deletion collection (drained, so the next round
/// starts empty). The borrow is released before the collector touches the objects.
pub fn take_candidates() -> Vec<Value> {
    CANDIDATES.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

// --- Cycle-collector primitives (used by `lang-gc`'s trial-deletion collector) ---
//
// The collector follows the heap's internal reference graph, trial-decrementing refcounts to
// discover objects kept alive only by a cycle. These expose the per-object color/buffered
// flags, raw refcount edits (no auto-free), child enumeration, and a child-preserving free.

/// Read an object's cycle-collector color.
pub(crate) fn color(value: Value) -> Color {
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.color
}

/// Set an object's cycle-collector color.
pub(crate) fn set_color(value: Value, color: Color) {
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.color = color;
}

/// Whether the object is in the collector's candidate-root buffer.
pub(crate) fn buffered(value: Value) -> bool {
    let obj = unsafe { &*obj_ptr(value) };
    obj.header.buffered
}

/// Mark/unmark the object as buffered in the candidate-root set.
pub(crate) fn set_buffered(value: Value, buffered: bool) {
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.buffered = buffered;
}

/// Raw refcount increment (no color logic). Used to restore counts during the collector's
/// scan phase.
pub(crate) fn rc_inc(value: Value) {
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.refcount += 1;
}

/// Raw refcount decrement that never frees (unlike [`dec_ref`]). Used for the collector's
/// trial deletion, which restores or reclaims separately.
pub(crate) fn rc_dec(value: Value) {
    let obj = unsafe { &mut *obj_ptr(value) };
    obj.header.refcount -= 1;
}

/// The pointer-valued children an object references (list/map/object/enum slots). Immediates
/// are excluded — they have no heap identity and cannot participate in a cycle.
pub(crate) fn children(value: Value) -> Vec<Value> {
    let obj = unsafe { &*obj_ptr(value) };
    let mut out = Vec::new();
    let mut push = |v: Value| {
        if v.is_pointer() {
            out.push(v);
        }
    };
    match &obj.payload {
        Payload::List(items)
        | Payload::Tuple(items)
        | Payload::Set(items)
        | Payload::Object { slots: items, .. }
        | Payload::Enum { data: items, .. } => items.iter().copied().for_each(&mut push),
        Payload::Map(entries) => entries.values().copied().for_each(&mut push),
        Payload::Closure { upvalues, .. } => upvalues.iter().copied().for_each(&mut push),
        Payload::Cell(inner) => push(*inner),
        // An iterator owns one reference to each source it holds.
        Payload::Iter(state) => state.children().into_iter().flatten().for_each(&mut push),
        // A future owns one reference to its thunk/step closure.
        Payload::Future(step) => push(*step),
        // A packed list holds only primitive words (no child references) — a GC leaf; a timer holds
        // only its integer deadline.
        Payload::Str(_)
        | Payload::Bytes(_)
        | Payload::Int(_)
        | Payload::NativeModule(_)
        | Payload::NativeFn(_)
        | Payload::PackedList { .. }
        | Payload::Timer(_)
        | Payload::Handle(..)
        | Payload::AsyncIo(_)
        | Payload::FileHandle(_) => {}
    }
    out
}

/// Free an object's own allocation **without** releasing its children — the cycle collector
/// frees every white object in a cycle itself, so each child is reclaimed on its own pass.
///
/// In `TrialDeletion` mode this is also the universal **deferral point**: a *buffered* object (one
/// the candidate buffer still references) is never freed here — it is painted black and left
/// allocated for the trial-deletion collector, which unbuffers it before reclaiming it. This makes
/// the deferral hold for *every* caller (the mode-aware release, and the VM's destructor-aware
/// `release_value`, which frees its last reference shallowly after running `__destruct`), so the
/// candidate buffer can never hold a dangling pointer regardless of which path reaches refcount 0.
pub(crate) fn free_shallow(value: Value) {
    if MODE.with(|m| m.get()) == CollectorMode::TrialDeletion && buffered(value) {
        set_color(value, Color::Black);
        return;
    }
    // SAFETY: the collector proved `value` is unreachable garbage and frees it exactly once;
    // children are freed by their own `free_shallow`, so they are not released here.
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
    registry_remove(value);
    live_dec();
    // Replace each child slot with an immediate so the `Vec`/`BTreeMap` drop does not touch
    // the (already independently freed) child objects — though dropping a `Value` is a no-op
    // regardless, this documents that ownership was surrendered.
    drop(boxed);
}

/// Read a file handle under a closure (it borrows the handle, so no reference escapes). The
/// caller must have checked the value is a `FileHandle`.
pub(crate) fn with_file_handle<R>(value: Value, f: impl FnOnce(&FileHandle) -> R) -> R {
    let obj = unsafe { &*obj_ptr(value) };
    let Payload::FileHandle(handle) = &obj.payload else {
        panic!("with_file_handle on a non-handle value");
    };
    f(handle)
}

/// Mutate a file handle under a closure — the cursor-advancing primitive for `read_line`/`read`/
/// `write`/`close`. Like [`set_slot`], this is single-threaded interior mutation of a heap object;
/// the handle holds no child `Value`s, so no retain/release bookkeeping is needed.
pub(crate) fn with_file_handle_mut<R>(value: Value, f: impl FnOnce(&mut FileHandle) -> R) -> R {
    let obj = unsafe { &mut *obj_ptr(value) };
    let Payload::FileHandle(handle) = &mut obj.payload else {
        panic!("with_file_handle_mut on a non-handle value");
    };
    f(handle)
}

/// Read the value held in a cell, returning a borrowed (not retained) copy. The caller must
/// have checked the value is a `Cell`.
pub(crate) fn cell_get(cell: Value) -> Value {
    let obj = unsafe { &*obj_ptr(cell) };
    let Payload::Cell(inner) = &obj.payload else {
        panic!("cell_get on a non-cell value");
    };
    *inner
}

/// Overwrite a cell's contents, retaining the new occupant and releasing the old (the cell
/// owns one reference to whatever it holds). The caller must have checked the value is a `Cell`.
pub(crate) fn cell_set(cell: Value, value: Value) {
    let obj = unsafe { &mut *obj_ptr(cell) };
    let Payload::Cell(inner) = &mut obj.payload else {
        panic!("cell_set on a non-cell value");
    };
    value.inc_ref();
    let old = std::mem::replace(inner, value);
    if old.dec_ref() {
        old.free();
    }
}

/// The captured upvalue cell at `index` of a closure, returning a borrowed (not retained)
/// copy. The caller must have checked the value is a `Closure`.
pub(crate) fn closure_upvalue(closure: Value, index: usize) -> Value {
    let obj = unsafe { &*obj_ptr(closure) };
    let Payload::Closure { upvalues, .. } = &obj.payload else {
        panic!("closure_upvalue on a non-closure value");
    };
    upvalues[index]
}

/// How many upvalue cells a closure captured. The caller must have checked it is a `Closure`.
pub(crate) fn closure_upvalue_count(closure: Value) -> usize {
    let obj = unsafe { &*obj_ptr(closure) };
    let Payload::Closure { upvalues, .. } = &obj.payload else {
        panic!("closure_upvalue_count on a non-closure value");
    };
    upvalues.len()
}

/// Overwrite object slot `index` with `value`, retaining the new occupant and releasing the
/// old. This is the heap mutation primitive that lets references form cycles (and the
/// foundation for field assignment in a later slice).
pub(crate) fn set_slot(object: Value, index: usize, value: Value) {
    let obj = unsafe { &mut *obj_ptr(object) };
    let Payload::Object { slots, .. } = &mut obj.payload else {
        panic!("set_slot on a non-object value");
    };
    value.inc_ref();
    let old = std::mem::replace(&mut slots[index], value);
    if old.dec_ref() {
        old.free();
    }
}

/// Overwrite object slot `index` with `value` (retaining the new occupant) and **return** the
/// displaced old value without releasing it — the caller owns it and decides its disposal (e.g. a
/// destructor-firing `release_value` rather than a plain free). The reference-count-neutral variant
/// of [`set_slot`], used by in-place struct reuse so a replaced field's `destruct` fires at the
/// right time (spec §4/§5).
pub(crate) fn replace_slot(object: Value, index: usize, value: Value) -> Value {
    let obj = unsafe { &mut *obj_ptr(object) };
    let Payload::Object { slots, .. } = &mut obj.payload else {
        panic!("replace_slot on a non-object value");
    };
    value.inc_ref();
    std::mem::replace(&mut slots[index], value)
}
