//! The runtime value representation for the M0 tree-walker.
//!
//! Deliberately a simple boxed `enum` in M0. M1 replaces this with the NaN-boxed
//! value representation and the shape-based object model; keeping it behind this type
//! (and the `display()`/`type_name()` methods) keeps that swap local.
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

use lang_stdlib::FileHandle;

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
    /// display, and equality are deterministic and identical to the VM's `Payload::Set`.
    Set(Rc<Vec<Value>>),
    /// An immutable string-keyed map. `BTreeMap` gives deterministic iteration order.
    Map(Rc<BTreeMap<String, Value>>),
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
    /// An `fs.open` file handle (M2.5): a mutable cursor. `Rc<RefCell<…>>` gives the shared,
    /// interior-mutable state the VM gets from its heap object; the `FileHandle` itself is the
    /// same shared type both backends advance, so behavior is identical by construction.
    FileHandle(Rc<RefCell<FileHandle>>),
    /// A lazy iterator (Track I.1a): a reference-semantic cursor over a list value — the tree-walker
    /// twin of the VM's `Payload::Iter`. `Rc<RefCell<…>>` gives the shared interior-mutable cursor so
    /// every alias advances the same iterator, exactly like a file handle.
    Iter(Rc<RefCell<IterState>>),
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
    /// The general boxed list: elements are full `Value`s in an `Rc`-shared vector.
    Boxed(Rc<Vec<Value>>),
    /// A flat `List<packed>`: elements packed as raw primitive words, materialized on access.
    Packed(PackedList),
}

impl ListRepr {
    /// The number of elements.
    pub(crate) fn len(&self) -> usize {
        match self {
            ListRepr::Boxed(items) => items.len(),
            ListRepr::Packed(packed) => packed.len(),
        }
    }

    /// The raw flat byte buffer of a packed list (`to_bytes`, P-PACK 4.4), or `None` for a boxed list
    /// (which has no canonical serialized form — the caller errors).
    pub(crate) fn packed_raw_bytes(&self) -> Option<Vec<u8>> {
        match self {
            ListRepr::Packed(packed) => Some((*packed.bytes).clone()),
            ListRepr::Boxed(_) => None,
        }
    }

    /// The elements as a boxed `Rc<Vec<Value>>`. For an already-boxed list this is a cheap
    /// `Rc::clone` (no element copy) — so routing a read-only op through it never regresses the boxed
    /// path; a packed buffer materializes every element into a fresh boxed vector.
    pub(crate) fn to_rc_vec(&self) -> Rc<Vec<Value>> {
        match self {
            ListRepr::Boxed(items) => Rc::clone(items),
            ListRepr::Packed(packed) => Rc::new(packed.materialize()),
        }
    }

    /// The element at `index`, cloned (a refcount bump for a heap value), or `None` if out of range.
    /// A packed buffer materializes just the one element — no full-list materialization.
    pub(crate) fn get(&self, index: usize) -> Option<Value> {
        match self {
            ListRepr::Boxed(items) => items.get(index).cloned(),
            ListRepr::Packed(packed) => packed.get(index),
        }
    }

    /// Element-wise equality, independent of representation (two lists are equal iff their elements
    /// are equal in order). A packed operand is materialized so equality is decided on `Value`s —
    /// never on raw words (which would, e.g., treat distinct float `NaN` bit-patterns as equal).
    pub(crate) fn elements_eq(&self, other: &ListRepr) -> bool {
        match (self, other) {
            (ListRepr::Boxed(a), ListRepr::Boxed(b)) => a == b,
            _ => *self.to_rc_vec() == *other.to_rc_vec(),
        }
    }
}

impl fmt::Debug for ListRepr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A packed list debugs identically to the boxed list it represents, so `Debug` never
        // exposes the layout (and the existing `List({repr:?})` shape is preserved).
        match self {
            ListRepr::Boxed(items) => write!(f, "{items:?}"),
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
}

/// One field of a [`PackedSchema`].
pub(crate) struct PackedSlot {
    pub(crate) kind: SlotKind,
}

/// A packed field's storage: a primitive occupying one word, or a nested packed struct flattened
/// inline (its own sub-schema describing how its fields are laid out contiguously).
pub(crate) enum SlotKind {
    Int,
    Float,
    /// A 32-bit float field (P-PACK Phase 3).
    F32,
    Bool,
    Struct(Rc<PackedSchema>),
}

impl PackedList {
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

