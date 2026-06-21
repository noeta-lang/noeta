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

use std::collections::BTreeMap;
use std::ptr;
use std::rc::Rc;

use lang_object::Shape;

use crate::Value;

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
/// index into the compiled module's proto table — M1.2 functions capture only globals
/// (read live from the global environment), so a closure needs no upvalue array yet; that
/// arrives with non-global capture in a later slice.
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
    Closure(u32),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Object { shape: Rc<Shape>, slots: Vec<Value> },
    Enum { shape: Rc<Shape>, data: Vec<Value> },
}

/// Allocate an object and return a NaN-boxed pointer [`Value`] owning one reference.
pub(crate) fn alloc(payload: Payload) -> Value {
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
    match &boxed.payload {
        Payload::List(items)
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
        Payload::Str(_) | Payload::Int(_) | Payload::Closure(_) => {}
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
        | Payload::Object { slots: items, .. }
        | Payload::Enum { data: items, .. } => items.iter().copied().for_each(&mut push),
        Payload::Map(entries) => entries.values().copied().for_each(&mut push),
        Payload::Str(_) | Payload::Int(_) | Payload::Closure(_) => {}
    }
    out
}

/// Free an object's own allocation **without** releasing its children — the cycle collector
/// frees every white object in a cycle itself, so each child is reclaimed on its own pass.
pub(crate) fn free_shallow(value: Value) {
    // SAFETY: the collector proved `value` is unreachable garbage and frees it exactly once;
    // children are freed by their own `free_shallow`, so they are not released here.
    let boxed = unsafe { Box::from_raw(obj_ptr(value)) };
    // Replace each child slot with an immediate so the `Vec`/`BTreeMap` drop does not touch
    // the (already independently freed) child objects — though dropping a `Value` is a no-op
    // regardless, this documents that ownership was surrendered.
    drop(boxed);
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
