//! A pure, deterministic projection of the AST into **reflection data** — the attribute manifest
//! plus a registry of every declared type's reflectable shape. Both backends build it from the same
//! [`Program`] via [`build`], so runtime reflection (attribute-system pass 2) is identical across
//! the tree-walker and the VM **by construction** — there is no second walk to drift from the first.
//! It carries no codegen or runtime meaning of its own; it is a read-only view of the program.

use crate::{AttrArg, AttrValue, Attribute, Expr, FieldDecl, Program, Stmt, UnaryOp};

/// Everything reflection needs about a program, derived purely from its AST.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReflectionInfo {
    /// Every `#[...]` data attribute, in source order, each keyed by the declaration it annotates.
    pub manifest: Vec<AttributeRecord>,
    /// Every declared struct/class/enum, in source order.
    pub types: Vec<TypeInfo>,
    /// The `(declaration, role)` index: for every declaration bearing an attribute that carries a
    /// `@role(Enum.Variant)` tag, the declaration's name paired with that role's enum and variant.
    /// This is the labeled dependency graph `roles_of()` surfaces — built beside the attribute
    /// manifest from the same AST, so both backends agree by construction.
    pub roles: Vec<RoleRecord>,
}

impl ReflectionInfo {
    /// The data attributes attached to `target`, in source order — the manifest query tooling and
    /// `attributes_of` use to discover, e.g., every type tagged `#[Entity]`.
    pub fn attributes_for<'a>(
        &'a self,
        target: &'a str,
    ) -> impl Iterator<Item = &'a AttributeRecord> {
        self.manifest.iter().filter(move |a| a.target == target)
    }

    /// The reflectable shape of a declared type by name.
    pub fn type_named(&self, name: &str) -> Option<&TypeInfo> {
        self.types.iter().find(|t| t.name == name)
    }

    /// The reflection [`TypeRepr`] of a **type reference** by name — a bare type name used as a value
    /// (`#[Encode(codec: JsonCodec)]`). Reports the same precise constructor a `type_of` over a value
    /// of that type would: a built-in scalar/collection maps to its lattice variant (`int` →
    /// `Type.Int`, `list` → `Type.List(Dyn)`), and a declared type maps by *kind* (`Type.Struct`/
    /// `Enum`/`Class`). Only a name with no known classification — an opaque import, or one of the
    /// abstract kind-types `Enum`/`Struct`/`Class` used directly — stays `Type.Named`, the honest
    /// unknown-kind fallback. Both backends build a type-ref through this one function, so the
    /// materialized `Type` value agrees across the differential by construction.
    pub fn type_ref_repr(&self, name: &str) -> TypeRepr {
        let dyn_box = || Box::new(TypeRepr::Dyn);
        match name {
            "int" => TypeRepr::Int,
            // P-PACK Phase 3 interim: `f32` reflects as `Float` until the prelude `Type` ADT gains an
            // `F32` case (a follow-up). Both backends share this builder, so they still agree.
            "float" | "f32" => TypeRepr::Float,
            "bool" => TypeRepr::Bool,
            "string" => TypeRepr::Str,
            "void" | "unit" => TypeRepr::Unit,
            "dyn" | "Any" => TypeRepr::Dyn,
            "list" | "List" => TypeRepr::List(dyn_box()),
            "set" | "Set" => TypeRepr::Set(dyn_box()),
            "map" | "Map" => TypeRepr::Map(dyn_box(), dyn_box()),
            "Option" => TypeRepr::Option(dyn_box()),
            "Result" => TypeRepr::Result(dyn_box(), dyn_box()),
            _ => match self.type_named(name).map(|t| t.kind) {
                Some(TypeKind::Struct) => TypeRepr::Struct(name.to_string(), Vec::new()),
                Some(TypeKind::Class) => TypeRepr::Class(name.to_string(), Vec::new()),
                Some(TypeKind::Enum) => TypeRepr::Enum(name.to_string(), Vec::new()),
                None => TypeRepr::Named(name.to_string(), Vec::new()),
            },
        }
    }
}

/// One `#[Name(args)]` attached to a declaration. Semantically a struct instance attached as
/// metadata; the runtime materializes it from the stored args (pass 2).
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeRecord {
    /// The annotated declaration's name (a type name today; pass 2 extends attributes to methods).
    pub target: String,
    /// The attribute's name (e.g. `Route`).
    pub name: String,
    /// The attribute's literal arguments (positional + named), straight from the AST.
    pub args: Vec<AttrArg>,
}

