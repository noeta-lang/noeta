//! Data-attribute (`#[...]`) validation — an `impl Checker` split out of the crate root to shrink
//! `lib.rs`. An attribute reduces to a struct constructed in annotation position, so this validates
//! the capability gate (`#[Foo]` requires `Foo` marked `@attribute`) and the construction (every
//! field set once, each literal assignable, defaults honored). Methods moved verbatim; the
//! declaration-checking path in `lib.rs` is the caller, and the small `record_attribute` /
//! `is_optional_attribute_field` recording helpers stay there (interleaved with the other recorders).

use super::*;

impl Checker {
    /// Validate the `#[...]` data attributes on a declaration. An attribute reduces to a struct
    /// constructed in annotation position, so two things are checked: the **capability gate** —
    /// `#[Foo(...)]` requires `Foo` to be a struct marked `@attribute` — and, when it is, the
    /// **construction** — the arguments must
    /// build a valid `Foo` (every field set once, each literal assignable to its field), the same
    /// all-fields-literal contract any struct construction obeys. The old `#[derive(...)]` codegen
    /// spelling is still rejected up front (E0017).
    pub(crate) fn check_attrs(&mut self, attrs: &[Attribute], target: TargetKind) {
        for attr in attrs {
            if attr.name == noeta_ast::BuiltinDirective::Derive.as_str() {
                self.error(
                    DiagnosticCode::InvalidAttribute,
                    attr.span,
                    "`#[derive(...)]` is not a data attribute",
                )
                .help(
                    "code generation now uses the `@derive(...)` directive; `#[...]` is for \
                         data attributes only",
                );
                continue;
            }
            // The capability gate: only a struct marked `@attribute` may be used as `#[Foo(...)]`.
            if !self.symbols.attributes.contains(&attr.name) {
                self.error(
                    DiagnosticCode::NotAnAttribute,
                    attr.name_span,
                    format!("`{}` cannot be used as an attribute", attr.name),
                )
                .help(
                    "an attribute is a record marked `@attribute`; declare the record with that \
                         directive",
                );
                continue;
            }
            // Placement gate (P2.5): when `Foo` declared `@attribute(Kind, …)`, this use site's kind
            // must be among the permitted ones, else `E0030`.
            if let Some(allowed) = self.symbols.attachable.get(&attr.name)
                && !allowed.contains(&target)
            {
                let permitted = allowed
                    .iter()
                    .map(|k| k.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                self.error(
                    DiagnosticCode::InvalidAttributeTarget,
                    attr.name_span,
                    format!(
                        "`{}` cannot attach to a {}; it is restricted to {permitted}",
                        attr.name,
                        target.label(),
                    ),
                )
                .help("change the target, or widen the `@attribute(...)` directive");
                continue;
            }
            self.check_attribute_construction(attr);
        }
    }

    /// Check that a `#[Foo(...)]` attribute's arguments construct a valid `Foo` — the all-fields
    /// contract of any struct literal, applied to the literal arguments. Positional arguments bind
    /// to fields in declaration order, named arguments by name; each field must be set exactly once
    /// (unset → `E0009`, unknown/overflowing → `E0005`) and each literal value must be assignable
    /// to its field's type (`E0007`). An identifier argument carries no static type, so its value
    /// is not type-checked (only its field binding is).
    fn check_attribute_construction(&mut self, attr: &Attribute) {
        let fields = self
            .symbols
            .records
            .get(&attr.name)
            .cloned()
            .unwrap_or_default();
        let mut filled = vec![false; fields.len()];
        let mut next_positional = 0usize;
        for arg in &attr.args {
            let target = match &arg.name {
                None => {
                    let i = next_positional;
                    next_positional += 1;
                    if i < fields.len() { Some(i) } else { None }
                }
                Some(fname) => fields.iter().position(|(n, _)| n == fname),
            };
            let Some(i) = target else {
                let what = match &arg.name {
                    Some(fname) => format!("has no field `{fname}`"),
                    None => format!("declares only {} field(s)", fields.len()),
                };
                self.error(
                    DiagnosticCode::UnknownName,
                    arg.span,
                    format!("attribute `{}` {what}", attr.name),
                );
                continue;
            };
            if filled[i] {
                self.error(
                    DiagnosticCode::UnknownName,
                    arg.span,
                    format!(
                        "field `{}` of attribute `{}` is set twice",
                        fields[i].0, attr.name
                    ),
                );
                continue;
            }
            filled[i] = true;
            let (fname, fty) = fields[i].clone();
            self.check_attr_value(&arg.value, &fty, &fname, arg.span);
        }
        for (i, (fname, fty)) in fields.iter().enumerate() {
            // A field with a default (`name: T = …`) is optional — it may be omitted (slice 6i).
            if !filled[i] && !self.is_optional_attribute_field(&attr.name, fname) {
                self.error(
                    DiagnosticCode::MissingField,
                    attr.span,
                    format!(
                        "attribute `{}` is missing field `{fname}: {fty}`",
                        attr.name
                    ),
                );
            }
        }
    }

    /// Recursively check an attribute-argument literal tree against the field type it must construct
    /// (`E0007` on a mismatch). Descends into composite literals — a list/set against `List<T>`/
    /// `Set<T>` checks each element against `T`, a map against `Map<K,V>` checks each value against
    /// `V`, a struct literal against a struct type checks each named field — and validates the
    /// nominal cases (enum value, type reference) by assignability. A `dyn`/`Unknown` expectation
    /// imposes nothing (the interior-hole tolerance the rest of the checker keeps). `field`/`span`
    /// locate the diagnostic.
    fn check_attr_value(&mut self, value: &AttrValue, expected: &Type, field: &str, span: Span) {
        if matches!(expected, Type::Dyn | Type::Unknown) {
            return;
        }
        let synth = match value {
            AttrValue::Str(_) => Type::String,
            AttrValue::Int(_) => Type::Int,
            AttrValue::Float(_) => Type::Float,
            AttrValue::Bool(_) => Type::Bool,
            AttrValue::List(items) => {
                if let Type::List(elem) = expected {
                    for item in items {
                        self.check_attr_value(item, elem, field, span);
                    }
                    return;
                }
                Type::List(Box::new(Type::Dyn))
            }
            AttrValue::Set(items) => {
                if let Type::Set(elem) = expected {
                    for item in items {
                        self.check_attr_value(item, elem, field, span);
                    }
                    return;
                }
                Type::Set(Box::new(Type::Dyn))
            }
            AttrValue::Map(entries) => {
                if let Type::Map(_, vty) = expected {
                    for (_, val) in entries {
                        self.check_attr_value(val, vty, field, span);
                    }
                    return;
                }
                Type::Map(Box::new(Type::String), Box::new(Type::Dyn))
            }
            AttrValue::Struct { type_name, fields } => {
                let rec_ty = Type::Named(type_name.clone(), Vec::new());
                if self.assignable(&rec_ty, expected) {
                    self.check_attr_struct_fields(type_name, fields, span);
                    return;
                }
                rec_ty
            }
            AttrValue::Enum { enum_name, .. } => match enum_name.as_str() {
                // The built-in `Option`/`Result` constructors carry their lattice type so they check
                // against an `?T`/`Result<…>` field; a user enum is its nominal type.
                "Option" => Type::Option(Box::new(Type::Dyn)),
                "Result" => Type::Result(Box::new(Type::Dyn), Box::new(Type::Dyn)),
                _ => Type::Named(enum_name.clone(), Vec::new()),
            },
            // A bare name in attribute position is a type reference — a value of the reflection
            // `Type` enum. It must name a real type (else E0013); a `Type` value is then assignable
            // to a `Type`-typed (or `dyn`) field.
            AttrValue::TypeRef(name) => {
                if !Type::is_builtin_name(name)
                    && !PRELUDE_TYPES.contains(&name.as_str())
                    && !self.symbols.types.contains(name)
                {
                    self.error(
                        DiagnosticCode::UnknownType,
                        span,
                        format!("unknown type `{name}` in attribute argument"),
                    )
                    .help(
                        "a bare name in an attribute argument is a type reference; name a \
                             declared type, an import, or a built-in (use `Enum.Variant` for an \
                             enum value)",
                    );
                    return;
                }
                Type::Named(noeta_ast::reflect::TYPE_ENUM.to_string(), Vec::new())
            }
        };
        if !self.assignable(&synth, expected) {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("value of type `{synth}` is not assignable to field `{field}: {expected}`"),
            );
        }
    }

    /// Check a struct-literal attribute argument's named fields against the declared struct's field
    /// types. Unknown field names and missing fields are tolerated here (the lenient interior
    /// posture); only the values supplied for declared fields are type-checked.
    fn check_attr_struct_fields(
        &mut self,
        type_name: &str,
        fields: &[(String, AttrValue)],
        span: Span,
    ) {
        let Some(decl) = self.symbols.records.get(type_name).cloned() else {
            return;
        };
        for (fname, fval) in fields {
            if let Some((_, fty)) = decl.iter().find(|(n, _)| n == fname) {
                self.check_attr_value(fval, fty, fname, span);
            }
        }
    }
}
