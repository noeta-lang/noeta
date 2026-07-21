//! The runtime value representation for the tree-walker.
//!
//! Deliberately a simple boxed `enum`. The VM backend (`noeta-value`) uses a NaN-boxed value
//! representation and a shape-based object model instead; the two backends run the same programs and
//! are asserted to agree (the differential oracle), so each is free to represent values its own way.
//! Keeping the tree-walker's model behind this type (and the `display()`/`type_name()` methods) is
//! what makes that independence local.
//!
//! `Debug` and `PartialEq` are hand-written rather than derived: function values hold
//! an `Rc<Scope>` whose graph can contain reference cycles (a global function captures
//! the global scope, which holds the function), so a derived recursive `Debug`/`PartialEq`
//! could loop forever. We print functions opaquely and treat them as never structurally
//! equal.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use noeta_ast::reflect::TypeRepr;
use noeta_stdlib::channel::SendPhase;

use crate::{Builtin, Closure, EnumDef, EnumValue, ObjectValue, TypeDef};

/// A runtime value.
#[derive(Clone)]
pub enum Value {
    /// The unit value, produced by statements and effectful calls.
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A 32-bit float (P-PACK Phase 3) — a distinct primitive from `Float`, with observable f32
    /// precision. Arithmetic rounds at f32 (see `apply_binary_op`); display matches the VM.
    F32(f32),
    Str(String),
    /// A raw immutable byte buffer (`bytes`, P-PACK 4.4) — the binary-serialization surface. `Rc`
    /// keeps copies cheap; equality is content-wise. Mirrors the VM's `Payload::Bytes`.
    Bytes(Rc<Vec<u8>>),
    /// An immutable list. The backing [`ListRepr`] is either the general boxed `Rc<Vec<Value>>`
    /// (any element type) or, for a `List<packed>`, a flat raw-primitive buffer (P-PACK Phase 2).
    /// `Rc` keeps copies cheap (map/filter produce new lists). The representation is invisible to
    /// `RunResult` — every operation observes the same elements either way.
    List(ListRepr),
    /// A tuple: a fixed-arity, heterogeneous, value-semantic positional aggregate (object-model
    /// slice 4). `Rc` keeps copies cheap; equality is structural (element-wise).
    Tuple(Rc<Vec<Value>>),
    /// An immutable set, held in canonical (sorted, de-duplicated) order so iteration,
    /// display, and equality are deterministic and identical to the VM's `Payload::Set`. The second
    /// field is the checker-resolved `Set(T)` reflected type (runtime type-argument reflection): sets
    /// have no literal, so it is carried from a source list's `List(T)` tag through `to_set`; `None`
    /// for a derived/mutated set. Invisible to value semantics (equality compares only the elements),
    /// the tree-walker twin of the VM's node tag.
    Set(Rc<Vec<Value>>, Option<Rc<TypeRepr>>),
    /// An immutable string-keyed map. `BTreeMap` gives deterministic iteration order. The second
    /// field is the checker-resolved `Map(K, V)` reflected type (runtime type-argument reflection,
    /// R1), set at literal construction so `type_of` recovers it after a `dyn` launder; `None` for a
    /// derived/mutated map. Invisible to value semantics — equality compares only the entries — the
    /// tree-walker twin of the VM's node tag.
    Map(
        Rc<BTreeMap<noeta_stdlib::MapKey, Value>>,
        Option<Rc<TypeRepr>>,
    ),
    /// A user-defined function or closure.
    Function(Rc<Closure>),
    /// A built-in (native) function from the prelude.
    Builtin(Builtin),
    /// An enum *type* (e.g. the value `Status`), used to construct variants.
    EnumType(Rc<EnumDef>),
    /// An enum *value* (e.g. `Status.Pending` or `OrderError.NegativePrice(2)`).
    Enum(Rc<EnumValue>),
    /// A struct or class *type* (e.g. the value `Order`), used to construct instances
    /// and call associated functions (`Order.new(...)`).
    Type(Rc<TypeDef>),
    /// A struct or class *instance* — a bag of named field values.
    Object(Rc<ObjectValue>),
    /// A Ring 2 native module (e.g. `json`), bound by `use std.{...}` and identified by its surface
    /// name; `module.func(args)` dispatches to native code via the extension registry.
    NativeModule(String),
    /// A selectively-imported native-module function (`use std.math.sqrt` → bare `sqrt`), holding
    /// the `(module, func)` pair. Called (or passed as a value) through the same
    /// `call_native_module` dispatch as a `module.func(...)` member call — the VM twin is
    /// `Payload::ModuleFn`, so the two backends agree by construction.
    ModuleFn(String, String),
    /// An unbound method handle (`Type.method` as a value): `(ty, method, associated)`. When called
    /// it dispatches by name — on its first argument (instance) or as an associated call
    /// (`associated`). The VM twin is `Payload::MethodHandle`, so the backends agree by construction.
    MethodHandle(String, String, bool),
    /// A **bound** method handle (`value.method`, EX.2b): the receiver captured at bind time.
    /// Calling it dispatches the method on the captured receiver. VM twin: `Payload::BoundMethod`.
    BoundMethod(Box<Value>, String),
    /// A registered extern-type value (extern-types X1) — the ONE hosting variant every
    /// registry-contributed type shares; the tree-walker twin of the VM's `Payload::Extern`.
    /// `Rc<RefCell<…>>` gives the shared, interior-mutable cell a mutating method needs
    /// (reference semantics, the FileHandle discipline generalized); a pure type never borrows
    /// mutably.
    Extern(Rc<RefCell<noeta_stdlib::ExternBox>>),
    /// A lazy iterator (Track I.1a): a reference-semantic cursor over a list value — the tree-walker
    /// twin of the VM's `Payload::Iter`. `Rc<RefCell<…>>` gives the shared interior-mutable cursor so
    /// every alias advances the same iterator, exactly like a file handle.
    Iter(Rc<RefCell<IterState>>),
    /// An async future (Track A): the tree-walker twin of the VM's `Payload::Future`. In A.1 it wraps
    /// a lazy thunk closure — run to completion when awaited, not at the `async fn` call. `Rc` keeps
    /// copies cheap and matches the VM's shared heap object.
    Future(Rc<Value>),
    /// A **leaf timer future** (Track A.2): the tree-walker twin of the VM's `Payload::Timer`.
    /// `sleep(ms)` produces one carrying the absolute logical deadline (ms) at which it is ready;
    /// polling it consults the executor clock and reports `Pending` until then.
    Timer(u64),
    /// The async **pending** sentinel (Track A.3): the tree-walker twin of the VM's `Value::pending`
    /// immediate — what an async state-machine step returns when it suspends at an `.await`. Never
    /// escapes to user code (every poll site catches it), so it has no surface type.
    Pending,
    /// A **task handle** (Track A.3b): the tree-walker twin of the VM's `Payload::Handle`. The
    /// `Future<T>` `spawn e` returns, referencing a task by its `(scope index, task index)` in the
    /// interpreter's concurrency-scope stack.
    Handle(crate::ScopeId, crate::TaskId),
    /// A **leaf async-read future** (Track A.4c): the tree-walker twin of the VM's
    /// `Payload::AsyncIo`. The `Future<string>` `fs.read_async(path)` returns, carrying the id that
    /// tickets the pending read in the injected [`noeta_stdlib::Executor`].
    AsyncIo(u64),
    /// A **channel sender endpoint** (isolates I.1): the tree-walker twin of the VM's
    /// `Payload::Sender`. The `Sender<T>` `channel::<T>(cap)` yields, carrying the channel's index
    /// into the interpreter's channel table; `tx.send(v)`/`tx.close()` dispatch on it.
    Sender(crate::ChannelId),
    /// A **channel receiver endpoint** (isolates I.1): the twin of the VM's `Payload::Receiver`;
    /// `rx.recv()` dispatches on it.
    Receiver(crate::ChannelId),
    /// A **leaf channel-send future** (isolates I.1): the twin of the VM's `Payload::ChannelSend`.
    /// `tx.send(v)` produces one, carrying the channel index and the message `v` (held until enqueued).
    /// The third field is its capacity-0 **rendezvous phase** (isolates I.4c), shared by `Rc<Cell>` so
    /// the transition to `Deposited` persists across the re-polls of the same awaited future; ignored
    /// for a buffered channel.
    ChannelSend(crate::ChannelId, Rc<Value>, Rc<std::cell::Cell<SendPhase>>),
    /// A **leaf channel-recv future** (isolates I.1): the twin of the VM's `Payload::ChannelRecv`.
    /// `rx.recv()` produces one, carrying the channel index.
    ChannelRecv(crate::ChannelId),
    // (`Reactive` lived here until higher-order-abi H5 — the handles are registry extern
    // types now, their contents in the extensions' retained arena.)
}