/// One `(declaration, role)` entry of the semantic-role index — a declaration's name paired with
/// the role an attribute it bears confers on it, identified by its `@semantic` enum and variant.
/// `roles_of()` materializes each into a `RoleBinding { target: string, role: Enum }` whose `role`
/// is the actual `enum_name.variant` enum value.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleRecord {
    /// The annotated declaration's name (the same target keying as the attribute manifest).
    pub target: String,
    /// The role's `@semantic` enum name (e.g. `Semantic`, `WebRole`).
    pub enum_name: String,
    /// The role's variant name (e.g. `EntryPoint`, `Controller`).
    pub variant: String,
}

/// The kind of a declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Struct,
    Class,
    Enum,
}

/// A declared type's reflectable shape: name, kind, and member names (declaration order). Field and
/// variant *types* are deliberately absent — they are erased at runtime, and reflection over a value
/// recovers names, not the static field types (which are a compile-time `type_of` concern).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeInfo {
    pub name: String,
    pub kind: TypeKind,
    /// Field names in declaration order (records/classes; empty for enums).
    pub fields: Vec<String>,
    /// Each field's **literal default** (object-model slice 6i), parallel to `fields`: `Some` when
    /// the field declared `name: T = <literal>`, `None` for a mandatory field or a non-literal
    /// default. Used to fill an omitted optional field when materializing an attribute instance, so
    /// `attributes_of` reports the declared default rather than a placeholder.
    pub field_defaults: Vec<Option<AttrValue>>,
    /// Variants in declaration order (enums; empty otherwise).
    pub variants: Vec<VariantInfo>,
}

/// An enum variant's reflectable shape.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantInfo {
    pub name: String,
    pub fields: Vec<String>,
}

/// Build the reflection info for a program. **Pure and deterministic**: the same AST always yields
/// the same [`ReflectionInfo`], so both backends calling this on the same [`Program`] agree without
/// any cross-backend coordination — the property the differential oracle depends on.
pub fn build(program: &Program) -> ReflectionInfo {
    let mut manifest = Vec::new();
    let mut types = Vec::new();
    // Attribute name → its `@role(Enum.Variant)` tags, harvested from the attribute records
    // themselves; joined with the manifest below so every *use* of a role-tagged attribute is
    // indexed. One entry per (attribute, role) pair — an attribute may carry several roles.
    let mut role_of: Vec<(String, (String, String))> = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Struct(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                push_field_attrs(&mut manifest, &decl.name, &decl.fields);
                // A role tag rides on the attribute struct; record each (validated) `Enum.Variant`
                // so every declaration the attribute annotates inherits it. A malformed `@role`
                // never reaches a runnable program (the checker rejects it).
                if let Some(roles) = decl.role.as_ref() {
                    for tag in roles {
                        role_of.push((
                            decl.name.clone(),
                            (tag.enum_name.clone(), tag.variant.clone()),
                        ));
                    }
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Struct,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    field_defaults: field_defaults(&decl.fields),
                    variants: Vec::new(),
                });
            }
            Stmt::Class(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                push_field_attrs(&mut manifest, &decl.name, &decl.fields);
                // A method's attributes are keyed by its qualified `Class.method` name, so a
                // `#[...]` on a method surfaces distinctly from the same name on another class.
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, &method.attrs);
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Class,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    field_defaults: field_defaults(&decl.fields),
                    variants: Vec::new(),
                });
            }
            // A top-level function carries attributes too (keyed by its bare name); it is not a
            // declared *type*, so it contributes to the manifest only, not the type registry.
            Stmt::Fn(decl) => push_attrs(&mut manifest, &decl.name, &decl.attrs),
            Stmt::Enum(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                // A variant's attributes are keyed by its qualified `Enum.Variant` name, mirroring
                // the `Type.field`/`Type.method` convention.
                for variant in &decl.variants {
                    let target = format!("{}.{}", decl.name, variant.name);
                    push_attrs(&mut manifest, &target, &variant.attrs);
                }
                // An enum method's attributes are keyed by its qualified `Enum.method` name, the same
                // convention class/struct methods use (object-model slice 3).
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, &method.attrs);
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Enum,
                    fields: Vec::new(),
                    field_defaults: Vec::new(),
                    variants: decl
                        .variants
                        .iter()
                        .map(|v| VariantInfo {
                            name: v.name.clone(),
                            fields: v.fields.iter().map(|f| f.name.clone()).collect(),
                        })
                        .collect(),
                });
            }
            _ => {}
        }
    }
    // Join the manifest with the role tags: every declaration bearing a role-tagged attribute is
    // indexed `(target, enum, variant)`. Identical entries (two attributes conferring the same role
    // on one declaration) are de-duplicated while preserving source order.
    let mut roles: Vec<RoleRecord> = Vec::new();
    for entry in &manifest {
        for (_, (enum_name, variant)) in role_of.iter().filter(|(name, _)| name == &entry.name) {
            let record = RoleRecord {
                target: entry.target.clone(),
                enum_name: enum_name.clone(),
                variant: variant.clone(),
            };
            if !roles.contains(&record) {
                roles.push(record);
            }
        }
    }
    ReflectionInfo {
        manifest,
        types,
        roles,
    }
}

