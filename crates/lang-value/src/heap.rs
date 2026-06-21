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
pub(crate) enum Payload {
    Str(String),
    Int(i64),
    Closure(u32),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// Allocate an object and return a NaN-boxed pointer [`Value`] owning one reference.
pub(crate) fn alloc(payload: Payload) -> Value {
    let raw = Box::into_raw(Box::new(Obj {
        header: ObjHeader { refcount: 1 },
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
        Payload::List(items) => {
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