/// The state machine behind a [`Value::Iter`] (Track I) — the tree-walker mirror of the VM's
/// `heap::IterState`. The base cursors a list; each adapter holds the source iterator(s) it pulls
/// from, so a pipeline fuses with no intermediate list.
#[derive(Debug)]
pub enum IterState {
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
    /// Yield `func(element)` for each element of `source` (`map(f)`, Track I.1c).
    Map { source: Value, func: Value },
    /// Yield the elements of `source` for which `pred(element)` is true (`filter(f)`, Track I.1c).
    Filter { source: Value, pred: Value },
    /// A generator (Track G): `step` is a closure called once per element, returning `?T`.
    Gen { step: Value },
}

/// The backing representation of a [`Value::List`] (P-PACK Phase 2). `Boxed` is the general form,
/// holding the same `Rc<Vec<Value>>` lists have always used — so every existing boxed-list code path
/// (copy-on-write reuse, refcount checks, `try_unwrap`) operates on the inner `Rc` unchanged.
/// `Packed` is the flat raw-primitive buffer for a `List<packed>` (2.3): one contiguous `Vec<u64>`
/// of primitive bits, interpreted through a [`PackedSchema`]. An operation not yet specialized for
/// it calls [`ListRepr::to_rc_vec`] (or [`ListRepr::get`]) to materialize boxed `Value::Object`s on
/// demand and runs the existing path, so the flat form stays invisible to `RunResult`.
#[derive(Clone)]
pub enum ListRepr {
    /// The general boxed list: elements are full `Value`s in an `Rc`-shared vector. `reflect` is the
    /// checker-resolved element type (runtime type-argument reflection, R1), set at literal
    /// construction so `type_of` recovers the list's element type after a `dyn` launder; `None` for
    /// every derived/mutated list (the tag survives pure aliasing only — a `Clone` of the `ListRepr`
    /// keeps it, but the list-producing ops rebuild through [`ListRepr::boxed`], which clears it). It
    /// lives beside the element vector, never inside it, so it is invisible to value semantics —
    /// `elements_eq` compares only the `Rc<Vec<Value>>`, the tree-walker twin of the VM's node tag.
    Boxed {
        items: Rc<Vec<Value>>,
        reflect: Option<Rc<TypeRepr>>,
    },
    /// A flat `List<packed>`: elements packed as raw primitive words, materialized on access.
    Packed(PackedList),
}

impl ListRepr {
    /// A boxed list with no reflected type tag — the constructor every list-producing path uses (a
    /// literal that carries a type stamps it afterward via [`ListRepr::with_reflect`]). Keeping the
    /// tag `None` here is what makes it survive pure aliasing only.
    pub(crate) fn boxed(items: Rc<Vec<Value>>) -> Self {
        ListRepr::Boxed {
            items,
            reflect: None,
        }
    }

