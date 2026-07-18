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

mod conc;
mod display;
mod heap;
mod ids;
mod iter;
mod ops;
mod packed;

pub use heap::{
    CollectorMode, Color, SharedRegion, SharedRoot, collector_mode, live_count, live_objects,
    live_peak, note_refcount_anomalies, refcount_anomalies, reset_peak, reset_refcount_anomalies,
    set_collector_mode, take_candidates,
};
pub use ids::{ChannelId, ScopeId, TaskId};
pub use ops::{
    OpError, apply_binary, apply_binary_wide, apply_unary, compare_primitive, compare_values,
    set_order, structural_compare,
};

use std::collections::BTreeMap;
use std::rc::Rc;

use noeta_ast::reflect::TypeRepr;
use noeta_bytecode::Builtin;
use noeta_object::Shape;

// The P-SSO string (24-byte, ≤24-byte content inline) inside `Payload::Str`. Re-exported so the
// one hot producer outside this crate — the VM's `BuildString` — can assemble its output in the
// payload's own representation and hand it over without a conversion.
pub use compact_str::CompactString;

use heap::Payload;

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

    /// A registered extern-type value (extern-types X1) — the general form of
    /// [`Value::file_handle`]. A GC leaf (the contract owns no child values).
    pub fn extern_value(value: noeta_ext_abi::ExternBox) -> Value {
        heap::alloc(Payload::Extern(value))
    }

    /// Whether this is an extern-type value.
    pub fn is_extern(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Extern(_)))
    }

    /// Read this extern value under a closure. The caller must have checked [`Value::is_extern`].
    pub fn with_extern<R>(self, f: impl FnOnce(&dyn noeta_ext_abi::ExternValue) -> R) -> R {
        heap::with_extern(self, f)
    }

    /// Mutate this extern value under a closure (the receiver of a mutating method). The caller
    /// must have checked [`Value::is_extern`].
    pub fn with_extern_mut<R>(self, f: impl FnOnce(&mut dyn noeta_ext_abi::ExternValue) -> R) -> R {
        heap::with_extern_mut(self, f)
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
                .map(|(k, v)| (noeta_ext_abi::MapKey::from(k), v))
                .collect(),
        ))
    }

    /// A heap map from already-built keys (extern-types X4) — the `MakeMap`/extern-key path.
    /// Later duplicates win (insertion order), matching the string builder's BTreeMap semantics.
    pub fn map_keyed(entries: Vec<(noeta_ext_abi::MapKey, Value)>) -> Value {
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
                    set_order(item, value).unwrap_or(std::cmp::Ordering::Equal)
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
                    set_order(item, target).unwrap_or(std::cmp::Ordering::Equal)
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
    /// [`MapKey`]: noeta_ext_abi::MapKey
    pub fn map_get_key(self, key: &noeta_ext_abi::MapKey) -> Option<Value> {
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
    /// [`MapKey`]: noeta_ext_abi::MapKey
    pub fn map_remove_key(self, key: &noeta_ext_abi::MapKey) -> Option<Value> {
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
    pub fn map_get_extern(self, key: &dyn noeta_ext_abi::ExternValue) -> Option<Value> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => entries.get(&noeta_ext_abi::ExternKeyRef(key)).copied(),
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
                    let mut kv: Vec<(&noeta_ext_abi::MapKey, &Value)> = entries.iter().collect();
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
    pub fn map_keys(self) -> Option<Vec<noeta_ext_abi::MapKey>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => {
                    let mut keys: Vec<noeta_ext_abi::MapKey> = entries.keys().cloned().collect();
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
    pub fn map_insert(self, key: noeta_ext_abi::MapKey, value: Value) -> Option<Value> {
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
    pub fn map_remove_extern(self, key: &dyn noeta_ext_abi::ExternValue) -> Option<Value> {
        debug_assert!(
            !self.is_map() || heap::refcount(self) == 1 && !heap::is_shared(self),
            "map_remove_extern requires a uniquely-owned map (the COW invariant)"
        );
        if self.is_map() {
            let removed = heap::with_payload_mut(self, |p| match p {
                Payload::Map(entries) => entries.remove(&noeta_ext_abi::ExternKeyRef(key)),
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
    pub fn map_entries_keyed(self) -> Option<Vec<(noeta_ext_abi::MapKey, Value)>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Map(entries) => {
                    let mut kv: Vec<(noeta_ext_abi::MapKey, Value)> =
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

    /// The object's `(field name, value)` pairs in shape (declaration) order — the reflection
    /// read `fields_of(value)` materializes (derive layer 3). The returned values share the
    /// object's references (NOT retained); the caller retains whatever it stores. `None` for a
    /// non-object (enums and scalars included).
    pub fn object_fields_for_reflection(self) -> Option<Vec<(String, Value)>> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::Object { shape, slots } => Some(
                    shape
                        .fields
                        .iter()
                        .cloned()
                        .zip(slots.iter().copied())
                        .collect(),
                ),
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
    /// [`noeta_ext_abi::PackedKeyField`] data — the erased integer word (immediate or boxed),
    /// bools, nested key-capable structs — plus the display form (render/JSON only, not
    /// identity). Both backends build keys from the same declarations, so identity, hash, and
    /// order agree by construction. The key is a snapshot holding no heap reference (`@packed`
    /// is value semantics, so it can never drift from an aliased original).
    pub fn packed_map_key(self) -> Option<noeta_ext_abi::MapKey> {
        let shape = self.shape()?;
        if !shape.key_capable {
            return None;
        }
        Some(noeta_ext_abi::MapKey::packed(
            &shape.name,
            self.packed_key_fields()?,
        ))
    }

    /// The [`packed_map_key`](Value::packed_map_key) field walk. `None` on a slot the capability
    /// contract excludes — defensive: a `key_capable` shape's slots are ints/bools/nested-capable
    /// by construction, so `None` here means a compiler bug, and the caller falls back to the
    /// ordinary key error rather than corrupting a map.
    fn packed_key_fields(self) -> Option<Vec<noeta_ext_abi::PackedKeyField>> {
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
                        Some(noeta_ext_abi::PackedKeyField::Bool(b))
                    } else if let Some(i) = v.as_int() {
                        Some(noeta_ext_abi::PackedKeyField::Int(i))
                    } else {
                        let shape = v.shape()?;
                        if !shape.key_capable {
                            return None;
                        }
                        Some(noeta_ext_abi::PackedKeyField::Struct(
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
    pub fn scalar_slots_into(self, out: &mut Vec<noeta_ext_abi::Scalar>) -> bool {
        use noeta_ext_abi::Scalar;
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
                // The extern type's human-facing short name (`Uuid`) — the display form of the
                // value's qualified identity (`std.id.Uuid`), exactly as user objects display
                // their shape's short name. Identity paths read `type_identity()` directly.
                self.with_extern(|e| e.type_display_name())
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

#[cfg(test)]
mod tests;
