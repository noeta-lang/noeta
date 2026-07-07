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
//! compiled module; the VM wraps each entry in an `Arc<Shape>` once and clones that handle into
//! every value of that shape, making shape identity a cheap pointer comparison. The handle is
//! atomic (`Arc`, P-PAR S1) because shared-region borrow-share hands promoted objects — shape
//! handle included — to other isolate threads; `Shape` itself is immutable plain data.
//!
//! Inline caches (monomorphic call-site/field-access caches keyed by shape) are a pure
//! performance layer over this representation — invisible in observable output — and are
//! deferred to a later optimization pass; field/slot resolution here is a direct lookup.

/// What kind of aggregate a [`Shape`] describes. Structs and classes differ only in whether
/// they carry methods (tracked by the compiler, not the shape); both lay out flat field
/// slots in declared order. `Opaque` is a `use`-imported stub whose real field set is unknown
/// until a literal supplies it (its slots are the literal's fields in sorted-key order, so its
/// display matches the M0 tree-walker's `BTreeMap`-ordered field bag). `Enum` describes one
/// `(enum, variant)` pair; its slots are the variant's positional data fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Struct,
    Class,
    Opaque,
    Enum,
}

/// The layout of one aggregate kind: its type name, the ordered slot names, and — for an
/// enum — the variant name and whether it is a built-in `Result`/`Option` (which display with
/// their bare constructor, `Ok(x)`/`none`, rather than `Type.Variant`).
#[derive(Debug, Clone)]
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
    /// Whether `==` on this type is **structural** (field-wise) rather than **reference identity**
    /// (object-model slice 2). True for every value kind (`struct`/`enum`/opaque) and for a
    /// reference `class` that is `Equatable` (derives it or hand-`impl`s `eq`); false only for a
    /// plain `class` with no `Equatable`, whose `==` falls back to identity (*same instance*). A
    /// derived property of the named type — deliberately **excluded from equality/hashing** below
    /// (it is not part of a shape's structural identity, which is name + fields + variant).
    pub structural_eq: bool,
}

// `structural_eq` is type metadata, not part of a shape's structural identity (two shapes are "the
// same shape" iff same kind/name/fields/variant/result-option). Hand-implemented to exclude it so a
// shape built without derive context (e.g. reflection materialization) still matches the compiler's
// interned shape for the same type.
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
        }
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
        }
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
/// element's `Arc<Shape>` so a materialized element shares shape identity with a directly-constructed
/// one, plus each field's kind in slot (declared) order and the total word width.
#[derive(Debug, Clone)]
pub struct PackedSchema {
    /// The element type's shape — materialized elements use this exact handle.
    pub shape: std::sync::Arc<Shape>,
    /// One entry per field, in `shape.fields` (slot) order.
    pub fields: Vec<PackedKind>,
    /// **Bytes** per element — the sum of each field's [`PackedKind::byte_width`] (P-PACK 3.2b: the
    /// VM stores a `List<packed>` as a byte buffer so an `f32` field is 4 bytes, not 8).
    pub byte_size: usize,
    /// Whether the list buffer is stored **column-major** (SoA: `[f0×n][f1×n]…`) rather than
    /// row-major (AoS: each element's fields contiguous) — the `@packed(layout: column)` attribute
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
    Bool,
    Struct(std::sync::Arc<PackedSchema>),
}

impl PackedKind {
    /// The number of bytes this field occupies in a packed buffer (P-PACK 3.2b): an `f32` is 4, the
    /// other primitives are 8, and a nested struct is its own `byte_size`.
    pub fn byte_width(&self) -> usize {
        match self {
            PackedKind::Bool => 1,
            PackedKind::F32 => 4,
            PackedKind::Int | PackedKind::Float => 8,
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