    /// This list's reflected element type (R1), or `None` if untagged (a packed or derived list).
    pub(crate) fn reflect(&self) -> Option<Rc<TypeRepr>> {
        match self {
            ListRepr::Boxed { reflect, .. } => reflect.clone(),
            ListRepr::Packed(_) => None,
        }
    }

    /// This list with its reflected element type set to `tag` (R1) — used at literal construction to
    /// stamp the checker-resolved type. A no-op on a packed list (which reflects head-only).
    pub(crate) fn with_reflect(self, tag: Option<Rc<TypeRepr>>) -> Self {
        match self {
            ListRepr::Boxed { items, .. } => ListRepr::Boxed {
                items,
                reflect: tag,
            },
            packed => packed,
        }
    }

    /// The number of elements.
    pub(crate) fn len(&self) -> usize {
        match self {
            ListRepr::Boxed { items, .. } => items.len(),
            ListRepr::Packed(packed) => packed.len(),
        }
    }

    /// The raw flat byte buffer of a packed list (`to_bytes`, P-PACK 4.4), or `None` for a boxed list
    /// (which has no canonical serialized form — the caller errors).
    pub(crate) fn packed_raw_bytes(&self) -> Option<Vec<u8>> {
        match self {
            ListRepr::Packed(packed) => Some((*packed.bytes).clone()),
            ListRepr::Boxed { .. } => None,
        }
    }

    /// The elements as a boxed `Rc<Vec<Value>>`. For an already-boxed list this is a cheap
    /// `Rc::clone` (no element copy) — so routing a read-only op through it never regresses the boxed
    /// path; a packed buffer materializes every element into a fresh boxed vector.
    pub(crate) fn to_rc_vec(&self) -> Rc<Vec<Value>> {
        match self {
            ListRepr::Boxed { items, .. } => Rc::clone(items),
            ListRepr::Packed(packed) => Rc::new(packed.materialize()),
        }
    }

    /// The element at `index`, cloned (a refcount bump for a heap value), or `None` if out of range.
    /// A packed buffer materializes just the one element — no full-list materialization.
    pub(crate) fn get(&self, index: usize) -> Option<Value> {
        match self {
            ListRepr::Boxed { items, .. } => items.get(index).cloned(),
            ListRepr::Packed(packed) => packed.get(index),
        }
    }

    /// Element-wise equality, independent of representation (two lists are equal iff their elements
    /// are equal in order). A packed operand is materialized so equality is decided on `Value`s —
    /// never on raw words (which would, e.g., treat distinct float `NaN` bit-patterns as equal).
    pub(crate) fn elements_eq(&self, other: &ListRepr) -> bool {
        match (self, other) {
            (ListRepr::Boxed { items: a, .. }, ListRepr::Boxed { items: b, .. }) => a == b,
            _ => *self.to_rc_vec() == *other.to_rc_vec(),
        }
    }
}

impl fmt::Debug for ListRepr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A packed list debugs identically to the boxed list it represents, so `Debug` never
        // exposes the layout (and the existing `List({repr:?})` shape is preserved).
        match self {
            ListRepr::Boxed { items, .. } => write!(f, "{items:?}"),
            ListRepr::Packed(packed) => write!(f, "{:?}", packed.materialize()),
        }
    }
}

/// A `List<packed>` stored as one contiguous raw-primitive buffer (P-PACK Phase 2.3). The `schema`
/// describes how to pack a `Value::Object` element into, and materialize it back from, `word_count`
/// consecutive words; `words` holds `len` elements end-to-end. `Rc` keeps clones cheap, exactly as
/// the boxed list's `Rc<Vec<Value>>` does. The representation is invisible to `RunResult`: every
/// element observed (index, iterate, display, compare, JSON) is materialized through `schema` to the
/// same `Value::Object` the boxed list would hold.
#[derive(Clone)]
pub struct PackedList {
    schema: Rc<PackedSchema>,
    bytes: Rc<Vec<u8>>,
}

impl fmt::Debug for PackedList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug as the boxed list it represents, never exposing the flat layout.
        write!(f, "{:?}", self.materialize())
    }
}

/// The resolved layout of one packed element: the type to materialize, its fields in slot order
/// (parallel to `def.fields`), and the element's total word width. Built once at the construction
/// site (it needs the interpreter's scope to resolve nested struct types) and shared by every
/// element of the list.
pub(crate) struct PackedSchema {
    /// The element type, used to build a materialized [`Value::Object`].
    pub(crate) def: Rc<TypeDef>,
    /// One entry per field, in `def.fields` (slot) order.
    pub(crate) fields: Vec<PackedSlot>,
    /// Bytes per element — the sum of each field's [`SlotKind::byte_width`] (P-PACK 3.2b: a byte-
    /// addressed buffer, so an `f32` field is 4 bytes, not 8).
    pub(crate) byte_size: usize,
    /// Whether the list buffer is stored column-major (`@packed(Layout.Column)`, P-SIMD C2) — the
    /// eval mirror of `noeta_object::PackedSchema::column`. Performance-only; observed values are
    /// identical either way (the differential pins that both backends agree).
    pub(crate) column: bool,
}

impl PackedSchema {
    /// The byte offset of field `slot` within a single row — the sum of the prior fields' widths.
    fn field_prefix(&self, slot: usize) -> usize {
        self.fields[..slot]
            .iter()
            .map(|s| s.kind.byte_width())
            .sum()
    }

    /// The number of elements a buffer of `len` bytes holds.
    fn count(&self, len: usize) -> usize {
        len.checked_div(self.byte_size).unwrap_or(0)
    }

    /// The byte offset of element `i`'s field `slot` in a buffer holding `count` elements — the one
    /// place the row/column layout axis is interpreted (mirrors `noeta_object::PackedSchema`).
    fn field_offset(&self, i: usize, slot: usize, count: usize) -> usize {
        let prefix = self.field_prefix(slot);
        if self.column {
            count * prefix + i * self.fields[slot].kind.byte_width()
        } else {
            i * self.byte_size + prefix
        }
    }
}

