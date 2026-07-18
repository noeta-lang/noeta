//! A pure, deterministic projection of the AST into **reflection data** — the attribute manifest
//! plus a registry of every declared type's reflectable shape. Both backends build it from the same
//! [`Program`] via [`build`], so runtime reflection (attribute-system pass 2) is identical across
//! the tree-walker and the VM **by construction** — there is no second walk to drift from the first.
//! It carries no codegen or runtime meaning of its own; it is a read-only view of the program.

use crate::{AttrArg, AttrValue, Attribute, Expr, FieldDecl, Program, Stmt, TypeRef, UnaryOp};
use noeta_span::Span;
use serde::{Deserialize, Serialize};

/// Everything reflection needs about a program, derived purely from its AST.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
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
    /// Every callable's declared parameter list, keyed by its target (a top-level fn's bare name,
    /// a method's qualified `Type.method` name — the same target keying the attribute manifest).
    /// This is what `params_of(target)` surfaces for a web framework's dependency injection, built
    /// beside the attribute manifest from the same AST so both backends agree by construction.
    pub params: Vec<ParamRecord>,
}

impl ReflectionInfo {
    /// Merge another program fragment's reflection into this one, **latest-wins** for any redeclared
    /// name. A REPL session builds one fragment per entry and accumulates them here, so a type or
    /// attribute declared in an earlier entry stays queryable (`attributes_of` / `type_of` /
    /// `roles_of` across entries) while a type *redefined* in a later entry supersedes its old records
    /// — matching how method dispatch resolves to the newest declaration. Records for names the
    /// incoming fragment does not touch are left in place; the fragment's own records are appended in
    /// source order after purging any they redeclare.
    pub fn accumulate(&mut self, fragment: ReflectionInfo) {
        // The declaration names this fragment (re)defines — a type it declares, or any attribute /
        // role target it carries. Their old records are superseded wholesale before the new ones land.
        let redeclared: std::collections::HashSet<&str> = fragment
            .types
            .iter()
            .map(|t| t.name.as_str())
            .chain(fragment.manifest.iter().map(|a| a.target.as_str()))
            .chain(fragment.roles.iter().map(|r| r.target.as_str()))
            .collect();
        self.types.retain(|t| !redeclared.contains(t.name.as_str()));
        self.manifest
            .retain(|a| !redeclared.contains(a.target.as_str()));
        self.roles
            .retain(|r| !redeclared.contains(r.target.as_str()));
        // Param records are keyed by a callable's target (`fn` or `Type.method`); a redeclared
        // callable purges its old params. A plain fn or method carries no attribute, so its target
        // is not in `redeclared` (which is built from type names + attribute/role targets) — key the
        // purge on the target's declaration base (the type name before `.`, or the bare fn name) and
        // on the incoming fragment's own param targets, so redefining a callable supersedes its old
        // parameter list even when it bears no attribute.
        let param_bases: std::collections::HashSet<&str> = fragment
            .params
            .iter()
            .map(|p| param_base(&p.target))
            .collect();
        self.params.retain(|p| {
            let base = param_base(&p.target);
            !redeclared.contains(base) && !param_bases.contains(base)
        });
        drop(redeclared);
        drop(param_bases);
        self.types.extend(fragment.types);
        self.manifest.extend(fragment.manifest);
        self.roles.extend(fragment.roles);
        self.params.extend(fragment.params);
    }