    /// If this is a `List<Vec3<f32>>` (element schema = exactly three `f32` fields), its shared
    /// schema and a copy of its byte buffer — the input to the bulk `vec` kernels (P-PACK 4.2).
    pub(crate) fn vec3_data(&self) -> Option<(Rc<PackedSchema>, Vec<u8>)> {
        if self.schema.fields.len() == 3
            && self
                .schema
                .fields
                .iter()
                .all(|f| matches!(f.kind, SlotKind::F32))
        {
            Some((Rc::clone(&self.schema), (*self.bytes).clone()))
        } else {
            None
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
        Rc::make_mut(&mut self.bytes).extend(staged);
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
    fn get(&self, index: usize) -> Option<Value> {
        if index >= self.len() {
            return None;
        }
        let offset = index * self.schema.byte_size;
        let (value, _) = unpack_object(&self.schema, &self.bytes, offset);
        Some(value)
    }

    /// Read a single field of the element at `index` (P-PACK 2.5+ fused `list[i].field`), decoding
    /// only that field's word(s) — no full-element materialization. Returns `None` if `index` is out
    /// of range or `name` is not a field (the checker only fuses real field reads on a packed type, so
    /// a hit is the norm; the caller falls back on `None`).
    pub(crate) fn field(&self, index: usize, name: &str) -> Option<Value> {
        if index >= self.len() {
            return None;
        }
        let slot = self.schema.def.slot_of(name)?;
        // Field `slot`'s byte offset within the element is the sum of the prior fields' widths.
        let mut at = index * self.schema.byte_size;
        for s in &self.schema.fields[..slot] {
            at += s.kind.byte_width();
        }
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
            SlotKind::Int | SlotKind::Float => 8,
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

/// Decode one field at byte `offset` into a boxed [`Value`] — the per-field counterpart of
/// [`unpack_object`], used by [`PackedList::field`] to read a single field without materializing the
/// whole element.
fn decode_slot(kind: &SlotKind, bytes: &[u8], offset: usize) -> Value {
    match kind {
        SlotKind::Int => Value::Int(read_u64(bytes, offset) as i64),
        SlotKind::Float => Value::Float(f64::from_bits(read_u64(bytes, offset))),
        SlotKind::F32 => Value::F32(f32::from_bits(read_u32(bytes, offset))),
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
            (SlotKind::Bool, Value::Bool(b)) => out.push(u8::from(*b)),
            (SlotKind::Struct(inner), nested) => pack_object(nested, inner, out)?,
            _ => return None,
        }
    }
    Some(())
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
    /// Construct a boxed list value from its elements (the general representation).
    pub(crate) fn list(items: Vec<Value>) -> Value {
        Value::List(ListRepr::Boxed(Rc::new(items)))
    }

    /// Construct a boxed list value from an already-shared `Rc<Vec<Value>>`.
    pub(crate) fn list_rc(items: Rc<Vec<Value>>) -> Value {
        Value::List(ListRepr::Boxed(items))
    }

    /// If this is a packed `List<Vec3<f32>>`, its schema and byte buffer (P-PACK 4.2 bulk `vec`).
    pub(crate) fn packed_vec3_data(&self) -> Option<(Rc<PackedSchema>, Vec<u8>)> {
        match self {
            Value::List(ListRepr::Packed(p)) => p.vec3_data(),
            _ => None,
        }
    }

    /// Build a packed `List` value directly from a result byte buffer + schema (P-PACK 4.2).
    pub(crate) fn packed_list_from(schema: Rc<PackedSchema>, bytes: Vec<u8>) -> Value {
        Value::List(ListRepr::Packed(PackedList::from_bytes(schema, bytes)))
    }

    /// The display form used by `echo`, `~` concatenation, and (later) interpolation.
    /// In M1 this becomes `Display` trait dispatch; in M0 it is built in per value kind.
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
            Value::Set(items) => {
                let parts: Vec<String> = items.iter().map(Value::repr).collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Map(entries) => {
                let parts: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{k:?}: {}", v.repr()))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Function(_) => "<fn>".to_string(),
            Value::Builtin(b) => format!("<builtin {}>", b.name()),
            Value::EnumType(def) => format!("<enum {}>", def.name()),
            Value::Enum(value) => value.display(),
            Value::Type(def) => format!("<type {}>", def.name()),
            Value::Object(object) => object.display(),
            Value::NativeModule(module) => format!("<module {module}>"),
            // `<file "path" (mode)>`, rendered by the shared handle so the VM matches exactly.
            Value::FileHandle(handle) => handle.borrow().display(),
            Value::Iter(_) => "<iterator>".to_string(),
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
            Value::Set(_) => "set",
            Value::Map(_) => "map",
            Value::Function(_) | Value::Builtin(_) => "function",
            Value::EnumType(_) => "enum type",
            Value::Enum(_) => "enum",
            Value::Type(_) => "type",
            Value::Object(_) => "object",
            Value::NativeModule(_) => "module",
            Value::FileHandle(_) => "file handle",
            Value::Iter(_) => "iterator",
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
            Value::Set(items) => write!(f, "Set({items:?})"),
            Value::Map(entries) => write!(f, "Map({entries:?})"),
            Value::Function(_) => write!(f, "Function(<fn>)"),
            Value::Builtin(b) => write!(f, "Builtin({})", b.name()),
            Value::EnumType(def) => write!(f, "EnumType({})", def.name()),
            Value::Enum(value) => write!(f, "Enum({})", value.display()),
            Value::Type(def) => write!(f, "Type({})", def.name()),
            Value::Object(object) => write!(f, "Object({})", object.display()),
            Value::NativeModule(module) => write!(f, "NativeModule({module})"),
            Value::FileHandle(handle) => write!(f, "FileHandle({})", handle.borrow().display()),
            Value::Iter(state) => write!(f, "Iter({:?})", state.borrow()),
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
            (Value::Set(a), Value::Set(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Enum(a), Value::Enum(b)) => a == b,
            (Value::Object(a), Value::Object(b)) => a == b,
            (Value::NativeModule(a), Value::NativeModule(b)) => a == b,
            // File handles compare by their full shared state, matching the VM by construction.
            (Value::FileHandle(a), Value::FileHandle(b)) => *a.borrow() == *b.borrow(),
            // Functions and types are not structurally comparable.
            _ => false,
        }
    }
}

/// Render a float deterministically. Whole-valued floats keep a trailing `.0` so they
/// are visibly distinct from ints (`3.0`, not `3`).
fn format_float(f: f64) -> String {
    lang_stdlib::format_float(f)
}

/// Display an `f32` (P-PACK Phase 3) at f32 precision (the shortest round-tripping f32 decimal), so
/// e.g. `0.1f32` shows `0.1`, not the f64-widened `0.10000000149…`. Delegates to the shared
/// [`lang_stdlib::format_f32`] so the two backends agree by construction.
pub(crate) fn format_f32(f: f32) -> String {
    lang_stdlib::format_f32(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: Vec<Value>) -> Value {
        Value::list(items)
    }

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(Rc::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
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