/// One field of a [`PackedSchema`].
pub(crate) struct PackedSlot {
    pub(crate) kind: SlotKind,
}

/// Project one packed field onto the seam's neutral [`noeta_stdlib::PackedField`] (N3.4).
fn seam_field(kind: &SlotKind) -> noeta_stdlib::PackedField {
    use noeta_stdlib::PackedField;
    match kind {
        SlotKind::Int => PackedField::Int,
        SlotKind::Float => PackedField::Float,
        SlotKind::F32 => PackedField::F32,
        SlotKind::F64 => PackedField::F64,
        SlotKind::IntN { bits, signed } => PackedField::IntN {
            bits: *bits,
            signed: *signed,
        },
        SlotKind::Bool => PackedField::Bool,
        SlotKind::Struct(inner) => {
            PackedField::Struct(inner.fields.iter().map(|f| seam_field(&f.kind)).collect())
        }
    }
}

/// A packed field's storage: a primitive occupying one word, or a nested packed struct flattened
/// inline (its own sub-schema describing how its fields are laid out contiguously).
pub(crate) enum SlotKind {
    Int,
    Float,
    /// A 32-bit float field (P-PACK Phase 3).
    F32,
    /// An explicit 64-bit float field `f64` (packed-widths arc) — 8 bytes, storage-identical to
    /// `Float`.
    F64,
    /// A fixed-width integer field `i8..i64`/`u8..u64` (packed-widths arc): `bits/8` bytes, `signed`
    /// deciding read-back extension.
    IntN { bits: u8, signed: bool },
    Bool,
    Struct(Rc<PackedSchema>),
}

impl PackedList {
    /// The neutral seam view of this list's element layout (package-manager N3.4) — what
    /// `NativeCtx::with_packed*` lends a raw-buffer kernel. The eval twin of the VM's projection
    /// from its interned `noeta_object::PackedSchema`.
    pub(crate) fn seam_view(&self) -> noeta_stdlib::PackedView {
        noeta_stdlib::PackedView {
            fields: self
                .schema
                .fields
                .iter()
                .map(|f| seam_field(&f.kind))
                .collect(),
            byte_size: self.schema.byte_size,
            column: self.schema.column,
            count: self.schema.count(self.bytes.len()),
        }
    }

    /// The raw byte buffer, borrowed (N3.4 `with_packed`).
    pub(crate) fn raw(&self) -> &[u8] {
        &self.bytes
    }

    /// A packed list **sharing this one's schema** over a fresh buffer — the eval half of
    /// `NativeCtx::make_packed_like` (N3.4). `bytes` must hold a whole number of elements.
    pub(crate) fn like(&self, bytes: Vec<u8>) -> PackedList {
        debug_assert!(
            self.schema.byte_size > 0 && bytes.len().is_multiple_of(self.schema.byte_size),
            "make_packed_like: a whole number of elements"
        );
        PackedList {
            schema: Rc::clone(&self.schema),
            bytes: Rc::new(bytes),
        }
    }

    /// Mutate the byte buffer through `f`, copy-on-write (`NativeCtx::with_packed_mut`, N3.4):
    /// `Rc::make_mut` mutates in place iff this list is the buffer's sole owner, else clones —
    /// exactly the value-semantics COW the boxed list's `Rc<Vec<Value>>` gets for free.
    pub(crate) fn mutate_bytes(&mut self, f: &mut dyn FnMut(&noeta_stdlib::PackedView, &mut [u8])) {
        let view = self.seam_view();
        f(
            &view,
            Rc::<Vec<u8>>::make_mut(&mut self.bytes).as_mut_slice(),
        );
    }

    /// An empty packed list with the given `schema` — the start of a streaming flat build
    /// (P-PACK 2.5). Elements are appended one at a time by [`PackedList::push`].
    pub(crate) fn empty(schema: Rc<PackedSchema>) -> PackedList {
        PackedList {
            schema,
            bytes: Rc::new(Vec::new()),
        }
    }

    /// A packed list directly from a result byte buffer (P-PACK 4.2 bulk kernels). The `bytes` must
    /// match `schema` (a whole number of `byte_size`-wide elements); the caller guarantees it.
    pub(crate) fn from_bytes(schema: Rc<PackedSchema>, bytes: Vec<u8>) -> PackedList {
        PackedList {
            schema,
            bytes: Rc::new(bytes),
        }
    }

    /// Pack one `element` onto the end of the buffer, extending it **in place** when uniquely owned
    /// (which it always is along the streaming-construction chain — the accumulator is an ANF temp).
    /// Returns `false` without modifying the buffer if the element fails to pack (a non-object, or a
    /// field whose runtime kind disagrees with the schema) so the caller can demote to a boxed list;
    /// the partial pack is staged in a scratch vector first so a failure leaves the buffer intact.
    #[must_use]
    pub(crate) fn push(&mut self, element: &Value) -> bool {
        let mut staged = Vec::with_capacity(self.schema.byte_size);
        if pack_object(element, &self.schema, &mut staged).is_none() {
            return false;
        }
        let buf = Rc::make_mut(&mut self.bytes);
        if self.schema.column {
            // Column-major append (P-SIMD C2): the new element joins the *end of every column*, so
            // the whole buffer is rebuilt (O(n)) — column layout trades cheap append for fast bulk
            // field math, as designed. `staged` is one row (fields in slot order).
            *buf = column_append(&self.schema, buf, &staged);
        } else {
            buf.extend(staged);
        }
        true
    }

    /// Materialize every element into a boxed vector — used when a packed build must demote (an
    /// element failed to pack) so construction can continue on the boxed path.
    pub(crate) fn to_boxed(&self) -> Vec<Value> {
        self.materialize()
    }

    /// The number of elements.
    fn len(&self) -> usize {
        self.bytes.len() / self.schema.byte_size
    }