    /// The parameter list declared for `target`, or empty if the target names no known callable — the
    /// projection `params_of(target)` materializes for dependency injection.
    pub fn params_for(&self, target: &str) -> &[ParamSig] {
        self.params
            .iter()
            .find(|p| p.target == target)
            .map(|p| p.params.as_slice())
            .unwrap_or(&[])
    }

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
        if let Some(scalar) = scalar_repr(name) {
            return scalar;
        }
        let dyn_box = || Box::new(TypeRepr::Dyn);
        match name {
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AttributeRecord {
    /// The annotated declaration's name (a type name today; pass 2 extends attributes to methods).
    pub target: String,
    /// The span of the annotated declaration's *name*, so tooling can locate the target in source
    /// (the runtime materialization ignores it).
    pub target_span: Span,
    /// The attribute's name (e.g. `Route`).
    pub name: String,
    /// The attribute's literal arguments (positional + named), straight from the AST.
    pub args: Vec<AttrArg>,
}

/// One `(declaration, role)` entry of the semantic-role index — a declaration's name paired with
/// the role an attribute it bears confers on it, identified by its `@semantic` enum and variant.
/// `roles_of()` materializes each into a `RoleBinding { target: string, role: Enum }` whose `role`
/// is the actual `enum_name.variant` enum value.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RoleRecord {
    /// The annotated declaration's name (the same target keying as the attribute manifest).
    pub target: String,
    /// The span of the annotated declaration's *name*, so tooling can locate the role bearer in
    /// source (the runtime `roles_of()` materialization ignores it).
    pub target_span: Span,
    /// The role's `@semantic` enum name (e.g. `Semantic`, `WebRole`).
    pub enum_name: String,
    /// The role's variant name (e.g. `EntryPoint`, `Controller`).
    pub variant: String,
}

/// One callable's declared parameter list — a top-level fn or a method — keyed by the same target
/// convention as the attribute manifest (a bare fn name, or a qualified `Type.method`). `params_of()`
/// materializes each into a `List<ParamInfo>` (each `{ name: string, type: Type }`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ParamRecord {
    /// The callable's target: a top-level fn's bare name, or a method's qualified `Type.method` name.
    pub target: String,
    /// The declared parameters, in source order.
    pub params: Vec<ParamSig>,
}

/// One declared parameter — its name and the reflection [`TypeRepr`] of its annotated type. An
/// unannotated parameter's type is [`TypeRepr::Dyn`]. `params_of()` materializes each into a
/// `ParamInfo { name: string, type: Type }` whose `type` is the `Type` ADT value `type_of` builds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ParamSig {
    pub name: String,
    pub ty: TypeRepr,
}

/// The declaration base a param record's target keys on for latest-wins purging: the type name
/// before the `.` of a `Type.method` target, or the whole name for a bare top-level fn.
fn param_base(target: &str) -> &str {
    target.split_once('.').map(|(ty, _)| ty).unwrap_or(target)
}

/// The kind of a declared type.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Struct,
    Class,
    Enum,
}

/// A declared type's reflectable shape: name, kind, and member names (declaration order). Field and
/// variant *types* are deliberately absent — they are erased at runtime, and reflection over a value
/// recovers names, not the static field types (which are a compile-time `type_of` concern).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
    // Every callable's declared parameter list, keyed by target (bare fn name or `Type.method`) — the
    // index `params_of(target)` surfaces, built in source order alongside the attribute manifest.
    let mut params: Vec<ParamRecord> = Vec::new();
    // Attribute name → its `@role(Enum.Variant)` tags, harvested from the attribute records
    // themselves; joined with the manifest below so every *use* of a role-tagged attribute is
    // indexed. One entry per (attribute, role) pair — an attribute may carry several roles.
    let mut role_of: Vec<(String, (String, String))> = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Struct(decl) => {
                push_attrs(&mut manifest, &decl.name, decl.name_span, &decl.attrs);
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
                for method in &decl.methods {
                    params.push(ParamRecord {
                        target: format!("{}.{}", decl.name, method.name),
                        params: param_sigs(&method.params),
                    });
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
                push_attrs(&mut manifest, &decl.name, decl.name_span, &decl.attrs);
                push_field_attrs(&mut manifest, &decl.name, &decl.fields);
                // A method's attributes are keyed by its qualified `Class.method` name, so a
                // `#[...]` on a method surfaces distinctly from the same name on another class.
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, method.name_span, &method.attrs);
                    params.push(ParamRecord {
                        target,
                        params: param_sigs(&method.params),
                    });
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
            Stmt::Fn(decl) => {
                push_attrs(&mut manifest, &decl.name, decl.name_span, &decl.attrs);
                params.push(ParamRecord {
                    target: decl.name.clone(),
                    params: param_sigs(&decl.params),
                });
            }
            // A trait carries `#[...]` data attributes keyed by its name (UT6), like a type —
            // surfaced via `attributes_of` (and inheriting a role transitively when annotated with a
            // role-bearing attribute). It is not a data type, so it adds no `TypeInfo`; its abstract
            // method signatures are not scanned (route/metadata attributes live on the concrete
            // `impl` methods, scanned via the class/struct arms). A direct `@role`/`@derive`/… on a
            // trait is a checker error, so a runnable program never carries one here.
            Stmt::Trait(decl) => {
                push_attrs(&mut manifest, &decl.name, decl.name_span, &decl.attrs);
                // A trait's abstract method signatures carry declared parameters too, keyed by the
                // `Trait.method` convention — surfaced via `params_of` like a concrete method's.
                for method in &decl.methods {
                    params.push(ParamRecord {
                        target: format!("{}.{}", decl.name, method.sig.name),
                        params: param_sigs(&method.sig.params),
                    });
                }
            }
            Stmt::Enum(decl) => {
                push_attrs(&mut manifest, &decl.name, decl.name_span, &decl.attrs);
                // A variant's attributes are keyed by its qualified `Enum.Variant` name, mirroring
                // the `Type.field`/`Type.method` convention.
                for variant in &decl.variants {
                    let target = format!("{}.{}", decl.name, variant.name);
                    push_attrs(&mut manifest, &target, variant.name_span, &variant.attrs);
                }
                // An enum method's attributes are keyed by its qualified `Enum.method` name, the same
                // convention class/struct methods use (object-model slice 3).
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, method.name_span, &method.attrs);
                    params.push(ParamRecord {
                        target,
                        params: param_sigs(&method.params),
                    });
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
                target_span: entry.target_span,
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
        params,
    }
}

