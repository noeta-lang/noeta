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
//! in `i64`. This is the representation the VM (`noeta-vm`) operates on through the safe
//! API here; all `unsafe` is quarantined to the [`heap`] module.
//!
//! Why a separate value type from the M0 tree-walker's `Value` enum? An `Rc<T>` cannot
//! live in a NaN-box pointer slot, so the two backends keep different value models and are
//! only ever compared on observable output (`RunResult`), never on representation.

mod heap;
mod ids;
mod ops;

pub use heap::{
    CollectorMode, Color, SharedRegion, SharedRoot, collector_mode, live_count, live_objects,
    live_peak, note_refcount_anomalies, refcount_anomalies, reset_peak, reset_refcount_anomalies,
    set_collector_mode, take_candidates,
};
pub use ids::{ChannelId, ScopeId, TaskId};
pub use ops::{
    OpError, apply_binary, apply_binary_wide, apply_unary, compare_primitive, compare_values,
    structural_compare,
};

use std::collections::BTreeMap;
use std::rc::Rc;

use noeta_ast::reflect::TypeRepr;
use noeta_bytecode::Builtin;
use noeta_object::{PackedKind, PackedSchema, Shape};

// The P-SSO string (24-byte, ≤24-byte content inline) inside `Payload::Str`. Re-exported so the
// one hot producer outside this crate — the VM's `BuildString` — can assemble its output in the
// payload's own representation and hand it over without a conversion.
pub use compact_str::CompactString;

use heap::{IterShape, IterState, Payload};

/// Why an iterator pull ([`Value::iter_next_apply`]) aborted (Track I.1c). The closure adapters
/// (`map`/`filter`) run user code, which the simple closure-free pull could not, so stepping is now
/// fallible. `Closure` carries the backend's own call error (generic `E`) verbatim; `FilterNotBool`
/// reports a `filter` predicate that returned a non-bool (its type name) for the backend to phrase as
/// a diagnostic. The backend maps both back into its native error.
#[derive(Debug)]
pub enum IterAbort<E> {
    /// A `map`/`filter` closure call failed; the backend's error is carried through unchanged.
    Closure(E),
    /// A `filter` predicate returned a value of this type instead of a `bool`.
    FilterNotBool(&'static str),
}

/// The kind of a heap value's payload — the public, `Copy` face of the internal `Payload`
/// discriminant, one variant per payload. See [`Value::heap_kind`]: classify a receiver once,
/// then dispatch on integer compares instead of re-dereferencing the heap per candidate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapKind {
    Str,
    Bytes,
    /// A registered extern-type value (extern-types X1).
    Extern,
    Int,
    Closure,
    Cell,
    List,
    Tuple,
    Set,
    Map,
    PackedList,
    Object,
    Enum,
    NativeModule,
    NativeFn,
    /// A selectively-imported module function (`use std.math.sqrt`, prelude-redesign P0).
    ModuleFn,
    /// An unbound method handle (`Type.method` as a value, prelude-redesign MH).
    MethodHandle,
    /// A bound method handle (`value.method`, receiver captured, prelude-redesign EX.2b).
    BoundMethod,
    Iter,
    Future,
    Timer,
    Handle,
    AsyncIo,
    Sender,
    Receiver,
    ChannelSend,
    ChannelRecv,
    IsolateFuture,
}

