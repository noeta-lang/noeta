//! Shapes: the layout descriptor for the object model.
//!
//! A [`Shape`] is the "hidden class" of a heap aggregate — a record/class instance or an
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

/// What kind of aggregate a [`Shape`] describes. Records and classes differ only in whether
/// they carry methods (tracked by the compiler, not the shape); both lay out flat field
/// slots in declared order. `Opaque` is a `use`-imported stub whose real field set is unknown
/// until a literal supplies it (its slots are the literal's fields in sorted-key order, so its
/// display matches the M0 tree-walker's `BTreeMap`-ordered field bag). `Enum` describes one
/// `(enum, variant)` pair; its slots are the variant's positional data fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShapeKind {
    Record,
    Class,
    Opaque,
    Enum,
}

/// The layout of one aggregate kind: its type name, the ordered slot names, and — for an
/// enum — the variant name and whether it is a built-in `Result`/`Option` (which display with
/// their bare constructor, `Ok(x)`/`none`, rather than `Type.Variant`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Shape {
    pub kind: ShapeKind,
    /// The type name for an object, or the enum name for an enum value.
    pub name: String,
    /// Slot names in slot order: declared fields (record/class), sorted fields (opaque), or
    /// the variant's positional data-field names (enum).
    pub fields: Vec<String>,
    /// The variant name (enum shapes only).
    pub variant: Option<String>,
    /// Whether this is a built-in `Result`/`Option` enum, affecting only display.
    pub builtin_result_option: bool,
}

impl Shape {
    /// A record/class/opaque object shape with the given ordered slot names.
    pub fn object(kind: ShapeKind, name: impl Into<String>, fields: Vec<String>) -> Shape {
        Shape {
            kind,
            name: name.into(),
            fields,
            variant: None,
            builtin_result_option: false,
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
        }
    }

    /// The slot index of `field`, or `None` if this shape has no such field.
    pub fn slot_of(&self, field: &str) -> Option<usize> {
        self.fields.iter().position(|f| f == field)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_lookup_follows_declared_order() {
        let shape = Shape::object(
            ShapeKind::Record,
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
