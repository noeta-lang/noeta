//! Shapes: the layout descriptor for the object model.
//!
//! A [`Shape`] is the "hidden class" of a heap aggregate — a struct/class instance or an
//! enum value. It names the type, lists the slots in a fixed order, and (for enums) records
//! the variant. The runtime value (`noeta-value`) stores a flat slot array plus a shared
//! handle to its shape, so two aggregates built the same way point at *one* shape rather than
//! each carrying a per-instance field bag (the naive representation the architecture rejects).
//!
//! Shapes are pure, immutable layout data — no runtime `Value` lives here, so this crate sits
//! below `noeta-value` in the dependency DAG. The compiler emits a flat shape table into the
//! compiled module; the VM [interns](intern_shape) each entry once (P-PAR S1b) and every value of
//! that shape carries the same `Copy` `&'static Shape`, making shape identity a cheap pointer
//! comparison with **zero refcount traffic** — and, being `'static` of a `Sync` type, a handle
//! shared-region borrow-share can hand to other isolate threads for free.
//!
//! Inline caches (monomorphic call-site/field-access caches keyed by shape) are a pure
//! performance layer over this representation — invisible in observable output — and are
//! shipped in the VM: a per-run cache side-array (sized to `Module.cache_slots`) stores the
//! last shape seen at each `LoadField`/`CallMethod` site. Field/slot resolution here in
//! `noeta-object` remains a direct lookup; the cache lives in `noeta-vm`.

use serde::{Deserialize, Serialize};
/// What kind of aggregate a [`Shape`] describes. Structs and classes differ only in whether
/// they carry methods (tracked by the compiler, not the shape); both lay out flat field
/// slots in declared order. `Opaque` is a `use`-imported stub whose real field set is unknown
/// until a literal supplies it (its slots are the literal's fields in sorted-key order, so its
/// display matches the M0 tree-walker's `BTreeMap`-ordered field bag). `Enum` describes one
/// `(enum, variant)` pair; its slots are the variant's positional data fields.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Struct,
    Class,
    Opaque,
    Enum,
}

/// The layout of one aggregate kind: its type name, the ordered slot names, and — for an
/// enum — the variant name and whether it is a built-in `Result`/`Option` (which display with
/// their bare constructor, `Ok(x)`/`none`, rather than `Type.Variant`).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Shape {
    pub kind: ShapeKind,
    /// The type name for an object, or the enum name for an enum value.
    pub name: String,
    /// Slot names in slot order: declared fields (struct/class), sorted fields (opaque), or
    /// the variant's positional data-field names (enum).
    pub fields: Vec<String>,
    /// The variant name (enum shapes only).
    pub variant: Option<String>,
    /// Whether this is a built-in `Result`/`Option` enum, affecting only display.
    pub builtin_result_option: bool,
    /// The variant's **declaration index** (enum shapes only, when known) — the primary key of
    /// derived `Comparable` ordering on enums (variant order, then payload fields). Type metadata
    /// like [`Shape::structural_eq`]: **excluded from equality/hashing** below, so a shape built
    /// without declaration context (reflection materialization, tests) still matches the
    /// compiler's interned shape. A compare reaching a `None` index is unordered (runtime error),
    /// never wrongly ordered.
    pub variant_index: Option<u32>,
    /// Whether `==` on this type is **structural** (field-wise) rather than **reference identity**
    /// (object-model slice 2). True for every value kind (`struct`/`enum`/opaque) and for a
    /// reference `class` that is `Equatable` (derives it or hand-`impl`s `eq`); false only for a
    /// plain `class` with no `Equatable`, whose `==` falls back to identity (*same instance*). A
    /// derived property of the named type — deliberately **excluded from equality/hashing** below
    /// (it is not part of a shape's structural identity, which is name + fields + variant).
    pub structural_eq: bool,
    /// Whether values of this type may **key a `Map` / member a `Set`** (P-PKEY): a `@packed`
    /// struct every one of whose fields is an integer/`bool` (or a nested key-capable packed
    /// struct) — content identity over a canonical fixed-width encoding, no floats. Computed by
    /// `noeta_ast::key_capable_packed` and baked in by the compiler. Type metadata like
    /// `structural_eq`: excluded from equality/hashing below.
    ///
    /// The `#[serde(default)]` is for the *self-describing* readers (the JSON/TOML paths tooling
    /// uses), and **not** for `.noeb` bundles, which this comment used to claim. postcard writes a
    /// struct's fields back to back with no tags and reads exactly as many as the current
    /// declaration has, so the default never fires: a bundle written before this field is misread
    /// from here on, not defaulted. `noeta-bundle`'s
    /// `serde_default_does_not_make_an_added_field_readable_by_postcard` checks that rather than
    /// restating it. What actually protects an older bundle is `FORMAT_VERSION`, which has moved
    /// many times since this field landed.
    #[serde(default)]
    pub key_capable: bool,
    /// The slot indices of this type's `#[std.json.Transient]` fields, ascending — the fields that
    /// **do not leave the program**, so the deep marshal (`json.stringify`, a native call's
    /// arguments, an isolate's output) omits them. Empty for the overwhelmingly common type that
    /// marks none, which is why it is a sparse index list rather than a per-slot flag.
    ///
    /// It rides on the shape because the deep marshal walks a value's slots against its shape and
    /// has nothing else to consult — no type table, no recipe, no checker. Type metadata like
    /// [`Shape::structural_eq`], so it is **excluded from equality/hashing** below: a shape built
    /// without declaration context (reflection materialization, an isolate rehydrating a value)
    /// still matches the compiler's interned shape for the same type, which is the shape carrying
    /// the real list.
    ///
    /// The `#[serde(default)]` is for the self-describing readers only, exactly as for
    /// `key_capable` — a `.noeb` bundle is postcard, where an added field is a `FORMAT_VERSION`
    /// question and never a defaulted one.
    #[serde(default)]
    pub transient_slots: Vec<u32>,
}