/// The NaN-box bit layout (see [`Value::NANBOX`]), the ABI contract between this crate's value
/// encoding and the JIT's native codegen. Every field is a raw bit pattern (or bound) the JIT feeds
/// straight into Cranelift constants.
#[derive(Debug, Clone, Copy)]
pub struct NanBoxLayout {
    /// Quiet-NaN prefix: a word is a tagged (non-float) value iff `bits & qnan == qnan`.
    pub qnan: u64,
    /// Sign bit; set on pointers.
    pub sign_bit: u64,
    /// Immediate small-int discriminator bit.
    pub int_tag: u64,
    /// Low-48-bit payload mask (heap address / small-int payload).
    pub ptr_mask: u64,
    /// The exact bit pattern of `unit`.
    pub unit_bits: u64,
    /// The exact bit pattern of `true`.
    pub true_bits: u64,
    /// The exact bit pattern of `false`.
    pub false_bits: u64,
    /// The exact bit pattern of the VM's unbound-global sentinel.
    pub unbound_bits: u64,
    /// Smallest / largest integer that stays an immediate (outside this range `int` boxes on the heap).
    pub int_min: i64,
    pub int_max: i64,
}

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
    /// The async **pending** sentinel (Track A.3): the singleton an async state-machine step returns
    /// when it suspends at an `.await`. A distinct immediate so it can never be confused with any user
    /// value (including `unit`, a valid completion). It never escapes to user code — every poll site
    /// catches it — so it has no surface type; it displays opaquely purely defensively.
    const TAG_PENDING: u64 = 3;
    /// The **unbound-global** sentinel: the VM stores its global slots as a `Vec<Value>` (P-JIT
    /// globals), and this immediate marks a slot that has never been bound (replacing the old
    /// `Option::None`). A distinct singleton so it can never collide with a real value; it never
    /// escapes to user code (a `LoadGlobal`/`TakeGlobal` of it raises E0005).
    const TAG_UNBOUND: u64 = 4;
    /// Largest immediate small-int magnitude (48-bit signed payload).
    const INT_MIN: i64 = -(1 << 47);
    const INT_MAX: i64 = (1 << 47) - 1;

    /// The NaN-box bit layout, exposed as the **single source of truth** for the JIT (`noeta-jit`),
    /// which emits inline tag checks and box/unbox sequences as native code and must encode values
    /// bit-for-bit identically to this crate's safe API. These fields *are* the private constants the
    /// constructors/accessors above use, so the JIT can never drift from the interpreter's encoding —
    /// a `noeta-value` test round-trips them against [`Value::int`]/[`Value::bool`]/[`Value::unit`].
    pub const NANBOX: NanBoxLayout = NanBoxLayout {
        qnan: Self::QNAN,
        sign_bit: Self::SIGN_BIT,
        int_tag: Self::INT_TAG,
        ptr_mask: Self::PTR_MASK,
        unit_bits: Self::QNAN | Self::TAG_UNIT,
        true_bits: Self::QNAN | Self::TAG_TRUE,
        false_bits: Self::QNAN | Self::TAG_FALSE,
        unbound_bits: Self::QNAN | Self::TAG_UNBOUND,
        int_min: Self::INT_MIN,
        int_max: Self::INT_MAX,
    };

    // --- Constructors ---

    /// The unit value.
    pub fn unit() -> Value {
        Value(Self::QNAN | Self::TAG_UNIT)
    }

    pub fn bool(b: bool) -> Value {
        Value(Self::QNAN | if b { Self::TAG_TRUE } else { Self::TAG_FALSE })
    }

    /// The async **pending** sentinel (Track A.3) — the value an async step returns to signal it
    /// suspended at an `.await`. An immediate singleton; never refcounted, never user-visible.
    pub fn pending() -> Value {
        Value(Self::QNAN | Self::TAG_PENDING)
    }

    /// The **unbound-global** sentinel — the VM's marker for a global slot that has never been bound
    /// (the `Vec<Value>` globals model, P-JIT). An immediate singleton; never refcounted, never
    /// user-visible (loading it raises E0005).
    pub fn unbound() -> Value {
        Value(Self::QNAN | Self::TAG_UNBOUND)
    }

    /// Whether this is the unbound-global sentinel (see [`Value::unbound`]).
    pub fn is_unbound(self) -> bool {
        self.0 == Value::unbound().0
    }

    /// Reconstruct a value from its raw NaN-boxed word — the inverse of [`Value::bits`]. Used by the
    /// JIT's runtime helpers (`noeta-jit`), which pass a value to the VM as its `u64` bits (the native
    /// ABI can't carry a `Value` type). The caller must pass bits this crate's encoding produced.
    pub fn from_bits(bits: u64) -> Value {
        Value(bits)
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

    /// A heap string (refcount 1). Content ≤ 24 bytes lives inline in the payload (P-SSO) —
    /// the value is then a single allocation.
    pub fn string(s: &str) -> Value {
        heap::alloc(Payload::Str(CompactString::new(s)))
    }

    /// A heap string (refcount 1) that **takes ownership** of an already-built buffer — no copy,
    /// unlike [`Value::string`] which copies a borrowed `&str`. Use when the caller already owns the
    /// buffer (e.g. `BuildString`'s interpolation output, assembled as a [`CompactString`] so a
    /// short result never touches the allocator).
    pub fn from_string(s: CompactString) -> Value {
        heap::alloc(Payload::Str(s))
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

    /// Whether this is a user closure (`Payload::Closure`, carrying captured upvalues) — not a native
    /// builtin function. Used by the destructor walk to reach a closure's captured values.
    pub fn is_closure(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Closure { .. }))
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

    /// This value's **reflected type tag** (runtime type-argument reflection, R1), or `None` if the
    /// value is untagged (an immediate, or a heap value whose construction site carried no type). Read
    /// by `type_of` to recover a container's element type after its static type was laundered through
    /// `dyn`. A cheap `Rc` clone; `None` for every non-pointer value.
    pub fn reflect(self) -> Option<Rc<TypeRepr>> {
        if self.is_pointer() {
            heap::reflect(self)
        } else {
            None
        }
    }

    /// The value's type as **surface syntax** (`List<int>`, `Point`): the reflected tag rendered with
    /// the same spelling the checker's types display with, falling back to the coarse kind name
    /// (`int`, `string`) for an untagged value. The one type spelling every tool shows the user —
    /// REPL `:type`, the debugger's Variables view, watch results — so they cannot drift apart.
    pub fn type_display(self) -> String {
        self.reflect()
            .map(|t| t.to_string())
            .unwrap_or_else(|| self.type_name().to_string())
    }

    /// Stamp (or clear) this value's reflected type tag (R1). Used at list-literal construction to
    /// record the checker-resolved element type. A no-op on a non-pointer value (an immediate carries
    /// no tag). The tag is invisible to value semantics — it lives beside the payload, never inside it.
    pub fn set_reflect(self, tag: Option<Rc<TypeRepr>>) {
        if self.is_pointer() {
            heap::set_reflect(self, tag);
        }
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
    pub fn packed_list(schema: &'static PackedSchema, bytes: Vec<u8>) -> Value {
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
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_push requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let mut staged = Vec::with_capacity(schema.byte_size);
                if element.pack_element(schema, &mut staged) {
                    if schema.column {
                        // Column-major append rebuilds the buffer (O(n)); see `column_append`.
                        *bytes = column_append(schema, bytes, &staged);
                    } else {
                        bytes.extend(staged);
                    }
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
                // Gather the element into row order first (a plain byte copy, no allocation), so the
                // materialization below is layout-agnostic. Row-major is a contiguous stride; column-
                // major scatters the element across its columns (P-SIMD C2).
                let elem = if schema.column {
                    let count = schema.count(bytes.len());
                    gather_row(schema, bytes, index, count)
                } else {
                    let offset = index * stride;
                    bytes[offset..offset + stride].to_vec()
                };
                (*schema, elem)
            }
            _ => unreachable!("packed_get on a non-packed list"),
        });
        unpack_element(schema, &elem, 0).0
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
                let count = schema.count(bytes.len());
                if index >= count {
                    return None;
                }
                let slot = schema.shape.slot_of(field)?;
                // The field's byte offset resolves through the layout axis (row vs column, P-SIMD C2).
                let at = schema.field_offset(index, slot, count);
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

    /// Borrow this packed list's schema and raw byte buffer for the duration of `f` — the
    /// zero-copy read under the native raw-buffer seam (`NativeCtx::with_packed`, package-manager
    /// N3.4; superseding the vec3-specific cloning accessors the bulk `vec` intercepts used).
    /// `None` (without running `f`) for anything that is not a packed list.
    pub fn with_packed_ref<R>(
        self,
        f: impl FnOnce(&'static PackedSchema, &[u8]) -> R,
    ) -> Option<R> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => Some(f(schema, bytes)),
            _ => None,
        })
    }

    /// This packed list's schema handle and a **copy** of its byte buffer — the allocating read
    /// for a caller that outlives the borrow (`NativeCtx::with_packed_mut`'s copy-on-write path).
    /// `None` for anything that is not a packed list.
    pub fn packed_parts(self) -> Option<(&'static PackedSchema, Vec<u8>)> {
        if !self.is_packed_list() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::PackedList { schema, bytes } => Some((*schema, bytes.clone())),
            _ => None,
        })
    }

    /// Mutate this packed list's byte buffer **in place** through `f` (the raw-buffer seam's
    /// proven-sole-ownership fast path). The caller must guarantee a uniquely-owned packed list
    /// (`refcount == 1`), like the other `*_in_place` ops.
    pub fn packed_mutate_in_place(self, f: impl FnOnce(&'static PackedSchema, &mut [u8])) {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_mutate_in_place requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => f(schema, bytes),
            _ => unreachable!("packed_mutate_in_place on a non-packed list"),
        });
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
                let buf = if schema.column {
                    let count = schema.count(bytes.len());
                    column_select(schema, bytes, indices, count)
                } else {
                    let stride = schema.byte_size;
                    let mut out = Vec::with_capacity(indices.len() * stride);
                    for &i in indices {
                        out.extend_from_slice(&bytes[i * stride..i * stride + stride]);
                    }
                    out
                };
                (*schema, buf)
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
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_set on a non-packed list"),
        });
        let stride = schema.byte_size;
        let mut staged = Vec::with_capacity(stride);
        if !element.pack_element(schema, &mut staged) {
            return None;
        }
        if schema.column {
            let count = schema.count(buf.len());
            column_set(schema, &mut buf, index, count, &staged);
        } else {
            buf[index * stride..index * stride + stride].copy_from_slice(&staged);
        }
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
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_set_in_place requires a uniquely-owned packed list"
        );
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes } => {
                let stride = schema.byte_size;
                let mut staged = Vec::with_capacity(stride);
                if element.pack_element(schema, &mut staged) {
                    if schema.column {
                        let count = schema.count(bytes.len());
                        column_set(schema, bytes, index, count, &staged);
                    } else {
                        bytes[index * stride..index * stride + stride].copy_from_slice(&staged);
                    }
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
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_concat on a non-packed list"),
        });
        let other_bytes = heap::with_payload(other, |p| match p {
            Payload::PackedList {
                schema: s2,
                bytes: b2,
            } => std::ptr::eq(schema.shape, s2.shape).then(|| b2.clone()),
            _ => None,
        })?;
        // Same shape ⇒ same layout. Row appends the buffers; column interleaves per column (P-SIMD C2).
        if schema.column {
            buf = column_concat(schema, &buf, &other_bytes);
        } else {
            buf.extend_from_slice(&other_bytes);
        }
        Some(Value::packed_list(schema, buf))
    }

    /// Append `other`'s elements to this packed list **in place** (P-PACK 2.6 reuse path for
    /// `acc = acc ~ xs`). The caller must guarantee a uniquely-owned packed list (`refcount == 1`).
    /// `other` is borrowed (its words copied). Returns `false` (buffer untouched) unless `other` is a
    /// packed list of the same layout, so the caller can fall back to the copy path.
    #[must_use]
    pub fn packed_extend_in_place(self, other: Value) -> bool {
        debug_assert!(
            self.is_packed_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "packed_extend_in_place requires a uniquely-owned packed list"
        );
        if !other.is_packed_list() {
            return false;
        }
        let (other_schema, other_bytes) = heap::with_payload(other, |p| match p {
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_extend_in_place on a non-packed list"),
        });
        heap::with_payload_mut(self, |p| match p {
            Payload::PackedList { schema, bytes }
                if std::ptr::eq(schema.shape, other_schema.shape) =>
            {
                if schema.column {
                    // Column layout must rebuild (each column grows in the middle of the buffer).
                    *bytes = column_concat(schema, bytes, &other_bytes);
                } else {
                    bytes.extend_from_slice(&other_bytes);
                }
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
            Payload::PackedList { schema, bytes } => (*schema, bytes.clone()),
            _ => unreachable!("packed_items on a non-packed list"),
        });
        let count = schema.count(bytes.len());
        let mut out = Vec::with_capacity(count);
        if schema.column {
            // Column-major: each element is scattered across columns — gather it to row order first.
            for i in 0..count {
                let row = gather_row(schema, &bytes, i, count);
                out.push(unpack_element(schema, &row, 0).0);
            }
        } else {
            let mut at = 0;
            for _ in 0..count {
                let (value, next) = unpack_element(schema, &bytes, at);
                out.push(value);
                at = next;
            }
        }
        out
    }

    /// A registered extern-type value (extern-types X1) — the general form of
    /// [`Value::file_handle`]. A GC leaf (the contract owns no child values).
    pub fn extern_value(value: noeta_stdlib::ExternBox) -> Value {
        heap::alloc(Payload::Extern(value))
    }

    /// Whether this is an extern-type value.
    pub fn is_extern(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Extern(_)))
    }

    /// Read this extern value under a closure. The caller must have checked [`Value::is_extern`].
    pub fn with_extern<R>(self, f: impl FnOnce(&dyn noeta_stdlib::ExternValue) -> R) -> R {
        heap::with_extern(self, f)
    }

    /// Mutate this extern value under a closure (the receiver of a mutating method). The caller
    /// must have checked [`Value::is_extern`].
    pub fn with_extern_mut<R>(self, f: impl FnOnce(&mut dyn noeta_stdlib::ExternValue) -> R) -> R {
        heap::with_extern_mut(self, f)
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

    /// A `map(f)` adapter: yields `func(element)` for each element of `source` (Track I.1c). Owns one
    /// reference to `source` and one to the closure `func`.
    pub fn iter_map(source: Value, func: Value) -> Value {
        source.inc_ref();
        func.inc_ref();
        heap::alloc(Payload::Iter(IterState::Map { source, func }))
    }

    /// A `filter(f)` adapter: yields the elements of `source` for which `pred(element)` is true
    /// (Track I.1c). Owns one reference to `source` and one to the closure `pred`.
    pub fn iter_filter(source: Value, pred: Value) -> Value {
        source.inc_ref();
        pred.inc_ref();
        heap::alloc(Payload::Iter(IterState::Filter { source, pred }))
    }

    /// A generator iterator (Track G): `step` is a closure (a state machine over `mut`-captured cells)
    /// invoked once per `next()` and returning `?T`. Owns one reference to the closure.
    pub fn iter_gen(step: Value) -> Value {
        step.inc_ref();
        heap::alloc(Payload::Iter(IterState::Gen { step }))
    }

    /// Whether this is an iterator.
    pub fn is_iter(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Iter(_)))
    }

    /// An async future (Track A): wraps `step`, the lazy thunk that runs the `async fn` body and
    /// returns the completion value (A.1). Owns one reference to the closure.
    pub fn make_future(step: Value) -> Value {
        step.inc_ref();
        heap::alloc(Payload::Future(step))
    }

    /// A **leaf timer future** (Track A.2): `sleep(ms)` produces one, carrying the absolute logical
    /// deadline (ms) at which it becomes ready. Unlike [`Self::make_future`] it wraps no closure — it
    /// is polled by consulting the executor clock (see [`Self::timer_deadline`]).
    pub fn make_timer(deadline: u64) -> Value {
        heap::alloc(Payload::Timer(deadline))
    }

    /// Whether this is a future — a step/thunk future ([`Payload::Future`]), a leaf timer
    /// ([`Payload::Timer`]), a task handle ([`Payload::Handle`]), or a leaf async-read
    /// ([`Payload::AsyncIo`]). All name their type "future" and display opaquely.
    pub fn is_future(self) -> bool {
        self.is_pointer()
            && heap::with_payload(self, |p| {
                matches!(
                    p,
                    Payload::Future(_)
                        | Payload::Timer(_)
                        | Payload::Handle(..)
                        | Payload::AsyncIo(_)
                        | Payload::ChannelSend(..)
                        | Payload::ChannelRecv(_)
                        | Payload::IsolateFuture(_)
                )
            })
    }

    /// Whether this is specifically a **step/thunk future** ([`Payload::Future`], a lowered
    /// `async fn` body) — the only future flavor the telemetry completion hook traces (T5c), on
    /// both backends identically. Non-retaining, unlike [`Self::future_step`].
    pub fn is_step_future(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Future(_)))
    }

    /// A **leaf isolate-result future** (isolates I.4b): a real-thread `isolate f(args)` yields one,
    /// carrying an id into the backend's isolate table (the worker's join handle + result receiver).
    /// Polled by harvesting the worker's marshalled result. VM-real path only.
    pub fn make_isolate_future(id: u32) -> Value {
        heap::alloc(Payload::IsolateFuture(id))
    }

    /// The backend isolate-table id of an [`Self::make_isolate_future`], if this is one.
    pub fn isolate_future_id(self) -> Option<u32> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::IsolateFuture(id) => Some(*id),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A **leaf async-read future** (Track A.4c): `fs.read_async(path)` produces one, carrying the id
    /// that tickets the pending read in the injected [`noeta_stdlib::Executor`]. Polled by consulting
    /// the executor (see [`Self::async_io_id`]); it wraps no closure.
    pub fn make_async_io(id: u64) -> Value {
        heap::alloc(Payload::AsyncIo(id))
    }

    /// The executor ticket id of a leaf async-read future, or `None` if this is not one.
    pub fn async_io_id(self) -> Option<u64> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::AsyncIo(id) => Some(*id),
            _ => None,
        })
    }

    /// Whether this is a leaf timer future.
    pub fn is_timer(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Timer(_)))
    }

    /// A **channel sender endpoint** (isolates I.1): the `Sender<T>` `channel::<T>(cap)` yields,
    /// carrying the channel's id into the backend's channel table. A GC leaf.
    pub fn make_sender(id: ChannelId) -> Value {
        heap::alloc(Payload::Sender(id))
    }

    /// A **channel receiver endpoint** (isolates I.1). A GC leaf like [`Self::make_sender`].
    pub fn make_receiver(id: ChannelId) -> Value {
        heap::alloc(Payload::Receiver(id))
    }

    /// The channel id of a sender endpoint, or `None` if this is not one.
    pub fn sender_id(self) -> Option<ChannelId> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Sender(id) => Some(*id),
            _ => None,
        })
    }

    /// The channel id of a receiver endpoint, or `None` if this is not one.
    pub fn receiver_id(self) -> Option<ChannelId> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Receiver(id) => Some(*id),
            _ => None,
        })
    }

    /// A **leaf channel-send future** (isolates I.1): `tx.send(v)` produces one, carrying the channel
    /// `id` and **retaining its own reference** to the message `value` (like [`Self::make_future`] with
    /// its closure) — held until the message is enqueued or the future is dropped. The caller's own
    /// reference to `value` is released by its normal end-of-life.
    pub fn make_channel_send(id: ChannelId, value: Value) -> Value {
        value.inc_ref();
        heap::alloc(Payload::ChannelSend(id, value))
    }

    /// A **leaf channel-recv future** (isolates I.1): `rx.recv()` produces one, carrying the channel id.
    pub fn make_channel_recv(id: ChannelId) -> Value {
        heap::alloc(Payload::ChannelRecv(id))
    }

    /// The channel id and a freshly-retained owning reference to the queued message of a channel-send
    /// future, or `None` if this is not one. The caller takes ownership of the returned message.
    pub fn channel_send_parts(self) -> Option<(ChannelId, Value)> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::ChannelSend(id, value) => {
                value.inc_ref();
                Some((*id, *value))
            }
            _ => None,
        })
    }

    /// The channel id of a channel-recv future, or `None` if this is not one.
    pub fn channel_recv_id(self) -> Option<ChannelId> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::ChannelRecv(id) => Some(*id),
            _ => None,
        })
    }

    /// A **task handle** (Track A.3b): the `Future<T>` `spawn e` returns, referencing a task by its
    /// `(scope index, task index)` in the backend's concurrency-scope stack. A GC leaf.
    pub fn make_handle(scope: ScopeId, task: TaskId) -> Value {
        heap::alloc(Payload::Handle(scope, task))
    }

    /// Whether this is a task handle.
    pub fn is_handle(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Handle(..)))
    }

    /// The `(scope index, task index)` a task handle references, or `None` if this is not a handle.
    pub fn handle_parts(self) -> Option<(ScopeId, TaskId)> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Handle(scope, task) => Some((*scope, *task)),
            _ => None,
        })
    }

    /// The absolute logical deadline (ms) of a leaf timer future, or `None` if this is not a timer.
    pub fn timer_deadline(self) -> Option<u64> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Timer(deadline) => Some(*deadline),
            _ => None,
        })
    }

    /// The thunk/step closure a future wraps — a freshly-retained owning reference the caller drives
    /// via the backend's call machinery (Track A). `None` if this is not a future.
    pub fn future_step(self) -> Option<Value> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Future(step) => {
                step.inc_ref();
                Some(*step)
            }
            _ => None,
        })
    }

    /// Advance the iterator, returning the next element — a freshly-retained owning reference the
    /// caller takes ownership of — or `None` at end. The caller must have checked [`Value::is_iter`].
    ///
    /// `apply(func, arg)` runs a `map`/`filter` closure on an element (consuming `arg`'s reference,
    /// returning an owned result), letting the closure adapters call back into the backend's call
    /// machinery (Track I.1c); a closure-free pipeline never reaches it. The generic `E` is the
    /// backend's own call-error type, surfaced through [`IterAbort::Closure`].
    ///
    /// **Borrow discipline (soundness):** an adapter reads its [`IterShape`] under a *short* borrow,
    /// then recurses into its source / runs the closure with **no** borrow held on this node, and
    /// finally writes any cursor change under another short borrow. So even if the user closure
    /// re-enters this same iterator, no live `&mut` to the node is aliased (miri-verified). Each
    /// source is also a distinct allocation (an iterator can never be its own source).
    pub fn iter_next_apply<E>(
        self,
        apply: &mut dyn FnMut(Value, Value) -> Result<Value, E>,
    ) -> Result<Option<Value>, IterAbort<E>> {
        loop {
            let shape = heap::with_payload(self, |p| match p {
                Payload::Iter(state) => Some(state.shape()),
                _ => None,
            });
            let Some(shape) = shape else {
                return Ok(None);
            };
            match shape {
                // No recursion and no user code: the cursor is read and advanced under one short
                // borrow. `list_get` shares the list's reference; retain it for the new owner.
                IterShape::List => {
                    return Ok(heap::with_payload_mut(self, |p| {
                        let Payload::Iter(IterState::List { list, cursor }) = p else {
                            return None;
                        };
                        let e = list.list_get(*cursor)?;
                        *cursor += 1;
                        e.inc_ref();
                        Some(e)
                    }));
                }
                IterShape::Take { source, remaining } => {
                    if remaining == 0 {
                        return Ok(None);
                    }
                    return Ok(match source.iter_next_apply(apply)? {
                        Some(e) => {
                            heap::with_payload_mut(self, |p| {
                                if let Payload::Iter(IterState::Take { remaining, .. }) = p {
                                    *remaining -= 1;
                                }
                            });
                            Some(e)
                        }
                        None => None,
                    });
                }
                IterShape::Drop { source, pending } => {
                    if pending > 0 {
                        match source.iter_next_apply(apply)? {
                            Some(skipped) => {
                                skipped.release(); // the skipped element's retained reference
                                heap::with_payload_mut(self, |p| {
                                    if let Payload::Iter(IterState::Drop { pending, .. }) = p {
                                        *pending -= 1;
                                    }
                                });
                                continue; // skip the next pending element
                            }
                            None => {
                                heap::with_payload_mut(self, |p| {
                                    if let Payload::Iter(IterState::Drop { pending, .. }) = p {
                                        *pending = 0;
                                    }
                                });
                                return Ok(None);
                            }
                        }
                    }
                    return source.iter_next_apply(apply);
                }
                IterShape::Chain { first, second } => {
                    if let Some(e) = first.iter_next_apply(apply)? {
                        return Ok(Some(e));
                    }
                    return second.iter_next_apply(apply);
                }
                // The source's element (already retained) and the immediate index are handed to the
                // new tuple, which takes ownership of one reference to each.
                IterShape::Enumerate { source, index } => {
                    return Ok(match source.iter_next_apply(apply)? {
                        Some(e) => {
                            let tuple = Value::tuple(vec![Value::int(index as i64), e]);
                            heap::with_payload_mut(self, |p| {
                                if let Payload::Iter(IterState::Enumerate { index, .. }) = p {
                                    *index += 1;
                                }
                            });
                            Some(tuple)
                        }
                        None => None,
                    });
                }
                // Pull from both, shorter wins. If `a` ran dry there is nothing to release; if only
                // `b` did, release `a`'s already-retained element so it does not leak.
                IterShape::Zip { a, b } => {
                    let Some(ea) = a.iter_next_apply(apply)? else {
                        return Ok(None);
                    };
                    return Ok(match b.iter_next_apply(apply)? {
                        Some(eb) => Some(Value::tuple(vec![ea, eb])),
                        None => {
                            ea.release();
                            None
                        }
                    });
                }
                // `apply` consumes the source element's reference and returns the mapped result (owned).
                // On a closure error the call already consumed that reference, so nothing leaks here.
                IterShape::Map { source, func } => {
                    let Some(e) = source.iter_next_apply(apply)? else {
                        return Ok(None);
                    };
                    return apply(func, e).map(Some).map_err(IterAbort::Closure);
                }
                // Retain the element once for the predicate call (which consumes a reference) and keep
                // one to hand back if it passes. On a closure error release the held reference; a
                // non-bool verdict is a typed abort the backend phrases as a diagnostic.
                IterShape::Filter { source, pred } => {
                    let Some(e) = source.iter_next_apply(apply)? else {
                        return Ok(None);
                    };
                    e.inc_ref();
                    let verdict = match apply(pred, e) {
                        Ok(v) => v,
                        Err(err) => {
                            e.release();
                            return Err(IterAbort::Closure(err));
                        }
                    };
                    match verdict.as_bool() {
                        Some(true) => {
                            verdict.release();
                            return Ok(Some(e));
                        }
                        Some(false) => {
                            verdict.release();
                            e.release();
                            continue; // try the next source element
                        }
                        None => {
                            let name = verdict.type_name();
                            verdict.release();
                            e.release();
                            return Err(IterAbort::FilterNotBool(name));
                        }
                    }
                }
                // A generator (Track G): run the step closure (one resume arg, here unit) and
                // interpret its returned `?T`. `option_take` consumes the returned Option wrapper.
                IterShape::Gen { step } => {
                    let opt = match apply(step, Value::unit()) {
                        Ok(v) => v,
                        Err(err) => return Err(IterAbort::Closure(err)),
                    };
                    return Ok(opt.option_take());
                }
            }
        }
    }

    /// Deconstruct an `Option` value a generator step returned: `some(x)` → `Some(x)` (the payload
    /// retained for the new owner), `none`/anything else → `None`. Consumes one reference to `self`
    /// (the Option wrapper).
    fn option_take(self) -> Option<Value> {
        if !self.is_pointer() {
            return None;
        }
        let extracted = heap::with_payload(self, |p| match p {
            Payload::Enum { shape, data } if shape.variant.as_deref() == Some("some") => {
                data.first().copied()
            }
            _ => None,
        });
        if let Some(x) = extracted {
            x.inc_ref(); // retain for the new owner before the wrapper is released
        }
        self.release(); // drop the Option wrapper (its `some` payload now survives via the bump above)
        extracted
    }

    /// Advance a **closure-free** iterator (no `map`/`filter` in the pipeline). The caller must have
    /// checked [`Value::is_iter`]; reaching a closure adapter without an applier panics. Used by the
    /// closure-free terminals below and the unit tests.
    pub fn iter_next(self) -> Option<Value> {
        let mut applier = |_: Value, _: Value| -> Result<Value, ()> {
            unreachable!("closure-free iterator reached a map/filter adapter without an applier")
        };
        match self.iter_next_apply(&mut applier) {
            Ok(v) => v,
            Err(_) => unreachable!("closure-free pipeline cannot abort"),
        }
    }

    /// Drain a closure-free iterator from its current cursor into a new list — each element retained
    /// into it. The caller must have checked [`Value::is_iter`].
    pub fn iter_collect(self) -> Value {
        let mut out = Vec::new();
        while let Some(e) = self.iter_next() {
            out.push(e);
        }
        Value::list(out)
    }

    /// Drain a closure-free iterator, summing its numeric elements (Track I.1b.2) — `int` if every
    /// element is an `int`, else `float`. Mirrors the eager `sum` builtin's accumulation exactly so
    /// the two paths agree. Each drained element's retained reference is released; on the first
    /// non-numeric element it is dropped and its type name returned as `Err` for the caller's
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

    /// A selectively-imported native-module function value (refcount 1), e.g. `sqrt` from
    /// `use std.math.sqrt` — the `(module, func)` pair to hand to `call_native_module`.
    pub fn module_fn(module: &str, func: &str) -> Value {
        heap::alloc(Payload::ModuleFn {
            module: module.to_string(),
            func: func.to_string(),
        })
    }

    /// The `(module, func)` pair, if this is a selectively-imported native-module function value.
    pub fn module_fn_parts(self) -> Option<(String, String)> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::ModuleFn { module, func } => Some((module.clone(), func.clone())),
                _ => None,
            })
        } else {
            None
        }
    }

    /// An unbound method handle value (refcount 1) — `Type.method` as a value.
    pub fn method_handle(ty: &str, method: &str, associated: bool) -> Value {
        heap::alloc(Payload::MethodHandle {
            ty: ty.to_string(),
            method: method.to_string(),
            associated,
        })
    }

    /// A **bound** method handle (refcount 1): `value.method` with the receiver captured
    /// (prelude-redesign EX.2b). Takes ownership of one reference to `recv`.
    pub fn bound_method(recv: Value, method: &str) -> Value {
        heap::alloc(Payload::BoundMethod {
            recv,
            method: method.to_string(),
        })
    }

    /// The `(receiver, method)` pair, if this is a bound method handle. The receiver is returned
    /// borrowed (no refcount change).
    pub fn bound_method_parts(self) -> Option<(Value, String)> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::BoundMethod { recv, method } => Some((*recv, method.clone())),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The `(ty, method, associated)` triple, if this is an unbound method handle value.
    pub fn method_handle_parts(self) -> Option<(String, String, bool)> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::MethodHandle {
                    ty,
                    method,
                    associated,
                } => Some((ty.clone(), method.clone(), *associated)),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A heap map (refcount 1), keyed by owned strings, presenting in sorted-key order. As with
    /// [`Value::list`], the map takes ownership of one reference to each value. The caller passes a
    /// `BTreeMap` (a convenient sorted builder); it is stored internally as a `HashMap` for O(1)
    /// access, and every order-observing accessor re-sorts, so nothing observable changes.
    pub fn map(entries: BTreeMap<String, Value>) -> Value {
        heap::alloc(Payload::Map(
            entries
                .into_iter()
                .map(|(k, v)| (noeta_stdlib::MapKey::from(k), v))
                .collect(),
        ))
    }

    /// A heap map from already-built keys (extern-types X4) — the `MakeMap`/extern-key path.
    /// Later duplicates win (insertion order), matching the string builder's BTreeMap semantics.
    pub fn map_keyed(entries: Vec<(noeta_stdlib::MapKey, Value)>) -> Value {
        heap::alloc(Payload::Map(entries.into_iter().collect()))
    }

    /// A heap object (refcount 1): a struct/class/opaque instance laying out `slots` in the
    /// `shape`'s field order. The object takes ownership of one reference to each slot value.
    pub fn object(shape: &'static Shape, slots: Vec<Value>) -> Value {
        heap::alloc(Payload::Object { shape, slots })
    }

    /// A heap enum value (refcount 1): a `(enum, variant)` instance carrying the variant's
    /// positional `data`. The value takes ownership of one reference to each data element.
    pub fn enum_value(shape: &'static Shape, data: Vec<Value>) -> Value {
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

    /// Whether this is the async pending sentinel (Track A.3).
    pub fn is_pending(self) -> bool {
        self.0 == Value::pending().0
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
                Payload::Str(s) => Some(s.as_str().to_owned()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A [`CompactString`] clone of the string value, if this is a heap string. Unlike
    /// [`Self::as_string`], inline content (≤ 24 bytes) clones without touching the allocator —
    /// use for map keys, which are stored in this representation.
    pub fn as_compact_string(self) -> Option<CompactString> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => Some(s.clone()),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Call `f` with a **borrowed** `&str` view of this value's string content — no clone, unlike
    /// [`Self::as_string`]. Returns `Some(f(..))` if this is a heap string, else `None`. Use for
    /// read-only string work (a `HashMap<String, _>` lookup by `&str`, a comparison) where an owned
    /// `String` would be pure waste.
    pub fn with_str<R>(self, f: impl FnOnce(&str) -> R) -> Option<R> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => Some(f(s.as_str())),
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

    /// Whether this is a heap string.
    pub fn is_string(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Str(_)))
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
            !self.is_set() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "set_insert_sorted requires a uniquely-owned set (the COW invariant)"
        );
        if self.is_set() {
            let inserted = heap::with_payload_mut(self, |p| match p {
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
            });
            // Content-changing → drop the reflected type tag (R1); see `list_extend`.
            heap::set_reflect(self, None);
            inserted
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
            !self.is_set() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "set_remove_sorted requires a uniquely-owned set (the COW invariant)"
        );
        if self.is_set() {
            let removed = heap::with_payload_mut(self, |p| match p {
                Payload::Set(items) => match items.binary_search_by(|&item| {
                    compare_primitive(item, target).unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    Ok(pos) => Some(items.remove(pos)),
                    Err(_) => None,
                },
                _ => None,
            });
            // Content-changing → drop the reflected type tag (R1); see `list_extend`.
            heap::set_reflect(self, None);
            removed
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

    /// The value for an owned [`MapKey`] probe — the packed-key lane (P-PKEY), where the key was
    /// just built from a value's content. Same sharing contract as [`Value::map_get`].
    ///
    /// [`MapKey`]: noeta_stdlib::MapKey
    pub fn map_get_key(self, key: &noeta_stdlib::MapKey) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => entries.get(key).copied(),
                _ => None,
            })
        } else {
            None
        }
    }

    /// Remove an owned [`MapKey`] (the packed-key lane, P-PKEY), returning the displaced value
    /// (ownership transfers to the caller). Mirrors [`Value::map_remove`].
    ///
    /// [`MapKey`]: noeta_stdlib::MapKey
    pub fn map_remove_key(self, key: &noeta_stdlib::MapKey) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.remove(key),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The value for an extern-type `key`, if this is a map containing it (extern-types X4).
    /// Probes through the extern contract with no key allocation. Same sharing contract as
    /// [`Value::map_get`].
    pub fn map_get_extern(self, key: &dyn noeta_stdlib::ExternValue) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => entries.get(&noeta_stdlib::ExternKeyRef(key)).copied(),
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
            self.is_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
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
        // A content-changing op yields a logically new list: drop the reflected type tag (R1) so the
        // reused node does not carry the original literal's element type. The tag survives pure
        // aliasing only — matching the tree-walker, which produces a fresh untagged list here.
        heap::set_reflect(self, None);
    }

    /// Move this string's buffer out, leaving it empty. Requires sole ownership (`refcount() == 1`)
    /// and a single-use value (the caller must not read it again) — used to hand a freshly-built
    /// map key straight to the `HashMap` instead of cloning it. The now-empty `Payload::Str` is a
    /// valid, cheap-to-free object, so the caller's later `Drop`/overwrite of the register is sound.
    pub fn take_string_in_place(self) -> CompactString {
        debug_assert!(
            self.is_string() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "take_string_in_place requires a uniquely-owned string"
        );
        heap::with_payload_mut(self, |p| {
            if let Payload::Str(buf) = p {
                std::mem::take(buf)
            } else {
                CompactString::default()
            }
        })
    }

    /// Append `s` to this string's buffer in place. Requires sole ownership (the COW invariant), so
    /// the caller must have checked `refcount() == 1` — this is what turns a `s = s ~ x` accumulator
    /// loop from O(n²) copies into amortized O(n) (`String`'s geometric growth), mirroring
    /// [`Self::list_extend`] for lists.
    pub fn str_push_in_place(self, s: &str) {
        debug_assert!(
            self.is_string() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "str_push_in_place requires a uniquely-owned string (the COW invariant)"
        );
        heap::with_payload_mut(self, |p| {
            if let Payload::Str(buf) = p {
                buf.push_str(s);
            }
        });
    }

    /// Push one `element` onto this boxed list's backing buffer **in place**, taking ownership of the
    /// caller's reference (no retain — the caller hands over one reference). The caller must guarantee
    /// a uniquely-owned list (`refcount == 1`). Used by the packed-list streaming demote fall-back
    /// (P-PACK 2.5). No-op if this is not a boxed list.
    pub fn list_push(self, element: Value) {
        debug_assert!(
            self.is_list() && heap::refcount(self) == 1 && !heap::is_shared(self),
            "list_push requires a uniquely-owned list (the COW invariant)"
        );
        heap::with_payload_mut(self, |p| {
            if let Payload::List(items) = p {
                items.push(element);
            }
        });
        // Content-changing → drop the reflected type tag (R1); see `list_extend`.
        heap::set_reflect(self, None);
    }

    /// Overwrite list slot `index` **in place** with `value`, returning the displaced value (whose
    /// reference is handed back to the caller to release). The caller must guarantee a uniquely-owned
    /// list (`refcount == 1`) and an in-range `index` — the copy-on-write `xs[i] = v` fast path:
    /// overwriting one slot of the existing buffer is O(1), versus cloning the whole list. Returns
    /// `unit` (a no-op) if this is not a list or `index` is out of range.
    pub fn list_replace_slot(self, index: usize, value: Value) -> Value {
        debug_assert!(
            !self.is_list() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "list_replace_slot requires a uniquely-owned list (the COW invariant)"
        );
        if self.is_pointer() {
            let displaced = heap::with_payload_mut(self, |p| match p {
                Payload::List(items) if index < items.len() => {
                    std::mem::replace(&mut items[index], value)
                }
                _ => Value::unit(),
            });
            // Content-changing → drop the reflected type tag (R1); see `list_extend`.
            heap::set_reflect(self, None);
            displaced
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
                Payload::Map(entries) => {
                    // Sorted-key order (the map is a HashMap internally); the shared `MapKey`
                    // order, identical to the tree-walker's BTreeMap iteration.
                    let mut kv: Vec<(&noeta_stdlib::MapKey, &Value)> = entries.iter().collect();
                    kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
                    Some(kv.into_iter().map(|(_, v)| *v).collect())
                }
                _ => None,
            })
        } else {
            None
        }
    }

    /// A map's keys in sorted order, if this is a map. Keys are plain owned [`MapKey`]s (never
    /// heap values — an extern key owns its box inline), so no refcounting is involved.
    pub fn map_keys(self) -> Option<Vec<noeta_stdlib::MapKey>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => {
                    let mut keys: Vec<noeta_stdlib::MapKey> = entries.keys().cloned().collect();
                    keys.sort_unstable();
                    Some(keys)
                }
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
    pub fn map_insert(self, key: noeta_stdlib::MapKey, value: Value) -> Option<Value> {
        debug_assert!(
            !self.is_map() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "map_insert requires a uniquely-owned map (the COW invariant)"
        );
        if self.is_map() {
            let displaced = heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.insert(key, value),
                _ => None,
            });
            // Content-changing → drop the reflected type tag (R1); see `list_extend`.
            heap::set_reflect(self, None);
            displaced
        } else {
            None
        }
    }

    /// Remove `key` from this map's backing buffer **in place**, returning the removed value (if
    /// present). Same uniqueness requirement and reference-handback contract as [`Value::map_insert`].
    pub fn map_remove(self, key: &str) -> Option<Value> {
        debug_assert!(
            !self.is_map() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "map_remove requires a uniquely-owned map (the COW invariant)"
        );
        if self.is_map() {
            let removed = heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.remove(key),
                _ => None,
            });
            // Content-changing → drop the reflected type tag (R1); see `list_extend`.
            heap::set_reflect(self, None);
            removed
        } else {
            None
        }
    }

    /// Remove an extern-type `key` **in place** (extern-types X4) — the extern twin of
    /// [`Value::map_remove`], same uniqueness requirement and handback contract.
    pub fn map_remove_extern(self, key: &dyn noeta_stdlib::ExternValue) -> Option<Value> {
        debug_assert!(
            !self.is_map() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "map_remove_extern requires a uniquely-owned map (the COW invariant)"
        );
        if self.is_map() {
            let removed = heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.remove(&noeta_stdlib::ExternKeyRef(key)),
                _ => None,
            });
            heap::set_reflect(self, None);
            removed
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
                // Collect the internal HashMap into a sorted BTreeMap (the return type callers
                // rely on for deterministic, sorted iteration). STRING view: an extern-keyed
                // entry presents its key's canonical display form (isolate marshalling is gated
                // to string-keyed maps by E0042 anyway; JSON keys are strings by definition).
                Payload::Map(entries) => Some(
                    entries
                        .iter()
                        .map(|(k, v)| (k.as_native_str(), *v))
                        .collect(),
                ),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A shallow clone of a map's full `MapKey → value` entries in sorted-key order
    /// (extern-types X4) — the keyed twin of [`Value::map_entries`], for derived-map rebuilds
    /// that must preserve extern keys. Values share references (not retained), like
    /// [`Value::map_entries`].
    pub fn map_entries_keyed(self) -> Option<Vec<(noeta_stdlib::MapKey, Value)>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => {
                    let mut kv: Vec<(noeta_stdlib::MapKey, Value)> =
                        entries.iter().map(|(k, v)| (k.clone(), *v)).collect();
                    kv.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                    Some(kv)
                }
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
    pub fn shape(self) -> Option<&'static Shape> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { shape, .. } | Payload::Enum { shape, .. } => Some(*shape),
                _ => None,
            })
        } else {
            None
        }
    }

    /// The object's shape **identity** as a raw pointer, without bumping the `Rc` refcount — the
    /// cheap key for an inline-cache hit test (`shape_ptr() == Some(Arc::as_ptr(&cached))`). The
    /// pointer is only valid while a live reference to the shape exists; the VM's cache holds an
    /// `&'static Shape` clone to keep the cached shape alive, so a hit comparison can never alias a freed
    /// shape. `None` for a non-object (an enum or a scalar).
    pub fn object_shape_ptr(self) -> Option<*const Shape> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { shape, .. } => Some(std::ptr::from_ref::<Shape>(shape)),
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

    /// The [`MapKey`] for a **key-capable `@packed` struct** value (P-PKEY), or `None` when this
    /// value is not one. Walks the fields in declaration order into plain
    /// [`noeta_stdlib::PackedKeyField`] data — the erased integer word (immediate or boxed),
    /// bools, nested key-capable structs — plus the display form (render/JSON only, not
    /// identity). Both backends build keys from the same declarations, so identity, hash, and
    /// order agree by construction. The key is a snapshot holding no heap reference (`@packed`
    /// is value semantics, so it can never drift from an aliased original).
    pub fn packed_map_key(self) -> Option<noeta_stdlib::MapKey> {
        let shape = self.shape()?;
        if !shape.key_capable {
            return None;
        }
        Some(noeta_stdlib::MapKey::packed(
            &shape.name,
            self.packed_key_fields()?,
        ))
    }

    /// The [`packed_map_key`](Value::packed_map_key) field walk. `None` on a slot the capability
    /// contract excludes — defensive: a `key_capable` shape's slots are ints/bools/nested-capable
    /// by construction, so `None` here means a compiler bug, and the caller falls back to the
    /// ordinary key error rather than corrupting a map.
    fn packed_key_fields(self) -> Option<Vec<noeta_stdlib::PackedKeyField>> {
        if !self.is_pointer() {
            return None;
        }
        // Borrow the slots in place — a key build is the hot map/set path, so no Vec clone.
        heap::with_payload(self, |p| {
            let Payload::Object { slots, .. } = p else {
                return None;
            };
            slots
                .iter()
                .map(|v| {
                    if let Some(b) = v.as_bool() {
                        Some(noeta_stdlib::PackedKeyField::Bool(b))
                    } else if let Some(i) = v.as_int() {
                        Some(noeta_stdlib::PackedKeyField::Int(i))
                    } else {
                        let shape = v.shape()?;
                        if !shape.key_capable {
                            return None;
                        }
                        Some(noeta_stdlib::PackedKeyField::Struct(
                            shape.name.as_str().into(),
                            v.packed_key_fields()?.into_boxed_slice(),
                        ))
                    }
                })
                .collect()
        })
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

    /// Fill `out` (cleared first) with this object's primitive fields in slot (declared) order —
    /// the allocation-free shallow scalar projection under the ctx element loops (package-manager
    /// N3.4). `false` for a non-object or any non-primitive field (with `out` left cleared).
    pub fn scalar_slots_into(self, out: &mut Vec<noeta_stdlib::Scalar>) -> bool {
        use noeta_stdlib::Scalar;
        out.clear();
        if !self.is_pointer() {
            return false;
        }
        heap::with_payload(self, |p| match p {
            Payload::Object { slots, .. } => {
                for s in slots {
                    let scalar = if let Some(n) = s.as_int() {
                        Scalar::Int(n)
                    } else if let Some(f) = s.as_f32() {
                        Scalar::F32(f)
                    } else if let Some(f) = s.as_float() {
                        Scalar::Float(f)
                    } else if let Some(b) = s.as_bool() {
                        Scalar::Bool(b)
                    } else {
                        out.clear();
                        return false;
                    };
                    out.push(scalar);
                }
                true
            }
            _ => false,
        })
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
    /// Append this value's [`display`](Self::display) form to `out` **without** the intermediate
    /// `String` that `push_str(&self.display())` would allocate. Fast paths cover the values that
    /// dominate string interpolation — a heap string (append its bytes, no clone), a small int, and a
    /// bool — and everything else falls back to `display()`, so the rendering is byte-identical.
    pub fn display_into(self, out: &mut CompactString) {
        if let Some(b) = self.as_bool() {
            out.push_str(if b { "true" } else { "false" });
        } else if self.is_small_int() {
            // `itoa`, not `write!`: the `fmt::Formatter` round-trip costs about as much as the
            // digits themselves on the short ints interpolation overwhelmingly renders.
            out.push_str(itoa::Buffer::new().format(self.as_int().unwrap()));
        } else if self.is_pointer() {
            let handled = heap::with_payload(self, |p| match p {
                Payload::Str(s) => {
                    out.push_str(s);
                    true
                }
                Payload::Int(i) => {
                    out.push_str(itoa::Buffer::new().format(*i));
                    true
                }
                _ => false,
            });
            if !handled {
                out.push_str(&self.display());
            }
        } else {
            out.push_str(&self.display());
        }
    }

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
            noeta_stdlib::format_float(self.as_float().unwrap())
        } else if self.is_f32() {
            // An immediate `f32` displays at f32 precision, byte-identical to the tree-walker.
            noeta_stdlib::format_f32(self.as_f32().unwrap())
        } else if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Str(s) => s.as_str().to_owned(),
                // A byte buffer renders as a length summary (`<N bytes>`) — opaque and identical on
                // both backends; its content round-trips through `from_bytes`, not display.
                Payload::Bytes(b) => format!("<{} bytes>", b.len()),
                Payload::Int(i) => i.to_string(),
                // Mirrors the M0 tree-walker's `Value::Function(_) => "<fn>"` (and `Builtin`).
                Payload::Closure { .. }
                | Payload::NativeFn(_)
                | Payload::ModuleFn { .. }
                | Payload::MethodHandle { .. }
                | Payload::BoundMethod { .. } => "<fn>".to_string(),
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
                    let mut kv: Vec<(&noeta_stdlib::MapKey, &Value)> = entries.iter().collect();
                    kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
                    // A string key keeps its quoted `{k:?}` form; an extern key renders its
                    // display form unquoted (`MapKey::render` — the shared contract).
                    let parts: Vec<String> = kv
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k.render(), v.repr()))
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
                    // Display strips a qualified identity to its short name (`App.Models.User` →
                    // `User`); the identity keyed on for dispatch/`is`/`as` stays qualified.
                    format!(
                        "{} {{{}}}",
                        noeta_ast::short_type_name(&shape.name),
                        parts.join(", ")
                    )
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
                            noeta_ast::short_type_name(&shape.name),
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
                // An extern-type value renders through its contract, identically on both backends.
                Payload::Extern(e) => e.display_string(),
                // An iterator is an opaque reference value (like a file handle).
                Payload::Iter { .. } => "<iterator>".to_string(),
                // A future — step, leaf timer, task handle, async-read, or channel op — is an opaque
                // reference.
                Payload::Future(_)
                | Payload::Timer(_)
                | Payload::Handle(..)
                | Payload::AsyncIo(_)
                | Payload::ChannelSend(..)
                | Payload::ChannelRecv(_)
                | Payload::IsolateFuture(_) => "<future>".to_string(),
                // Channel endpoints are opaque reference values (like an iterator/file handle).
                Payload::Sender(_) => "<sender>".to_string(),
                Payload::Receiver(_) => "<receiver>".to_string(),
                // Handled by the early return at the top of `display`.
                Payload::PackedList { .. } => unreachable!("packed list demoted before display"),
            })
        } else {
            // The unit value (and any other singleton) displays as empty, as in M0.
            String::new()
        }
    }

    /// The JSON encoding synthesized by `@derive(ToJson)` (and `json.stringify`). Marshals the value
    /// into the neutral [`noeta_stdlib::NativeValue`] tree (see [`Self::to_native_deep`]) and runs the
    /// shared [`noeta_stdlib::json::stringify`], so the tree-walker — driving the same walk over its
    /// own marshalled tree — produces byte-identical output by construction.
    pub fn to_json(self) -> String {
        noeta_stdlib::json::stringify(&self.to_native_deep())
    }

    /// Deeply marshal this value into the neutral [`noeta_stdlib::NativeValue`] tree the shared JSON
    /// serializer ([`noeta_stdlib::json::stringify`]) consumes — the VM half of `json.stringify` and
    /// `@derive(Serialize<Json>)`. Numbers become scalars; strings, enum variants, and the opaque
    /// length/`<fn>`/`<module …>` summaries become [`NativeValue::Str`]; lists/tuples/sets become a
    /// [`NativeValue::List`]; maps and objects a [`NativeValue::Map`]. Read-only — it never changes a
    /// refcount (a packed list materializes a temporary that is released here).
    pub fn to_native_deep(self) -> noeta_stdlib::NativeValue {
        use noeta_stdlib::{NativeValue, Scalar};
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
                Payload::Str(s) => NativeValue::Str(s.as_str().to_owned()),
                // A byte buffer has no JSON representation (it is the *binary* alternative): a length
                // summary string, so `json.stringify` never panics.
                Payload::Bytes(b) => NativeValue::Str(format!("<{} bytes>", b.len())),
                // An extern-type value marshals as itself; the shared serializer renders its
                // display form as a JSON string (a `Uuid` is its canonical string).
                Payload::Extern(e) => NativeValue::Extern(e.clone()),
                Payload::Int(i) => NativeValue::Scalar(Scalar::Int(*i)),
                // Lists, tuples, and sets all serialize as a JSON array (JSON has neither tuple nor
                // set), so they marshal to one neutral list.
                Payload::List(items) | Payload::Tuple(items) | Payload::Set(items) => {
                    NativeValue::List(items.iter().map(|v| v.to_native_deep()).collect())
                }
                Payload::Map(entries) => {
                    // NativeValue::Map is an ordered Vec; present in sorted-key order. An extern
                    // key marshals as its canonical display form (JSON keys are strings).
                    let mut kv: Vec<(&noeta_stdlib::MapKey, &Value)> = entries.iter().collect();
                    kv.sort_unstable_by(|a, b| a.0.cmp(b.0));
                    NativeValue::Map(
                        kv.into_iter()
                            .map(|(k, v)| (k.as_native_str(), v.to_native_deep()))
                            .collect(),
                    )
                }
                Payload::Object { shape, slots } => NativeValue::Map(
                    shape
                        .fields
                        .iter()
                        .zip(slots)
                        .map(|(name, v)| (name.clone(), v.to_native_deep()))
                        .collect(),
                ),
                Payload::Closure { .. }
                | Payload::NativeFn(_)
                | Payload::ModuleFn { .. }
                | Payload::MethodHandle { .. }
                | Payload::BoundMethod { .. } => NativeValue::Str("<fn>".to_string()),
                Payload::Cell(inner) => inner.to_native_deep(),
                Payload::Enum { shape, .. } => {
                    NativeValue::Str(shape.variant.as_deref().unwrap_or(&shape.name).to_string())
                }
                Payload::NativeModule(name) => NativeValue::Str(format!("<module {name}>")),
                // An iterator has no JSON analog either — its opaque display form.
                Payload::Iter { .. } => NativeValue::Str("<iterator>".to_string()),
                // A future has no JSON analog — its opaque display form.
                Payload::Future(_)
                | Payload::Timer(_)
                | Payload::Handle(..)
                | Payload::AsyncIo(_)
                | Payload::ChannelSend(..)
                | Payload::ChannelRecv(_)
                | Payload::IsolateFuture(_) => NativeValue::Str("<future>".to_string()),
                // Channel endpoints have no JSON analog — their opaque display form.
                Payload::Sender(_) => NativeValue::Str("<sender>".to_string()),
                Payload::Receiver(_) => NativeValue::Str("<receiver>".to_string()),
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

    /// The kind of this value's heap payload (`None` for immediates) — a cheap `Copy`
    /// discriminant for one-dereference dispatch. A dispatch ladder that probes candidate
    /// receiver types in sequence (`is_map()`, `is_list()`, `as_string()`, …) pays a heap
    /// dereference per probe; classifying once and comparing kinds turns every subsequent
    /// rung into an integer compare. Note the mapping is variant-exact: `is_list()` is
    /// `List | PackedList`, so a caller replacing it must test both kinds.
    #[inline]
    pub fn heap_kind(self) -> Option<HeapKind> {
        if !self.is_pointer() {
            return None;
        }
        Some(heap::with_payload(self, |p| match p {
            Payload::Str(_) => HeapKind::Str,
            Payload::Bytes(_) => HeapKind::Bytes,
            Payload::Extern(_) => HeapKind::Extern,
            Payload::Int(_) => HeapKind::Int,
            Payload::Closure { .. } => HeapKind::Closure,
            Payload::Cell(_) => HeapKind::Cell,
            Payload::List(_) => HeapKind::List,
            Payload::Tuple(_) => HeapKind::Tuple,
            Payload::Set(_) => HeapKind::Set,
            Payload::Map(_) => HeapKind::Map,
            Payload::PackedList { .. } => HeapKind::PackedList,
            Payload::Object { .. } => HeapKind::Object,
            Payload::Enum { .. } => HeapKind::Enum,
            Payload::ModuleFn { .. } => HeapKind::ModuleFn,
            Payload::MethodHandle { .. } => HeapKind::MethodHandle,
            Payload::BoundMethod { .. } => HeapKind::BoundMethod,
            Payload::NativeModule(_) => HeapKind::NativeModule,
            Payload::NativeFn(_) => HeapKind::NativeFn,
            Payload::Iter(_) => HeapKind::Iter,
            Payload::Future(_) => HeapKind::Future,
            Payload::Timer { .. } => HeapKind::Timer,
            Payload::Handle { .. } => HeapKind::Handle,
            Payload::AsyncIo { .. } => HeapKind::AsyncIo,
            Payload::Sender(_) => HeapKind::Sender,
            Payload::Receiver(_) => HeapKind::Receiver,
            Payload::ChannelSend { .. } => HeapKind::ChannelSend,
            Payload::ChannelRecv { .. } => HeapKind::ChannelRecv,
            Payload::IsolateFuture { .. } => HeapKind::IsolateFuture,
        }))
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
            if self.as_closure().is_some()
                || self.as_native_fn().is_some()
                || self.module_fn_parts().is_some()
                || self.method_handle_parts().is_some()
                || self.bound_method_parts().is_some()
            {
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
            } else if self.is_iter() {
                "iterator"
            } else if self.is_future() {
                "future"
            } else if self.sender_id().is_some() {
                "sender"
            } else if self.receiver_id().is_some() {
                "receiver"
            } else if self.is_bytes() {
                "bytes"
            } else if self.is_extern() {
                // The registered extern type's own name (`Uuid`), from the value contract.
                self.with_extern(|e| e.type_name())
            } else {
                "string"
            }
        } else {
            "unit"
        }
    }

    // --- Refcount management (the GC policy layer lives in `noeta-gc`) ---

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

    /// Whether this is a **borrow-shared** heap object (isolates I.3) — one promoted into a
    /// [`SharedRegion`] and reachable read-only from other isolates, on which `retain`/`release`
    /// no-op. `false` for immediates and ordinary (local) objects.
    pub fn is_shared(self) -> bool {
        self.is_pointer() && heap::is_shared(self)
    }

    /// Whether this heap value may be **mutated in place** under the COW invariant: the caller
    /// holds the only reference (`refcount == 1`) *and* the object is not borrow-shared (P-PAR
    /// S2). A shared object's refcount is frozen at 1 (retain/release no-op), so a bare
    /// `refcount() == 1` test would wrongly treat a corpus borrowed from a [`SharedRegion`] as
    /// uniquely owned and mutate a buffer other isolate threads are reading — every in-place
    /// fast path must gate on this, never on `refcount()` alone.
    pub fn is_uniquely_owned(self) -> bool {
        self.is_pointer() && heap::refcount(self) == 1 && !heap::is_shared(self)
    }

    /// Whether this value's whole graph can be promoted into a [`SharedRegion`] (P-PAR S2) —
    /// `Send` **data** kinds only. A function value, bound method, or channel endpoint is
    /// `Wire`-shippable but not promotable, so an argument containing one keeps the copy path.
    pub fn is_promotable_graph(self) -> bool {
        heap::promotable_graph(self)
    }

    /// Increment the refcount (no-op for immediates, and for a borrow-shared object).
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

    // --- Cycle-collector primitives (the trial-deletion collector lives in `noeta-gc`) ---
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
    (Value::object(schema.shape, slots), at)
}

