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

use std::cell::Cell;
use std::collections::BTreeMap;
use std::ptr;
use std::rc::Rc;

use lang_bytecode::Builtin;
use lang_object::Shape;
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
    /// A set, stored as its canonical (sorted, de-duplicated) element vector — so iteration,
    /// display, and equality are deterministic and identical to the tree-walker. It owns one
    /// reference to each element, freed like a list's.
    Set(Vec<Value>),
    Map(BTreeMap<String, Value>),
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
}

/// Allocate an object and return a NaN-boxed pointer [`Value`] owning one reference.
pub(crate) fn alloc(payload: Payload) -> Value {
    live_inc();
    let raw = Box::into_raw(Box::new(Obj {
        header: ObjHeader {
            refcount: 1,
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
    Value(Value::SIGN_BIT | Value::QNAN | (addr as u64 & Value::PTR_MASK))
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
    live_dec();
    match &boxed.payload {
        Payload::List(items)
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
        Payload::Str(_)
        | Payload::Int(_)
        | Payload::NativeModule(_)
        | Payload::NativeFn(_)
        | Payload::FileHandle(_) => {}
    }
    drop(boxed);
}

/// Drop one owned reference to a value a freed container held, freeing it at zero. The
/// child is a distinct object from the container being freed, so there is no aliasing.
fn release_child(value: Value) {
    if value.dec_ref() {
        value.free();
    }
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
        | Payload::Set(items)
        | Payload::Object { slots: items, .. }
        | Payload::Enum { data: items, .. } => items.iter().copied().for_each(&mut push),
        Payload::Map(entries) => entries.values().copied().for_each(&mut push),
        Payload::Closure { upvalues, .. } => upvalues.iter().copied().for_each(&mut push),
        Payload::Cell(inner) => push(*inner),
        Payload::Str(_)
        | Payload::Int(_)
        | Payload::NativeModule(_)
        | Payload::NativeFn(_)
        | Payload::FileHandle(_) => {}
    }
    out
}

/// Free an object's own allocation **without** releasing its children — the cycle collector
/// frees every white object in a cycle itself, so each child is reclaimed on its own pass.
pub(crate) fn free_shallow(value: Value) {
    // SAFETY: the collector proved `value` is unreachable garbage and frees it exactly once;
    // children are freed by their own `free_shallow`, so they are not released here.
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
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
