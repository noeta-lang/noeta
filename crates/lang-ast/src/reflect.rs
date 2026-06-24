//! A pure, deterministic projection of the AST into **reflection data** — the attribute manifest
//! plus a registry of every declared type's reflectable shape. Both backends build it from the same
//! [`Program`] via [`build`], so runtime reflection (attribute-system pass 2) is identical across
//! the tree-walker and the VM **by construction** — there is no second walk to drift from the first.
//! It carries no codegen or runtime meaning of its own; it is a read-only view of the program.

use crate::{AttrArg, AttrValue, Attribute, FieldDecl, Program, Stmt};

/// Everything reflection needs about a program, derived purely from its AST.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ReflectionInfo {
    /// Every `#[...]` data attribute, in source order, each keyed by the declaration it annotates.
    pub manifest: Vec<AttributeRecord>,
    /// Every declared record/class/enum, in source order.
    pub types: Vec<TypeInfo>,
    /// The `(declaration, Role)` index (P2.7): for every declaration bearing an attribute that
    /// carries a `@role(...)` tag, the declaration's name paired with that role's variant name. This
    /// is the labeled dependency graph `roles_of()` surfaces — built beside the attribute manifest
    /// from the same AST, so both backends agree by construction.
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
}

/// One `#[Name(args)]` attached to a declaration. Semantically a record instance attached as
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

/// One `(declaration, Role)` entry of the semantic-role index — a declaration's name paired with
/// the `Role` variant an attribute it bears confers on it. `roles_of()` materializes each into a
/// `RoleBinding { target: string, role: Role }`.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleRecord {
    /// The annotated declaration's name (the same target keying as the attribute manifest).
    pub target: String,
    /// The conferred role's `Role.*` variant name (e.g. `EntryPoint`).
    pub role: String,
}

/// The kind of a declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Record,
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
    // Attribute name → its `@role(...)` variant, harvested from the attribute records themselves;
    // joined with the manifest below so every *use* of a role-tagged attribute is indexed.
    let mut role_of: Vec<(String, String)> = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Record(decl) => {
                push_attrs(&mut manifest, &decl.name, &decl.attrs);
                push_field_attrs(&mut manifest, &decl.name, &decl.fields);
                // A role tag rides on the attribute record; record its (single, validated) variant
                // so each declaration the attribute annotates inherits it. A malformed `@role`
                // never reaches a runnable program (the checker rejects it), so taking the first
                // listed variant is correct for every program this actually materializes.
                if let Some(role) = decl.role.as_ref().and_then(|r| r.first()) {
                    role_of.push((decl.name.clone(), role.0.clone()));
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Record,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
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
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Enum,
                    fields: Vec::new(),
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
    // indexed `(target, role)`. Identical pairs (two attributes conferring the same role on one
    // declaration) are de-duplicated while preserving source order.
    let mut roles: Vec<RoleRecord> = Vec::new();
    for entry in &manifest {
        if let Some((_, role)) = role_of.iter().find(|(name, _)| name == &entry.name) {
            let record = RoleRecord {
                target: entry.target.clone(),
                role: role.clone(),
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

/// Resolve an attribute's arguments to its record's field values, in declaration order — the shared
/// mapping both backends use to materialize the attribute instance, so they build identical values.
/// Positional arguments fill fields left to right; named arguments bind by field name. The use-site
/// construction check (E0009/E0007/E0005) guarantees a well-formed program supplies exactly one
/// value per field, so a runnable program never leaves a field unresolved.
pub fn materialize_args(attr: &AttributeRecord, fields: &[String]) -> Vec<AttrValue> {
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
    // A field with no supplied value cannot occur in a runnable program (the construction check
    // rejects it); the `unit` fallback is unreachable defensive code.
    values
        .into_iter()
        .map(|v| v.unwrap_or(AttrValue::Bool(false)))
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
    /// A declared type (record/class/enum) by name, with its type arguments (erased to `Dyn` at
    /// runtime fidelity, precise at compile-time fidelity).
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
            TypeRepr::Named(_, _) => "Named",
            TypeRepr::Fn(_, _) => "Fn",
            TypeRepr::Union(_) => "Union",
        }
    }
}

/// The `Type` prelude enum's name (the language type `type_of` returns and users match on).
pub const TYPE_ENUM: &str = "Type";

/// The `Role` prelude enum's name — the small blessed set of architectural roles an attribute may
/// confer with `@role(...)`, and the type `roles_of()` yields per binding.
pub const ROLE_ENUM: &str = "Role";

/// The `RoleBinding` prelude record's name — `{ target: string, role: Role }`, the element type of
/// `roles_of()`'s result list.
pub const ROLE_BINDING: &str = "RoleBinding";

/// The blessed `Role.*` variants, in declaration order. The single source of truth for the role
/// vocabulary, shared by the parser-side `@role(...)` validation (checker), the prelude-enum
/// registration, and both backends' materialization — so a `@role` tag, the enum users match on,
/// and the value `roles_of()` builds all agree. All are payload-free (a richer parameterized form,
/// e.g. `Layer(name)`, would need comptime to evaluate per use site and is deferred).
pub const ROLE_VARIANTS: &[&str] = &[
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