/// Gather element `index`'s fields (a buffer of `count` elements) into a fresh **row-order** byte
/// vector (fields contiguous, slot order) — the inverse of the column scatter (P-SIMD C2). For a
/// row-major buffer this simply copies the element's contiguous stride; for a column-major one it
/// pulls each field from its column. The row-order result feeds [`unpack_element`], so materializing
/// an element is layout-agnostic once gathered. Pure byte copies — no `Value` allocation, so it is
/// safe to call while a heap payload is borrowed.
fn gather_row(schema: &PackedSchema, bytes: &[u8], index: usize, count: usize) -> Vec<u8> {
    let mut row = Vec::with_capacity(schema.byte_size);
    for (slot, kind) in schema.fields.iter().enumerate() {
        let off = schema.field_offset(index, slot, count);
        row.extend_from_slice(&bytes[off..off + kind.byte_width()]);
    }
    row
}

/// Append one packed `row` (`byte_size` bytes, slot order) to a column-major buffer, rebuilding it so
/// each field's column gains the new element at its end (P-SIMD C2). O(n) — column layout trades
/// cheap append for fast bulk field math.
fn column_append(schema: &PackedSchema, buf: &[u8], row: &[u8]) -> Vec<u8> {
    let n = schema.count(buf.len());
    let mut out = Vec::with_capacity(buf.len() + schema.byte_size);
    let mut row_at = 0;
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let base = n * schema.field_prefix(slot);
        out.extend_from_slice(&buf[base..base + n * w]);
        out.extend_from_slice(&row[row_at..row_at + w]);
        row_at += w;
    }
    out
}