    /// Materialize the element at `index` into a boxed `Value::Object`, or `None` if out of range.
    /// Reads each field at its layout-resolved offset ([`PackedSchema::field_offset`]), so a row and
    /// a column list yield an identical element (differing only in *where* the bytes were read from).
    fn get(&self, index: usize) -> Option<Value> {
        let n = self.len();
        if index >= n {
            return None;
        }
        let mut slots = Vec::with_capacity(self.schema.fields.len());
        for (slot, s) in self.schema.fields.iter().enumerate() {
            let off = self.schema.field_offset(index, slot, n);
            slots.push(decode_slot(&s.kind, &self.bytes, off));
        }
        let object = ObjectValue::new(Rc::clone(&self.schema.def), slots);
        Some(Value::Object(Rc::new(object)))
    }

    /// Read a single field of the element at `index` (P-PACK 2.5+ fused `list[i].field`), decoding
    /// only that field's word(s) — no full-element materialization. Returns `None` if `index` is out
    /// of range or `name` is not a field (the checker only fuses real field reads on a packed type, so
    /// a hit is the norm; the caller falls back on `None`).
    pub(crate) fn field(&self, index: usize, name: &str) -> Option<Value> {
        let n = self.len();
        if index >= n {
            return None;
        }
        let slot = self.schema.def.slot_of(name)?;
        let at = self.schema.field_offset(index, slot, n);
        Some(decode_slot(&self.schema.fields[slot].kind, &self.bytes, at))
    }

    /// Materialize every element into a boxed vector — the fallback for any op not specialized for
    /// the flat representation.
    fn materialize(&self) -> Vec<Value> {
        (0..self.len())
            .map(|i| self.get(i).expect("index in range"))
            .collect()
    }
}

impl SlotKind {
    /// The number of bytes this field occupies (P-PACK 3.2b): an `f32` is 4, the other primitives 8,
    /// a nested struct its own `byte_size`.
    fn byte_width(&self) -> usize {
        match self {
            SlotKind::Bool => 1,
            SlotKind::F32 => 4,
            SlotKind::Int | SlotKind::Float | SlotKind::F64 => 8,
            SlotKind::IntN { bits, .. } => (*bits as usize) / 8,
            SlotKind::Struct(inner) => inner.byte_size,
        }
    }
}

/// Read 8 little-endian bytes at `offset` as a `u64` (the `int`/`float`/`bool` storage word).
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// Read 4 little-endian bytes at `offset` as a `u32` (the `f32` storage word).
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Read a fixed-width integer slot (`bits/8` little-endian bytes at `offset`) back into the runtime's
/// 8-byte `int` (packed-widths arc). A **signed** slot sign-extends its top bit (a stored `-1i8`
/// reads back `-1`); an **unsigned** slot zero-extends (`255u8` reads back `255`). The runtime scalar
/// is width-erased, so the result is a plain `i64` regardless of the stored width.
fn read_intn(bytes: &[u8], offset: usize, bits: u8, signed: bool) -> i64 {
    let n = (bits as usize) / 8;
    let mut raw: u64 = 0;
    for i in 0..n {
        raw |= (bytes[offset + i] as u64) << (8 * i);
    }
    if signed && bits < 64 {
        let sign_bit = 1u64 << (bits - 1);
        if raw & sign_bit != 0 {
            raw |= !((1u64 << bits) - 1);
        }
    }
    raw as i64
}

/// Append a fixed-width integer's low `bits/8` little-endian bytes to `out` (packed-widths arc). The
/// runtime carries the value as an 8-byte `int`; only its low bytes are stored, so the checker's
/// range rules are what keep a value inside the slot (a raw `from_bytes` buffer is trusted as-is).
fn write_intn(out: &mut Vec<u8>, value: i64, bits: u8) {
    let n = (bits as usize) / 8;
    let raw = value as u64;
    for i in 0..n {
        out.push((raw >> (8 * i)) as u8);
    }
}

/// Decode one field at byte `offset` into a boxed [`Value`] — the per-field counterpart of
/// [`unpack_object`], used by [`PackedList::field`] to read a single field without materializing the
/// whole element.
fn decode_slot(kind: &SlotKind, bytes: &[u8], offset: usize) -> Value {
    match kind {
        SlotKind::Int => Value::Int(read_u64(bytes, offset) as i64),
        SlotKind::Float => Value::Float(f64::from_bits(read_u64(bytes, offset))),
        SlotKind::F32 => Value::F32(f32::from_bits(read_u32(bytes, offset))),
        SlotKind::F64 => Value::Float(f64::from_bits(read_u64(bytes, offset))),
        SlotKind::IntN { bits, signed } => Value::Int(read_intn(bytes, offset, *bits, *signed)),
        SlotKind::Bool => Value::Bool(bytes[offset] != 0),
        SlotKind::Struct(inner) => unpack_object(inner, bytes, offset).0,
    }
}

/// Pack one element `value` (a `Value::Object` of `schema`'s type) onto the end of `out` — each
/// primitive field as its little-endian bytes (`f32` 4, others 8), recursing into nested packed
/// structs. Returns `None` on any shape mismatch.
fn pack_object(value: &Value, schema: &PackedSchema, out: &mut Vec<u8>) -> Option<()> {
    let Value::Object(object) = value else {
        return None;
    };
    let slots = object.slots.borrow();
    if slots.len() != schema.fields.len() {
        return None;
    }
    for (slot, field) in schema.fields.iter().zip(slots.iter()) {
        match (&slot.kind, field) {
            (SlotKind::Int, Value::Int(i)) => out.extend_from_slice(&(*i as u64).to_le_bytes()),
            (SlotKind::Float, Value::Float(x)) => out.extend_from_slice(&x.to_bits().to_le_bytes()),
            (SlotKind::F32, Value::F32(f)) => out.extend_from_slice(&f.to_bits().to_le_bytes()),
            // `f64`/`iN`/`uN` fields carry width-erased scalars at runtime (a `Float`/`Int`); only
            // the buffer slot is narrowed (packed-widths arc).
            (SlotKind::F64, Value::Float(x)) => out.extend_from_slice(&x.to_bits().to_le_bytes()),
            (SlotKind::IntN { bits, .. }, Value::Int(i)) => write_intn(out, *i, *bits),
            (SlotKind::Bool, Value::Bool(b)) => out.push(u8::from(*b)),
            (SlotKind::Struct(inner), nested) => pack_object(nested, inner, out)?,
            _ => return None,
        }
    }
    Some(())
}