// `structural_eq`, `key_capable` and `transient_slots` are type metadata, not part of a shape's
// structural identity (two shapes are "the same shape" iff same kind/name/fields/variant/result-option).
// Hand-implemented to exclude them so a shape built without derive context (e.g. reflection
// materialization) still matches the compiler's interned shape for the same type.
impl PartialEq for Shape {
    fn eq(&self, other: &Shape) -> bool {
        self.kind == other.kind
            && self.name == other.name
            && self.fields == other.fields
            && self.variant == other.variant
            && self.builtin_result_option == other.builtin_result_option
    }
}
impl Eq for Shape {}
impl std::hash::Hash for Shape {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.name.hash(state);
        self.fields.hash(state);
        self.variant.hash(state);
        self.builtin_result_option.hash(state);
    }
}

impl Shape {
    /// A struct/class/opaque object shape with the given ordered slot names. `==` defaults to
    /// structural for every kind except a plain `class` (reference identity); a class that is
    /// `Equatable` must be built via [`Shape::object_equatable`].
    pub fn object(kind: ShapeKind, name: impl Into<String>, fields: Vec<String>) -> Shape {
        let structural_eq = kind != ShapeKind::Class;
        Shape::object_equatable(kind, name, fields, structural_eq)
    }

    /// A struct/class/opaque object shape with an explicit `structural_eq` — used by the compiler,
    /// which knows whether a `class` is `Equatable` (derives it or hand-`impl`s `eq`) and so whether
    /// its `==` is structural rather than reference identity.
    pub fn object_equatable(
        kind: ShapeKind,
        name: impl Into<String>,
        fields: Vec<String>,
        structural_eq: bool,
    ) -> Shape {
        Shape {
            kind,
            name: name.into(),
            fields,
            variant: None,
            builtin_result_option: false,
            structural_eq,
            variant_index: None,
            key_capable: false,
            transient_slots: Vec::new(),
        }
    }

    /// Mark the shape key-capable (P-PKEY) — chained by the compiler, which computed the
    /// program's key-capable packed set; every other construction site (reflection, isolates)
    /// leaves the default `false` and resolves against the compiler's canonical interned shape.
    pub fn with_key_capable(mut self, key_capable: bool) -> Shape {
        self.key_capable = key_capable;
        self
    }

    /// Mark which slots are `#[Transient]` — chained by the backends, which read the declaration's
    /// field attributes; every other construction site leaves the list empty and resolves against
    /// the compiler's canonical interned shape, exactly as for [`Shape::with_key_capable`].
    pub fn with_transient_slots(mut self, transient_slots: Vec<u32>) -> Shape {
        self.transient_slots = transient_slots;
        self
    }