/// Build a new column-major buffer holding the selected `indices` of a column-major buffer of `count`
/// elements (P-SIMD C2) — each field's column is the gather of that field across the selected
/// elements. Mirrors [`Value::packed_select`]'s row-block copy for the column layout.
fn column_select(schema: &PackedSchema, buf: &[u8], indices: &[usize], count: usize) -> Vec<u8> {
    let m = indices.len();
    let mut out = vec![0u8; m * schema.byte_size];
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let new_base = m * schema.field_prefix(slot);
        for (j, &i) in indices.iter().enumerate() {
            let src = schema.field_offset(i, slot, count);
            out[new_base + j * w..new_base + j * w + w].copy_from_slice(&buf[src..src + w]);
        }
    }
    out
}

/// Overwrite element `index`'s fields in a column-major buffer of `count` elements with one packed
/// `row` (slot order), writing each field into its column (P-SIMD C2).
fn column_set(schema: &PackedSchema, buf: &mut [u8], index: usize, count: usize, row: &[u8]) {
    let mut row_at = 0;
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let dst = schema.field_offset(index, slot, count);
        buf[dst..dst + w].copy_from_slice(&row[row_at..row_at + w]);
        row_at += w;
    }
}

/// Concatenate two column-major buffers of the same schema into a new one (P-SIMD C2): each field's
/// output column is `a`'s column followed by `b`'s. Mirrors the row path's buffer append.
fn column_concat(schema: &PackedSchema, a: &[u8], b: &[u8]) -> Vec<u8> {
    let na = schema.count(a.len());
    let nb = schema.count(b.len());
    let total = na + nb;
    let mut out = vec![0u8; total * schema.byte_size];
    for (slot, kind) in schema.fields.iter().enumerate() {
        let w = kind.byte_width();
        let prefix = schema.field_prefix(slot);
        let a_base = na * prefix;
        let b_base = nb * prefix;
        let out_base = total * prefix;
        out[out_base..out_base + na * w].copy_from_slice(&a[a_base..a_base + na * w]);
        out[out_base + na * w..out_base + (na + nb) * w]
            .copy_from_slice(&b[b_base..b_base + nb * w]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_object::ShapeKind;
    use proptest::prelude::*;

    /// The [`Value::NANBOX`] layout the JIT compiles against must reproduce this crate's own encoding
    /// bit-for-bit — the whole point of exposing it as one source of truth. Round-trip the singletons
    /// and a small int through both the safe API and the raw layout formulas the JIT uses.
    #[test]
    fn nanbox_layout_matches_the_value_encoding() {
        let l = Value::NANBOX;
        assert_eq!(l.unit_bits, Value::unit().0);
        assert_eq!(l.true_bits, Value::bool(true).0);
        assert_eq!(l.false_bits, Value::bool(false).0);
        assert_eq!(l.unbound_bits, Value::unbound().0);
        // Boxing a small int: `qnan | int_tag | (i & ptr_mask)` — the exact sequence the JIT emits.
        for i in [0i64, 1, -1, 42, -42, l.int_max, l.int_min] {
            let boxed = l.qnan | l.int_tag | (i as u64 & l.ptr_mask);
            assert_eq!(boxed, Value::int(i).0, "boxing {i}");
            // Unboxing: sign-extend the low 48 bits.
            let p = boxed & l.ptr_mask;
            let unboxed = ((p << 16) as i64) >> 16;
            assert_eq!(unboxed, i, "unboxing {i}");
        }
        // The small-int tag test the JIT inlines: `(bits & (sign|qnan|int_tag)) == (qnan|int_tag)`.
        let mask = l.sign_bit | l.qnan | l.int_tag;
        let want = l.qnan | l.int_tag;
        assert_eq!(Value::int(5).0 & mask, want, "5 reads as small int");
        assert_ne!(Value::bool(true).0 & mask, want, "true is not a small int");
        assert_ne!(Value::unit().0 & mask, want, "unit is not a small int");
        // The pointer test: `(bits & (sign|qnan)) == (sign|qnan)`.
        let pmask = l.sign_bit | l.qnan;
        assert_ne!(Value::int(5).0 & pmask, pmask, "small int is not a pointer");
        let s = Value::string("hi");
        assert_eq!(s.0 & pmask, pmask, "a heap string is a pointer");
        s.free();
    }

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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
            column: false,
        });

        // Pack two `V { x, y }` instances into one flat buffer (the source objects are freed after).
        let mut bytes = Vec::new();
        for (x, y) in [(3_i64, 1_i64), (1, 2)] {
            let obj = Value::object(shape, vec![Value::int(x), Value::int(y)]);
            assert!(obj.pack_element(schema, &mut bytes));
            obj.release();
        }
        assert_eq!(bytes.len(), 32); // 2 elements × 2 int fields × 8 bytes

        let list = Value::packed_list(schema, bytes);
        assert!(list.is_packed_list());
        assert!(list.is_list());
        assert_eq!(list.list_len(), Some(2));
        assert_eq!(list.type_name(), "list");

        // A single element materializes to an owned object, shape-identical to a constructed one.
        let first = list.packed_get(0);
        assert_eq!(first.display(), "V {x: 3, y: 1}");
        let constructed = Value::object(shape, vec![Value::int(3), Value::int(1)]);
        assert!(
            apply_binary(noeta_ast::BinaryOp::Eq, first, constructed)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        first.release();
        constructed.release();

        // The whole list displays and compares as the boxed equivalent.
        assert_eq!(list.display(), "[V {x: 3, y: 1}, V {x: 1, y: 2}]");
        let boxed = Value::list(vec![
            Value::object(shape, vec![Value::int(3), Value::int(1)]),
            Value::object(shape, vec![Value::int(1), Value::int(2)]),
        ]);
        assert!(
            apply_binary(noeta_ast::BinaryOp::Eq, list, boxed)
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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
            column: false,
        });

        let list = Value::packed_list(schema, Vec::new());
        assert_eq!(list.list_len(), Some(0));
        for (x, y) in [(3_i64, 1_i64), (1, 2), (7, 9)] {
            let obj = Value::object(shape, vec![Value::int(x), Value::int(y)]);
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
        let elem = Value::object(shape, vec![Value::int(5), Value::int(6)]);
        boxed.list_push(elem); // boxed now owns the reference handed over
        assert_eq!(boxed.list_len(), Some(1));
        assert_eq!(boxed.display(), "[V {x: 5, y: 6}]");
        boxed.release(); // frees the list and its owned element, so miri sees no leak
    }

    #[test]
    fn packed_column_layout_round_trips_and_frees() {
        // P-SIMD C2: a `@packed(layout: column)` list stores each field contiguously across elements
        // (`[x×n][y×n]`) yet observes identically to a row list. Build it by streaming push (exercising
        // `column_append`'s O(n) rebuild), then check get / field / select / set / concat all read the
        // right values through the column offset math — and free clean under miri. Mixed field widths
        // (`int` 8 + `bool` 1) stress the non-uniform column strides.
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "P",
            vec!["v".into(), "flag".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::Int, PackedKind::Bool],
            byte_size: 9,
            column: true,
        });

        // Stream three elements in — each `column_append` rebuilds the buffer in column order.
        let list = Value::packed_list(schema, Vec::new());
        for (v, flag) in [(10_i64, true), (20, false), (30, true)] {
            let obj = Value::object(shape, vec![Value::int(v), Value::bool(flag)]);
            assert!(list.packed_push(obj));
            obj.release();
        }
        assert_eq!(list.list_len(), Some(3));
        // Buffer is column-major: `[v0 v1 v2 (24 bytes)][flag0 flag1 flag2 (3 bytes)]`.
        let raw = list.packed_bytes().unwrap();
        assert_eq!(raw.len(), 27);
        assert_eq!(&raw[0..8], &10_i64.to_le_bytes());
        assert_eq!(&raw[8..16], &20_i64.to_le_bytes());
        assert_eq!(&raw[24..27], &[1u8, 0, 1]); // the bool column, contiguous

        // Per-element gather and per-field read both resolve through the column offsets.
        let mid = list.packed_get(1);
        assert_eq!(mid.display(), "P {v: 20, flag: false}");
        mid.release();
        assert_eq!(list.packed_field(2, "v").unwrap().as_int(), Some(30));
        assert_eq!(list.packed_field(0, "flag").unwrap().as_bool(), Some(true));

        // Whole-list display equals the boxed equivalent (materialize gathers every column).
        assert_eq!(
            list.display(),
            "[P {v: 10, flag: true}, P {v: 20, flag: false}, P {v: 30, flag: true}]"
        );

        // `select` (reverse) and `set` keep the column layout and the right values.
        let rev = list.packed_select(&[2, 1, 0]);
        assert_eq!(
            rev.display(),
            "[P {v: 30, flag: true}, P {v: 20, flag: false}, P {v: 10, flag: true}]"
        );
        rev.release();

        let repl = Value::object(shape, vec![Value::int(99), Value::bool(false)]);
        let set = list.packed_set(1, repl).unwrap();
        assert_eq!(
            set.display(),
            "[P {v: 10, flag: true}, P {v: 99, flag: false}, P {v: 30, flag: true}]"
        );
        repl.release();
        set.release();

        // `concat` of two column lists interleaves per column.
        let other = Value::packed_list(schema, Vec::new());
        let tail = Value::object(shape, vec![Value::int(40), Value::bool(false)]);
        assert!(other.packed_push(tail));
        tail.release();
        let joined = list.packed_concat(other).unwrap();
        assert_eq!(joined.list_len(), Some(4));
        assert_eq!(joined.packed_field(3, "v").unwrap().as_int(), Some(40));
        let head = joined.packed_get(0);
        assert_eq!(head.display(), "P {v: 10, flag: true}");
        head.release();
        joined.release();
        other.release();

        list.release(); // a leaf — frees the column buffer, no child release; miri sees no leak
    }

    #[test]
    fn future_wraps_and_frees_its_step() {
        // Track A: a future owns one reference to its thunk/step closure (a list stands in for the
        // closure here). Constructing retains the step; `future_step` hands back a fresh retained
        // reference; releasing the future releases its held reference — all leak-clean under miri.
        let step = Value::list(vec![Value::int(1), Value::int(2)]);
        let fut = Value::make_future(step); // the future retains a second reference to `step`
        assert!(fut.is_future());
        assert_eq!(fut.type_name(), "future");
        assert_eq!(fut.display(), "<future>");

        let got = fut.future_step().expect("a future yields its step");
        assert_eq!(got.display(), "[1, 2]"); // a freshly-retained reference to the wrapped step
        got.release(); // drop the borrowed step reference

        step.release(); // drop the caller's original reference; the future still holds one
        fut.release(); // frees the future and its held step reference → step freed, no leak
    }

    #[test]
    fn timer_carries_its_deadline_and_frees_cleanly() {
        // Track A.2: a leaf timer future carries only its integer deadline (no heap children), names
        // itself "future", displays opaquely, and frees as a plain node — leak-clean under miri.
        let timer = Value::make_timer(42);
        assert!(timer.is_future());
        assert!(timer.is_timer());
        assert!(!timer.is_future() || timer.future_step().is_none()); // a timer wraps no closure
        assert_eq!(timer.timer_deadline(), Some(42));
        assert_eq!(timer.type_name(), "future");
        assert_eq!(timer.display(), "<future>");
        timer.release(); // frees the node, no leak
    }

    #[test]
    fn handle_carries_its_indices_and_frees_cleanly() {
        // Track A.3b: a task handle is a GC leaf holding `(scope, task)` indices; it names itself
        // "future", displays opaquely, and frees as a plain node — leak-clean under miri.
        let handle = Value::make_handle(ScopeId::from_index(2), TaskId::from_index(5));
        assert!(handle.is_future());
        assert!(handle.is_handle());
        assert_eq!(
            handle.handle_parts(),
            Some((ScopeId::from_index(2), TaskId::from_index(5)))
        );
        assert_eq!(handle.type_name(), "future");
        assert_eq!(handle.display(), "<future>");
        handle.release(); // frees the node, no leak
    }

    #[test]
    fn async_io_carries_its_ticket_and_frees_cleanly() {
        // Track A.4c/A.10: a leaf async-IO future is a GC leaf holding the executor ticket id; it names
        // itself "future", displays opaquely, and frees as a plain node — leak-clean under miri.
        let io = Value::make_async_io(7);
        assert!(io.is_future());
        assert_eq!(io.async_io_id(), Some(7));
        assert!(io.future_step().is_none()); // it wraps no closure
        assert!(!io.is_timer() && !io.is_handle());
        assert_eq!(io.type_name(), "future");
        assert_eq!(io.display(), "<future>");
        io.release(); // frees the node, no leak
    }

    #[test]
    fn channel_endpoints_are_leaves_and_free_cleanly() {
        // Isolates I.1: a sender/receiver endpoint is a GC leaf holding a channel id; it names itself
        // "sender"/"receiver", displays opaquely, and frees as a plain node — leak-clean under miri.
        let tx = Value::make_sender(ChannelId::from_index(3));
        assert_eq!(tx.sender_id(), Some(ChannelId::from_index(3)));
        assert_eq!(tx.receiver_id(), None);
        assert_eq!(tx.type_name(), "sender");
        assert_eq!(tx.display(), "<sender>");
        tx.release();

        let rx = Value::make_receiver(ChannelId::from_index(3));
        assert_eq!(rx.receiver_id(), Some(ChannelId::from_index(3)));
        assert_eq!(rx.type_name(), "receiver");
        assert_eq!(rx.display(), "<receiver>");
        rx.release();

        // Isolates I.4b: an isolate-result future is a GC leaf carrying a backend isolate-table id;
        // it reports as a "future", displays opaquely, and frees as a plain node.
        let h = Value::make_isolate_future(9);
        assert_eq!(h.isolate_future_id(), Some(9));
        assert!(h.is_future());
        assert_eq!(h.type_name(), "future");
        assert_eq!(h.display(), "<future>");
        h.release();
    }

    #[test]
    fn channel_send_future_owns_its_message_and_frees_cleanly() {
        // Isolates I.1: a channel-send future is a future GC node owning one reference to its message
        // (like `make_future`'s closure). Dropping the future must release that reference — no leak,
        // no double-free — which miri verifies. `channel_send_parts` hands back a fresh owning ref.
        let msg = Value::string("hi"); // refcount 1
        let fut = Value::make_channel_send(ChannelId::from_index(5), msg); // retains msg → refcount 2
        assert!(fut.is_future());
        msg.release(); // drop the caller's original reference → refcount 1 (the future's)
        let (id, borrowed) = fut.channel_send_parts().expect("a channel-send future");
        assert_eq!(id, ChannelId::from_index(5));
        borrowed.release(); // channel_send_parts retained a fresh ref; balance it
        fut.release(); // frees the future and its remaining message reference — no leak

        let recv = Value::make_channel_recv(ChannelId::from_index(5));
        assert!(recv.is_future());
        assert_eq!(recv.channel_recv_id(), Some(ChannelId::from_index(5)));
        recv.release();
    }

    /// Build a `Point { x: int; y: int }` struct value on a shared shape (helper for the I.3 tests).
    fn point(shape: &&'static Shape, x: i64, y: i64) -> Value {
        Value::object(shape, vec![Value::int(x), Value::int(y)])
    }

    #[test]
    fn shared_object_retain_release_are_noops() {
        // Isolates I.3: a promoted (borrow-shared) object's refcount is never written — `inc_ref`/
        // `release` no-op on it, so a storm of them across "isolates" touches no count and frees
        // nothing. The region alone owns it and frees it wholesale. miri verifies no use-after-free.
        let base = live_count();
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "Point",
            vec!["x".into(), "y".into()],
        ));
        let local = point(&shape, 3, 4); // an ordinary local object, refcount 1

        let mut region = SharedRegion::new();
        let shared = region.promote(local);
        assert!(shared.is_shared());
        assert!(!local.is_shared()); // the original stays local — promotion is a copy, not a move
        assert_eq!(shared.refcount(), 1);

        // Simulate N isolates each borrowing the shared object: retain/release many times. On a shared
        // object these are no-ops, so the count stays put and the object is never reclaimed.
        for _ in 0..1000 {
            shared.inc_ref();
            shared.release();
        }
        assert_eq!(
            shared.refcount(),
            1,
            "a shared object's count is never written"
        );
        // Still fully readable after the storm (never freed).
        assert_eq!(shared.display(), "Point {x: 3, y: 4}");

        local.release(); // the local original frees normally (refcount → 0)
        region.free_all(); // the region frees the shared graph wholesale
        assert_eq!(live_count(), base, "region balance: promoted N, freed N");
    }

    #[test]
    fn shared_region_deep_copies_and_is_independent() {
        // Isolates I.3: promotion deep-copies the whole graph into shared objects. The copy equals the
        // original structurally, yet is independent — freeing the entire original leaves the shared
        // copy intact and readable (it shares no allocation with the original). Then `free_all` returns
        // the live count exactly to baseline (leak-clean, both allocations balanced).
        let base = live_count();
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "Point",
            vec!["x".into(), "y".into()],
        ));
        // A nested value graph: a list of two structs, each holding boxed ints (and the list itself).
        let original = Value::list(vec![point(&shape, 1, 2), point(&shape, 3, 4)]);

        let mut region = SharedRegion::new();
        let shared = region.promote(original);
        assert_ne!(
            shared.bits(),
            original.bits(),
            "a copy, not the same allocation"
        );
        assert!(shared.is_shared());
        assert!(
            apply_binary(noeta_ast::BinaryOp::Eq, shared, original)
                .unwrap()
                .as_bool()
                .unwrap(),
            "the promoted copy equals the original structurally"
        );

        // Free the entire original graph. If promotion had aliased any of its allocations, the shared
        // copy would now dangle — miri would catch a use-after-free on the read below.
        original.release();
        assert_eq!(
            shared.display(),
            "[Point {x: 1, y: 2}, Point {x: 3, y: 4}]",
            "the shared copy is unaffected by freeing the original"
        );

        region.free_all();
        assert_eq!(live_count(), base);
    }

    #[test]
    fn promotion_preserves_dag_sharing() {
        // Isolates I.3: when the same object is reachable by two paths (a DAG — value semantics let two
        // slots hold one allocation), promotion copies it **once** via its memo, so the promoted graph
        // preserves that sharing rather than duplicating. Freeing the region then frees the shared node
        // exactly once — miri would flag a double-free otherwise.
        let base = live_count();
        let shape =
            noeta_object::intern_shape(Shape::object(ShapeKind::Struct, "P", vec!["x".into()]));
        let p = Value::object(shape, vec![Value::int(7)]); // refcount 1
        // A tuple *transfers* one reference per slot in, so to alias `p` in both slots we owe it two
        // references: bump the count once more, then hand both to the tuple (refcount 2).
        p.inc_ref();
        let pair = Value::tuple(vec![p, p]); // both slots point at the SAME object → refcount 2

        let mut region = SharedRegion::new();
        let shared = region.promote(pair);
        let a = shared.tuple_field(0).unwrap();
        let b = shared.tuple_field(1).unwrap();
        assert_eq!(
            a.bits(),
            b.bits(),
            "the shared subgraph is promoted once, not twice"
        );
        assert!(a.is_shared());
        // region holds: the tuple + the single shared `P` (deduped) = 2 objects.
        assert_eq!(region.len(), 2);

        pair.release();
        region.free_all();
        assert_eq!(live_count(), base);
    }

    #[test]
    fn promote_passes_immediates_through() {
        // Isolates I.3: an immediate (a small int) carries no refcount and no heap identity, so
        // promoting it returns it unchanged and adds nothing to the region.
        let mut region = SharedRegion::new();
        let promoted = region.promote(Value::int(42));
        assert_eq!(promoted.as_int(), Some(42));
        assert!(!promoted.is_shared());
        assert!(region.is_empty());
        region.free_all();
    }

    #[test]
    fn promote_with_memo_dedups_across_calls() {
        // P-PAR S2: the spawn path keeps one memo across every `isolate f(corpus)` in flight, so
        // fanning one corpus to N workers promotes once — the second call returns the same
        // promoted root and the region grows by nothing.
        let base = live_count();
        let corpus = Value::list(vec![Value::string("a"), Value::string("b")]);
        let mut region = SharedRegion::new();
        let mut memo = std::collections::HashMap::new();
        let first = region.promote_with(corpus, &mut memo);
        let after_first = region.len();
        let second = region.promote_with(corpus, &mut memo);
        assert_eq!(
            first.bits(),
            second.bits(),
            "memo hit returns the same root"
        );
        assert_eq!(region.len(), after_first, "a memo hit adds nothing");
        corpus.release();
        region.free_all();
        assert_eq!(live_count(), base);
    }

    #[test]
    fn shared_values_are_never_uniquely_owned() {
        // P-PAR S2 (the COW gate): a shared object's refcount is frozen at 1, so the in-place
        // mutation fast paths must consult `is_uniquely_owned`, which excludes it — a worker
        // "mutating" a borrowed corpus copies instead of touching the shared buffer.
        let base = live_count();
        let local = Value::list(vec![Value::int(1)]);
        assert!(local.is_uniquely_owned());
        let mut region = SharedRegion::new();
        let shared = region.promote(local);
        assert_eq!(
            shared.refcount(),
            1,
            "a shared object's count is frozen at 1"
        );
        assert!(
            !shared.is_uniquely_owned(),
            "shared must never pass the COW uniqueness gate"
        );
        local.release();
        region.free_all();
        assert_eq!(live_count(), base);
    }

    #[test]
    fn promotable_graph_accepts_data_and_rejects_functions() {
        // P-PAR S2: promotability = Send *data* kinds. A closure is Wire-shippable but has no
        // promoted form, so a graph containing one falls back to the copy path.
        let list = Value::list(vec![Value::int(1), Value::string("x")]);
        assert!(list.is_promotable_graph());
        let f = Value::closure(0, Vec::new());
        assert!(!f.is_promotable_graph());
        let holding = Value::list(vec![f]);
        f.inc_ref(); // the list owns one reference; keep ours for the assert below
        assert!(!holding.is_promotable_graph());
        holding.release();
        f.release();
        list.release();
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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "P",
            vec!["a".into(), "b".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::F32, PackedKind::Int],
            byte_size: 12,
            column: false,
        });
        let mut bytes = Vec::new();
        for (a, b) in [(0.1f32 + 0.2, 7_i64), (-1.5, 1_000_000)] {
            let obj = Value::object(shape, vec![Value::f32(a), Value::int(b)]);
            assert!(obj.pack_element(schema, &mut bytes));
            obj.release();
        }
        assert_eq!(bytes.len(), 24); // 2 elements × 12 bytes

        let list = Value::packed_list(schema, bytes);
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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
            column: false,
        });
        // Two elements: (3, 1) and (7, 9).
        let list = Value::packed_list(schema, ibytes(&[3, 1, 7, 9]));

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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
            column: false,
        });
        // Three elements: (3,1), (1,2), (7,9).
        let list = Value::packed_list(schema, ibytes(&[3, 1, 1, 2, 7, 9]));

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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "V",
            vec!["x".into(), "y".into()],
        ));
        let schema = noeta_object::intern_schema(PackedSchema {
            shape,
            fields: vec![PackedKind::Int, PackedKind::Int],
            byte_size: 16,
            column: false,
        });
        let mk = |vals: &[i64]| Value::packed_list(schema, ibytes(vals));
        let elem = |x: i64, y: i64| Value::object(shape, vec![Value::int(x), Value::int(y)]);

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
            crate::ops::apply_binary(noeta_ast::BinaryOp::Eq, len, len2)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        assert!(
            !crate::ops::apply_binary(noeta_ast::BinaryOp::Eq, len, map)
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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "Item",
            vec!["price".into(), "qty".into()],
        ));
        let obj = Value::object(shape, vec![Value::float(2.5), Value::int(4)]);
        assert_eq!(obj.type_name(), "object");
        assert_eq!(obj.display(), "Item {price: 2.5, qty: 4}");
        assert_eq!(obj.field("price").unwrap().as_float(), Some(2.5));
        assert!(obj.field("missing").is_none());
        // Same shape handle (the `Rc`) is shared, not copied per-instance.
        let obj2 = Value::object(shape, vec![Value::float(2.5), Value::int(4)]);
        assert!(std::ptr::eq(obj.shape().unwrap(), obj2.shape().unwrap()));
        // Structural equality (M0 parity): same type + equal fields.
        assert!(
            apply_binary(noeta_ast::BinaryOp::Eq, obj, obj2)
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
        let shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            "Version",
            vec!["major".into(), "minor".into()],
        ));
        let v19 = Value::object(shape, vec![Value::int(1), Value::int(9)]);
        let v20 = Value::object(shape, vec![Value::int(2), Value::int(0)]);
        let v19b = Value::object(shape, vec![Value::int(1), Value::int(9)]);
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
        let pending =
            noeta_object::intern_shape(Shape::enum_variant("Status", "Pending", vec![], false));
        let a = Value::enum_value(pending, vec![]);
        assert_eq!(a.type_name(), "enum");
        assert_eq!(a.display(), "Status.Pending");
        let b = Value::enum_value(pending, vec![]);
        assert!(
            apply_binary(noeta_ast::BinaryOp::Eq, a, b)
                .unwrap()
                .as_bool()
                == Some(true)
        );

        // A built-in Result variant displays bare, with its data unquoted.
        let err = noeta_object::intern_shape(Shape::enum_variant(
            "Result",
            "Err",
            vec!["0".into()],
            true,
        ));
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
    fn reflect_tag_is_invisible_to_value_semantics_and_leaks_nothing() {
        // The R1 reflected-type tag lives beside the payload: it round-trips through construction and
        // free with no residual (a leaf `Rc<TypeRepr>`, not a child object, so `live_count` is
        // unchanged by tagging), and it is invisible to equality — a tagged and an untagged list of
        // the same elements compare equal.
        use noeta_ast::reflect::TypeRepr;
        let before = live_count();
        let tagged = Value::list(vec![Value::int(1), Value::int(2)]);
        tagged.set_reflect(Some(Rc::new(TypeRepr::List(Box::new(TypeRepr::Int)))));
        let untagged = Value::list(vec![Value::int(1), Value::int(2)]);
        // Tagging allocates no heap object (small ints are immediate): two lists, nothing else.
        assert_eq!(live_count(), before + 2);
        // The tag is readable, and clearing it (the mutation path) drops back to `None`.
        assert!(tagged.reflect().is_some());
        assert!(untagged.reflect().is_none());
        // Equality ignores the tag entirely.
        assert!(
            apply_binary(noeta_ast::BinaryOp::Eq, tagged, untagged)
                .unwrap()
                .as_bool()
                .unwrap()
        );
        // A content-changing op clears the tag (refcount-independent — matches the tree-walker).
        let displaced = tagged.list_replace_slot(0, Value::int(9));
        assert!(displaced.as_int().is_some());
        assert!(tagged.reflect().is_none());
        assert!(tagged.dec_ref());
        tagged.free();
        assert!(untagged.dec_ref());
        untagged.free();
        assert_eq!(live_count(), before, "the tag leaves no residual heap");
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

    #[test]
    fn iterator_closure_adapters_free_without_leaking() {
        // Track I.1c: `map`/`filter` call back into a supplied applier (the real backend runs a user
        // closure; here a Rust closure stands in). Exercises the source-element-consumed-by-apply path,
        // the adapter owning a heap "closure" value, filter's keep/drop refcounting, and the non-bool
        // error path — all of which must return the live count to baseline.
        let before = live_count();

        // `map`: each int element → a fresh heap string. `func` is a heap stand-in the adapter retains.
        let lm = Value::list(vec![Value::int(1), Value::int(2), Value::int(3)]);
        let im = Value::iter(lm);
        lm.release();
        let func = Value::string("f");
        let mapped = Value::iter_map(im, func);
        im.release();
        func.release(); // the adapter is now the sole owner of `im` and `func`
        let mut to_str = |_f: Value, arg: Value| -> Result<Value, ()> {
            let n = arg.as_int().expect("int element");
            arg.release(); // the call consumes the argument's reference
            Ok(Value::string(&n.to_string()))
        };
        let mut out = Vec::new();
        while let Some(e) = mapped.iter_next_apply(&mut to_str).expect("no abort") {
            out.push(e);
        }
        assert_eq!(out.len(), 3);
        for e in out {
            e.release();
        }
        mapped.release(); // frees mapped → im → lm and the func string
        assert_eq!(live_count(), before);

        // `filter`: keep the even elements — exercises the inc_ref + keep-or-release element paths.
        let lf = Value::list(vec![
            Value::int(1),
            Value::int(2),
            Value::int(3),
            Value::int(4),
        ]);
        let iff = Value::iter(lf);
        lf.release();
        let pred = Value::string("p");
        let filtered = Value::iter_filter(iff, pred);
        iff.release();
        pred.release();
        let mut even = |_f: Value, arg: Value| -> Result<Value, ()> {
            let keep = arg.as_int().is_some_and(|n| n % 2 == 0);
            arg.release(); // the predicate consumes the argument
            Ok(Value::bool(keep))
        };
        let mut kept = Vec::new();
        while let Some(e) = filtered.iter_next_apply(&mut even).expect("no abort") {
            kept.push(e);
        }
        assert_eq!(kept.len(), 2);
        for e in kept {
            e.release();
        }
        filtered.release();
        assert_eq!(live_count(), before);

        // `filter`'s non-bool error path: the held element is released as the abort is raised.
        let le = Value::list(vec![Value::int(5)]);
        let ie = Value::iter(le);
        le.release();
        let filtered2 = Value::iter_filter(ie, Value::int(0));
        ie.release();
        let mut bad = |_f: Value, arg: Value| -> Result<Value, ()> {
            arg.release();
            Ok(Value::int(99)) // a non-bool verdict
        };
        match filtered2.iter_next_apply(&mut bad) {
            Err(IterAbort::FilterNotBool(name)) => assert_eq!(name, "int"),
            _ => panic!("expected a FilterNotBool abort"),
        }
        filtered2.release();
        assert_eq!(live_count(), before);
    }

    #[test]
    fn generator_iterator_steps_and_frees_without_leaking() {
        // Track G: a `Gen` iterator drives a step closure returning `?T`. Here a Rust closure stands
        // in for the lowered generator (the real backend passes a real closure), building an `Option`
        // each call. Exercises `option_take`'s some-payload extraction + Option-wrapper release with
        // heap elements, plus the adapter owning the heap step value — live count returns to baseline.
        let before = live_count();

        let some_shape = noeta_object::intern_shape(noeta_object::Shape::enum_variant(
            "Option",
            "some",
            vec!["0".into()],
            true,
        ));
        let none_shape = noeta_object::intern_shape(noeta_object::Shape::enum_variant(
            "Option",
            "none",
            Vec::new(),
            true,
        ));

        let step = Value::string("step"); // heap stand-in for the closure the adapter retains
        let g = Value::iter_gen(step);
        step.release(); // `g` is now the sole owner of `step`

        let mut calls = 0;
        let mut apply = |_s: Value, arg: Value| -> Result<Value, ()> {
            arg.release(); // the resume argument is consumed by the call
            calls += 1;
            Ok(match calls {
                1 => Value::enum_value(some_shape, vec![Value::string("a")]),
                2 => Value::enum_value(some_shape, vec![Value::string("b")]),
                _ => Value::enum_value(none_shape, Vec::new()),
            })
        };

        let mut out = Vec::new();
        while let Some(e) = g.iter_next_apply(&mut apply).expect("no abort") {
            out.push(e);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_string().as_deref(), Some("a"));
        for e in out {
            e.release();
        }
        g.release(); // frees the generator and its step value
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