/// Compute each field's literal default (object-model slice 6i), parallel to the field list: `Some`
/// for a `name: T = <literal>` field whose default folds to a constant, `None` otherwise. Used to
/// populate [`TypeInfo::field_defaults`].
fn field_defaults(fields: &[FieldDecl]) -> Vec<Option<AttrValue>> {
    fields
        .iter()
        .map(|f| f.default.as_ref().and_then(fold_const_expr))
        .collect()
}

/// Fold a constant expression to an [`AttrValue`], or `None` if it is not a literal. A focused subset
/// of the attribute-argument grammar — scalars, a negated numeric literal, and lists thereof — enough
/// for the literal field defaults an attribute carries; a richer default (a call, a name) folds to
/// `None` and the field is treated as having no materializable default.
fn fold_const_expr(expr: &Expr) -> Option<AttrValue> {
    Some(match expr {
        Expr::Str { value, .. } => AttrValue::Str(value.clone()),
        Expr::Int { value, .. } => AttrValue::Int(*value),
        Expr::Float { value, .. } => AttrValue::Float(*value),
        Expr::Bool { value, .. } => AttrValue::Bool(*value),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => match fold_const_expr(operand)? {
            AttrValue::Int(n) => AttrValue::Int(-n),
            AttrValue::Float(f) => AttrValue::Float(-f),
            _ => return None,
        },
        Expr::List { items, .. } => AttrValue::List(
            items
                .iter()
                .map(fold_const_expr)
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => return None,
    })
}

/// The built-in test-metadata attributes' field shapes (object-model slice 6h/6i) — `(field names,
/// their literal defaults)` — for `Skip`/`Name`/`Group`/`Data`. These are prelude `@attribute`
/// structs the checker registers but that never reach [`build`]'s AST walk (they are not declared in
/// the program), so their shape is supplied here for materialization. Only `Skip` has an optional
/// field (`reason`, default `""`); the rest carry one mandatory field.
fn builtin_attribute_shape(name: &str) -> Option<(Vec<String>, Vec<Option<AttrValue>>)> {
    let one = |field: &str| (vec![field.to_string()], vec![None]);
    Some(match name {
        TEST_ATTR_SKIP => (
            vec!["reason".to_string()],
            vec![Some(AttrValue::Str(String::new()))],
        ),
        TEST_ATTR_NAME | TEST_ATTR_GROUP => one("value"),
        TEST_ATTR_DATA => one("rows"),
        _ => return None,
    })
}

/// The materialization shape of an attribute named `type_name`: its field names and their literal
/// defaults. Resolved from the reflected type registry (user-declared attributes) first, then the
/// built-in test attributes, else empty. The boolean is whether it is a struct (vs a class) — only a
/// class materializes with a class shape.
pub fn attribute_shape(type_name: &str, info: &ReflectionInfo) -> AttributeShape {
    if let Some(t) = info.type_named(type_name) {
        return AttributeShape {
            fields: t.fields.clone(),
            defaults: t.field_defaults.clone(),
            is_struct: !matches!(t.kind, TypeKind::Class),
        };
    }
    let (fields, defaults) = builtin_attribute_shape(type_name).unwrap_or_default();
    AttributeShape {
        fields,
        defaults,
        is_struct: true,
    }
}

/// An attribute type's materialization shape — its field names, their literal defaults (parallel),
/// and whether it is a struct. Returned by [`attribute_shape`] so both backends build the same value.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AttributeShape {
    pub fields: Vec<String>,
    pub defaults: Vec<Option<AttrValue>>,
    pub is_struct: bool,
}

