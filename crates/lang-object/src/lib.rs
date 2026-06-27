//! Shapes: the layout descriptor for the object model.
//!
//! A [`Shape`] is the "hidden class" of a heap aggregate — a struct/class instance or an
//! enum value. It names the type, lists the slots in a fixed order, and (for enums) records
//! the variant. The runtime value (`lang-value`) stores a flat slot array plus a shared
//! handle to its shape, so two aggregates built the same way point at *one* shape rather than
//! each carrying a per-instance field bag (the naive representation the architecture rejects).
//!
//! Shapes are pure, immutable layout data — no runtime `Value` lives here, so this crate sits
//! below `lang-value` in the dependency DAG. The compiler emits a flat shape table into the
//! compiled module; the VM wraps each entry in an `Rc<Shape>` once and clones that handle into
//! every value of that shape, making shape identity a cheap pointer comparison.
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
/// element's `Rc<Shape>` so a materialized element shares shape identity with a directly-constructed
/// one, plus each field's kind in slot (declared) order and the total word width.
#[derive(Debug, Clone)]
pub struct PackedSchema {
    /// The element type's shape — materialized elements use this exact handle.
    pub shape: std::rc::Rc<Shape>,
    /// One entry per field, in `shape.fields` (slot) order.
    pub fields: Vec<PackedKind>,
    /// Words per element — the sum of each field's width.
    pub word_count: usize,
}

/// A packed field's storage: a primitive occupying one word, or a nested packed struct flattened
/// inline (its own sub-schema laid out contiguously in the parent's buffer).
#[derive(Debug, Clone)]
pub enum PackedKind {
    Int,
    Float,
    Bool,
    Struct(std::rc::Rc<PackedSchema>),
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