/// Project a callable's declared parameters onto their reflection [`ParamSig`]s — each parameter's
/// name paired with the [`TypeRepr`] of its annotated type (an unannotated parameter is
/// [`TypeRepr::Dyn`]). Shared by every callable arm of [`build`] so a fn, method, and trait method
/// sig all surface their parameters identically.
fn param_sigs(params: &[crate::Param]) -> Vec<ParamSig> {
    params
        .iter()
        .map(|p| ParamSig {
            name: p.name.clone(),
            ty: p.ty.as_ref().map(typeref_to_repr).unwrap_or(TypeRepr::Dyn),
        })
        .collect()
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

/// The materialization shape of an attribute named `type_name`: its field names and their literal
/// defaults, resolved from the reflection artifact — which carries user-declared attributes from
/// the AST walk *and* extension-declared ones embedded at compile time
/// (`noeta_check::extend_reflection`, tier-extensions port). The boolean is whether it is a
/// struct (vs a class) — only a class materializes with a class shape.
pub fn attribute_shape(type_name: &str, info: &ReflectionInfo) -> AttributeShape {
    if let Some(t) = info.type_named(type_name) {
        return AttributeShape {
            fields: t.fields.clone(),
            defaults: t.field_defaults.clone(),
            is_struct: !matches!(t.kind, TypeKind::Class),
        };
    }
    // Unknown to the artifact — including an extension attribute in an artifact built before the
    // registry embed (impossible on the normal compile paths, which all call
    // `noeta_check::extend_reflection`). The honest empty shape.
    AttributeShape::default()
}

/// An attribute type's materialization shape — its field names, their literal defaults (parallel),
/// and whether it is a struct. Returned by [`attribute_shape`] so both backends build the same value.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum TypeRepr {
    Int,
    Float,
    /// The 32-bit float scalar `f32` (P-PACK Phase 3; variant name `F32`).
    F32,
    Bool,
    /// The `string` scalar (variant name `String`, mirroring the lattice).
    Str,
    /// The `bytes` scalar — a raw byte buffer (P-PACK 4.4; variant name `Bytes`).
    Bytes,
    Unit,
    Dyn,
    /// A **trait object** `dyn Trait` — the dynamic top *refined by a trait bound*, carrying the trait
    /// name. Distinct from bare `Dyn` so reflection (`params_of`) can recover which trait a parameter
    /// is bound to — what a framework needs to inject a service by its interface (`fn(store: dyn
    /// Store)`). The runtime value still carries its own concrete type; this is the declared bound.
    DynTrait(String),
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
    /// This type's **type arguments** (runtime type-argument reflection, R3), in order: a container's
    /// element/key/value types, a generic nominal type's arguments. Empty for a scalar or a
    /// non-generic nominal. Used to compare a narrow target's arguments against a value's reflected tag.
    pub fn type_args(&self) -> Vec<&TypeRepr> {
        match self {
            TypeRepr::List(t) | TypeRepr::Set(t) | TypeRepr::Option(t) => vec![t],
            TypeRepr::Map(k, v) | TypeRepr::Result(k, v) => vec![k, v],
            TypeRepr::Enum(_, args)
            | TypeRepr::Struct(_, args)
            | TypeRepr::Class(_, args)
            | TypeRepr::Named(_, args) => args.iter().collect(),
            _ => Vec::new(),
        }
    }

    /// The nominal type name for a declared `struct`/`class`/`enum` or an unknown-kind `Named`, else
    /// `None`. Two nominal reprs of the same name are the same head **regardless of kind** — a narrow
    /// target built without kind information is `Named`, while a value's tag knows its kind
    /// (`Struct`/`Class`/`Enum`), so R3 matching keys on the name, not the kind.
    fn nominal_name(&self) -> Option<&str> {
        match self {
            TypeRepr::Enum(n, _)
            | TypeRepr::Struct(n, _)
            | TypeRepr::Class(n, _)
            | TypeRepr::Named(n, _) => Some(n),
            _ => None,
        }
    }

    /// The `Type.*` enum variant name this descriptor constructs — the single source of truth for
    /// the prelude enum's variant naming, shared by both backends and the checker registration.
    pub fn variant_name(&self) -> &'static str {
        match self {
            TypeRepr::Int => "Int",
            TypeRepr::Float => "Float",
            TypeRepr::F32 => "F32",
            TypeRepr::Bool => "Bool",
            TypeRepr::Str => "String",
            TypeRepr::Bytes => "Bytes",
            TypeRepr::Unit => "Unit",
            TypeRepr::Dyn => "Dyn",
            TypeRepr::DynTrait(_) => "DynTrait",
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

/// A [`TypeRepr`] displays as its **Noeta surface spelling** — the source form a developer
/// recognizes: scalars by keyword (`int`, `string`, `void`), containers as `List<T>` / `Map<K, V>`,
/// optionals as `?T`, unions as `A | B`, function types as `(A, B) -> R`, and nominal types by name
/// with `<…>` type arguments. Shared by every human-facing surface (LSP hover, the debugger's
/// Variables view) so a type reads the same everywhere.
impl std::fmt::Display for TypeRepr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeRepr::Int => f.write_str("int"),
            TypeRepr::Float => f.write_str("float"),
            TypeRepr::F32 => f.write_str("f32"),
            TypeRepr::Bool => f.write_str("bool"),
            TypeRepr::Str => f.write_str("string"),
            TypeRepr::Bytes => f.write_str("bytes"),
            TypeRepr::Unit => f.write_str("void"),
            TypeRepr::Dyn => f.write_str("dyn"),
            TypeRepr::DynTrait(name) => write!(f, "dyn {name}"),
            TypeRepr::List(t) => write!(f, "List<{t}>"),
            TypeRepr::Set(t) => write!(f, "Set<{t}>"),
            TypeRepr::Option(t) => write!(f, "?{t}"),
            TypeRepr::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            TypeRepr::Result(o, e) => write!(f, "Result<{o}, {e}>"),
            TypeRepr::Enum(name, args)
            | TypeRepr::Struct(name, args)
            | TypeRepr::Class(name, args)
            | TypeRepr::Named(name, args) => {
                f.write_str(name)?;
                if !args.is_empty() {
                    write!(f, "<{}>", join_types(args))?;
                }
                Ok(())
            }
            TypeRepr::Fn(params, ret) => write!(f, "({}) -> {ret}", join_types(params)),
            TypeRepr::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{m}")?;
                }
                Ok(())
            }
        }
    }
}