/// Resolve an attribute's arguments to its struct's field values, in declaration order — the shared
/// mapping both backends use to materialize the attribute instance, so they build identical values.
/// Positional arguments fill fields left to right; named arguments bind by field name. An omitted
/// **optional** field (one with a default) is filled from `defaults`; the construction check
/// (E0009/E0007/E0005) guarantees every *mandatory* field is supplied, so the `unit` fallback is
/// unreachable for a runnable program.
pub fn materialize_args(
    attr: &AttributeRecord,
    fields: &[String],
    defaults: &[Option<AttrValue>],
) -> Vec<AttrValue> {
    let mut values: Vec<Option<AttrValue>> = vec![None; fields.len()];
    let mut next_positional = 0usize;
    for arg in &attr.args {
        let idx = match &arg.name {
            Some(fname) => fields.iter().position(|f| f == fname),
            None => {
                let i = next_positional;
                next_positional += 1;
                Some(i)
            }
        };
        if let Some(i) = idx
            && i < values.len()
        {
            values[i] = Some(arg.value.clone());
        }
    }
    values
        .into_iter()
        .enumerate()
        .map(|(i, v)| {
            v.or_else(|| defaults.get(i).cloned().flatten())
                .unwrap_or(AttrValue::Bool(false))
        })
        .collect()
}

/// A backend-agnostic descriptor of the prelude `Type` ADT — the shared vocabulary `type_of`
/// classifies a value into, mirroring the checker's type lattice. Both backends classify their
/// native value into a `TypeRepr` and then build the prelude `Type` enum from it identically (the
/// `Ordering` precedent), so the reflected `Type` value is the same across the differential by
/// construction. The variant names returned by [`TypeRepr::variant_name`] are the canonical
/// `Type.*` constructors users match on.
///
/// At runtime fidelity (B) generics are erased, so a container's element types are [`TypeRepr::Dyn`]
/// (`type_of([1])` is `List(Dyn)`); the compile-time full-fidelity path (P2.3) builds a precise
/// `TypeRepr` from the checker's inferred type and reuses the same construction.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeRepr {
    Int,
    Float,
    Bool,
    /// The `string` scalar (variant name `String`, mirroring the lattice).
    Str,
    Unit,
    Dyn,
    List(Box<TypeRepr>),
    Set(Box<TypeRepr>),
    Option(Box<TypeRepr>),
    Map(Box<TypeRepr>, Box<TypeRepr>),
    Result(Box<TypeRepr>, Box<TypeRepr>),
    /// A declared **enum** type by name, with its type arguments. The reflection `Type` ADT
    /// distinguishes the three nominal kinds (so a consumer can branch on enum-vs-struct-vs-class
    /// from a `type_of` result alone); both backends classify a value's shape kind into the matching
    /// variant. Type arguments are erased to `Dyn` at runtime fidelity, precise at compile-time.
    Enum(String, Vec<TypeRepr>),
    /// A declared **struct** type by name, with its type arguments.
    Struct(String, Vec<TypeRepr>),
    /// A declared **class** type by name, with its type arguments.
    Class(String, Vec<TypeRepr>),
    /// A nominal type whose **kind is not statically known** — an opaque imported type at
    /// compile-time fidelity. (At runtime fidelity the value's shape kind is always known, so the
    /// kind-specific variants are used.)
    Named(String, Vec<TypeRepr>),
    Fn(Vec<TypeRepr>, Box<TypeRepr>),
    Union(Vec<TypeRepr>),
}

impl TypeRepr {
    /// The `Type.*` enum variant name this descriptor constructs — the single source of truth for
    /// the prelude enum's variant naming, shared by both backends and the checker registration.
    pub fn variant_name(&self) -> &'static str {
        match self {
            TypeRepr::Int => "Int",
            TypeRepr::Float => "Float",
            TypeRepr::Bool => "Bool",
            TypeRepr::Str => "String",
            TypeRepr::Unit => "Unit",
            TypeRepr::Dyn => "Dyn",
            TypeRepr::List(_) => "List",
            TypeRepr::Set(_) => "Set",
            TypeRepr::Option(_) => "Option",
            TypeRepr::Map(_, _) => "Map",
            TypeRepr::Result(_, _) => "Result",
            TypeRepr::Enum(_, _) => "Enum",
            TypeRepr::Struct(_, _) => "Struct",
            TypeRepr::Class(_, _) => "Class",
            TypeRepr::Named(_, _) => "Named",
            TypeRepr::Fn(_, _) => "Fn",
            TypeRepr::Union(_) => "Union",
        }
    }
}

/// The `Type` prelude enum's name (the language type `type_of` returns and users match on).
pub const TYPE_ENUM: &str = "Type";

/// The built-in `Semantic` prelude enum's name — the language's own `@semantic` role vocabulary,
/// referenced as `@role(Semantic.EntryPoint)`. A user promotes any enum to the same status with the
/// `@semantic` directive; `Semantic` is implicitly semantic.
pub const SEMANTIC_ENUM: &str = "Semantic";

