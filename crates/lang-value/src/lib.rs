//! The M1 runtime value: a NaN-boxed 64-bit word.
//!
//! Every value is one `u64`. Doubles are stored as their own bit pattern; everything else
//! lives in the unused encoding space of a quiet NaN. The scheme (a refinement of the
//! classic Lox/Wren tagging):
//!
//! ```text
//!   float        : any bits where (bits & QNAN) != QNAN          (NaN is canonicalized)
//!   pointer      : SIGN | QNAN | addr48                          (heap object, refcounted)
//!   small int    : QNAN | INT_TAG | payload48 (sign-extended)    (immediate, ±2^47)
//!   unit/bool    : QNAN | tag                                    (immediate singletons)
//! ```
//!
//! `i64` magnitudes beyond the 48-bit immediate range are boxed on the heap so full i64
//! wrapping semantics survive — only storage differs, never arithmetic, which always runs
//! in `i64`. This is the representation the VM (`lang-vm`) operates on through the safe
//! API here; all `unsafe` is quarantined to the [`heap`] module.
//!
//! Why a separate value type from the M0 tree-walker's `Value` enum? An `Rc<T>` cannot
//! live in a NaN-box pointer slot, so the two backends keep different value models and are
//! only ever compared on observable output (`RunResult`), never on representation.

mod heap;
mod ops;

pub use heap::{
    CollectorMode, Color, collector_mode, live_count, live_objects, live_peak, reset_peak,
    set_collector_mode, take_candidates,
};
pub use ops::{OpError, apply_binary, apply_unary, compare_primitive, structural_compare};

use std::collections::BTreeMap;
use std::rc::Rc;

use lang_bytecode::Builtin;
use lang_object::Shape;
use lang_stdlib::FileHandle;

use heap::Payload;

/// A NaN-boxed runtime value (one 64-bit word). `Copy`: it is just an integer; ownership of
/// any heap object it points at is tracked by refcount, not by Rust's move semantics.
#[derive(Clone, Copy)]
pub struct Value(pub(crate) u64);

impl Value {
    // --- NaN-box layout constants ---
    /// Quiet-NaN prefix (exponent all ones + the two top mantissa bits). A word is a tagged
    /// (non-float) value iff all these bits are set.
    pub(crate) const QNAN: u64 = 0x7ffc_0000_0000_0000;
    /// Sign bit; set on pointers to distinguish them from immediate tagged values.
    pub(crate) const SIGN_BIT: u64 = 0x8000_0000_0000_0000;
    /// Low 48 bits — the heap address payload (canonical user-space pointers fit).
    pub(crate) const PTR_MASK: u64 = 0x0000_ffff_ffff_ffff;
    /// Discriminates an immediate small int from the unit/bool singletons (a free QNAN bit).
    const INT_TAG: u64 = 1 << 49;
    /// Low-bit tags for the immediate singletons.
    const TAG_UNIT: u64 = 0;
    const TAG_FALSE: u64 = 1;
    const TAG_TRUE: u64 = 2;
    /// Largest immediate small-int magnitude (48-bit signed payload).
    const INT_MIN: i64 = -(1 << 47);
    const INT_MAX: i64 = (1 << 47) - 1;

    // --- Constructors ---

    /// The unit value.
    pub fn unit() -> Value {
        Value(Self::QNAN | Self::TAG_UNIT)
    }

    pub fn bool(b: bool) -> Value {
        Value(Self::QNAN | if b { Self::TAG_TRUE } else { Self::TAG_FALSE })
    }

    /// A float. Any NaN is canonicalized to the standard quiet NaN so it can never collide
    /// with the tag space (canonical NaN has bit 50 clear; the tag prefix needs it set).
    pub fn float(f: f64) -> Value {
        if f.is_nan() {
            Value(0x7ff8_0000_0000_0000)
        } else {
            Value(f.to_bits())
        }
    }

    /// An integer: immediate when it fits the 48-bit range, boxed otherwise. Either way the
    /// value round-trips through [`Value::as_int`] as a full `i64`.
    pub fn int(i: i64) -> Value {
        if (Self::INT_MIN..=Self::INT_MAX).contains(&i) {
            Value(Self::QNAN | Self::INT_TAG | (i as u64 & Self::PTR_MASK))
        } else {
            heap::alloc(Payload::Int(i))
        }
    }

    /// A heap string (refcount 1).
    pub fn string(s: &str) -> Value {
        heap::alloc(Payload::Str(s.to_string()))
    }

    /// A heap closure (refcount 1) referencing function prototype `proto` in the module's
    /// proto table, capturing `upvalues` (the cells for enclosing-function locals it reads;
    /// empty for a top-level `fn`/closure). Ownership of one reference to each cell transfers
    /// in, like [`Value::list`]'s elements.
    pub fn closure(proto: u32, upvalues: Vec<Value>) -> Value {
        heap::alloc(Payload::Closure { proto, upvalues })
    }

    /// A heap cell (refcount 1) holding `inner` — the shared storage for a captured local.
    /// Ownership of one reference to `inner` transfers in (the cell releases it when freed).
    pub fn cell(inner: Value) -> Value {
        heap::alloc(Payload::Cell(inner))
    }

    /// Read the value held in a cell. The caller must have checked [`Value::is_cell`].
    pub fn cell_get(self) -> Value {
        heap::cell_get(self)
    }

    /// Overwrite a cell's contents (retain new, release old). The caller must have checked
    /// [`Value::is_cell`].
    pub fn cell_set(self, value: Value) {
        heap::cell_set(self, value);
    }