/// Append one packed `row` (`byte_size` bytes, fields in slot order) to a column-major buffer,
/// returning the rebuilt buffer with each field's column extended by the new element (P-SIMD C2).
/// O(n): column layout stores each field contiguously, so a new element inserts into the middle of
/// the buffer at every column's end. Used only on the `Layout.Column` path.
fn column_append(schema: &PackedSchema, buf: &[u8], row: &[u8]) -> Vec<u8> {
    let n = schema.count(buf.len());
    let mut out = Vec::with_capacity(buf.len() + schema.byte_size);
    let mut row_at = 0;
    for (slot, s) in schema.fields.iter().enumerate() {
        let w = s.kind.byte_width();
        let base = n * schema.field_prefix(slot);
        out.extend_from_slice(&buf[base..base + n * w]);
        out.extend_from_slice(&row[row_at..row_at + w]);
        row_at += w;
    }
    out
}

/// Materialize one element from `bytes` starting at byte `offset`, returning the value and the offset
/// just past it (so nested structs and the caller advance in lock-step with [`pack_object`]).
fn unpack_object(schema: &PackedSchema, bytes: &[u8], offset: usize) -> (Value, usize) {
    let mut slots = Vec::with_capacity(schema.fields.len());
    let mut at = offset;
    for slot in &schema.fields {
        match &slot.kind {
            SlotKind::Int => {
                slots.push(Value::Int(read_u64(bytes, at) as i64));
                at += 8;
            }
            SlotKind::Float => {
                slots.push(Value::Float(f64::from_bits(read_u64(bytes, at))));
                at += 8;
            }
            SlotKind::F32 => {
                slots.push(Value::F32(f32::from_bits(read_u32(bytes, at))));
                at += 4;
            }
            SlotKind::F64 => {
                slots.push(Value::Float(f64::from_bits(read_u64(bytes, at))));
                at += 8;
            }
            SlotKind::IntN { bits, signed } => {
                slots.push(Value::Int(read_intn(bytes, at, *bits, *signed)));
                at += (*bits as usize) / 8;
            }
            SlotKind::Bool => {
                slots.push(Value::Bool(bytes[at] != 0));
                at += 1;
            }
            SlotKind::Struct(inner) => {
                let (nested, next) = unpack_object(inner, bytes, at);
                slots.push(nested);
                at = next;
            }
        }
    }
    let object = ObjectValue::new(Rc::clone(&schema.def), slots);
    (Value::Object(Rc::new(object)), at)
}

impl Value {
    /// Construct a boxed list value from its elements (the general representation). Untagged (R1);
    /// a literal that carries a reflected type stamps it afterward via [`ListRepr::with_reflect`].
    pub(crate) fn list(items: Vec<Value>) -> Value {
        Value::List(ListRepr::boxed(Rc::new(items)))
    }

    /// Construct a boxed list value from an already-shared `Rc<Vec<Value>>`. Untagged (R1).
    pub(crate) fn list_rc(items: Rc<Vec<Value>>) -> Value {
        Value::List(ListRepr::boxed(items))
    }

    /// Construct a map value from its shared entries, **untagged** (R1) — the constructor every
    /// map-producing path uses. A literal that carries a reflected `Map(K, V)` type stamps it via
    /// [`Value::map_value_tagged`]; every other map (derived, mutated) stays untagged and reflects
    /// head-only.
    pub(crate) fn map_value(entries: Rc<BTreeMap<noeta_stdlib::MapKey, Value>>) -> Value {
        Value::Map(entries, None)
    }

    /// As [`Value::map_value`], but carrying the reflected `Map(K, V)` type (R1) — used only at map
    /// literal construction.
    pub(crate) fn map_value_tagged(
        entries: Rc<BTreeMap<noeta_stdlib::MapKey, Value>>,
        reflect: Option<Rc<TypeRepr>>,
    ) -> Value {
        Value::Map(entries, reflect)
    }

    /// Construct a set value from its canonical elements, **untagged** — the constructor every
    /// set-producing path uses (`to_set` on a tagged list stamps the derived `Set(T)` via
    /// [`Value::set_value_tagged`]; every other set stays untagged and reflects head-only).
    pub(crate) fn set_value(items: Rc<Vec<Value>>) -> Value {
        Value::Set(items, None)
    }

    /// As [`Value::set_value`], but carrying a reflected `Set(T)` type — used by `to_set` to carry the
    /// element type from the source list's tag.
    pub(crate) fn set_value_tagged(items: Rc<Vec<Value>>, reflect: Option<Rc<TypeRepr>>) -> Value {
        Value::Set(items, reflect)
    }

    /// Build a packed `List` value directly from a result byte buffer + schema (P-PACK 4.2).
    pub(crate) fn packed_list_from(schema: Rc<PackedSchema>, bytes: Vec<u8>) -> Value {
        Value::List(ListRepr::Packed(PackedList::from_bytes(schema, bytes)))
    }