    /// Whether slot `i` is transient — the deep-marshal question, asked per slot. Short-circuits on
    /// the empty list, which is what almost every type carries.
    pub fn is_transient_slot(&self, i: usize) -> bool {
        !self.transient_slots.is_empty() && self.transient_slots.contains(&(i as u32))
    }

    /// An enum-variant shape: `name` is the enum, `variant` the case, `fields` the positional
    /// data-field names.
    pub fn enum_variant(
        name: impl Into<String>,
        variant: impl Into<String>,
        fields: Vec<String>,
        builtin_result_option: bool,
    ) -> Shape {
        Shape {
            kind: ShapeKind::Enum,
            name: name.into(),
            fields,
            variant: Some(variant.into()),
            builtin_result_option,
            // Enums are a value kind: `==` is structural.
            structural_eq: true,
            variant_index: None,
            key_capable: false,
            transient_slots: Vec::new(),
        }
    }

    /// [`Shape::enum_variant`] plus the variant's declaration index — what derived `Comparable`
    /// orders by. Used where the declaration is at hand (the compiler; the built-in enums whose
    /// order is defined: `none < some`, `Ok < Err`, `Less < Equal < Greater`).
    pub fn with_variant_index(mut self, index: u32) -> Shape {
        self.variant_index = Some(index);
        self
    }

    /// The slot index of `field`, or `None` if this shape has no such field.
    pub fn slot_of(&self, field: &str) -> Option<usize> {
        self.fields.iter().position(|f| f == field)
    }
}

/// The runtime layout of a `List<packed>` element (P-PACK Phase 2.4) — how to pack a value-struct
/// instance into, and materialize it back from, a contiguous run of raw primitive words. Built once
/// per packed list type (the VM resolves it at module load from the compiled
/// `PackedSchemaDef`/shape table; the tree-walker has its own equivalent over `TypeDef`). Holds the
/// element's interned `&'static Shape` so a materialized element shares shape identity with a
/// directly-constructed one, plus each field's kind in slot (declared) order and the total width.
#[derive(Debug, Clone)]
pub struct PackedSchema {
    /// The element type's shape — materialized elements use this exact handle. **`None`** marks a
    /// bare-scalar element (packed-widths bare-scalar arc): a `List<i32>`/`List<f32>` has one scalar
    /// field and no struct wrapper, so it materializes to a bare `int`/`f32` rather than an object.
    pub shape: Option<&'static Shape>,
    /// One entry per field, in `shape.fields` (slot) order. A scalar element (`shape == None`) holds
    /// exactly one — the element's own primitive kind.
    pub fields: Vec<PackedKind>,
    /// **Bytes** per element — the sum of each field's [`PackedKind::byte_width`] (P-PACK 3.2b: the
    /// VM stores a `List<packed>` as a byte buffer so an `f32` field is 4 bytes, not 8).
    pub byte_size: usize,
    /// Whether the list buffer is stored **column-major** (SoA: `[f0×n][f1×n]…`) rather than
    /// row-major (AoS: each element's fields contiguous) — the `@packed(Layout.Column)` attribute
    /// (P-SIMD C2). A pure *performance* property: every op reads it to pick the byte offset, but the
    /// observed value is identical either way (differential holds by construction). Top-level fields
    /// become columns; a nested `@packed` field stays a contiguous per-element chunk until leaf-
    /// flattening (C5) splits it into leaf columns.
    pub column: bool,
}

impl PackedSchema {
    /// The byte offset of field `slot` within a single row — the sum of the prior fields' widths.
    /// Shared by the row and column offset math.
    pub fn field_prefix(&self, slot: usize) -> usize {
        self.fields[..slot].iter().map(|k| k.byte_width()).sum()
    }

    /// The number of elements a buffer of `len` bytes holds (`len / byte_size`; 0 for a zero-width
    /// element, which never occurs for a real packed struct).
    pub fn count(&self, len: usize) -> usize {
        len.checked_div(self.byte_size).unwrap_or(0)
    }