/// Comma-join a type list for `<…>` arguments and `(…)` parameters.
fn join_types(types: &[TypeRepr]) -> String {
    types
        .iter()
        .map(TypeRepr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Project a surface [`TypeRef`] onto a reflection [`TypeRepr`], **without kind information** (runtime
/// type-argument reflection, R3): a declared `struct`/`class`/`enum` maps to [`TypeRepr::Named`]
/// (the R3 matcher keys on the name, not the kind, so `Named("Box")` matches a value tagged
/// `Struct("Box")`). Built-in scalars/collections map to their lattice variant; a `?T` is `Option<T>`.
/// Used to turn a narrow target (`x is List<int>`) into the shape compared against a value's tag.
/// The [`TypeRepr`] of a built-in **scalar** type name — one with no type arguments and no nominal
/// resolution. `None` for a container (`List`/`Map`/…), a user type, or `Any`-shaped name that the
/// two mappers below handle differently. Shared by [`ReflectionInfo::type_ref_repr`] and
/// [`typeref_to_repr`], which agree on the scalars but diverge on containers (bare `dyn` args vs the
/// real `TypeRef` args) and on the fallback (nominal-kind lookup vs `TypeRepr::Named`).
fn scalar_repr(name: &str) -> Option<TypeRepr> {
    Some(match name {
        "int" => TypeRepr::Int,
        "float" => TypeRepr::Float,
        "f32" => TypeRepr::F32,
        "bool" => TypeRepr::Bool,
        "string" => TypeRepr::Str,
        "bytes" => TypeRepr::Bytes,
        "void" | "unit" => TypeRepr::Unit,
        "dyn" | "Any" => TypeRepr::Dyn,
        _ => return None,
    })
}

pub fn typeref_to_repr(ty: &TypeRef) -> TypeRepr {
    let boxed = |t: &TypeRef| Box::new(typeref_to_repr(t));
    let dyn_box = || Box::new(TypeRepr::Dyn);
    match ty {
        TypeRef::Union { members, .. } => {
            TypeRepr::Union(members.iter().map(typeref_to_repr).collect())
        }
        TypeRef::Optional { inner, .. } => TypeRepr::Option(boxed(inner)),
        // A trait object reflects as `DynTrait(name)` — the dynamic top refined by its trait bound, so
        // reflection can recover which trait a parameter is bound to (service injection by interface).
        TypeRef::DynTrait { trait_name, .. } => TypeRepr::DynTrait(trait_name.clone()),
        TypeRef::Tuple { .. } => TypeRepr::Dyn,
        TypeRef::Fn { params, ret, .. } => TypeRepr::Fn(
            params.iter().map(typeref_to_repr).collect(),
            Box::new(typeref_to_repr(ret)),
        ),
        TypeRef::Named { name, args, .. } => {
            if let Some(scalar) = scalar_repr(name) {
                return scalar;
            }
            let arg = |i: usize| args.get(i).map(boxed).unwrap_or_else(dyn_box);
            match name.as_str() {
                "List" | "list" => TypeRepr::List(arg(0)),
                "Set" | "set" => TypeRepr::Set(arg(0)),
                "Map" | "map" => TypeRepr::Map(arg(0), arg(1)),
                "Option" => TypeRepr::Option(arg(0)),
                "Result" => TypeRepr::Result(arg(0), arg(1)),
                _ => TypeRepr::Named(name.clone(), args.iter().map(typeref_to_repr).collect()),
            }
        }
    }
}

/// Whether one narrow-target type argument (`expected`, from `x is List<int>`) matches a value's
/// reflected argument (`actual`, from its R1/R2 tag or its head-only classification) — runtime
/// type-argument reflection, R3. A [`TypeRepr::Dyn`] on **either** side is a wildcard: the target
/// `<dyn>` matches anything, and an untagged/unknown actual (whose args classify to `Dyn`) is not
/// rejected — preserving the head-only behavior for values that carry no tag. Otherwise the
/// constructors must agree (nominal types by **name**, tolerant of kind) and their own arguments
/// match recursively.
pub fn arg_matches(expected: &TypeRepr, actual: &TypeRepr) -> bool {
    use TypeRepr::*;
    match (expected, actual) {
        (Dyn, _) | (_, Dyn) => true,
        (List(e), List(a)) | (Set(e), Set(a)) | (Option(e), Option(a)) => arg_matches(e, a),
        (Map(ek, ev), Map(ak, av)) | (Result(ek, ev), Result(ak, av)) => {
            arg_matches(ek, ak) && arg_matches(ev, av)
        }
        (Int, Int)
        | (Float, Float)
        | (F32, F32)
        | (Bool, Bool)
        | (Str, Str)
        | (Bytes, Bytes)
        | (Unit, Unit) => true,
        (DynTrait(e), DynTrait(a)) => e == a,
        (Union(es), a) => es.iter().any(|e| arg_matches(e, a)),
        (e, Union(as_)) => as_.iter().any(|a| arg_matches(e, a)),
        // A nominal type matches by name (kind-tolerant: a `Named` target vs a `Struct`/`Class`/`Enum`
        // tag), then argument-wise.
        (e, a) => match (e.nominal_name(), a.nominal_name()) {
            (Some(en), Some(an)) if en == an => {
                let (ea, aa) = (e.type_args(), a.type_args());
                ea.len() == aa.len() && ea.iter().zip(aa).all(|(x, y)| arg_matches(x, y))
            }
            _ => false,
        },
    }
}

/// Whether a parametrized narrow target's arguments (`target_args`, e.g. the `int` of `x is List<int>`)
/// match a value's reflected type `actual` (its R1/R2 tag, or the head-only classification for an
/// untagged value) — runtime type-argument reflection, R3. The head constructor is assumed already
/// matched by the caller's head-only test; this checks the arguments position-wise via [`arg_matches`]
/// (so an untagged value, whose reflected args are `Dyn`, matches any target — the head-only fallback).
pub fn narrow_args_match(target_args: &[TypeRepr], actual: &TypeRepr) -> bool {
    let actual_args = actual.type_args();
    target_args.len() == actual_args.len()
        && target_args
            .iter()
            .zip(actual_args)
            .all(|(e, a)| arg_matches(e, a))
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

/// The `ParamInfo` prelude struct's name — `{ name: string, type: Type }`, the element type of
/// `params_of()`'s result list. `type` is the reflection `Type` ADT value (the same ADT `type_of`
/// returns), built from the parameter's declared type annotation.
pub const PARAM_INFO: &str = "ParamInfo";

/// The built-in **test-metadata attributes** (object-model slice 6h) — prelude `@attribute` structs
/// the test runner reads off a `@test`/`@bench` fn: `#[Skip]` (zero fields, mark as skipped),
/// `#[Name("…")]` (display name), `#[Group("…")]` (category for `--group` filtering), `#[Data([…])]`
/// (parameterized rows). The single source of truth shared by the checker's prelude registration and
/// the runner that interprets them.
pub const TEST_ATTR_SKIP: &str = "Skip";
pub const TEST_ATTR_NAME: &str = "Name";
pub const TEST_ATTR_GROUP: &str = "Group";
pub const TEST_ATTR_DATA: &str = "Data";

/// The **tier-knob attribute** of the `bench` tier: `#[Bench(iterations: N)]` on a bench fn sets its
/// iteration count. A `@bench(iterations: N) { … }` block directive is distribution sugar — it
/// stamps this attribute onto each contained fn that does not already carry one (a per-fn attribute
/// wins over the block's). One mandatory `iterations: int` field; validated by the ordinary
/// attribute construction gate, read by the bench runner.
pub const TIER_ATTR_BENCH: &str = "Bench";

/// The `doc` tier's attribute: activation with the `doc` tier live stamps `#[Doc("…")]` onto the
/// declaration a `@doc { … }` block documents (adjacency-resolved), giving runtime docstrings via
/// `attributes_of`. On a normal build the doc blocks strip at lowering and nothing is stamped, so
/// production carries no doc text. One mandatory `text: string` field.
pub const TIER_ATTR_DOC: &str = "Doc";

/// The prelude struct a declared tier's runner receives its roots as (tier-providers T2):
/// `TierRoot { name: string, run: () -> void }` — one per activated fn. The checker registers it
/// as a prelude type; dispatch constructs instances in the synthesized runner-call fragment.
pub const TIER_ROOT: &str = "TierRoot";

/// The prelude struct a declared **text** tier's runner receives its roots as (text-tiers arc):
/// `TierText { target: string, text: string }` — one per activated verbatim body. `target` is the
/// adjacency-resolved declaration name (`""` for a module/section block); `text` is the body with
/// the `\{`/`\}`/`\\` escapes undone. The checker registers it as a prelude type; dispatch
/// constructs instances in the synthesized runner-call fragment.
pub const TIER_TEXT: &str = "TierText";

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

/// The prelude `Layout` enum's name — the storage-layout vocabulary `@packed` takes
/// (`@packed(Layout.Column)`). Like [`SEMANTIC_ENUM`] it is directive vocabulary, not a runtime
/// value: the parser resolves the argument syntactically, and the prelude registers the enum so
/// tooling (hover, completion, docs) sees one authoritative declaration.
pub const LAYOUT_ENUM: &str = "Layout";

/// The `Layout.*` variants, in declaration order, mirroring [`PackedLayout`](crate::PackedLayout):
/// `Row` (AoS, the bare-`@packed` default) and `Column` (SoA, P-SIMD). The single source of truth
/// the parser validates `@packed(Layout.…)` against and completion offers.
pub const LAYOUT_VARIANTS: &[&str] = &["Row", "Column"];

/// Push each field's `#[...]` attributes, keyed by the qualified `Type.field` name (mirroring the
/// `Type.method` convention), so a `#[Column(...)]` on a property surfaces distinctly per owner.
fn push_field_attrs(manifest: &mut Vec<AttributeRecord>, type_name: &str, fields: &[FieldDecl]) {
    for field in fields {
        let target = format!("{}.{}", type_name, field.name);
        push_attrs(manifest, &target, field.name_span, &field.attrs);
    }
}

fn push_attrs(
    manifest: &mut Vec<AttributeRecord>,
    target: &str,
    target_span: Span,
    attrs: &[Attribute],
) {
    for attr in attrs {
        manifest.push(AttributeRecord {
            target: target.to_string(),
            target_span,
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PackedLayout {
    /// The packed struct's type name — the nominal type of a materialized element.
    pub type_name: String,
    /// The fields in declared (slot) order.
    pub fields: Vec<PackedField>,
    /// Whether lists of this element are stored **column-major** — the `@packed(Layout.Column)`
    /// attribute (P-SIMD C2). A performance-only property (see `noeta_object::PackedSchema::column`);
    /// carried here so the compiler can thread it into the runtime schema both backends read.
    pub column: bool,
}

/// One field of a [`PackedLayout`]: its name (for materializing the boxed value) and its kind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PackedField {
    pub name: String,
    pub kind: PackedKind,
}

/// A packed field's kind: a primitive occupying one word, or a nested packed struct flattened inline.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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

    /// The number of **bytes** one value of this layout occupies in the byte-addressed packed buffer
    /// (P-PACK 3.2b): `bool` is 1 byte, `f32` is 4, `int`/`float` are 8, a nested struct its own
    /// `byte_size`. Both backends use this layout (the eval reference was narrowed to bytes too); the
    /// legacy `word_count` survives only for any remaining word-addressed callers.
    pub fn byte_size(&self) -> usize {
        self.fields
            .iter()
            .map(|f| match &f.kind {
                PackedKind::Bool => 1,
                PackedKind::F32 => 4,
                PackedKind::Int | PackedKind::Float => 8,
                PackedKind::Struct(inner) => inner.byte_size(),
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(t: TypeRepr) -> Box<TypeRepr> {
        Box::new(t)
    }

    #[test]
    fn scalars_display_as_keywords() {
        assert_eq!(TypeRepr::Int.to_string(), "int");
        assert_eq!(TypeRepr::Str.to_string(), "string");
        assert_eq!(TypeRepr::Unit.to_string(), "void");
        assert_eq!(TypeRepr::Dyn.to_string(), "dyn");
    }

    #[test]
    fn containers_nest() {
        assert_eq!(
            TypeRepr::List(boxed(TypeRepr::Int)).to_string(),
            "List<int>"
        );
        assert_eq!(
            TypeRepr::Map(boxed(TypeRepr::Str), boxed(TypeRepr::Int)).to_string(),
            "Map<string, int>"
        );
        assert_eq!(
            TypeRepr::List(boxed(TypeRepr::Option(boxed(TypeRepr::Int)))).to_string(),
            "List<?int>"
        );
    }

    #[test]
    fn nominal_with_and_without_args() {
        assert_eq!(
            TypeRepr::Struct("Point".to_string(), vec![]).to_string(),
            "Point"
        );
        assert_eq!(
            TypeRepr::Class("Box".to_string(), vec![TypeRepr::Int]).to_string(),
            "Box<int>"
        );
    }

    #[test]
    fn function_and_union() {
        assert_eq!(
            TypeRepr::Fn(vec![TypeRepr::Int, TypeRepr::Str], boxed(TypeRepr::Bool)).to_string(),
            "(int, string) -> bool"
        );
        assert_eq!(
            TypeRepr::Union(vec![TypeRepr::Int, TypeRepr::Str]).to_string(),
            "int | string"
        );
    }
}