    /// The display form used by `echo`, `~` concatenation, and interpolation. Built in per value
    /// kind here in the tree-walker; a user type's `Display` `impl` is dispatched a level up (in the
    /// interpreter), mirroring how the VM renders values.
    pub fn display(&self) -> String {
        match self {
            Value::Unit => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::F32(f) => format_f32(*f),
            Value::Str(s) => s.clone(),
            // A byte buffer renders as a length summary (`<N bytes>`) — opaque, identical on the VM;
            // its content round-trips through `from_bytes`, not display.
            Value::Bytes(b) => format!("<{} bytes>", b.len()),
            Value::List(repr) => {
                let parts: Vec<String> = repr.to_rc_vec().iter().map(Value::repr).collect();
                format!("[{}]", parts.join(", "))
            }
            // A tuple renders parenthesized with `repr` elements (`(1, "a")`) — the VM matches.
            Value::Tuple(items) => {
                let parts: Vec<String> = items.iter().map(Value::repr).collect();
                format!("({})", parts.join(", "))
            }
            // Braces with no key colons (`{1, 2, 3}`) distinguish a set from a non-empty map;
            // an empty set is `{}`, like an empty map.
            Value::Set(items, _) => {
                let parts: Vec<String> = items.iter().map(Value::repr).collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Map(entries, _) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.render(), v.repr()))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Function(_) => "<fn>".to_string(),
            // A selectively-imported module function renders as a function (matching the VM's
            // `Payload::ModuleFn` → `<fn>`), not with the module's `<module …>` form.
            Value::ModuleFn(..) | Value::MethodHandle(..) | Value::BoundMethod(..) => {
                "<fn>".to_string()
            }
            Value::Builtin(b) => format!("<builtin {}>", b.name()),
            Value::EnumType(def) => format!("<enum {}>", def.name()),
            Value::Enum(value) => value.display(),
            Value::Type(def) => format!("<type {}>", def.name()),
            Value::Object(object) => object.display(),
            Value::NativeModule(module) => format!("<module {module}>"),
            // `<file "path" (mode)>`, rendered by the shared handle so the VM matches exactly.
            // An extern-type value renders through its contract, identically on both backends.
            Value::Extern(e) => e.borrow().display_string(),
            Value::Iter(_) => "<iterator>".to_string(),
            Value::Future(_)
            | Value::Timer(_)
            | Value::Handle(..)
            | Value::AsyncIo(_)
            | Value::ChannelSend(..)
            | Value::ChannelRecv(_) => "<future>".to_string(),
            Value::Sender(_) => "<sender>".to_string(),
            Value::Receiver(_) => "<receiver>".to_string(),
            Value::Pending => "<pending>".to_string(),
        }
    }

    /// The representation of a value *inside* a collection or object: strings are quoted
    /// so the structure stays legible (`["a", "b"]`, not `[a, b]`).
    pub(crate) fn repr(&self) -> String {
        match self {
            Value::Str(s) => format!("{s:?}"),
            other => other.display(),
        }
    }

    /// The user-facing name of this value's type, for diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::F32(_) => "f32",
            Value::Str(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Tuple(_) => "tuple",
            Value::Set(..) => "set",
            Value::Map(..) => "map",
            Value::Function(_)
            | Value::Builtin(_)
            | Value::ModuleFn(..)
            | Value::MethodHandle(..)
            | Value::BoundMethod(..) => "function",
            Value::EnumType(_) => "enum type",
            Value::Enum(_) => "enum",
            Value::Type(_) => "type",
            Value::Object(_) => "object",
            Value::NativeModule(_) => "module",
            // The extern type's human-facing short name (`Uuid`) — the display form of the
            // value's qualified identity (`std.id.Uuid`); identity paths read `type_identity()`.
            Value::Extern(e) => e.borrow().type_display_name(),
            Value::Iter(_) => "iterator",
            Value::Future(_)
            | Value::Timer(_)
            | Value::Handle(..)
            | Value::AsyncIo(_)
            | Value::ChannelSend(..)
            | Value::ChannelRecv(_) => "future",
            Value::Sender(_) => "sender",
            Value::Receiver(_) => "receiver",
            Value::Pending => "pending",
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, "Unit"),
            Value::Bool(b) => write!(f, "Bool({b})"),
            Value::Int(i) => write!(f, "Int({i})"),
            Value::Float(x) => write!(f, "Float({x})"),
            Value::F32(x) => write!(f, "F32({x})"),
            Value::Str(s) => write!(f, "Str({s:?})"),
            Value::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
            Value::List(repr) => write!(f, "List({repr:?})"),
            Value::Tuple(items) => write!(f, "Tuple({items:?})"),
            Value::Set(items, _) => write!(f, "Set({items:?})"),
            Value::Map(entries, _) => write!(f, "Map({entries:?})"),
            Value::Function(_) => write!(f, "Function(<fn>)"),
            Value::ModuleFn(module, func) => write!(f, "ModuleFn({module}.{func})"),
            Value::MethodHandle(ty, method, associated) => {
                write!(
                    f,
                    "MethodHandle({ty}.{method}{})",
                    if *associated { " assoc" } else { "" }
                )
            }
            Value::BoundMethod(recv, method) => write!(f, "BoundMethod({recv:?}.{method})"),
            Value::Builtin(b) => write!(f, "Builtin({})", b.name()),
            Value::EnumType(def) => write!(f, "EnumType({})", def.name()),
            Value::Enum(value) => write!(f, "Enum({})", value.display()),
            Value::Type(def) => write!(f, "Type({})", def.name()),
            Value::Object(object) => write!(f, "Object({})", object.display()),
            Value::NativeModule(module) => write!(f, "NativeModule({module})"),
            Value::Extern(e) => write!(f, "Extern({})", e.borrow().display_string()),
            Value::Iter(state) => write!(f, "Iter({:?})", state.borrow()),
            Value::Future(thunk) => write!(f, "Future({thunk:?})"),
            Value::Timer(deadline) => write!(f, "Timer({deadline})"),
            Value::Pending => write!(f, "Pending"),
            Value::Handle(scope, task) => write!(f, "Handle({}, {})", scope.index(), task.index()),
            Value::AsyncIo(id) => write!(f, "AsyncIo({id})"),
            Value::Sender(id) => write!(f, "Sender({})", id.index()),
            Value::Receiver(id) => write!(f, "Receiver({})", id.index()),
            Value::ChannelSend(id, value, _) => write!(f, "ChannelSend({}, {value:?})", id.index()),
            Value::ChannelRecv(id) => write!(f, "ChannelRecv({})", id.index()),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::F32(a), Value::F32(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Bytes(a), Value::Bytes(b)) => a == b,
            (Value::List(a), Value::List(b)) => a.elements_eq(b),
            (Value::Tuple(a), Value::Tuple(b)) => a == b,
            (Value::Set(a, _), Value::Set(b, _)) => a == b,
            (Value::Map(a, _), Value::Map(b, _)) => a == b,
            (Value::Enum(a), Value::Enum(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => a == b,
            (Value::NativeModule(a), Value::NativeModule(b)) => a == b,
            // A selectively-imported module function compares by its `(module, func)` pair.
            (Value::ModuleFn(am, af), Value::ModuleFn(bm, bf)) => am == bm && af == bf,
            // A method handle compares by its `(ty, method, associated)` triple.
            (Value::MethodHandle(at, am, aa), Value::MethodHandle(bt, bm, ba)) => {
                at == bt && am == bm && aa == ba
            }
            // A bound handle compares by method name + receiver equality.
            (Value::BoundMethod(ra, ma), Value::BoundMethod(rb, mb)) => ma == mb && ra == rb,
            // Extern-type values compare through their contract (extern-types X2). This impl is
            // the one enum/list/tuple/set *payload* comparisons route through, so a missing arm
            // here is the classic silent-wrong-`false` hole (`some(u) == some(u)` was false).
            (Value::Extern(a), Value::Extern(b)) => a.borrow().eq_value(&**b.borrow()),
            // Functions and types are not structurally comparable.
            _ => false,
        }
    }
}