    /// The byte offset of element `i`'s field `slot` in a buffer holding `count` elements. Row-major
    /// packs each element contiguously (`i·byte_size + prefix`); column-major packs each field's
    /// values contiguously across all elements (`count·prefix + i·width`). This is the one place the
    /// layout axis is interpreted for per-field access — `get`/`field`/`set` all route through it.
    pub fn field_offset(&self, i: usize, slot: usize, count: usize) -> usize {
        let prefix = self.field_prefix(slot);
        if self.column {
            count * prefix + i * self.fields[slot].byte_width()
        } else {
            i * self.byte_size + prefix
        }
    }
}

/// A packed field's storage: a primitive occupying a fixed run of bytes, or a nested packed struct
/// flattened inline (its own sub-schema laid out contiguously in the parent's buffer).
#[derive(Debug, Clone)]
pub enum PackedKind {
    Int,
    Float,
    /// A 32-bit float field (P-PACK Phase 3) — **4 bytes** (slice 3.2b), half an `int`/`float`.
    F32,
    /// An explicit 64-bit float field `f64` (packed-widths arc) — **8 bytes**, storage-identical to
    /// `Float` but a distinct kind so packed reflection can report `f64` rather than `float`.
    F64,
    /// A fixed-width integer field `i8..i64`/`u8..u64` (packed-widths arc): `bits/8` bytes, packed at
    /// its natural width. `signed` decides the read-back extension — a signed slot sign-extends its
    /// top bit, an unsigned one zero-extends — so a stored `-1i8` reads back `-1` and a `255u8` reads
    /// back `255`. The runtime *scalar* stays an 8-byte `int` (Tier W erasure); only the buffer slot
    /// is narrowed.
    IntN {
        /// One of 8, 16, 32, 64.
        bits: u8,
        /// `true` for the `iN` family, `false` for `uN`.
        signed: bool,
    },
    Bool,
    Struct(&'static PackedSchema),
}

impl PackedKind {
    /// The number of bytes this field occupies in a packed buffer: `bool` is 1, `f32`/`i32`/`u32`
    /// are 4, `int`/`float`/`f64`/`i64`/`u64` are 8, an `i8`/`u8` is 1, an `i16`/`u16` is 2, and a
    /// nested struct is its own `byte_size` (packed-widths arc — every width packs at its width).
    pub fn byte_width(&self) -> usize {
        match self {
            PackedKind::Bool => 1,
            PackedKind::F32 => 4,
            PackedKind::Int | PackedKind::Float | PackedKind::F64 => 8,
            PackedKind::IntN { bits, .. } => (*bits as usize) / 8,
            PackedKind::Struct(inner) => inner.byte_size,
        }
    }
}

// P-PAR S1: shape/schema handles ride inside shared-region objects that other isolate threads
// borrow, so both types must stay `Send + Sync` (immutable plain data). Compile-time lock — a
// future non-`Send` field is a build error here, not a latent data race.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Shape>();
    assert_send_sync::<PackedSchema>();
};

/// The process-wide shape/schema interner (P-PAR S1b). Runtime values carry a bare
/// `&'static Shape` — a `Copy` handle with **zero refcount traffic** on the object hot path
/// (construction, functional update, destruction), where per-object `Rc` cost ~2 plain RMWs and
/// the `Arc` prerequisite for cross-thread borrow-share benched +10–12% on `vm_field_assign`
/// (2 atomic RMWs per update). Interning gives the handle its `'static` lifetime: each distinct
/// shape is leaked exactly once and every later request dedups onto it, so growth is bounded by
/// the number of *distinct* types the process ever loads (a REPL re-loading a module re-uses its
/// entries) — the hidden-class arena every production VM ends up with.
///
/// The dedup key is **every field including `structural_eq`** — deliberately stricter than
/// `Shape`'s hand-written `PartialEq` (which excludes it so reflection-materialized shapes
/// compare equal to compiler-interned ones): a REPL redefinition can legitimately flip a class's
/// `Equatable`-ness, and those two shapes must stay distinct objects. Code relying on `PartialEq`
/// sees exactly the old semantics; code relying on pointer identity (inline caches, packed
/// materialization, isolate `shape_index`) only ever gains sharing.
///
/// Interning locks a process mutex, so it belongs on **cold paths only** (module load,
/// reflection); per-value hot sites cache their interned handle in a `OnceLock` instead.
mod intern {
    use super::{PackedKind, PackedSchema, Shape, ShapeKind};
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    /// All of [`Shape`], including `structural_eq` (see module doc for why the stricter key).
    #[derive(PartialEq, Eq, Hash)]
    struct ShapeKey {
        kind: ShapeKind,
        name: String,
        fields: Vec<String>,
        variant: Option<String>,
        builtin_result_option: bool,
        structural_eq: bool,
    }

