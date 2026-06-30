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
use lang_object::{PackedKind, PackedSchema, Shape};
use lang_stdlib::FileHandle;

use heap::{IterState, Payload};

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
    /// Discriminates an immediate `f32` (P-PACK Phase 3): a distinct free QNAN bit, one below
    /// `INT_TAG`. The 32 f32 bits live in the low 32 of the payload (bits 32–47 stay zero, and bit 49
    /// — `INT_TAG` — stays clear, so an `f32` is neither a small int nor a float/pointer/singleton).
    const F32_TAG: u64 = 1 << 48;
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

    /// A heap byte buffer (`bytes`, refcount 1), taking ownership of `data` (P-PACK 4.4).
    pub fn bytes(data: Vec<u8>) -> Value {
        heap::alloc(Payload::Bytes(data))
    }

    /// Whether this is a `bytes` value.
    pub fn is_bytes(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Bytes(_)))
    }

    /// A copy of this `bytes` value's buffer, or `None` if it is not a `bytes`.
    pub fn bytes_data(self) -> Option<Vec<u8>> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Bytes(b) => Some(b.clone()),
            _ => None,
        })
    }

    /// The length of this `bytes` value's buffer, or `None` if it is not a `bytes`.
    pub fn bytes_len(self) -> Option<usize> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Bytes(b) => Some(b.len()),
            _ => None,
        })
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

    /// A heap tuple (refcount 1) — a fixed-arity, value-semantic positional aggregate (object-model
    /// slice 4). Ownership of one reference to each element transfers in, exactly like [`Value::list`].
    pub fn tuple(items: Vec<Value>) -> Value {
        heap::alloc(Payload::Tuple(items))
    }

    /// A heap set (refcount 1). `items` must already be in canonical form — sorted and
    /// de-duplicated — since the set type relies on that for deterministic iteration, display,
    /// and equality. Ownership of one reference to each element transfers in, like [`Value::list`].
    pub fn set(items: Vec<Value>) -> Value {
        heap::alloc(Payload::Set(items))
    }

    /// A flat `List<packed>` value (refcount 1, P-PACK 2.4): `bytes` holds the elements packed as raw
    /// primitive bytes (`schema.byte_size` bytes each — an `f32` field is 4 bytes, P-PACK 3.2b),
    /// interpreted through `schema`. A leaf — it owns no child `Value`s (only primitive bytes), so
    /// freeing it just drops the buffer. The elements are materialized on demand (index, iterate,
    /// demote), so the layout is invisible to `RunResult`.
    pub fn packed_list(schema: Rc<PackedSchema>, bytes: Vec<u8>) -> Value {
        heap::alloc(Payload::PackedList { schema, bytes })
    }

    /// Whether this is a flat packed list (the `List<packed>` representation, P-PACK 2.4).
    pub fn is_packed_list(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::PackedList { .. }))
    }

    /// Pack this value (a value-struct instance) onto the end of `out` per `schema` — each primitive
    /// field as its little-endian bytes (`int`/`float`/`bool` 8, `f32` 4; P-PACK 3.2b), recursing into
    /// nested packed structs. Returns `false` on any shape mismatch (a non-object, wrong field count,
    /// or a field whose runtime kind disagrees) so the caller can fall back to a boxed list — the flat
    /// form is only ever used when exactly correct.
    pub fn pack_element(self, schema: &PackedSchema, out: &mut Vec<u8>) -> bool {
        heap::with_payload(self, |p| match p {
            Payload::Object { slots, .. } if slots.len() == schema.fields.len() => {
                for (kind, &slot) in schema.fields.iter().zip(slots.iter()) {
                    let ok = match kind {
                        PackedKind::Int => slot
                            .as_int()
                            .map(|i| out.extend_from_slice(&(i as u64).to_le_bytes()))
                            .is_some(),
                        PackedKind::Float => slot
                            .as_float()
                            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
                            .is_some(),
                        PackedKind::F32 => slot
                            .as_f32()
                            .map(|f| out.extend_from_slice(&f.to_bits().to_le_bytes()))
                            .is_some(),
                        PackedKind::Bool => slot.as_bool().map(|b| out.push(u8::from(b))).is_some(),
                        PackedKind::Struct(inner) => slot.pack_element(inner, out),
                    };
                    if !ok {
                        return false;
                    }
                }
                true
            }
            _ => false,
        })
    }

    /// Pack `element` (a value-struct instance) onto the end of this packed list's buffer **in
    /// place** (P-PACK 2.5 streaming construction). The caller must guarantee a uniquely-owned packed
    /// list (`refcount == 1`) — true for the streaming accumulator, which is never aliased. The
    /// element's primitives are *copied* into the buffer (not retained), so the caller still owns the
    /// element value and must release it. Returns `false` without modifying the buffer if `element`
    /// fails to pack (staged into a scratch vector first), so the caller can demote to a boxed list.
    #[must_use]
    pub fn packed_push(self, element: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1,
            "packed_push requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let mut staged = Vec::with_capacity(schema.byte_size);
                if element.pack_element(schema, &mut staged) {
                    bytes.extend(staged);
                    true
                } else {
                    false
                }
            }
            _ => false,
        })
    }

    /// Demote a list to an **owned** boxed list the caller must release: a packed list materializes
    /// into a fresh `Payload::List` of owned objects (refcount 1 each, owned by the list); an
    /// already-boxed list is returned with one extra reference. Either way the result is a boxed
    /// list value with an independent reference — so a generic list op can reuse the boxed code path
    /// on a packed list and then `release` the result, with no double-counting. The caller must have
    /// checked [`Value::is_list`].
    pub fn realize_list(self) -> Value {
        if self.is_packed_list() {
            Value::list(self.packed_items())
        } else {
            self.inc_ref();
            self
        }
    }

    /// Materialize the packed element at `index` into an owned `Value::Object` (refcount 1) — a
    /// single-element read with no full-list materialization. The caller owns the returned value.
    /// `index` must be in bounds (callers check via [`Value::list_len`]).
    pub fn packed_get(self, index: usize) -> Value {
        let (schema, elem) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let stride = schema.byte_size;
                let offset = index * stride;
                (Rc::clone(schema), bytes[offset..offset + stride].to_vec())
            }
            _ => unreachable!("packed_get on a non-packed list"),
        });
        unpack_element(&schema, &elem, 0).0
    }

    /// Read a single field of the packed element at `index` (P-PACK 2.5+ fused `list[i].field`),
    /// decoding only that field's word(s) — a primitive materializes directly, a nested packed struct
    /// is unpacked from its inline sub-range. Returns the owned field value (refcount 1), or `None`
    /// if `index` is out of range or `field` is not in the element schema (the checker only fuses
    /// real field reads on a packed type, so a hit is the norm; the caller falls back on `None`). No
    /// full-element materialization — this is the scalar-access win over `packed_get`.
    pub fn packed_field(self, index: usize, field: &str) -> Option<Value> {
        let (kind, slice) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let count = bytes.len() / schema.byte_size;
                if index >= count {
                    return None;
                }
                let slot = schema.shape.slot_of(field)?;
                // Field `slot`'s byte offset within the element is the sum of the prior fields' widths.
                let mut at = index * schema.byte_size;
                for kind in &schema.fields[..slot] {
                    at += kind.byte_width();
                }
                let width = schema.fields[slot].byte_width();
                Some((schema.fields[slot].clone(), bytes[at..at + width].to_vec()))
            }
            _ => None,
        })?;
        Some(decode_packed_field(&kind, &slice, 0))
    }

    /// The raw flat byte buffer of a packed list (`to_bytes`, P-PACK 4.4), regardless of element
    /// schema; `None` for a boxed list (which has no canonical serialized form).
    pub fn packed_bytes(self) -> Option<Vec<u8>> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { bytes, .. } => Some(bytes.clone()),
            _ => None,
        })
    }

    /// If this is a packed `List<Vec3<f32>>` — a flat buffer whose element schema is exactly three
    /// `f32` fields — return its shared schema and a copy of its byte buffer (the input to the bulk
    /// `vec` kernels, P-PACK 4.2). `None` for a boxed list or any other element schema, so the caller
    /// takes the scalar fallback.
    pub fn packed_vec3_data(self) -> Option<(Rc<PackedSchema>, Vec<u8>)> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes }
                if schema.fields.len() == 3
                    && schema.fields.iter().all(|k| matches!(k, PackedKind::F32)) =>
            {
                Some((Rc::clone(schema), bytes.clone()))
            }
            _ => None,
        })
    }

    /// Build a new flat packed list from selected element `indices` of this one, copying each
    /// selected element's word-block verbatim — no per-element materialization (P-PACK 2.6). The
    /// schema is shared (an `Rc` clone). This keeps a `List<packed>` *flat* through the selection
    /// producers (`reverse`/`slice`/`filter`) instead of demoting to N boxed objects. A packed list
    /// is a GC leaf, so the new buffer owns no child references; the caller owns the result (rc 1).
    /// Every index must be in range (callers validate against [`Value::list_len`]).
    pub fn packed_select(self, indices: &[usize]) -> Value {
        let (schema, buf) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let stride = schema.byte_size;
                let mut out = Vec::with_capacity(indices.len() * stride);
                for &i in indices {
                    out.extend_from_slice(&bytes[i * stride..i * stride + stride]);
                }
                (Rc::clone(schema), out)
            }
            _ => unreachable!("packed_select on a non-packed list"),
        });
        Value::packed_list(schema, buf)
    }

    /// Build a new flat packed list with element `index` replaced by `element` (P-PACK 2.6 flat
    /// `set`). The element's primitives are copied into a fresh buffer; the caller still owns
    /// `element`. Returns `None` (so the caller demotes) if `element` does not pack into the schema.
    pub fn packed_set(self, index: usize, element: Value) -> Option<Value> {
        let (schema, mut buf) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => (Rc::clone(schema), bytes.clone()),
            _ => unreachable!("packed_set on a non-packed list"),
        });
        let stride = schema.byte_size;
        let mut staged = Vec::with_capacity(stride);
        if !element.pack_element(&schema, &mut staged) {
            return None;
        }
        buf[index * stride..index * stride + stride].copy_from_slice(&staged);
        Some(Value::packed_list(schema, buf))
    }

    /// Overwrite element `index` of this packed list **in place** with `element` (P-PACK 2.6 reuse
    /// path for `acc = acc.set(i, v)`). The caller must guarantee a uniquely-owned packed list
    /// (`refcount == 1`). `element`'s primitives are copied into the buffer (no retain); the caller
    /// still owns `element`. Returns `false` (buffer untouched) if `element` does not pack, so the
    /// caller can fall back to the copy path.
    #[must_use]
    pub fn packed_set_in_place(self, index: usize, element: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1,
            "packed_set_in_place requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let stride = schema.byte_size;
                let mut staged = Vec::with_capacity(stride);
                if element.pack_element(schema, &mut staged) {
                    bytes[index * stride..index * stride + stride].copy_from_slice(&staged);
                    true
                } else {
                    false
                }
            }
            _ => false,
        })
    }

    /// Concatenate two packed lists of the **same layout** into a new flat packed list (P-PACK 2.6
    /// `a ~ b`), copying both word buffers. Returns `None` (so the caller demotes) unless both are
    /// packed and share an element shape. Both operands are borrowed (the caller still owns them).
    pub fn packed_concat(self, other: Value) -> Option<Value> {
        if !self.is_packed_list() || !other.is_packed_list() {
            return None;
        }
        let (schema, mut buf) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => (Rc::clone(schema), bytes.clone()),
            _ => unreachable!("packed_concat on a non-packed list"),
        });
        let other_bytes = heap::with_payload(other, |p| match p {
            Payload::PackedList {
                schema: s2,
                bytes: b2,
            } => Rc::ptr_eq(&schema.shape, &s2.shape).then(|| b2.clone()),
            _ => None,
        })?;
        buf.extend_from_slice(&other_bytes);
        Some(Value::packed_list(schema, buf))
    }

    /// Append `other`'s elements to this packed list **in place** (P-PACK 2.6 reuse path for
    /// `acc = acc ~ xs`). The caller must guarantee a uniquely-owned packed list (`refcount == 1`).
    /// `other` is borrowed (its words copied). Returns `false` (buffer untouched) unless `other` is a
    /// packed list of the same layout, so the caller can fall back to the copy path.
    #[must_use]
    pub fn packed_extend_in_place(self, other: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1,
            "packed_extend_in_place requires a uniquely-owned packed list"
        );
        if !other.is_packed_list() {
            return false;
        }
        let (other_schema, other_bytes) = heap::with_payload(other, |p| match p {
            Payload::PackedList { schema, bytes } => (Rc::clone(schema), bytes.clone()),
            _ => unreachable!("packed_extend_in_place on a non-packed list"),
        });
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes }
                if Rc::ptr_eq(&schema.shape, &other_schema.shape) =>
            {
                bytes.extend_from_slice(&other_bytes);
                true
            }
            _ => false,
        })
    }

    /// Materialize every packed element into an owned vector (each refcount 1). Used by
    /// [`Value::realize_list`]; the words are copied out before allocating so no heap borrow is held
    /// across element construction.
    fn packed_items(self) -> Vec<Value> {
        let (schema, bytes) = heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => (Rc::clone(schema), bytes.clone()),
            _ => unreachable!("packed_items on a non-packed list"),
        });
        let count = bytes.len() / schema.byte_size;
        let mut out = Vec::with_capacity(count);
        let mut at = 0;
        for _ in 0..count {
            let (value, next) = unpack_element(&schema, &bytes, at);
            out.push(value);
            at = next;
        }
        out
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

    /// A lazy iterator value (Track I.1a) cursoring over `list` from the start. The iterator owns one
    /// reference to its backing list (retained here); the caller's reference to `list` is untouched.
    pub fn iter(list: Value) -> Value {
        list.inc_ref();
        heap::alloc(Payload::Iter(IterState::List { list, cursor: 0 }))
    }

    /// A `take(n)` adapter: yields at most `n` elements from `source` (Track I.1b). The adapter owns
    /// one reference to `source` (retained here); the caller's reference to `source` is untouched.
    pub fn iter_take(source: Value, n: usize) -> Value {
        source.inc_ref();
        heap::alloc(Payload::Iter(IterState::Take {
            source,
            remaining: n,
        }))
    }

    /// A `drop(n)` adapter: skips the first `n` elements of `source`, yields the rest (Track I.1b).
    pub fn iter_drop(source: Value, n: usize) -> Value {
        source.inc_ref();
        heap::alloc(Payload::Iter(IterState::Drop { source, pending: n }))
    }

    /// A `chain(other)` adapter: yields all of `first`, then all of `second` (Track I.1b). Owns one
    /// reference to each.
    pub fn iter_chain(first: Value, second: Value) -> Value {
        first.inc_ref();
        second.inc_ref();
        heap::alloc(Payload::Iter(IterState::Chain { first, second }))
    }

    /// An `enumerate()` adapter: yields `(index, element)` tuples from `source`, indexing from 0
    /// (Track I.1b.2). Owns one reference to `source`.
    pub fn iter_enumerate(source: Value) -> Value {
        source.inc_ref();
        heap::alloc(Payload::Iter(IterState::Enumerate { source, index: 0 }))
    }

    /// A `zip(other)` adapter: yields `(a_elem, b_elem)` tuples, stopping at the shorter source
    /// (Track I.1b.2). Owns one reference to each source.
    pub fn iter_zip(a: Value, b: Value) -> Value {
        a.inc_ref();
        b.inc_ref();
        heap::alloc(Payload::Iter(IterState::Zip { a, b }))
    }

    /// Whether this is an iterator.
    pub fn is_iter(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Iter(_)))
    }

    /// Advance the iterator, returning the next element — a freshly-retained owning reference the
    /// caller takes ownership of — or `None` at end. The caller must have checked [`Value::is_iter`].
    ///
    /// An adapter pulls from its source(s) by calling `iter_next` on them recursively. Each source is
    /// a **distinct** heap object from `self` (an iterator can never be its own source — construction
    /// always wraps an existing one), so the recursive `with_payload_mut` accesses a different
    /// allocation than the one held here (no aliasing; miri-verified).
    pub fn iter_next(self) -> Option<Value> {
        heap::with_payload_mut(self, |p| {
            let Payload::Iter(state) = p else {
                return None;
            };
            match state {
                IterState::List { list, cursor } => {
                    let e = list.list_get(*cursor)?;
                    *cursor += 1;
                    // `list_get` shares the list's reference; retain it for the new owner.
                    e.inc_ref();
                    Some(e)
                }
                // The source's `iter_next` already retains the element it returns, so it is handed
                // straight back.
                IterState::Take { source, remaining } => {
                    if *remaining == 0 {
                        return None;
                    }
                    let e = source.iter_next()?;
                    *remaining -= 1;
                    Some(e)
                }
                IterState::Drop { source, pending } => {
                    while *pending > 0 {
                        match source.iter_next() {
                            Some(skipped) => {
                                skipped.release(); // drop the skipped element's retained reference
                                *pending -= 1;
                            }
                            None => {
                                *pending = 0;
                                return None;
                            }
                        }
                    }
                    source.iter_next()
                }
                IterState::Chain { first, second } => {
                    first.iter_next().or_else(|| second.iter_next())
                }
                // The source's element (already retained by its `iter_next`) and the immediate index
                // are handed to the new tuple, which takes ownership of one reference to each.
                IterState::Enumerate { source, index } => {
                    let e = source.iter_next()?;
                    let tuple = Value::tuple(vec![Value::int(*index as i64), e]);
                    *index += 1;
                    Some(tuple)
                }
                // Pull from both, shorter wins. If `a` ran dry there is nothing to release; if only
                // `b` did, release `a`'s already-retained element so it does not leak.
                IterState::Zip { a, b } => {
                    let ea = a.iter_next()?;
                    match b.iter_next() {
                        Some(eb) => Some(Value::tuple(vec![ea, eb])),
                        None => {
                            ea.release();
                            None
                        }
                    }
                }
            }
        })
    }

    /// Drain the iterator from its current cursor into a new list — each element retained into it
    /// (via [`Value::iter_next`]). The caller must have checked [`Value::is_iter`].
    pub fn iter_collect(self) -> Value {
        let mut out = Vec::new();
        while let Some(e) = self.iter_next() {
            out.push(e);
        }
        Value::list(out)
    }

    /// Drain the iterator, summing its numeric elements (Track I.1b.2) — `int` if every element is an
    /// `int`, else `float`. Mirrors the eager `sum` builtin's accumulation exactly so the two paths
    /// agree. Each drained element's retained reference is released; on the first non-numeric element
    /// it (and the partial state) is dropped and its type name returned as `Err` for the caller's
    /// diagnostic. The caller must have checked [`Value::is_iter`].
    pub fn iter_sum(self) -> Result<Value, &'static str> {
        let mut int_total: i64 = 0;
        let mut float_total: f64 = 0.0;
        let mut any_float = false;
        while let Some(e) = self.iter_next() {
            if let Some(i) = e.as_int() {
                int_total = int_total.wrapping_add(i);
            } else if let Some(f) = e.as_float() {
                any_float = true;
                float_total += f;
            } else {
                let name = e.type_name();
                e.release();
                return Err(name);
            }
            e.release();
        }
        Ok(if any_float {
            Value::float(float_total + int_total as f64)
        } else {
            Value::int(int_total)
        })
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

    /// A 32-bit float (P-PACK Phase 3) — an **immediate** value (no heap allocation, not refcounted),
    /// its 32 bits NaN-boxed under `F32_TAG`.
    pub fn f32(f: f32) -> Value {
        Value(Self::QNAN | Self::F32_TAG | u64::from(f.to_bits()))
    }

    /// Whether this is an immediate `f32` value.
    pub fn is_f32(self) -> bool {
        !self.is_float() && !self.is_pointer() && (self.0 & Self::F32_TAG) != 0
    }

    /// The `f32` value, if this is one.
    pub fn as_f32(self) -> Option<f32> {
        if self.is_f32() {
            Some(f32::from_bits((self.0 & 0xffff_ffff) as u32))
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

    /// Whether this is a heap list — boxed (`Payload::List`) or flat-packed (`Payload::PackedList`,
    /// P-PACK 2.4). Both are observably lists; a packed one materializes through
    /// [`Value::realize_list`] / [`Value::packed_get`] for any op not specialized for the flat form.
    pub fn is_list(self) -> bool {
        self.is_pointer()
            && heap::with_payload(self, |p| {
                matches!(p, Payload::List(_) | Payload::PackedList { .. })
            })
    }

    /// Whether this is a heap tuple.
    pub fn is_tuple(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Tuple(_)))
    }

    /// The element at positional index `i`, if this is a tuple and `i` is in bounds. Returns a copy
    /// of the `Value` (a NaN-boxed word); the caller retains it if it keeps it.
    pub fn tuple_field(self, i: usize) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Tuple(items) => items.get(i).copied(),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A clone of this tuple's elements, if it is a tuple.
    pub fn tuple_items(self) -> Option<Vec<Value>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Tuple(items) => Some(items.clone()),
                _ => None,
            })
        } else {
            None
        }
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
                // A packed list's length is its byte count divided by the per-element stride — O(1),
                // no materialization.
                Payload::PackedList { schema, bytes } => Some(bytes.len() / schema.byte_size),
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

    /// Push one `element` onto this boxed list's backing buffer **in place**, taking ownership of the
    /// caller's reference (no retain — the caller hands over one reference). The caller must guarantee
    /// a uniquely-owned list (`refcount == 1`). Used by the packed-list streaming demote fall-back
    /// (P-PACK 2.5). No-op if this is not a boxed list.
    pub fn list_push(self, element: Value) {
        debug_assert!(
            self.is_list() && heap::refcount(self) == 1,
            "list_push requires a uniquely-owned list (the COW invariant)"
        );
        heap::with_payload_mut(self, |p| {
            if let Payload::List(items) = p {
                items.push(element);
            }
        });
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
        // A packed list (P-PACK 2.4) has no specialized display: materialize a temporary boxed list,
        // render it (identically to the boxed equivalent), and release the temporary.
        if self.is_packed_list() {
            let boxed = self.realize_list();
            let out = boxed.display();
            boxed.release();
            return out;
        }
        if let Some(b) = self.as_bool() {
            b.to_string()
        } else if self.is_small_int() {
            self.as_int().unwrap().to_string()
        } else if self.is_float() {
            lang_stdlib::format_float(self.as_float().unwrap())
        } else if self.is_f32() {
            // An immediate `f32` displays at f32 precision, byte-identical to the tree-walker.
            lang_stdlib::format_f32(self.as_f32().unwrap())
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => s.clone(),
                // A byte buffer renders as a length summary (`<N bytes>`) — opaque and identical on
                // both backends; its content round-trips through `from_bytes`, not display.
                Payload::Bytes(b) => format!("<{} bytes>", b.len()),
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
                // A tuple renders parenthesized with `repr` elements (`(1, "a")`), the positional
                // counterpart of a list's brackets.
                Payload::Tuple(items) => {
                    let parts: Vec<String> = items.iter().map(|v| v.repr()).collect();
                    format!("({})", parts.join(", "))
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
                // An iterator is an opaque reference value (like a file handle).
                Payload::Iter { .. } => "<iterator>".to_string(),
                // Handled by the early return at the top of `display`.
                Payload::PackedList { .. } => unreachable!("packed list demoted before display"),
            })
        } else {
            // The unit value (and any other singleton) displays as empty, as in M0.
            String::new()
        }
    }

    /// The JSON encoding synthesized by `@derive(ToJson)` (and `json.stringify`). Marshals the value
    /// into the neutral [`lang_stdlib::NativeValue`] tree (see [`Self::to_native_deep`]) and runs the
    /// shared [`lang_stdlib::json::stringify`], so the tree-walker — driving the same walk over its
    /// own marshalled tree — produces byte-identical output by construction.
    pub fn to_json(self) -> String {
        lang_stdlib::json::stringify(&self.to_native_deep())
    }

    /// Deeply marshal this value into the neutral [`lang_stdlib::NativeValue`] tree the shared JSON
    /// serializer ([`lang_stdlib::json::stringify`]) consumes — the VM half of `json.stringify` and
    /// `@derive(Serialize<Json>)`. Numbers become scalars; strings, enum variants, and the opaque
    /// length/`<fn>`/`<module …>` summaries become [`NativeValue::Str`]; lists/tuples/sets become a
    /// [`NativeValue::List`]; maps and objects a [`NativeValue::Map`]. Read-only — it never changes a
    /// refcount (a packed list materializes a temporary that is released here).
    pub fn to_native_deep(self) -> lang_stdlib::NativeValue {
        use lang_stdlib::{NativeValue, Scalar};
        // A packed list serializes via a temporary boxed materialization, identical to the boxed form.
        if self.is_packed_list() {
            let boxed = self.realize_list();
            let out = boxed.to_native_deep();
            boxed.release();
            return out;
        }
        if let Some(b) = self.as_bool() {
            NativeValue::Scalar(Scalar::Bool(b))
        } else if self.is_small_int() {
            NativeValue::Scalar(Scalar::Int(self.as_int().unwrap()))
        } else if self.is_float() {
            NativeValue::Scalar(Scalar::Float(self.as_float().unwrap()))
        } else if self.is_f32() {
            NativeValue::Scalar(Scalar::F32(self.as_f32().unwrap()))
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => NativeValue::Str(s.clone()),
                // A byte buffer has no JSON representation (it is the *binary* alternative): a length
                // summary string, so `json.stringify` never panics.
                Payload::Bytes(b) => NativeValue::Str(format!("<{} bytes>", b.len())),
                Payload::Int(i) => NativeValue::Scalar(Scalar::Int(*i)),
                // Lists, tuples, and sets all serialize as a JSON array (JSON has neither tuple nor
                // set), so they marshal to one neutral list.
                Payload::List(items) | Payload::Tuple(items) | Payload::Set(items) => {
                    NativeValue::List(items.iter().map(|v| v.to_native_deep()).collect())
                }
                Payload::Map(entries) => NativeValue::Map(
                    entries
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_native_deep()))
                        .collect(),
                ),
                Payload::Object { shape, slots } => NativeValue::Map(
                    shape
                        .fields
                        .iter()
                        .zip(slots)
                        .map(|(name, v)| (name.clone(), v.to_native_deep()))
                        .collect(),
                ),
                Payload::Closure { .. } | Payload::NativeFn(_) => {
                    NativeValue::Str("<fn>".to_string())
                }
                Payload::Cell(inner) => inner.to_native_deep(),
                Payload::Enum { shape, .. } => {
                    NativeValue::Str(shape.variant.as_deref().unwrap_or(&shape.name).to_string())
                }
                Payload::NativeModule(name) => NativeValue::Str(format!("<module {name}>")),
                // A handle has no JSON analog; its quoted display form, like a closure.
                Payload::FileHandle(handle) => NativeValue::Str(handle.display()),
                // An iterator has no JSON analog either — its opaque display form.
                Payload::Iter { .. } => NativeValue::Str("<iterator>".to_string()),
                // Handled by the early return at the top.
                Payload::PackedList { .. } => {
                    unreachable!("packed list demoted before to_native_deep")
                }
            })
        } else {
            NativeValue::Unit
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
        } else if self.is_f32() {
            "f32"
        } else if self.is_pointer() {
            // Boxed ints were already caught by `as_int` above, so a pointer here is a
            // closure, list, map, or string. M0 names both user functions and builtins
            // "function".
            if self.as_closure().is_some() || self.as_native_fn().is_some() {
                "function"
            } else if self.is_list() {
                "list"
            } else if self.is_tuple() {
                "tuple"
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
            } else if self.is_iter() {
                "iterator"
            } else if self.is_bytes() {
                "bytes"
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

    /// The object's creation sequence — its allocation age (object-model slice 2c), used by the
    /// cycle collector to finalize reclaimed members in a deterministic reverse-creation order. `0`
    /// for non-pointer values (they are never collected).
    pub fn gc_seq(self) -> u32 {
        if self.is_pointer() {
            heap::seq(self)
        } else {
            0
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
/// Materialize one packed element from `words` starting at `offset`, returning the owned
/// `Value::Object` (refcount 1) and the offset just past it — so nested structs and the caller
/// advance in lock-step with [`Value::pack_element`]. Each primitive becomes an immediate (or a
/// boxed int for a large magnitude); each nested struct recurses, the parent object owning its
/// reference. The object reuses `schema.shape`, so it is shape-identical to a constructed instance.
/// Read 8 little-endian bytes at `offset` as a `u64` (the storage word for `int`/`float`/`bool`).
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Read 4 little-endian bytes at `offset` as a `u32` (the storage word for `f32`).
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Decode one packed field at byte `offset` into an owned [`Value`] — the per-field counterpart of
/// [`unpack_element`], used by [`Value::packed_field`] to read a single field without materializing
/// the whole element (P-PACK 3.2b byte-addressed).
fn decode_packed_field(kind: &PackedKind, bytes: &[u8], offset: usize) -> Value {
    match kind {
        PackedKind::Int => Value::int(read_u64(bytes, offset) as i64),
        PackedKind::Float => Value::float(f64::from_bits(read_u64(bytes, offset))),
        PackedKind::F32 => Value::f32(f32::from_bits(read_u32(bytes, offset))),
        PackedKind::Bool => Value::bool(bytes[offset] != 0),
        PackedKind::Struct(inner) => unpack_element(inner, bytes, offset).0,
    }
}

fn unpack_element(schema: &PackedSchema, bytes: &[u8], offset: usize) -> (Value, usize) {
    let mut slots = Vec::with_capacity(schema.fields.len());
    let mut at = offset;
    for kind in &schema.fields {
        match kind {
            PackedKind::Int => {
                slots.push(Value::int(read_u64(bytes, at) as i64));
                at += 8;
            }
            PackedKind::Float => {
                slots.push(Value::float(f64::from_bits(read_u64(bytes, at))));
                at += 8;
            }
            PackedKind::F32 => {
                slots.push(Value::f32(f32::from_bits(read_u32(bytes, at))));
                at += 4;
            }
            PackedKind::Bool => {
                slots.push(Value::bool(bytes[at] != 0));
                at += 1;
            }
            PackedKind::Struct(inner) => {
                let (nested, next) = unpack_element(inner, bytes, at);
                slots.push(nested);
                at = next;
            }
        }
    }
    (Value::object(Rc::clone(&schema.shape), slots), at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_object::ShapeKind;
    use proptest::prelude::*;

    /// Build a packed byte buffer from a sequence of `int` field values (each an 8-byte LE word) —
    /// the byte-addressed form of the old `Vec<u64>` literals (P-PACK 3.2b).
    fn ibytes(vals: &[i64]) -> Vec<u8> {
        vals.iter()
            .flat_map(|v| (*v as u64).to_le_bytes())
            .collect()
    }

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
    fn tuple_display_type_name_and_projection() {
        // Object-model slice 4: a heap tuple renders parenthesized with `repr` elements, names its
        // type, and projects by position. Freed at the end so miri sees no leak (it owns one
        // reference per element).
        let t = Value::tuple(vec![Value::int(1), Value::string("two"), Value::float(3.0)]);
        assert_eq!(t.display(), "(1, \"two\", 3.0)");
        assert_eq!(t.type_name(), "tuple");
        assert!(t.is_tuple());
        assert_eq!(t.tuple_field(0).unwrap().as_int(), Some(1));
        assert_eq!(t.tuple_field(1).unwrap().as_string().unwrap(), "two");
        assert!(t.tuple_field(3).is_none());
        // Release the sole reference: frees the tuple and its owned elements (the boxed string),
        // so miri sees no leak.
        t.release();
    }

    #[test]
    fn packed_list_round_trips_and_frees() {
        // P-PACK 2.4: a flat `List<packed>` packs elements into raw words and materializes them on
        // demand. Exercise construct → len → packed_get → realize → equality → display → free so
        // miri checks the materialize/free paths for use-after-free or double-free.
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = Rc::new(PackedSchema {
            shape: Rc::clone(&shape),
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
        });

        // Pack two `V { x, y }` instances into one flat buffer (the source objects are freed after).
        let mut bytes = Vec::new();
        for (x, y) in [(3_i64, 1_i64), (1, 2)] {
            let obj = Value::object(Rc::clone(&shape), vec![Value::int(x), Value::int(y)]);
            assert!(obj.pack_element(&schema, &mut bytes));
            obj.release();
        }
        assert_eq!(bytes.len(), 32); // 2 elements × 2 int fields × 8 bytes

        let list = Value::packed_list(Rc::clone(&schema), bytes);
        assert!(list.is_packed_list());
        assert!(list.is_list());
        assert_eq!(list.list_len(), Some(2));
        assert_eq!(list.type_name(), "list");

        // A single element materializes to an owned object, shape-identical to a constructed one.
        let first = list.packed_get(0);
        assert_eq!(first.display(), "V {x: 3, y: 1}");
        let constructed = Value::object(Rc::clone(&shape), vec![Value::int(3), Value::int(1)]);
        assert!(
            apply_binary(lang_ast::BinaryOp::Eq, first, constructed)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        first.release();
        constructed.release();

        // The whole list displays and compares as the boxed equivalent.
        assert_eq!(list.display(), "[V {x: 3, y: 1}, V {x: 1, y: 2}]");
        let boxed = Value::list(vec![
            Value::object(Rc::clone(&shape), vec![Value::int(3), Value::int(1)]),
            Value::object(Rc::clone(&shape), vec![Value::int(1), Value::int(2)]),
        ]);
        assert!(
            apply_binary(lang_ast::BinaryOp::Eq, list, boxed)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        boxed.release();

        // Release the packed list (a leaf — frees the buffer, no child release), so miri sees no leak.
        list.release();
    }

    #[test]
    fn packed_push_streams_in_place_and_frees() {
        // P-PACK 2.5: streaming construction — start from an empty packed list and `packed_push` each
        // element in place (the in-place path the VM uses). `packed_push` reads the element through
        // the heap while mutating the list through the heap (two distinct objects), so this exercises
        // that nested `with_payload_mut`/`with_payload` access for use-after-free under miri, and
        // confirms the element object is freed by the caller after its primitives are copied.
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = Rc::new(PackedSchema {
            shape: Rc::clone(&shape),
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
        });

        let list = Value::packed_list(Rc::clone(&schema), Vec::new());
        assert_eq!(list.list_len(), Some(0));
        for (x, y) in [(3_i64, 1_i64), (1, 2), (7, 9)] {
            let obj = Value::object(Rc::clone(&shape), vec![Value::int(x), Value::int(y)]);
            assert!(list.packed_push(obj)); // primitives copied into the buffer (no retain)
            obj.release(); // the caller still owns the element — free it (its data is now in `words`)
        }
        assert_eq!(list.list_len(), Some(3));
        assert_eq!(
            list.display(),
            "[V {x: 3, y: 1}, V {x: 1, y: 2}, V {x: 7, y: 9}]"
        );
        list.release();

        // The demote fall-back: push onto a boxed list in place (the caller hands over one reference).
        let boxed = Value::list(Vec::new());
        let elem = Value::object(Rc::clone(&shape), vec![Value::int(5), Value::int(6)]);
        boxed.list_push(elem); // boxed now owns the reference handed over
        assert_eq!(boxed.list_len(), Some(1));
        assert_eq!(boxed.display(), "[V {x: 5, y: 6}]");
        boxed.release(); // frees the list and its owned element, so miri sees no leak
    }

    #[test]
    fn f32_is_immediate_and_round_trips() {
        // P-PACK Phase 3: an `f32` is an *immediate* NaN-boxed value — no heap allocation, not
        // refcounted (like an immediate int/float). Round-trips through `as_f32`, names itself `f32`,
        // and is distinguishable from every other immediate.
        let v = Value::f32(1.5);
        assert!(!v.is_pointer(), "f32 must be immediate, not a heap pointer");
        assert_eq!(v.as_f32(), Some(1.5));
        assert!(v.is_f32());
        assert_eq!(v.as_float(), None); // not an f64 float
        assert_eq!(v.as_int(), None); // not an int
        assert_eq!(v.as_bool(), None);
        assert!(!v.is_unit());
        assert_eq!(v.type_name(), "f32");
        assert_eq!(v.display(), "1.5");

        // f32 precision is observable: 0.1 + 0.2 at f32 is exactly 0.3 (f64 would be 0.30000…04).
        assert_eq!(Value::f32(0.1 + 0.2_f32).display(), "0.3");
        // Bit patterns round-trip exactly, including the awkward ones.
        for f in [0.0f32, -0.0, 1.0, -2.5, f32::MAX, f32::MIN, f32::EPSILON] {
            assert_eq!(Value::f32(f).as_f32(), Some(f), "round-trip {f}");
        }
        // An immediate int and an immediate f32 with the same numeric value are distinct values.
        assert_ne!(Value::f32(3.0).0, Value::int(3).0);
    }

    #[test]
    fn packed_f32_byte_layout_round_trips() {
        // P-PACK 3.2b: an `f32` packed field is 4 bytes, an `int` 8 — a mixed `{f32, int}` element is
        // 12 bytes, exercising unaligned byte offsets (the int starts at byte 4). Pack two, then read
        // each field back: the f32 keeps f32 precision and the int its full value. Checked under miri.
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "P",
            vec!["a".into(), "b".into()],
        ));
        let schema = Rc::new(PackedSchema {
            shape: Rc::clone(&shape),
            fields: vec![PackedKind::F32, PackedKind::Int],
            byte_size: 12,
        });
        let mut bytes = Vec::new();
        for (a, b) in [(0.1f32 + 0.2, 7_i64), (-1.5, 1_000_000)] {
            let obj = Value::object(Rc::clone(&shape), vec![Value::f32(a), Value::int(b)]);
            assert!(obj.pack_element(&schema, &mut bytes));
            obj.release();
        }
        assert_eq!(bytes.len(), 24); // 2 elements × 12 bytes

        let list = Value::packed_list(Rc::clone(&schema), bytes);
        assert_eq!(list.list_len(), Some(2));
        assert_eq!(
            list.display(),
            "[P {a: 0.3, b: 7}, P {a: -1.5, b: 1000000}]"
        );
        // Fused single-field reads land on the right byte offsets.
        assert_eq!(list.packed_field(0, "a").and_then(Value::as_f32), Some(0.3));
        assert_eq!(list.packed_field(0, "b").and_then(Value::as_int), Some(7));
        assert_eq!(
            list.packed_field(1, "a").and_then(Value::as_f32),
            Some(-1.5)
        );
        assert_eq!(
            list.packed_field(1, "b").and_then(Value::as_int),
            Some(1_000_000)
        );
        list.release();
    }

    #[test]
    fn packed_field_reads_one_field_without_materializing() {
        // P-PACK 2.5+: the fused `list[i].field` read decodes a single field's word, returning an
        // owned primitive (or `None` for an out-of-range index / unknown field). Exercised under miri
        // to confirm the targeted slice read borrows the buffer correctly and leaks nothing.
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = Rc::new(PackedSchema {
            shape: Rc::clone(&shape),
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
        });
        // Two elements: (3, 1) and (7, 9).
        let list = Value::packed_list(Rc::clone(&schema), ibytes(&[3, 1, 7, 9]));

        assert_eq!(list.packed_field(0, "x").and_then(Value::as_int), Some(3));
        assert_eq!(list.packed_field(0, "y").and_then(Value::as_int), Some(1));
        assert_eq!(list.packed_field(1, "x").and_then(Value::as_int), Some(7));
        assert_eq!(list.packed_field(1, "y").and_then(Value::as_int), Some(9));
        // Out of range and unknown field both decline (the caller then falls back).
        assert!(list.packed_field(2, "x").is_none());
        assert!(list.packed_field(0, "z").is_none());

        list.release();
    }

    #[test]
    fn packed_select_keeps_the_list_flat() {
        // P-PACK 2.6: `packed_select` rebuilds a flat buffer from chosen element word-blocks (the
        // selection producers reverse/slice/filter). The result is still a packed list (no demote) and
        // owns no child refs, so it frees cleanly — checked under miri.
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = Rc::new(PackedSchema {
            shape: Rc::clone(&shape),
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
        });
        // Three elements: (3,1), (1,2), (7,9).
        let list = Value::packed_list(Rc::clone(&schema), ibytes(&[3, 1, 1, 2, 7, 9]));

        // Reverse-order selection yields a flat packed list with the blocks reordered.
        let reversed = list.packed_select(&[2, 1, 0]);
        assert!(reversed.is_packed_list());
        assert_eq!(reversed.list_len(), Some(3));
        assert_eq!(
            reversed.display(),
            "[V {x: 7, y: 9}, V {x: 1, y: 2}, V {x: 3, y: 1}]"
        );
        reversed.release();

        // A subset selection (a filter/slice keeping a prefix) stays flat too.
        let kept = list.packed_select(&[0, 2]);
        assert!(kept.is_packed_list());
        assert_eq!(kept.display(), "[V {x: 3, y: 1}, V {x: 7, y: 9}]");
        kept.release();

        list.release();
    }

    #[test]
    fn packed_set_and_concat_keep_the_list_flat() {
        // P-PACK 2.6: `set`/`~` on a packed list stay flat. `packed_set` (copy) and
        // `packed_set_in_place` (sole-owner overwrite) replace one element's words; `packed_concat`
        // (copy) and `packed_extend_in_place` (sole-owner append) join same-layout buffers. All
        // results are still packed lists owning no child refs — checked under miri.
        let shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = Rc::new(PackedSchema {
            shape: Rc::clone(&shape),
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
        });
        let mk = |vals: &[i64]| Value::packed_list(Rc::clone(&schema), ibytes(vals));
        let elem =
            |x: i64, y: i64| Value::object(Rc::clone(&shape), vec![Value::int(x), Value::int(y)]);

        // Functional `set`: a fresh flat list with one element replaced; the original is untouched.
        let list = mk(&[1, 1, 2, 2, 3, 3]);
        let nine = elem(9, 9);
        let updated = list.packed_set(1, nine).expect("packs");
        nine.release();
        assert!(updated.is_packed_list());
        assert_eq!(
            updated.display(),
            "[V {x: 1, y: 1}, V {x: 9, y: 9}, V {x: 3, y: 3}]"
        );
        assert_eq!(
            list.display(),
            "[V {x: 1, y: 1}, V {x: 2, y: 2}, V {x: 3, y: 3}]"
        );
        updated.release();

        // In-place `set` (sole owner): overwrite one block, the list keeps its single reference.
        let eight = elem(8, 8);
        assert!(list.packed_set_in_place(0, eight));
        eight.release();
        assert!(list.is_packed_list());
        assert_eq!(
            list.display(),
            "[V {x: 8, y: 8}, V {x: 2, y: 2}, V {x: 3, y: 3}]"
        );

        // Functional concat of same-layout lists → a flat join; both inputs survive.
        let other = mk(&[7, 7]);
        let joined = list.packed_concat(other).expect("same layout");
        assert!(joined.is_packed_list());
        assert_eq!(joined.list_len(), Some(4));
        assert_eq!(other.list_len(), Some(1)); // input unchanged
        joined.release();

        // In-place extend (sole owner): append `other`'s words; `other` is borrowed (still owned).
        assert!(list.packed_extend_in_place(other));
        assert!(list.is_packed_list());
        assert_eq!(
            list.display(),
            "[V {x: 8, y: 8}, V {x: 2, y: 2}, V {x: 3, y: 3}, V {x: 7, y: 7}]"
        );
        other.release();
        list.release();
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

    #[test]
    fn iterator_advances_collects_and_frees_without_leaking() {
        // Heap elements (strings) so the refcount + unsafe heap-access paths in `iter`/`iter_next`/
        // `iter_collect` are actually exercised (immediates would no-op the refcounting). The leak
        // oracle's invariant: live count returns to baseline once every reference is released.
        let before = live_count();
        let list = Value::list(vec![Value::string("a"), Value::string("b")]); // list + 2 strings
        let it = Value::iter(list); // +1 (iter); retains the list (rc 2)
        // The iterator now owns the list; drop this local reference (rc → 1, still alive).
        assert!(!list.dec_ref());

        // `next()` hands back a freshly-retained element.
        let a = it.iter_next().unwrap();
        assert_eq!(a.as_string().as_deref(), Some("a"));
        // The backing list still owns "a", so this only drops the next()-retained reference.
        assert!(!a.dec_ref());

        // `collect()` drains the rest into a new list (["b"]).
        let rest = it.iter_collect();
        assert_eq!(rest.list_len(), Some(1));
        assert!(rest.dec_ref());
        rest.free();

        // A drained iterator yields `none` and is safe to keep calling.
        assert!(it.iter_next().is_none());

        // Freeing the iterator releases its backing list, which frees its elements.
        assert!(it.dec_ref());
        it.free();
        assert_eq!(live_count(), before);
    }

    #[test]
    fn iterator_adapters_free_without_leaking() {
        // Heap elements again, exercising the recursive `iter_next` (Take → Chain → List), `drop`'s
        // skip-release, and the multi-source GC node (Chain owns two children). Live count must
        // return to baseline after the whole pipeline is released.
        let before = live_count();
        let l1 = Value::list(vec![Value::string("a"), Value::string("b")]);
        let l2 = Value::list(vec![Value::string("c")]);
        let i1 = Value::iter(l1);
        l1.release(); // i1 is now l1's sole owner
        let i2 = Value::iter(l2);
        l2.release();
        let chained = Value::iter_chain(i1, i2); // retains i1, i2
        i1.release();
        i2.release(); // `chained` is now the sole owner of i1, i2
        let taken = Value::iter_take(chained, 2); // retains chained
        chained.release();
        // take(2) over chain([a, b], [c]) yields "a", "b".
        let collected = taken.iter_collect();
        assert_eq!(collected.list_len(), Some(2));
        collected.release();
        taken.release(); // frees taken → chained → i1/i2 → l1/l2 → their strings
        assert_eq!(live_count(), before);

        // `drop` skips (and releases) the first n, yielding the rest.
        let l3 = Value::list(vec![
            Value::string("x"),
            Value::string("y"),
            Value::string("z"),
        ]);
        let i3 = Value::iter(l3);
        l3.release();
        let dropped = Value::iter_drop(i3, 2); // retains i3
        i3.release();
        let rest = dropped.iter_collect(); // ["z"]
        assert_eq!(rest.list_len(), Some(1));
        rest.release();
        dropped.release();
        assert_eq!(live_count(), before);
    }

    #[test]
    fn iterator_tuple_adapters_and_sum_free_without_leaking() {
        // Track I.1b.2: `enumerate` builds `(int, element)` tuples (each owning the heap element
        // `iter_next` handed it), `zip` pairs two sources and releases a leftover element of the
        // longer one, and `sum`'s error path releases the offending heap element. Live count must
        // return to baseline throughout.
        let before = live_count();

        // `enumerate` over heap strings → a list of `(index, string)` tuples.
        let le = Value::list(vec![Value::string("a"), Value::string("b")]);
        let ie = Value::iter(le);
        le.release();
        let enumerated = Value::iter_enumerate(ie);
        ie.release();
        let collected = enumerated.iter_collect(); // [(0, "a"), (1, "b")]
        assert_eq!(collected.list_len(), Some(2));
        collected.release();
        enumerated.release();
        assert_eq!(live_count(), before);

        // `zip` with a longer left source: the leftover "c" pulled from `a` after `b` runs dry must
        // be released, not leaked.
        let la = Value::list(vec![
            Value::string("a"),
            Value::string("b"),
            Value::string("c"),
        ]);
        let lb = Value::list(vec![Value::string("x")]);
        let ia = Value::iter(la);
        la.release();
        let ib = Value::iter(lb);
        lb.release();
        let zipped = Value::iter_zip(ia, ib); // retains ia, ib
        ia.release();
        ib.release();
        let pairs = zipped.iter_collect(); // [("a", "x")] — "b" stays in the source, "c" consumed+freed
        assert_eq!(pairs.list_len(), Some(1));
        pairs.release();
        zipped.release();
        assert_eq!(live_count(), before);

        // `sum` over ints totals to an immediate and frees the iterator.
        let ls = Value::list(vec![Value::int(1), Value::int(2), Value::int(3)]);
        let is = Value::iter(ls);
        ls.release();
        assert_eq!(is.iter_sum().unwrap().as_int(), Some(6));
        is.release();
        assert_eq!(live_count(), before);

        // `sum`'s error path: a heap (non-numeric) element is released as the error is raised.
        let lerr = Value::list(vec![Value::int(1), Value::string("nope")]);
        let ierr = Value::iter(lerr);
        lerr.release();
        assert_eq!(ierr.iter_sum().unwrap_err(), "string");
        ierr.release();
        assert_eq!(live_count(), before);
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