/// Render a float deterministically. Whole-valued floats keep a trailing `.0` so they
/// are visibly distinct from ints (`3.0`, not `3`).
fn format_float(f: f64) -> String {
    noeta_stdlib::format_float(f)
}

/// Display an `f32` (P-PACK Phase 3) at f32 precision (the shortest round-tripping f32 decimal), so
/// e.g. `0.1f32` shows `0.1`, not the f64-widened `0.10000000149…`. Delegates to the shared
/// [`noeta_stdlib::format_f32`] so the two backends agree by construction.
pub(crate) fn format_f32(f: f32) -> String {
    noeta_stdlib::format_f32(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: Vec<Value>) -> Value {
        Value::list(items)
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::map_value(Rc::new(
            pairs
                .iter()
                .map(|(k, v)| (noeta_stdlib::MapKey::from(*k), v.clone()))
                .collect(),
        ))
    }

    #[test]
    fn display_of_scalars() {
        assert_eq!(Value::Unit.display(), "");
        assert_eq!(Value::Bool(true).display(), "true");
        assert_eq!(Value::Bool(false).display(), "false");
        assert_eq!(Value::Int(-42).display(), "-42");
        assert_eq!(Value::Str("hi".into()).display(), "hi");
    }

    #[test]
    fn float_formatting_keeps_whole_values_distinct_from_ints() {
        assert_eq!(format_float(3.0), "3.0");
        assert_eq!(format_float(-2.0), "-2.0");
        assert_eq!(format_float(2.5), "2.5");
        assert_eq!(format_float(-1.25), "-1.25");
        assert_eq!(format_float(0.0), "0.0");
        // Non-finite values fall back to the default formatting rather than `.0`.
        assert_eq!(format_float(f64::INFINITY), "inf");
        assert_eq!(format_float(f64::NAN), "NaN");
    }

    #[test]
    fn repr_quotes_strings_but_display_does_not() {
        let s = Value::Str("x".into());
        assert_eq!(s.display(), "x");
        assert_eq!(s.repr(), "\"x\"");
        // Non-strings render the same either way.
        assert_eq!(Value::Int(1).repr(), "1");
    }

    #[test]
    fn collections_use_repr_for_their_elements() {
        assert_eq!(
            list(vec![Value::Int(1), Value::Str("a".into())]).display(),
            "[1, \"a\"]"
        );
        // Maps iterate in deterministic (sorted) key order.
        assert_eq!(
            map(&[("b", Value::Int(2)), ("a", Value::Int(1))]).display(),
            "{\"a\": 1, \"b\": 2}"
        );
        assert_eq!(list(vec![]).display(), "[]");
    }

    #[test]
    fn type_names() {
        assert_eq!(Value::Unit.type_name(), "unit");
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::Int(0).type_name(), "int");
        assert_eq!(Value::Float(0.0).type_name(), "float");
        assert_eq!(Value::Str(String::new()).type_name(), "string");
        assert_eq!(list(vec![]).type_name(), "list");
        assert_eq!(map(&[]).type_name(), "map");
        assert_eq!(Value::Builtin(Builtin::Len).type_name(), "function");
    }

    #[test]
    fn structural_equality_and_cross_kind_inequality() {
        assert_eq!(Value::Int(1), Value::Int(1));
        assert_ne!(Value::Int(1), Value::Int(2));
        assert_eq!(Value::Unit, Value::Unit);
        assert_eq!(list(vec![Value::Int(1)]), list(vec![Value::Int(1)]));
        assert_ne!(list(vec![Value::Int(1)]), list(vec![Value::Int(2)]));
        // Different kinds are never equal; functions are never equal even to themselves.
        assert_ne!(Value::Int(1), Value::Bool(true));
        assert_ne!(Value::Builtin(Builtin::Len), Value::Builtin(Builtin::Len));
    }

    #[test]
    fn debug_is_shallow_and_does_not_panic() {
        // Debug must never recurse into the (possibly cyclic) closure scope graph.
        assert_eq!(format!("{:?}", Value::Int(7)), "Int(7)");
        assert_eq!(
            format!("{:?}", Value::Builtin(Builtin::Sum)),
            "Builtin(sum)"
        );
    }
}