    impl ShapeKey {
        fn of(shape: &Shape) -> ShapeKey {
            ShapeKey {
                kind: shape.kind,
                name: shape.name.clone(),
                fields: shape.fields.clone(),
                variant: shape.variant.clone(),
                builtin_result_option: shape.builtin_result_option,
                structural_eq: shape.structural_eq,
            }
        }
    }

    /// A schema's identity: its (already-interned) shape by address, its field kinds with nested
    /// schemas by address, and the layout axis. Pointer-keying the nested parts is sound because
    /// they are themselves interned (same content ⇒ same address).
    #[derive(PartialEq, Eq, Hash)]
    struct SchemaKey {
        shape: Option<usize>,
        fields: Vec<KindKey>,
        byte_size: usize,
        column: bool,
    }

    #[derive(PartialEq, Eq, Hash)]
    enum KindKey {
        Int,
        Float,
        F32,
        F64,
        IntN { bits: u8, signed: bool },
        Bool,
        Struct(usize),
    }

    impl SchemaKey {
        fn of(schema: &PackedSchema) -> SchemaKey {
            SchemaKey {
                shape: schema.shape.map(|s| std::ptr::from_ref(s).addr()),
                fields: schema
                    .fields
                    .iter()
                    .map(|k| match k {
                        PackedKind::Int => KindKey::Int,
                        PackedKind::Float => KindKey::Float,
                        PackedKind::F32 => KindKey::F32,
                        PackedKind::F64 => KindKey::F64,
                        PackedKind::IntN { bits, signed } => KindKey::IntN {
                            bits: *bits,
                            signed: *signed,
                        },
                        PackedKind::Bool => KindKey::Bool,
                        PackedKind::Struct(inner) => {
                            KindKey::Struct(std::ptr::from_ref::<PackedSchema>(inner).addr())
                        }
                    })
                    .collect(),
                byte_size: schema.byte_size,
                column: schema.column,
            }
        }
    }

    /// Intern `shape`: the one `&'static Shape` every structurally-identical request shares.
    pub fn shape(shape: Shape) -> &'static Shape {
        static SHAPES: OnceLock<Mutex<HashMap<ShapeKey, &'static Shape>>> = OnceLock::new();
        let mut map = SHAPES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("shape interner poisoned");
        map.entry(ShapeKey::of(&shape))
            .or_insert_with(|| Box::leak(Box::new(shape)))
    }

    /// Intern `schema` (its `shape` and nested `Struct` schemas must already be interned).
    pub fn schema(schema: PackedSchema) -> &'static PackedSchema {
        static SCHEMAS: OnceLock<Mutex<HashMap<SchemaKey, &'static PackedSchema>>> =
            OnceLock::new();
        let mut map = SCHEMAS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("schema interner poisoned");
        map.entry(SchemaKey::of(&schema))
            .or_insert_with(|| Box::leak(Box::new(schema)))
    }
}

/// Intern a [`Shape`], returning the process-wide shared `&'static Shape` (see [`intern`]).
pub fn intern_shape(shape: Shape) -> &'static Shape {
    intern::shape(shape)
}

/// Intern a [`PackedSchema`] (see [`intern`]). Its `shape` and any nested `Struct` field schemas
/// must already be interned handles.
pub fn intern_schema(schema: PackedSchema) -> &'static PackedSchema {
    intern::schema(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_lookup_follows_declared_order() {
        let shape = Shape::object(
            ShapeKind::Struct,
            "Item",
            vec!["price".into(), "qty".into()],
        );
        assert_eq!(shape.slot_of("price"), Some(0));
        assert_eq!(shape.slot_of("qty"), Some(1));
        assert_eq!(shape.slot_of("missing"), None);
    }

    #[test]
    fn enum_shape_records_its_variant() {
        let shape = Shape::enum_variant("Result", "Ok", vec!["0".into()], true);
        assert_eq!(shape.kind, ShapeKind::Enum);
        assert_eq!(shape.variant.as_deref(), Some("Ok"));
        assert!(shape.builtin_result_option);
    }
}