    /// Whether this is a heap cell (captured-local storage; never user-visible).
    pub fn is_cell(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Cell(_)))
    }

    /// A first-class prelude builtin value (`len`/`map`/`filter`/`sum`).
    pub fn native_fn(func: Builtin) -> Value {
        heap::alloc(Payload::NativeFn(func))
    }

    /// The builtin this value dispatches on, if it is a first-class prelude function.
    pub fn as_native_fn(self) -> Option<Builtin> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::NativeFn(func) => Some(*func),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The captured upvalue cell at `index` of a closure. The caller must have checked
    /// [`Value::as_closure`].
    pub fn closure_upvalue(self, index: usize) -> Value {
        heap::closure_upvalue(self, index)
    }

    /// How many upvalue cells this closure captured. The caller must have checked
    /// [`Value::as_closure`].
    pub fn closure_upvalue_count(self) -> usize {
        heap::closure_upvalue_count(self)
    }

    /// A heap list (refcount 1). The list takes ownership of one reference to each element,
    /// so the caller must have already retained any value it puts in `items` (and must not
    /// release it afterward); the list releases them when it is freed.
    pub fn list(items: Vec<Value>) -> Value {
        heap::alloc(Payload::List(items))
    }

    /// A heap set (refcount 1). `items` must already be in canonical form — sorted and
    /// de-duplicated — since the set type relies on that for deterministic iteration, display,
    /// and equality. Ownership of one reference to each element transfers in, like [`Value::list`].
    pub fn set(items: Vec<Value>) -> Value {
        heap::alloc(Payload::Set(items))
    }

    /// An `fs.open` file handle value (refcount 1). The handle owns only `String`s, so unlike a
    /// collection it takes no child-value references.
    pub fn file_handle(handle: FileHandle) -> Value {
        heap::alloc(Payload::FileHandle(handle))
    }

    /// Whether this is a file handle.
    pub fn is_file_handle(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::FileHandle(_)))
    }

    /// Read this file handle under a closure. The caller must have checked [`Value::is_file_handle`].
    pub fn with_file_handle<R>(self, f: impl FnOnce(&FileHandle) -> R) -> R {
        heap::with_file_handle(self, f)
    }

    /// Mutate this file handle under a closure (advance the cursor / buffer a write / close). The
    /// caller must have checked [`Value::is_file_handle`].
    pub fn with_file_handle_mut<R>(self, f: impl FnOnce(&mut FileHandle) -> R) -> R {
        heap::with_file_handle_mut(self, f)
    }

    /// A Ring 2 native module value (refcount 1), identified by its surface name (e.g. `"json"`).
    pub fn native_module(name: &str) -> Value {
        heap::alloc(Payload::NativeModule(name.to_string()))
    }

    /// The native module's surface name, if this is a native module value.
    pub fn native_module_name(self) -> Option<String> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::NativeModule(name) => Some(name.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A heap map (refcount 1), keyed by owned strings, iterating in sorted-key order. As
    /// with [`Value::list`], the map takes ownership of one reference to each value.
    pub fn map(entries: BTreeMap<String, Value>) -> Value {
        heap::alloc(Payload::Map(entries))
    }

    /// A heap object (refcount 1): a struct/class/opaque instance laying out `slots` in the
    /// `shape`'s field order. The object takes ownership of one reference to each slot value.
    pub fn object(shape: Rc<Shape>, slots: Vec<Value>) -> Value {
        heap::alloc(Payload::Object { shape, slots })
    }

    /// A heap enum value (refcount 1): a `(enum, variant)` instance carrying the variant's
    /// positional `data`. The value takes ownership of one reference to each data element.
    pub fn enum_value(shape: Rc<Shape>, data: Vec<Value>) -> Value {
        heap::alloc(Payload::Enum { shape, data })
    }

    // --- Classification ---

    fn is_float(self) -> bool {
        (self.0 & Self::QNAN) != Self::QNAN
    }

    pub fn is_pointer(self) -> bool {
        (self.0 & (Self::SIGN_BIT | Self::QNAN)) == (Self::SIGN_BIT | Self::QNAN)
    }

    fn is_small_int(self) -> bool {
        !self.is_float() && !self.is_pointer() && (self.0 & Self::INT_TAG) != 0
    }

    /// Whether this is the unit value.
    pub fn is_unit(self) -> bool {
        self.0 == Value::unit().0
    }

    /// The boolean payload, if this is `true`/`false`.
    pub fn as_bool(self) -> Option<bool> {
        if self.0 == Value::bool(true).0 {
            Some(true)
        } else if self.0 == Value::bool(false).0 {
            Some(false)
        } else {
            None
        }
    }

    /// The integer value, reading either an immediate small int or a boxed `i64`.
    pub fn as_int(self) -> Option<i64> {
        if self.is_small_int() {
            let p = self.0 & Self::PTR_MASK;
            // Sign-extend the 48-bit payload to a full i64.
            Some(((p << 16) as i64) >> 16)
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Int(i) => Some(*i),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The float value, if this is a float.
    pub fn as_float(self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    /// A clone of the string value, if this is a heap string.
    pub fn as_string(self) -> Option<String> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => Some(s.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The function-prototype index, if this is a closure.
    pub fn as_closure(self) -> Option<u32> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Closure { proto, .. } => Some(*proto),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Whether this is a heap list.
    pub fn is_list(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::List(_)))
    }

    /// Whether this is a heap map.
    pub fn is_map(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Map(_)))
    }

    /// Whether this is a heap set.
    pub fn is_set(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Set(_)))
    }

    /// The number of elements, if this is a set.
    pub fn set_len(self) -> Option<usize> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Set(items) => Some(items.len()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A shallow copy of a set's canonical (sorted, de-duplicated) elements, if this is a set.
    /// As with [`Value::list_items`], the copied values share the set's references and are not
    /// retained.
    pub fn set_items(self) -> Option<Vec<Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Set(items) => Some(items.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The first (smallest) element of this canonically-ordered set, if it is a non-empty set —
    /// an O(1) peek used to check a candidate element's orderability against the set (a set is
    /// homogeneous in its orderability class, so comparing against the first element suffices)
    /// before a binary-search insert/remove, without cloning the whole buffer.
    pub fn set_first(self) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Set(items) => items.first().copied(),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Binary-search-insert `value` into this canonically-ordered set's backing buffer **in place**,
    /// keeping it sorted and de-duplicated. Returns `true` if `value` was newly inserted (the set
    /// took ownership, so the caller must transfer a reference), `false` if an equal element was
    /// already present (a no-op; the caller still owns `value`). The caller must guarantee a
    /// uniquely-owned set (`refcount == 1`) and that `value` is orderable against the set's elements
    /// (see [`Value::set_first`]) — the copy-on-write `set.add(x)` fast path, mutating the existing
    /// buffer (O(n) shift, O(log n) compares) instead of cloning + re-sorting. Returns `false` if
    /// not a set.
    pub fn set_insert_sorted(self, value: Value) -> bool {
        debug_assert!(
            !self.is_set() || heap::refcount(self) == 1,
            "set_insert_sorted requires a uniquely-owned set (the COW invariant)"
        );
        if self.is_set() {
            heap::with_payload_mut(self, |p| match p {
                Payload::Set(items) => match items.binary_search_by(|&item| {
                    compare_primitive(item, value).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    Ok(_) => false,
                    Err(pos) => {
                        items.insert(pos, value);
                        true
                    }
                },
                _ => false,
            })
        } else {
            false
        }
    }

    /// Binary-search-remove an element equal to `target` from this canonical set's backing buffer
    /// **in place**, returning the removed value (whose reference is handed back to the caller to
    /// release) or `None` if no equal element was present (a no-op). Same uniqueness + orderability
    /// contract as [`Value::set_insert_sorted`]; the copy-on-write `set.remove(x)` fast path.
    pub fn set_remove_sorted(self, target: Value) -> Option<Value> {
        debug_assert!(
            !self.is_set() || heap::refcount(self) == 1,
            "set_remove_sorted requires a uniquely-owned set (the COW invariant)"
        );
        if self.is_set() {
            heap::with_payload_mut(self, |p| match p {
                Payload::Set(items) => match items.binary_search_by(|&item| {
                    compare_primitive(item, target).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    Ok(pos) => Some(items.remove(pos)),
                    Err(_) => None,
                },
                _ => None,
            })
        } else {
            None
        }
    }

    /// The number of elements, if this is a list.
    pub fn list_len(self) -> Option<usize> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::List(items) => Some(items.len()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The number of entries, if this is a map.
    pub fn map_len(self) -> Option<usize> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => Some(entries.len()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The value for `key`, if this is a map containing that key. The returned value shares
    /// the map's reference (it is *not* retained); the caller must retain it before storing it
    /// as an independent owner.
    pub fn map_get(self, key: &str) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => entries.get(key).copied(),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The element at `index`, if this is a list and the index is in bounds. The returned
    /// value shares the list's reference (it is *not* retained); the caller must retain it
    /// before storing it as an independent owner.
    pub fn list_get(self, index: usize) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::List(items) => items.get(index).copied(),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Append `other`'s elements to this list's backing buffer **in place**, retaining each (the
    /// list takes ownership of one reference per appended element). The caller must guarantee this
    /// is a uniquely-owned list (`refcount == 1`) — this is the copy-on-write append fast path, so
    /// mutating the shared buffer is sound only when no other owner can observe it. `other` is
    /// borrowed (untouched). No-op if either value is not a list.
    pub fn list_extend(self, other: Value) {
        debug_assert!(
            self.is_list() && heap::refcount(self) == 1,
            "list_extend requires a uniquely-owned list (the COW invariant)"
        );
        if let Some(others) = other.list_items() {
            heap::with_payload_mut(self, |p| {
                if let Payload::List(items) = p {
                    items.reserve(others.len());
                    for o in others {
                        o.inc_ref();
                        items.push(o);
                    }
                }
            });
        }
    }

    /// Overwrite list slot `index` **in place** with `value`, returning the displaced value (whose
    /// reference is handed back to the caller to release). The caller must guarantee a uniquely-owned
    /// list (`refcount == 1`) and an in-range `index` — the copy-on-write `xs[i] = v` fast path:
    /// overwriting one slot of the existing buffer is O(1), versus cloning the whole list. Returns
    /// `unit` (a no-op) if this is not a list or `index` is out of range.
    pub fn list_replace_slot(self, index: usize, value: Value) -> Value {
        debug_assert!(
            !self.is_list() || heap::refcount(self) == 1,
            "list_replace_slot requires a uniquely-owned list (the COW invariant)"
        );
        if self.is_pointer() {
            heap::with_payload_mut(self, |p| match p {
                Payload::List(items) if index < items.len() => {
                    std::mem::replace(&mut items[index], value)
                }
                _ => Value::unit(),
            })
        } else {
            Value::unit()
        }
    }

    /// A shallow copy of a list's elements, if this is a list. The copied values share the
    /// list's references (they are *not* retained); the caller decides whether to retain.
    pub fn list_items(self) -> Option<Vec<Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::List(items) => Some(items.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A map's values in sorted-key order, if this is a map. As with [`Value::list_items`],
    /// the copied values share the map's references and are not retained.
    pub fn map_values(self) -> Option<Vec<Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => Some(entries.values().copied().collect()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A map's keys in sorted order, if this is a map. Keys are plain owned strings (not heap
    /// values), so no refcounting is involved.
    pub fn map_keys(self) -> Option<Vec<String>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => Some(entries.keys().cloned().collect()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Insert `key → value` into this map's backing buffer **in place**, returning the displaced
    /// value (if `key` was already present). The caller must guarantee a uniquely-owned map
    /// (`refcount == 1`) — this is the copy-on-write map-update fast path, so mutating the shared
    /// buffer is sound only when no other owner can observe it. The map takes ownership of `value`
    /// (the caller transfers a reference); the returned displaced value's reference is handed back to
    /// the caller to release. Returns `None` (a no-op) if this is not a map.
    pub fn map_insert(self, key: String, value: Value) -> Option<Value> {
        debug_assert!(
            !self.is_map() || heap::refcount(self) == 1,
            "map_insert requires a uniquely-owned map (the COW invariant)"
        );
        if self.is_map() {
            heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.insert(key, value),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Remove `key` from this map's backing buffer **in place**, returning the removed value (if
    /// present). Same uniqueness requirement and reference-handback contract as [`Value::map_insert`].
    pub fn map_remove(self, key: &str) -> Option<Value> {
        debug_assert!(
            !self.is_map() || heap::refcount(self) == 1,
            "map_remove requires a uniquely-owned map (the COW invariant)"
        );
        if self.is_map() {
            heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.remove(key),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A shallow clone of a map's `key → value` entries, if this is a map. As with
    /// [`Value::map_values`], the copied values **share** the map's references and are *not*
    /// retained; the caller decides whether to retain (e.g. when building a derived map with
    /// [`Value::map`]).
    pub fn map_entries(self) -> Option<BTreeMap<String, Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => Some(entries.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Whether this is a shaped object (struct/class/opaque instance).
    pub fn is_object(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Object { .. }))
    }

    /// Whether this is an enum value.
    pub fn is_enum(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Enum { .. }))
    }

    /// A clone of this value's shape handle, if it is an object or enum.
    pub fn shape(self) -> Option<Rc<Shape>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { shape, .. } | Payload::Enum { shape, .. } => Some(shape.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The object's shape **identity** as a raw pointer, without bumping the `Rc` refcount — the
    /// cheap key for an inline-cache hit test (`shape_ptr() == Some(Rc::as_ptr(&cached))`). The
    /// pointer is only valid while a live reference to the shape exists; the VM's cache holds an
    /// `Rc<Shape>` clone to keep the cached shape alive, so a hit comparison can never alias a freed
    /// shape. `None` for a non-object (an enum or a scalar).
    pub fn object_shape_ptr(self) -> Option<*const Shape> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { shape, .. } => Some(Rc::as_ptr(shape)),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The value in object slot `index` (shape order), if this is an object with that slot. Like
    /// [`Value::field`] the returned value shares the object's reference (not retained). Lets a
    /// resolved/cached slot index be read directly, skipping the `slot_of` field-name scan.
    pub fn slot_at(self, index: usize) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { slots, .. } => slots.get(index).copied(),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The value of object field `name`, if this is an object with that field. The returned
    /// value shares the object's reference (it is *not* retained); the caller must retain it
    /// before storing it as an independent owner.
    pub fn field(self, name: &str) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { shape, slots } => shape.slot_of(name).map(|i| slots[i]),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The object's slot values in shape order, if this is an object. Shares references.
    pub fn slots(self) -> Option<Vec<Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { slots, .. } => Some(slots.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The enum variant's positional data, if this is an enum value. Shares references.
    pub fn enum_data(self) -> Option<Vec<Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Enum { data, .. } => Some(data.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    // --- Display (mirrors the M0 tree-walker's `Value::display`) ---

    /// The display form used by `echo` and `~` concatenation.
    pub fn display(self) -> String {
        if let Some(b) = self.as_bool() {
            b.to_string()
        } else if self.is_small_int() {
            self.as_int().unwrap().to_string()
        } else if self.is_float() {
            format_float(self.as_float().unwrap())
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => s.clone(),
                Payload::Int(i) => i.to_string(),
                // Mirrors the M0 tree-walker's `Value::Function(_) => "<fn>"` (and `Builtin`).
                Payload::Closure { .. } | Payload::NativeFn(_) => "<fn>".to_string(),
                // A cell is internal capture storage and never reaches a display site (the
                // compiler derefs it first); render transparently as its contents if it ever does.
                Payload::Cell(inner) => inner.display(),
                // Collections render their elements with `repr` (strings quoted), exactly
                // like the M0 tree-walker's `Value::List`/`Value::Map` display.
                Payload::List(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                    format!("[{}]", parts.join(", "))
                }
                // A set renders with braces and no key colons (`{1, 2, 3}`), distinguishing it
                // from a non-empty map; an empty set is `{}`, like an empty map.
                Payload::Set(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                    format!("{{{}}}", parts.join(", "))
                }
                Payload::Map(entries) => {
                    let parts: Vec<String> = entries
                        .iter()
                        .map(|(k, v)| format!("{k:?}: {}", v.repr()))
                        .collect();
                    format!("{{{}}}", parts.join(", "))
                }
                // `Type {field: repr, ...}` in slot (declared) order — M0's `ObjectValue`.
                Payload::Object { shape, slots } => {
                    let parts: Vec<String> = shape
                        .fields
                        .iter()
                        .zip(slots)
                        .map(|(name, v)| format!("{name}: {}", v.repr()))
                        .collect();
                    format!("{} {{{}}}", shape.name, parts.join(", "))
                }
                // `Ok(x)`/`none` for built-in Result/Option, else `Type.Variant(data...)`;
                // a no-data variant is just the head. Data renders with `display` (unquoted),
                // matching M0's `EnumValue::display`.
                Payload::Enum { shape, data } => {
                    let head = if shape.builtin_result_option {
                        shape.variant.clone().unwrap_or_default()
                    } else {
                        format!(
                            "{}.{}",
                            shape.name,
                            shape.variant.clone().unwrap_or_default()
                        )
                    };
                    if data.is_empty() {
                        head
                    } else {
                        let parts: Vec<String> = data.iter().map(|v| v.display()).collect();
                        format!("{head}({})", parts.join(", "))
                    }
                }
                Payload::NativeModule(name) => format!("<module {name}>"),
                // `<file "path" (mode)>`, rendered by the shared handle so both backends match.
                Payload::FileHandle(handle) => handle.display(),
            })
        } else {
            // The unit value (and any other singleton) displays as empty, as in M0.
            String::new()
        }
    }

    /// The JSON encoding synthesized by `@derive(ToJson)`, mirrored exactly by the tree-walker.
    /// Scalars reuse `display` (so the two backends format numbers identically); strings are
    /// quoted and escaped via [`json_string`]; lists become JSON arrays, maps and objects JSON
    /// objects (objects in declared slot order). The unit value is `null`; a value with no JSON
    /// analog (closure/enum) falls back to its quoted display form.
    pub fn to_json(self) -> String {
        if let Some(b) = self.as_bool() {
            b.to_string()
        } else if self.is_small_int() {
            self.as_int().unwrap().to_string()
        } else if self.is_float() {
            format_float(self.as_float().unwrap())
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => json_string(s),
                Payload::Int(i) => i.to_string(),
                Payload::List(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.to_json()).collect();
                    format!("[{}]", parts.join(","))
                }
                // A set serializes as a JSON array (JSON has no set type).
                Payload::Set(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.to_json()).collect();
                    format!("[{}]", parts.join(","))
                }
                Payload::Map(entries) => {
                    let parts: Vec<String> = entries
                        .iter()
                        .map(|(k, v)| format!("{}:{}", json_string(k), v.to_json()))
                        .collect();
                    format!("{{{}}}", parts.join(","))
                }
                Payload::Object { shape, slots } => {
                    let parts: Vec<String> = shape
                        .fields
                        .iter()
                        .zip(slots)
                        .map(|(name, v)| format!("{}:{}", json_string(name), v.to_json()))
                        .collect();
                    format!("{{{}}}", parts.join(","))
                }
                Payload::Closure { .. } | Payload::NativeFn(_) => json_string("<fn>"),
                Payload::Cell(inner) => inner.to_json(),
                Payload::Enum { shape, .. } => {
                    json_string(shape.variant.as_deref().unwrap_or(&shape.name))
                }
                Payload::NativeModule(name) => json_string(&format!("<module {name}>")),
                // A handle has no JSON analog; fall back to its quoted display form, like a closure.
                Payload::FileHandle(handle) => json_string(&handle.display()),
            })
        } else {
            "null".to_string()
        }
    }

    /// The representation of a value *inside* a collection: strings are quoted so the
    /// structure stays legible (`["a", "b"]`, not `[a, b]`). Mirrors M0's `Value::repr`.
    pub fn repr(self) -> String {
        match self.as_string() {
            Some(s) => format!("{s:?}"),
            None => self.display(),
        }
    }

    /// The user-facing type name, for diagnostics (mirrors M0's `Value::type_name`).
    pub fn type_name(self) -> &'static str {
        if self.as_bool().is_some() {
            "bool"
        } else if self.as_int().is_some() {
            "int"
        } else if self.is_float() {
            "float"
        } else if self.is_pointer() {
            // Boxed ints were already caught by `as_int` above, so a pointer here is a
            // closure, list, map, or string. M0 names both user functions and builtins
            // "function".
            if self.as_closure().is_some() || self.as_native_fn().is_some() {
                "function"
            } else if self.is_list() {
                "list"
            } else if self.is_set() {
                "set"
            } else if self.is_map() {
                "map"
            } else if self.is_object() {
                "object"
            } else if self.is_enum() {
                "enum"
            } else if self.native_module_name().is_some() {
                "module"
            } else if self.is_file_handle() {
                "file handle"
            } else {
                "string"
            }
        } else {
            "unit"
        }
    }

    // --- Refcount management (the GC policy layer lives in `lang-gc`) ---

    /// The current reference count (0 for immediates, which are not refcounted). A count of 1
    /// means this is the last reference — the GC uses this to run a destructor on the
    /// about-to-be-final release.
    pub fn refcount(self) -> u32 {
        if self.is_pointer() {
            heap::refcount(self)
        } else {
            0
        }
    }

    /// Increment the refcount (no-op for immediates).
    pub fn inc_ref(self) {
        if self.is_pointer() {
            heap::inc_ref(self);
        }
    }

    /// Decrement the refcount; return `true` if it reached zero and the value should be
    /// [`free`](Value::free)d. No-op (`false`) for immediates.
    pub fn dec_ref(self) -> bool {
        if self.is_pointer() {
            heap::dec_ref(self)
        } else {
            false
        }
    }

    /// Free a heap value whose refcount has reached zero. Must only follow a `dec_ref`
    /// that returned `true`.
    pub fn free(self) {
        if self.is_pointer() {
            heap::free(self);
        }
    }

    /// The raw NaN-boxed word — a stable identity key for a value (two `Value`s are the same object
    /// iff their bits match). Used by the cycle collector to dedup frees by address without
    /// dereferencing (so a value already freed this collection is skipped, not read).
    pub fn bits(self) -> u64 {
        self.0
    }

    /// Drop one owning reference, reclaiming through the **active cycle collector** (Phase 6.4):
    /// a prompt refcount free in `Trace` mode, or the Bacon–Rajan `Decrement` (buffer a surviving
    /// cycle-capable root, defer a buffered object's dealloc) in `TrialDeletion` mode. This is the
    /// release the runtime should use; `dec_ref` + `free` is the lower-level pair the collector and
    /// the `Trace` path build on.
    pub fn release(self) {
        heap::release(self);
    }

    // --- Cycle-collector primitives (the trial-deletion collector lives in `lang-gc`) ---
    //
    // These expose the heap's per-object color/buffered flags, raw (non-freeing) refcount
    // edits, internal child enumeration, and a child-preserving free, so the collector can
    // trace the reference graph. They are no-ops/empty for immediates, which cannot cycle.

    /// This object's collector color (`Black` for immediates, which never cycle).
    pub fn gc_color(self) -> Color {
        if self.is_pointer() {
            heap::color(self)
        } else {
            Color::Black
        }
    }

    /// Set this object's collector color (no-op for immediates).
    pub fn gc_set_color(self, color: Color) {
        if self.is_pointer() {
            heap::set_color(self, color);
        }
    }

    /// Whether this object is in the collector's candidate-root buffer.
    pub fn gc_buffered(self) -> bool {
        self.is_pointer() && heap::buffered(self)
    }

    /// Mark/unmark this object as buffered (no-op for immediates).
    pub fn gc_set_buffered(self, buffered: bool) {
        if self.is_pointer() {
            heap::set_buffered(self, buffered);
        }
    }

    /// Raw refcount increment with no color logic (collector scan phase).
    pub fn gc_rc_inc(self) {
        if self.is_pointer() {
            heap::rc_inc(self);
        }
    }

    /// Raw refcount decrement that never frees (collector trial deletion).
    pub fn gc_rc_dec(self) {
        if self.is_pointer() {
            heap::rc_dec(self);
        }
    }

    /// The pointer-valued children this object references (empty for immediates and leaves).
    pub fn gc_children(self) -> Vec<Value> {
        if self.is_pointer() {
            heap::children(self)
        } else {
            Vec::new()
        }
    }

    /// Free this object's own allocation without releasing its children (the collector frees
    /// each cycle member itself). Must only be called by the collector on proven garbage.
    pub fn gc_free_shallow(self) {
        if self.is_pointer() {
            heap::free_shallow(self);
        }
    }

    /// Overwrite object slot `index` with `value` (retaining the new, releasing the old) — the
    /// heap mutation that lets references form cycles, and the basis for future field
    /// assignment. Panics if this is not an object.
    pub fn set_slot(self, index: usize, value: Value) {
        heap::set_slot(self, index, value);
    }

    /// Overwrite object slot `index` with `value` (retaining the new occupant) and return the
    /// displaced old value **without releasing it**, so the caller can run its destructor at the
    /// right time. Panics if this is not an object. See [`heap::replace_slot`].
    pub fn replace_slot(self, index: usize, value: Value) -> Value {
        heap::replace_slot(self, index, value)
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Show the logical value, not the raw word — but stay shallow and allocation-free
        // where possible.
        if let Some(b) = self.as_bool() {
            write!(f, "Bool({b})")
        } else if let Some(i) = self.as_int() {
            write!(f, "Int({i})")
        } else if let Some(x) = self.as_float() {
            write!(f, "Float({x})")
        } else if let Some(proto) = self.as_closure() {
            write!(f, "Closure(proto={proto})")
        } else if self.is_list() {
            write!(f, "List(len={})", self.list_len().unwrap())
        } else if self.is_map() {
            write!(f, "Map(len={})", self.map_len().unwrap())
        } else if self.is_object() || self.is_enum() {
            // Shallow: name the shape rather than recursing into slots.
            let shape = self.shape().unwrap();
            match &shape.variant {
                Some(variant) => write!(f, "Enum({}.{variant})", shape.name),
                None => write!(f, "Object({})", shape.name),
            }
        } else if self.is_pointer() {
            write!(f, "Str({:?})", self.as_string().unwrap_or_default())
        } else {
            write!(f, "Unit")
        }
    }
}

/// Render a float deterministically: whole-valued floats keep a trailing `.0` so they are
/// visibly distinct from ints (mirrors the M0 tree-walker exactly).
fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

/// Encode a string as a JSON string literal: surrounding quotes plus the mandatory escapes.
/// The tree-walker carries a byte-identical copy, so `@derive(ToJson)` produces the same output
/// under both backends.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_object::ShapeKind;
    use proptest::prelude::*;

    #[test]
    fn immediates_round_trip() {
        assert_eq!(Value::unit().display(), "");
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert_eq!(Value::int(42).as_int(), Some(42));
        assert_eq!(Value::int(-42).as_int(), Some(-42));
        assert_eq!(Value::float(2.5).as_float(), Some(2.5));
    }

    #[test]
    fn big_ints_box_and_keep_full_i64() {
        for i in [
            i64::MAX,
            i64::MIN,
            1 << 50,
            -(1 << 50),
            1 << 47,
            -(1 << 47) - 1,
        ] {
            let v = Value::int(i);
            assert_eq!(v.as_int(), Some(i), "round-trip {i}");
            // Each is boxed (outside the immediate range); free it so miri sees no leak.
            assert!(v.is_pointer(), "{i} should box");
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn small_int_boundaries_stay_immediate() {
        assert!(!Value::int(Value::INT_MAX).is_pointer());
        assert!(!Value::int(Value::INT_MIN).is_pointer());
        // Just outside the immediate range, integers box (and must be freed for miri).
        for i in [Value::INT_MAX + 1, Value::INT_MIN - 1] {
            let v = Value::int(i);
            assert!(v.is_pointer());
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn nan_is_canonicalized_and_classified_as_float() {
        let v = Value::float(f64::NAN);
        assert!(v.as_float().unwrap().is_nan());
        assert_eq!(v.type_name(), "float");
        assert!(!v.is_pointer());
    }

    #[test]
    fn strings_round_trip_and_free() {
        let v = Value::string("héllo");
        assert_eq!(v.as_string().as_deref(), Some("héllo"));
        assert_eq!(v.display(), "héllo");
        assert_eq!(v.type_name(), "string");
        assert!(v.dec_ref());
        v.free();
    }

    #[test]
    fn closures_round_trip_and_free() {
        let v = Value::closure(7, Vec::new());
        assert_eq!(v.as_closure(), Some(7));
        assert_eq!(v.type_name(), "function");
        assert_eq!(v.display(), "<fn>");
        // A closure is not an int/string/bool, so it never compares "equal" numerically.
        assert_eq!(v.as_int(), None);
        assert_eq!(v.as_string(), None);
        assert!(v.dec_ref());
        v.free();
    }

    #[test]
    fn lists_display_with_repr_and_free_their_elements() {
        // The list owns one reference to each element; building it from retained values and
        // then freeing it must release them (miri verifies no leak and no double-free).
        let a = Value::string("a");
        let items = vec![Value::int(1), a, Value::int(3)];
        let list = Value::list(items);
        assert_eq!(list.type_name(), "list");
        // Strings are quoted inside a collection; bare ints are not.
        assert_eq!(list.display(), "[1, \"a\", 3]");
        assert_eq!(list.list_len(), Some(3));
        assert!(list.dec_ref());
        list.free();
    }

    #[test]
    fn cells_box_and_update_their_contents() {
        let cell = Value::cell(Value::int(1));
        assert!(cell.is_cell());
        assert_eq!(cell.cell_get().as_int(), Some(1));
        // `cell_set` retains the new occupant for the cell and releases the old; the caller still
        // owns its own reference (as a VM register would), so release it here.
        let s = Value::string("two");
        cell.cell_set(s);
        if s.dec_ref() {
            s.free();
        }
        assert_eq!(cell.cell_get().as_string().as_deref(), Some("two"));
        assert!(cell.dec_ref());
        cell.free();
    }

    #[test]
    fn closure_owns_its_upvalue_cells() {
        let cell = Value::cell(Value::int(42));
        // The closure takes ownership of one reference to the cell.
        let closure = Value::closure(3, vec![cell]);
        assert_eq!(closure.as_closure(), Some(3));
        assert_eq!(closure.closure_upvalue_count(), 1);
        assert_eq!(closure.closure_upvalue(0).cell_get().as_int(), Some(42));
        // Freeing the closure releases its upvalue cell (and the int the cell held).
        assert!(closure.dec_ref());
        closure.free();
    }

    #[test]
    fn native_fn_values_round_trip_and_compare_by_builtin() {
        let len = Value::native_fn(Builtin::Len);
        let len2 = Value::native_fn(Builtin::Len);
        let map = Value::native_fn(Builtin::Map);
        assert_eq!(len.as_native_fn(), Some(Builtin::Len));
        assert_eq!(len.type_name(), "function");
        assert_eq!(len.display(), "<fn>");
        // Same builtin compares equal; different builtins do not (matches `Value::Builtin`).
        // `apply_binary` borrows its operands, so each value is freed explicitly below.
        assert!(
            crate::ops::apply_binary(lang_ast::BinaryOp::Eq, len, len2)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        assert!(
            !crate::ops::apply_binary(lang_ast::BinaryOp::Eq, len, map)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        for v in [len, len2, map] {
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn nested_lists_free_recursively() {
        let inner = Value::list(vec![Value::string("x"), Value::string("y")]);
        let outer = Value::list(vec![inner, Value::int(7)]);
        assert_eq!(outer.display(), "[[\"x\", \"y\"], 7]");
        assert!(outer.dec_ref());
        outer.free();
    }

    #[test]
    fn maps_iterate_in_sorted_key_order() {
        let mut entries = BTreeMap::new();
        entries.insert("b".to_string(), Value::int(2));
        entries.insert("a".to_string(), Value::string("v"));
        let map = Value::map(entries);
        assert_eq!(map.type_name(), "map");
        assert_eq!(map.display(), "{\"a\": \"v\", \"b\": 2}");
        assert_eq!(map.map_len(), Some(2));
        let values = map.map_values().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].as_string().as_deref(), Some("v"));
        assert_eq!(values[1].as_int(), Some(2));
        assert!(map.dec_ref());
        map.free();
    }

    #[test]
    fn empty_collections_display_distinctly() {
        let list = Value::list(vec![]);
        assert_eq!(list.display(), "[]");
        let map = Value::map(BTreeMap::new());
        assert_eq!(map.display(), "{}");
        assert!(list.dec_ref());
        list.free();
        assert!(map.dec_ref());
        map.free();
    }

    #[test]
    fn objects_display_in_slot_order_and_free_their_slots() {
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "Item",
            vec!["price".into(), "qty".into()],
        ));
        let obj = Value::object(shape.clone(), vec![Value::float(2.5), Value::int(4)]);
        assert_eq!(obj.type_name(), "object");
        assert_eq!(obj.display(), "Item {price: 2.5, qty: 4}");
        assert_eq!(obj.field("price").unwrap().as_float(), Some(2.5));
        assert!(obj.field("missing").is_none());
        // Same shape handle (the `Rc`) is shared, not copied per-instance.
        let obj2 = Value::object(shape.clone(), vec![Value::float(2.5), Value::int(4)]);
        assert!(Rc::ptr_eq(&obj.shape().unwrap(), &obj2.shape().unwrap()));
        // Structural equality (M0 parity): same type + equal fields.
        assert!(
            apply_binary(lang_ast::BinaryOp::Eq, obj, obj2)
                .unwrap()
                .as_bool()
                == Some(true)
        );
        for v in [obj, obj2] {
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn structural_compare_orders_objects_lexicographically() {
        use std::cmp::Ordering;
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "Version",
            vec!["major".into(), "minor".into()],
        ));
        let v19 = Value::object(shape.clone(), vec![Value::int(1), Value::int(9)]);
        let v20 = Value::object(shape.clone(), vec![Value::int(2), Value::int(0)]);
        let v19b = Value::object(shape.clone(), vec![Value::int(1), Value::int(9)]);
        // major dominates; equal major falls to minor; equal objects compare Equal.
        assert_eq!(structural_compare(v19, v20), Some(Ordering::Less));
        assert_eq!(structural_compare(v20, v19), Some(Ordering::Greater));
        assert_eq!(structural_compare(v19, v19b), Some(Ordering::Equal));
        // A primitive on one side is not an object pair: no defined order.
        assert_eq!(structural_compare(v19, Value::int(1)), None);
        assert_eq!(
            compare_primitive(Value::int(3), Value::int(5)),
            Some(Ordering::Less)
        );
        for v in [v19, v20, v19b] {
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn enum_values_display_and_compare() {
        let pending = Rc::new(Shape::enum_variant("Status", "Pending", vec![], false));
        let a = Value::enum_value(pending.clone(), vec![]);
        assert_eq!(a.type_name(), "enum");
        assert_eq!(a.display(), "Status.Pending");
        let b = Value::enum_value(pending.clone(), vec![]);
        assert!(
            apply_binary(lang_ast::BinaryOp::Eq, a, b)
                .unwrap()
                .as_bool()
                == Some(true)
        );

        // A built-in Result variant displays bare, with its data unquoted.
        let err = Rc::new(Shape::enum_variant("Result", "Err", vec!["0".into()], true));
        let e = Value::enum_value(err, vec![Value::string("boom")]);
        assert_eq!(e.display(), "Err(boom)");
        for v in [a, b, e] {
            assert!(v.dec_ref());
            v.free();
        }
    }

    #[test]
    fn live_count_tracks_alloc_and_free() {
        // The leak oracle's measuring stick: every allocation bumps the live count and every
        // reclamation drops it, so a build-then-free round trip returns to the starting value.
        let before = live_count();
        let s = Value::string("x");
        let list = Value::list(vec![Value::string("a"), Value::string("b")]);
        // string + (list + its two element strings) = 4 live objects.
        assert_eq!(live_count(), before + 4);
        assert!(s.dec_ref());
        s.free();
        assert!(list.dec_ref());
        list.free(); // frees the list and recursively its two elements
        assert_eq!(live_count(), before);
    }

    #[test]
    fn refcount_keeps_object_alive() {
        let v = Value::string("x");
        v.inc_ref(); // count 2
        assert!(!v.dec_ref()); // count 1, not freed
        assert_eq!(v.as_string().as_deref(), Some("x"));
        assert!(v.dec_ref()); // count 0
        v.free();
    }

    proptest! {
        // Disable on-disk failure persistence: its default backend calls `getcwd` to absolutize the
        // source path, which Miri's isolation forbids (so `cargo miri test` aborted here). Regression
        // seeds are a convenience we don't rely on, and dropping them lets these properties run under
        // Miri alongside the rest of the crate.
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn float_round_trips(bits in any::<u64>()) {
            let f = f64::from_bits(bits);
            let v = Value::float(f);
            if f.is_nan() {
                prop_assert!(v.as_float().unwrap().is_nan());
            } else {
                prop_assert_eq!(v.as_float(), Some(f));
            }
            prop_assert!(!v.is_pointer());
        }

        #[test]
        fn int_round_trips(i in any::<i64>()) {
            let v = Value::int(i);
            prop_assert_eq!(v.as_int(), Some(i));
            if v.dec_ref() { v.free(); }
        }
    }
}