/// The `RoleBinding` prelude struct's name — `{ target: string, role: Enum }`, the element type of
/// `roles_of()`'s result list. `role` is typed as the abstract `Enum` kind because a binding's role
/// may be any `@semantic` enum (the built-in `Semantic` or a user one), not a single fixed type.
pub const ROLE_BINDING: &str = "RoleBinding";

/// The built-in **test-metadata attributes** (object-model slice 6h) — prelude `@attribute` structs
/// the test runner reads off a `@test`/`@bench` fn: `#[Skip]` (zero fields, mark as skipped),
/// `#[Name("…")]` (display name), `#[Group("…")]` (category for `--group` filtering), `#[Data([…])]`
/// (parameterized rows). The single source of truth shared by the checker's prelude registration and
/// the runner that interprets them.
pub const TEST_ATTR_SKIP: &str = "Skip";
pub const TEST_ATTR_NAME: &str = "Name";
pub const TEST_ATTR_GROUP: &str = "Group";
pub const TEST_ATTR_DATA: &str = "Data";

/// The built-in `Semantic.*` variants, in declaration order. The single source of truth for the
/// language's own role vocabulary, shared by the prelude-enum registration and both backends'
/// materialization. All are payload-free (a richer parameterized form, e.g. `Layer(name)`, would
/// need comptime to evaluate per use site and is deferred).
pub const SEMANTIC_VARIANTS: &[&str] = &[
    "EntryPoint",
    "PersistenceBoundary",
    "TrustBoundary",
    "Sink",
    "Layer",
];

/// Push each field's `#[...]` attributes, keyed by the qualified `Type.field` name (mirroring the
/// `Type.method` convention), so a `#[Column(...)]` on a property surfaces distinctly per owner.
fn push_field_attrs(manifest: &mut Vec<AttributeRecord>, type_name: &str, fields: &[FieldDecl]) {
    for field in fields {
        let target = format!("{}.{}", type_name, field.name);
        push_attrs(manifest, &target, &field.attrs);
    }
}

fn push_attrs(manifest: &mut Vec<AttributeRecord>, target: &str, attrs: &[Attribute]) {
    for attr in attrs {
        manifest.push(AttributeRecord {
            target: target.to_string(),
            name: attr.name.clone(),
            args: attr.args.clone(),
        });
    }
}

/// The flat memory layout of a `@packed` struct value (P-PACK Phase 2): its fields in declared (slot)
/// order, each a primitive — one machine word, pre-Phase-3 — or a nested packed struct laid out
/// recursively. It fully describes how to **pack** a boxed value into a raw word buffer and
/// **unpack** one back (the field names + kinds let a backend rebuild the nested objects without
/// storing field kinds in its own shape). Built by the checker (which knows field types and `@packed`
/// membership) and keyed by the list-construction span, so both backends pack/unpack identically —
/// the flat `List<packed>` representation stays invisible to `RunResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct PackedLayout {
    /// The packed struct's type name — the nominal type of a materialized element.
    pub type_name: String,
    /// The fields in declared (slot) order.
    pub fields: Vec<PackedField>,
}

/// One field of a [`PackedLayout`]: its name (for materializing the boxed value) and its kind.
#[derive(Debug, Clone, PartialEq)]
pub struct PackedField {
    pub name: String,
    pub kind: PackedKind,
}

/// A packed field's kind: a primitive occupying one word, or a nested packed struct flattened inline.
#[derive(Debug, Clone, PartialEq)]
pub enum PackedKind {
    Int,
    Float,
    /// A 32-bit float field (P-PACK Phase 3). One word like the other primitives in slice 3.2a; slice
    /// 3.2b narrows it to a 4-byte slot.
    F32,
    Bool,
    /// A nested `@packed` struct, laid out contiguously in the parent's buffer.
    Struct(Box<PackedLayout>),
}

impl PackedLayout {
    /// The number of machine words one value of this layout occupies — the sum of each field's width
    /// (a primitive is 1; a nested struct is its own `word_count`). Pre-Phase-3 every primitive is one
    /// 64-bit word; Phase 3 (`f32`) will narrow specific slots.
    pub fn word_count(&self) -> usize {
        self.fields
            .iter()
            .map(|f| match &f.kind {
                PackedKind::Int | PackedKind::Float | PackedKind::F32 | PackedKind::Bool => 1,
                PackedKind::Struct(inner) => inner.word_count(),
            })
            .sum()
    }
}
