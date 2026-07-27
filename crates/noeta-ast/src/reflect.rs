//! A pure, deterministic projection of the AST into **reflection data** — the attribute manifest
//! plus a registry of every declared type's reflectable shape. Both backends build it from the same
//! [`Program`] via [`build`], so runtime reflection (attribute-system pass 2) is identical across
//! the tree-walker and the VM **by construction** — there is no second walk to drift from the first.
//! It carries no codegen or runtime meaning of its own; it is a read-only view of the program.

use crate::{
    AttrArg, AttrValue, Attribute, BuiltinTy, Expr, FieldDecl, Program, Stmt, TypeRef, UnaryOp,
};
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
    /// Every **declared trait implementation**, in source order: which trait each nominal type
    /// implements, from standalone `impl Trait for T`, in-body `impl Trait { … }` blocks,
    /// `@derive(Trait)`, and a native type's ABI-advertised impls (the registry projection a caller
    /// passes to [`build`]). Both the runtime `x is dyn Trait` / `x.as<dyn Trait>()` membership
    /// test and the `traits_of(value)` reflection query read this ONE table — the same declarations
    /// that make trait-method dispatch resolve — so "is" and "would a trait method call work"
    /// cannot disagree. See [`TraitImplRecord`] for the naming discipline.
    pub trait_impls: Vec<TraitImplRecord>,
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
        // Every callable this fragment (re)declares, by the target its parameters are keyed under.
        // A callable always emits a `ParamRecord` (`push_params` emits both renderings together), so
        // this set is exactly "the callables whose parameter lists this fragment redefines".
        let fragment_callables: std::collections::HashSet<&str> =
            fragment.params.iter().map(|p| p.target.as_str()).collect();
        self.types.retain(|t| !redeclared.contains(t.name.as_str()));
        self.manifest.retain(|a| {
            match split_param_attr_target(&a.target) {
                // A parameter row lives and dies with its callable's parameter list, not with its
                // own key: redeclaring `fn build(target: string)` without the `#[Arg]` it used to
                // carry must *drop* the old row, and the new fragment names no such target to
                // supersede it. Keying the purge on the callable is the same move the `params`
                // purge below makes, for the same reason — and it is why the parameter key is
                // built to be splittable back into its callable at all.
                Some((callable, _)) => {
                    !fragment_callables.contains(callable)
                        && !redeclared.contains(param_base(callable))
                }
                None => !redeclared.contains(a.target.as_str()),
            }
        });
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
        // A redeclared type's trait impls are superseded wholesale — the fragment's own records
        // (re-collected from its `impl`s/derives) land below, exactly like its `TypeInfo`.
        self.trait_impls
            .retain(|r| !redeclared.contains(r.type_name.as_str()));
        drop(redeclared);
        drop(param_bases);
        drop(fragment_callables);
        self.types.extend(fragment.types);
        self.manifest.extend(fragment.manifest);
        self.roles.extend(fragment.roles);
        self.params.extend(fragment.params);
        for record in fragment.trait_impls {
            if !self.trait_impls.contains(&record) {
                self.trait_impls.push(record);
            }
        }
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

    /// The data attributes attached to one **parameter** of `callable`, in source order.
    ///
    /// The join that makes `ParamInfo.attrs` a *view* of the attribute manifest rather than a second
    /// copy of it: `params_of` materializes each parameter's attributes through here, and
    /// `attributes_of::<T>()` reads the very same rows off the same table. Both go through
    /// [`param_attr_target`] for the key, so the two surfaces cannot disagree about which attribute
    /// belongs to which parameter — they are one fact rendered twice.
    pub fn param_attributes_for(&self, callable: &str, param: &str) -> Vec<&AttributeRecord> {
        let key = param_attr_target(callable, param);
        self.manifest.iter().filter(|a| a.target == key).collect()
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

    /// The **type-level field schema** of the declared struct/class `type_name` — one
    /// [`FieldSpecData`] per field in declaration order — the data `field_specs_of::<T>()` /
    /// `field_specs_of(name)` materialize. An unknown type (or an enum) yields the empty list, the
    /// same "nothing to report" answer `params_of` gives an unknown target, so a framework can probe
    /// a type name without a guard. Both backends read this one accessor, so the materialized
    /// `List<FieldSpec>` agrees across the differential by construction.
    pub fn field_specs(&self, type_name: &str) -> Vec<FieldSpecData<'_>> {
        let Some(info) = self.type_named(type_name) else {
            return Vec::new();
        };
        info.fields
            .iter()
            .enumerate()
            .map(|(i, name)| FieldSpecData {
                name,
                ty: info.field_types.get(i).unwrap_or(&TypeRepr::Dyn),
                optional: info.field_optional.get(i).copied().unwrap_or(false),
            })
            .collect()
    }

    /// The reflection [`TypeRepr`] of a **type reference** — a type name used as a value
    /// (`#[Encode(codec: JsonCodec)]`, `#[Builds(target: List<int>)]`), given its head `name` and
    /// its generic `args`. Reports the same precise constructor a `type_of` over a value of that
    /// type would: a built-in scalar/collection maps to its lattice variant (`int` → `Type.Int`,
    /// `List<int>` → `Type.List(Type.Int)`), and a declared type maps by *kind* (`Type.Struct`/
    /// `Enum`/`Class`). Only a name with no known classification — an opaque import, or one of the
    /// abstract kind-types `Enum`/`Struct`/`Class` used directly — stays `Type.Named`, the honest
    /// unknown-kind fallback. Both backends build a type-ref through this one function, so the
    /// materialized `Type` value agrees across the differential by construction.
    ///
    /// The arguments are projected **recursively and kind-aware**, so `List<JsonCodec>` is
    /// `Type.List(Type.Struct("JsonCodec", []))` rather than a head with the arguments erased. Pass
    /// `&[]` for a genuinely argument-less reference (a bare name used as an `invoke` receiver).
    pub fn type_ref_repr(&self, name: &str, args: &[TypeRef]) -> TypeRepr {
        named_repr(name, args, &|n, a| self.nominal_repr(n, a), true)
    }

    /// The reflection [`TypeRepr`] of a surface [`TypeRef`], **kind-aware** — the structural
    /// counterpart of [`type_ref_repr`](Self::type_ref_repr), reached for the nested arguments of a
    /// generic type reference (`Map<string, List<int>>`) and for the forms a bare name cannot spell
    /// (`?T`, `A | B`, `(A) -> B`). Differs from the free [`typeref_to_repr`] only in classifying a
    /// declared nominal by its kind instead of the kind-agnostic [`TypeRepr::Named`].
    pub fn typeref_repr(&self, ty: &TypeRef) -> TypeRepr {
        typeref_repr_with(ty, &|n, a| self.nominal_repr(n, a), true)
    }

    /// The **qualified trait names** the nominal type `type_name` implements — sorted and deduped,
    /// the exact list `traits_of(value)` materializes. An unknown or impl-less type yields the
    /// empty list (the same "nothing to report" answer `fields_of`/`params_of` give), so a
    /// framework can probe any value without a guard. Both backends read this one accessor, so the
    /// materialized `List<string>` agrees across the differential by construction.
    pub fn traits_for(&self, type_name: &str) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .trait_impls
            .iter()
            .filter(|r| r.type_name == type_name)
            .map(|r| r.trait_name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Whether the nominal type `type_name` has a **registered** `impl` of `trait_name` — the
    /// membership test behind a precise `x is dyn Trait` / `x.as<dyn Trait>()`. Reads the same
    /// [`Self::trait_impls`] table `traits_of` surfaces, so the two can never disagree.
    pub fn type_implements(&self, type_name: &str, trait_name: &str) -> bool {
        self.trait_impls
            .iter()
            .any(|r| r.type_name == type_name && r.trait_name == trait_name)
    }

    /// Classify a **declared** nominal name by its kind, carrying `args` through. The one half of
    /// the type-ref projection that needs the type registry — everything else is shared with
    /// [`typeref_to_repr`] via [`named_repr`].
    fn nominal_repr(&self, name: &str, args: Vec<TypeRepr>) -> TypeRepr {
        match self.type_named(name).map(|t| t.kind) {
            Some(TypeKind::Struct) => TypeRepr::Struct(name.to_string(), args),
            Some(TypeKind::Class) => TypeRepr::Class(name.to_string(), args),
            Some(TypeKind::Enum) => TypeRepr::Enum(name.to_string(), args),
            None => TypeRepr::Named(name.to_string(), args),
        }
    }
}

/// One declared trait implementation: `type_name implements trait_name`.
///
/// `type_name` is the implementing type's **runtime tag**: the linked declaration name a value's
/// shape carries (qualified for a namespaced module's type, bare for an entry-file type), or a
/// native type's qualified identity (`std.p2p.GCounter`) for an ABI-advertised impl — exactly the
/// name `runtime_matches`/`narrow_matches` compare a nominal narrowing against.
///
/// `trait_name` is the trait's **canonical identity**: the linked `.noe` trait name (qualified by
/// the loader for a namespaced module's trait), a native [`ExtTrait`]'s qualified `namespace.name`
/// identity (`std.vec.Kernels` — a local `use` alias is resolved through the program's own `use`
/// statements at [`build`] time), or a built-in trait's bare name (`Comparable`). This is the same
/// identity a lowered `dyn Trait` narrowing target resolves to, so the membership test is a string
/// comparison with no second normalization pass.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TraitImplRecord {
    pub type_name: String,
    pub trait_name: String,
}

/// The registry projection of **native** trait data [`build`] joins with a program's own
/// declarations — plain data because this crate cannot see the extension registry (the same seam
/// as `native_roles`). Assembled by `noeta_ir::native_trait_impls` from the lowering's registry;
/// `Default::default()` is the pure-`.noe` path (byte-identical result to before it existed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NativeTraitImpls {
    /// Every registered native [`ExtTrait`]'s qualified identity (`std.vec.Kernels`), so a local
    /// `use`-bound spelling (`Kernels`, `vec.Kernels`) written in an `impl`/`@derive` resolves to
    /// the one identity an implementing value's membership is keyed on.
    pub traits: Vec<String>,
    /// Native type advertisements: each native type's qualified identity paired with the trait
    /// names its ABI declares (`ExtType::traits` / `ExtFielded::traits` / `ExtEnum::traits`, as
    /// written — a short or qualified native-trait name, or a built-in trait name).
    pub type_impls: Vec<(String, Vec<String>)>,
    /// Every native **derive recipe**'s name (`ExtDerive` — `@derive(Inspect)`). A recipe
    /// synthesizes methods but implements no trait, so a derive naming one is *excluded* from the
    /// membership table (every other derive a runnable program carries names a real trait — the
    /// checker rejected the rest).
    pub derives: Vec<String>,
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
/// materializes each into a `List<ParamInfo>` (each `{ name: string, type: Type, optional: bool }`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ParamRecord {
    /// The callable's target: a top-level fn's bare name, or a method's qualified `Type.method` name.
    pub target: String,
    /// The declared parameters, in source order.
    pub params: Vec<ParamSig>,
}

/// One declared parameter — its name, the reflection [`TypeRepr`] of its annotated type, and
/// whether it is optional. An unannotated parameter's type is [`TypeRepr::Dyn`]. `params_of()`
/// materializes each into a `ParamInfo { name: string, type: Type, optional: bool }` whose `type` is
/// the `Type` ADT value `type_of` builds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ParamSig {
    pub name: String,
    pub ty: TypeRepr,
    /// Whether a call may leave this parameter unsupplied — [`crate::Param::is_optional`], i.e. the
    /// parameter declared a default. Deliberately a *flag*, not the default value: a default is an
    /// arbitrary [`crate::Expr`], so surfacing the value would drag const evaluation into the
    /// reflection manifest, and optionality is the fact a signature-driven consumer actually needs
    /// (a CLI framework mapping required parameters to positional arguments and optional ones to
    /// flags; a router splitting required from optional query parameters).
    pub optional: bool,
}

/// The declaration base a param record's target keys on for latest-wins purging: the type name
/// before the `.` of a `Type.method` target, or the whole name for a bare top-level fn.
fn param_base(target: &str) -> &str {
    target.split_once('.').map(|(ty, _)| ty).unwrap_or(target)
}

/// The separator between a callable's target and one of its **parameters** in an attribute-manifest
/// key: `Tools.build#target`, `build#release`.
///
/// Deliberately *not* a third `.`. A parameter attribute needs three components where the rest of
/// the manifest needs two, and the existing two-component form is already ambiguous under a third:
/// `build.target` would read equally as "parameter `target` of the free function `build`" and as
/// "method `target` of the type `build`", with no way for a reader to choose. Every consumer that
/// splits a target — [`param_base`] here, and the `para/aether` package, which takes `parts[0]` /
/// `parts[1]` off a naive `split(".")` — would have silently picked one reading.
///
/// A distinct separator makes the key **self-describing** instead: the presence of a `#` *is* the
/// statement "this target names a parameter", so one rule ([`split_param_attr_target`]) decides it
/// for every reader, and the dotted rule that came before is left meaning exactly what it always
/// meant. Nothing that splits on `.` today changes behaviour, because no key it can see grew a
/// component.
pub const PARAM_ATTR_SEP: char = '#';

/// The attribute-manifest key of one **parameter** of `callable` — the write half of the rule
/// [`split_param_attr_target`] reads. `callable` is the target the parameter's callable is already
/// known by (a bare fn name, or a qualified `Type.method`), so a parameter key extends its
/// callable's key rather than being spelled independently of it.
pub fn param_attr_target(callable: &str, param: &str) -> String {
    format!("{callable}{PARAM_ATTR_SEP}{param}")
}

/// Split a manifest target back into `(callable, parameter)`, or `None` if it does not name a
/// parameter — the read half of [`param_attr_target`], and the **only** place a target string is
/// interpreted as naming a parameter. Both the `params_of` join and the latest-wins purge go
/// through it, so "what a parameter key looks like" cannot come to mean two things.
pub fn split_param_attr_target(target: &str) -> Option<(&str, &str)> {
    target.split_once(PARAM_ATTR_SEP)
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
    /// Each field's declared type as a reflection [`TypeRepr`], parallel to `fields` (empty for
    /// enums). Captured from the DECLARATION, so — unlike the runtime-erased `type_of` on a value —
    /// it is precise (a `List<int>` field is `TypeRepr::List(Int)`, not `List(Dyn)`). An unannotated
    /// field is [`TypeRepr::Dyn`]. Surfaced by the type-level `field_specs_of` query; the value-level
    /// `fields_of` deliberately does not use it (it reflects values, not declared types).
    pub field_types: Vec<TypeRepr>,
    /// Whether each field declared a default, parallel to `fields` (empty for enums). `true` iff
    /// [`FieldDecl::default`] is `Some` — i.e. the runtime can fill the field when it is omitted, the
    /// exact condition a dynamic `construct` uses to decide a missing field is allowed. Distinct from
    /// `field_defaults` below, which carries only the *literal* subset an attribute materializer can
    /// fold; this flag covers any default expression.
    pub field_optional: Vec<bool>,
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

/// Build the reflection info for a program. **Pure and deterministic**: the same AST + `native_roles`
/// always yields the same [`ReflectionInfo`], so both backends calling this on the same [`Program`]
/// agree without any cross-backend coordination — the property the differential oracle depends on.
///
/// `native_roles` is the plain-data projection of any **native** `@role`-bearing `@attribute` structs
/// (native type-declaration unification, Slice D3): `(attribute FQN, [(enum, variant)])` pairs a caller
/// assembles from the registry via [`crate`]-external `Registry::native_roles`. This crate cannot see
/// the extension registry, so a native role reaches the join as this table. Merged into the same
/// `role_of` the `.noe` `@role` tags populate, keyed by the **qualified** attribute identity a linked
/// native attribute application carries — so an in-program application of a native role-bearing
/// attribute confers the role exactly as a `.noe` one does. Pass `&[]` for the pure `.noe` path; the
/// result is then byte-identical to before this parameter existed.
/// `native_traits` is the same plain-data seam for **trait membership** (the precise `is dyn
/// Trait` test and `traits_of`): [`collect_trait_impls`] joins it with the program's own
/// `impl`/`@derive` declarations. Pass `&NativeTraitImpls::default()` for the registry-less path —
/// the program's own declarations are still recorded.
pub fn build(
    program: &Program,
    native_roles: &[(String, Vec<(String, String)>)],
    native_traits: &NativeTraitImpls,
) -> ReflectionInfo {
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
                push_attrs(
                    &mut manifest,
                    &decl.name,
                    decl.name_span,
                    &decl.decorators.attrs,
                );
                push_field_attrs(&mut manifest, &decl.name, &decl.fields);
                // A role tag rides on the attribute struct; record each (validated) `Enum.Variant`
                // so every declaration the attribute annotates inherits it. A malformed `@role`
                // never reaches a runnable program (the checker rejects it).
                if let Some(roles) = decl.decorators.role.as_ref() {
                    for tag in roles {
                        role_of.push((
                            decl.name.clone(),
                            (tag.enum_name.clone(), tag.variant.clone()),
                        ));
                    }
                }
                for method in &decl.methods {
                    push_params(
                        &mut manifest,
                        &mut params,
                        format!("{}.{}", decl.name, method.name),
                        &method.params,
                    );
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Struct,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    field_types: field_types(&decl.fields),
                    field_optional: field_optional(&decl.fields),
                    field_defaults: field_defaults(&decl.fields),
                    variants: Vec::new(),
                });
            }
            Stmt::Class(decl) => {
                push_attrs(
                    &mut manifest,
                    &decl.name,
                    decl.name_span,
                    &decl.decorators.attrs,
                );
                push_field_attrs(&mut manifest, &decl.name, &decl.fields);
                // A method's attributes are keyed by its qualified `Class.method` name, so a
                // `#[...]` on a method surfaces distinctly from the same name on another class.
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, method.name_span, &method.attrs);
                    push_params(&mut manifest, &mut params, target, &method.params);
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Class,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    field_types: field_types(&decl.fields),
                    field_optional: field_optional(&decl.fields),
                    field_defaults: field_defaults(&decl.fields),
                    variants: Vec::new(),
                });
            }
            // A top-level function carries attributes too (keyed by its bare name); it is not a
            // declared *type*, so it contributes to the manifest only, not the type registry.
            Stmt::Fn(decl) => {
                // `FnDecl` keeps its own `attrs` — only the four *type* declaration kinds moved
                // their decorators into `Decorators`. A `fn` carries `#[...]` attributes and a
                // `@tier(...)` declaration, neither of which is a type decorator.
                push_attrs(&mut manifest, &decl.name, decl.name_span, &decl.attrs);
                push_params(&mut manifest, &mut params, decl.name.clone(), &decl.params);
            }
            // A trait carries `#[...]` data attributes keyed by its name (UT6), like a type —
            // surfaced via `attributes_of` (and inheriting a role transitively when annotated with a
            // role-bearing attribute). It is not a data type, so it adds no `TypeInfo`; its abstract
            // method signatures are not scanned (route/metadata attributes live on the concrete
            // `impl` methods, scanned via the class/struct arms). A direct `@role`/`@derive`/… on a
            // trait is a checker error, so a runnable program never carries one here.
            Stmt::Trait(decl) => {
                push_attrs(
                    &mut manifest,
                    &decl.name,
                    decl.name_span,
                    &decl.decorators.attrs,
                );
                // A trait's abstract method signatures carry declared parameters too, keyed by the
                // `Trait.method` convention — surfaced via `params_of` like a concrete method's.
                for method in &decl.methods {
                    push_params(
                        &mut manifest,
                        &mut params,
                        format!("{}.{}", decl.name, method.sig.name),
                        &method.sig.params,
                    );
                }
            }
            Stmt::Enum(decl) => {
                push_attrs(
                    &mut manifest,
                    &decl.name,
                    decl.name_span,
                    &decl.decorators.attrs,
                );
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
                    push_params(&mut manifest, &mut params, target, &method.params);
                }
                types.push(TypeInfo {
                    name: decl.name.clone(),
                    kind: TypeKind::Enum,
                    fields: Vec::new(),
                    field_types: Vec::new(),
                    field_optional: Vec::new(),
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
    // Native `@role`-bearing attributes (Slice D3): merge the registry-assembled tags into `role_of`
    // keyed by the attribute's qualified identity — the identity a linked native attribute application
    // carries in the manifest — so the join below treats a native role-bearing attribute exactly like a
    // `.noe` one. Empty for the pure `.noe` path (byte-identical result).
    for (attr, tags) in native_roles {
        for (enum_name, variant) in tags {
            role_of.push((attr.clone(), (enum_name.clone(), variant.clone())));
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
        trait_impls: collect_trait_impls(program, native_traits),
    }
}

/// Build the **trait-membership table** ([`ReflectionInfo::trait_impls`]) — every `(type, trait)`
/// pair the program registers an implementation for, from the same declarations that make trait
/// dispatch resolve:
///
/// - a standalone `impl Trait for T { … }`,
/// - an in-body `impl Trait { … }` block on a struct/class/enum,
/// - a `@derive(Trait)` (built-in and user traits alike; a native *derive recipe* is excluded —
///   it synthesizes methods but implements no trait),
/// - a native type's ABI-advertised impls (`native.type_impls`, keyed by qualified identity).
///
/// Trait names are canonicalized: a native trait's local `use` spelling (`Kernels`,
/// `vec.Kernels`) resolves to its qualified identity (`std.vec.Kernels`) through the program's own
/// `use` statements; every other name is recorded as the linked AST spells it (the loader already
/// qualified a namespaced module's traits, and a built-in trait is its bare name). Pure and
/// deterministic like the rest of [`build`], so both backends agree by construction.
fn collect_trait_impls(program: &Program, native: &NativeTraitImpls) -> Vec<TraitImplRecord> {
    use crate::ImplBlock;
    // Local spelling → canonical native-trait identity, from the program's `use` statements: a
    // leaf import (`use std.vec.Kernels [as K]`) binds its local name; a module/namespace import
    // (`use std.vec`) binds the dotted projection (`vec.Kernels`).
    let mut aliases: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for stmt in &program.stmts {
        let Stmt::Use { path, names, .. } = stmt else {
            continue;
        };
        let prefix = path.join(".");
        for n in names {
            let local = n.local();
            let qualified = format!("{prefix}.{}", n.name);
            for q in &native.traits {
                if *q == qualified {
                    aliases.insert(local.to_string(), q);
                } else if let Some(rest) = q.strip_prefix(&qualified)
                    && let Some(short) = rest.strip_prefix('.')
                    && !short.contains('.')
                {
                    aliases.insert(format!("{local}.{short}"), q);
                }
            }
        }
    }
    let canon = |name: &str| -> String {
        aliases
            .get(name)
            .map(|q| (*q).to_string())
            .unwrap_or_else(|| name.to_string())
    };
    let mut records: Vec<TraitImplRecord> = Vec::new();
    fn push(records: &mut Vec<TraitImplRecord>, type_name: &str, trait_name: String) {
        let record = TraitImplRecord {
            type_name: type_name.to_string(),
            trait_name,
        };
        if !records.contains(&record) {
            records.push(record);
        }
    }
    let is_recipe = |name: &str| native.derives.iter().any(|d| d == name);
    let body = |records: &mut Vec<TraitImplRecord>,
                type_name: &str,
                impls: &[ImplBlock],
                derives: &[crate::DeriveSpec]| {
        for block in impls {
            push(records, type_name, canon(&block.trait_name));
        }
        for spec in derives {
            // A native derive recipe implements no trait; every other derive a runnable program
            // carries names a real trait (built-in, user, or native — the checker gated the rest).
            if !is_recipe(&spec.name) {
                push(records, type_name, canon(&spec.name));
            }
        }
    };
    for stmt in &program.stmts {
        match stmt {
            Stmt::Impl(decl) => push(&mut records, &decl.target, canon(&decl.trait_name)),
            Stmt::Struct(d) => body(&mut records, &d.name, &d.impls, &d.decorators.derives),
            Stmt::Class(d) => body(&mut records, &d.name, &d.impls, &d.decorators.derives),
            Stmt::Enum(d) => body(&mut records, &d.name, &d.impls, &d.decorators.derives),
            _ => {}
        }
    }
    // Native advertisements: an ABI-declared name resolves against the registered native traits
    // (exact qualified spelling, or a unique short name); anything else — a built-in trait name
    // like `"Comparable"`/`"Mergeable"` — is recorded as written.
    //
    // Each row is keyed by the native type's QUALIFIED identity (what an extern value's
    // `type_identity()` reports) — and ALSO by its short name when no `.noe` declaration in this
    // program claims it, because a native fielded/enum instance's runtime shape carries whatever
    // name the extension materialized it under (often the short name). The declared-name guard
    // keeps a user type from inheriting a same-short-named native type's advertisements.
    let declared: std::collections::HashSet<&str> = program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Struct(d) => Some(d.name.as_str()),
            Stmt::Class(d) => Some(d.name.as_str()),
            Stmt::Enum(d) => Some(d.name.as_str()),
            _ => None,
        })
        .collect();
    for (type_name, advertised) in &native.type_impls {
        for name in advertised {
            let canonical = native
                .traits
                .iter()
                .find(|q| *q == name || q.rsplit('.').next() == Some(name))
                .cloned()
                .unwrap_or_else(|| name.clone());
            push(&mut records, type_name, canonical.clone());
            if let Some(short) = type_name.rsplit('.').next()
                && short != type_name
                && !declared.contains(short)
            {
                push(&mut records, short, canonical);
            }
        }
    }
    records
}

/// Project a callable's declared parameters onto their reflection [`ParamSig`]s — each parameter's
/// name paired with the [`TypeRepr`] of its annotated type (an unannotated parameter is
/// [`TypeRepr::Dyn`]) and its optionality. Shared by every callable arm of [`build`] so a fn,
/// method, and trait method sig all surface their parameters identically.
/// Record one callable's parameters — **both** of the renderings they have, from one walk.
///
/// A callable's parameter list reaches reflection twice: as the [`ParamRecord`] `params_of(target)`
/// materializes, and as attribute-manifest rows keyed [`param_attr_target`] so `attributes_of::<T>()`
/// discovers a parameter attribute exactly as it discovers one on a field or a method. They are one
/// fact, so one function emits both: there is no parameter list that can appear in the manifest but
/// not in `params_of`, and no key the two sides could spell differently.
///
/// The `params_of` side then *joins back* on the manifest at materialization time rather than
/// carrying its own copy of the attributes — see `ParamSig`, which is deliberately unchanged. That
/// is what keeps the two renderings a projection of one table instead of two tables that must be
/// kept in step.
fn push_params(
    manifest: &mut Vec<AttributeRecord>,
    params: &mut Vec<ParamRecord>,
    target: String,
    decls: &[crate::Param],
) {
    for p in decls {
        push_attrs(
            manifest,
            &param_attr_target(&target, &p.name),
            p.name_span,
            &p.attrs,
        );
    }
    params.push(ParamRecord {
        target,
        params: param_sigs(decls),
    });
}

fn param_sigs(params: &[crate::Param]) -> Vec<ParamSig> {
    params
        .iter()
        .map(|p| ParamSig {
            name: p.name.clone(),
            ty: p.ty.as_ref().map(typeref_to_repr).unwrap_or(TypeRepr::Dyn),
            // Through `Param::is_optional`, not an open-coded `default.is_some()`: reflection must
            // report the same optionality the checker's arity rule enforces.
            optional: p.is_optional(),
        })
        .collect()
}

/// Each field's declared type as a reflection [`TypeRepr`], parallel to the field list — an
/// unannotated field is [`TypeRepr::Dyn`]. The type-level twin of [`param_sigs`]'s type capture, so
/// a field schema and a parameter schema report a declared type the same way.
fn field_types(fields: &[FieldDecl]) -> Vec<TypeRepr> {
    fields
        .iter()
        .map(|f| f.ty.as_ref().map(typeref_to_repr).unwrap_or(TypeRepr::Dyn))
        .collect()
}

/// Whether each field declared a default, parallel to the field list — the optionality a dynamic
/// constructor reads. Any default expression counts (not only a literal one), matching the runtime
/// default thunks both backends compile per field.
fn field_optional(fields: &[FieldDecl]) -> Vec<bool> {
    fields.iter().map(|f| f.default.is_some()).collect()
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
///
/// **The one definition of "a literal default"**, so every consumer draws the fillable/required line
/// in the same place: [`TypeInfo::field_defaults`] (what an attribute materializer folds) and the
/// checker's `type_to_recipe` (what a JSON decode bakes into a `FieldDefault::Literal`) both call it.
pub fn fold_const_expr(expr: &Expr) -> Option<AttrValue> {
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

/// One field of a type-level schema — the borrowed view [`ReflectionInfo::field_specs`] returns,
/// which both backends materialize into a prelude `FieldSpec { name, type, optional }` value.
#[derive(Debug)]
pub struct FieldSpecData<'a> {
    pub name: &'a str,
    pub ty: &'a TypeRepr,
    pub optional: bool,
}

/// A concrete-scalar field type as a friendly lowercase name (`int`/`float`/`bool`/`string`/`bytes`),
/// or `None` when the field type is not an enforced scalar — a `dyn`, a collection, or a nominal
/// type, all of which a dynamic [`construct`](fn@crate::reflect::plan_construct) accepts without a
/// runtime type-check (the callee's own typing is the backstop, mirroring how `para/cli`'s `coerce`
/// passes an unknown type through). Widths erase: `iN`/`f32`/`f64` check as `int`/`float`, matching
/// how a runtime scalar value classifies.
fn enforced_scalar(ty: &TypeRepr) -> Option<&'static str> {
    match ty {
        TypeRepr::Int | TypeRepr::IntN { .. } => Some("int"),
        TypeRepr::Float | TypeRepr::F32 | TypeRepr::F64 => Some("float"),
        TypeRepr::Bool => Some("bool"),
        TypeRepr::Str => Some("string"),
        TypeRepr::Bytes => Some("bytes"),
        _ => None,
    }
}

/// The friendly lowercase name of a value's runtime head-repr, for a construct type-mismatch
/// message. Computed from the [`TypeRepr`] both backends' classifiers agree on, so the message is
/// byte-identical across the differential (rather than each backend's own `type_name`).
fn value_repr_name(ty: &TypeRepr) -> String {
    match enforced_scalar(ty) {
        Some(name) => name.to_string(),
        None => match ty {
            TypeRepr::Str => "string".to_string(),
            TypeRepr::List(_) => "list".to_string(),
            TypeRepr::Set(_) => "set".to_string(),
            TypeRepr::Map(_, _) => "map".to_string(),
            TypeRepr::Option(_) => "option".to_string(),
            TypeRepr::Unit => "unit".to_string(),
            TypeRepr::Enum(n, _)
            | TypeRepr::Struct(n, _)
            | TypeRepr::Class(n, _)
            | TypeRepr::Named(n, _) => n.clone(),
            other => other.variant_name().to_lowercase(),
        },
    }
}

/// Plan a dynamic struct construction: validate a positional value list (in declaration order)
/// against a type's field `specs`, given each supplied value's runtime head-repr `value_reprs` (which
/// both backends compute with their own `type_of` classifier — the differential guarantees they
/// agree, so this shared decision yields identical outcomes and identical error strings). On success
/// returns the field name for each slot to fill — the pairing a backend feeds into its existing
/// struct-literal construction path — leaving every omitted-but-defaulted field for that path to
/// fill from its default thunk. Errors (returned as a ready-to-surface message, no leading `error:`):
///   * more values than fields;
///   * a value whose runtime scalar kind disagrees with a concrete-scalar field type;
///   * a missing field that declared no default.
///
/// Because a missing field is rejected here unless it is optional, the construction path this feeds
/// never hits its own missing-field abort — so the two error surfaces do not overlap.
pub fn plan_construct<'a>(
    type_name: &str,
    specs: &[FieldSpecData<'a>],
    value_reprs: &[TypeRepr],
) -> Result<Vec<&'a str>, String> {
    if value_reprs.len() > specs.len() {
        return Err(format!(
            "`{type_name}` has {} field(s), but {} value(s) were given",
            specs.len(),
            value_reprs.len()
        ));
    }
    let mut fill: Vec<&str> = Vec::new();
    for (i, spec) in specs.iter().enumerate() {
        if i < value_reprs.len() {
            if let Some(expected) = enforced_scalar(spec.ty) {
                let got = value_repr_name(&value_reprs[i]);
                if got != expected {
                    return Err(format!(
                        "field `{}` of `{type_name}` expects {expected}, got {got}",
                        spec.name
                    ));
                }
            }
            fill.push(spec.name);
        } else if !spec.optional {
            return Err(format!(
                "missing required field `{}` of `{type_name}`",
                spec.name
            ));
        }
    }
    Ok(fill)
}

/// Validate a **named** dynamic construction: a set of `provided` field values, each `(field name,
/// runtime value head-repr)`, against a type's field `specs`. The named counterpart of
/// [`plan_construct`] — the form a framework uses when it binds fields by name (a CLI expanding a
/// struct parameter into `--field` flags, which arrive sparsely and in any order). Unlike the
/// positional form there is no gap problem: a field is supplied or it is not, so a middle field can
/// be omitted while a later one is supplied. Errors (ready-to-surface messages):
///   * a provided name that is not a field of the type;
///   * a provided value whose runtime scalar kind disagrees with a concrete-scalar field type;
///   * a field that is neither provided nor defaulted.
///
/// On success the caller builds the object from the provided `(name, value)` pairs; the construction
/// path fills every unprovided field from its default (this validated that each such field has one).
pub fn plan_construct_named(
    type_name: &str,
    specs: &[FieldSpecData<'_>],
    provided: &[(String, TypeRepr)],
) -> Result<(), String> {
    for (name, repr) in provided {
        let Some(spec) = specs.iter().find(|s| s.name == name) else {
            return Err(format!("`{type_name}` has no field `{name}`"));
        };
        if let Some(expected) = enforced_scalar(spec.ty) {
            let got = value_repr_name(repr);
            if got != expected {
                return Err(format!(
                    "field `{name}` of `{type_name}` expects {expected}, got {got}"
                ));
            }
        }
    }
    for spec in specs {
        let supplied = provided.iter().any(|(n, _)| n == spec.name);
        if !supplied && !spec.optional {
            return Err(format!(
                "missing required field `{}` of `{type_name}`",
                spec.name
            ));
        }
    }
    Ok(())
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
    /// The explicit 64-bit float `f64` (packed-widths arc; variant name `F64`). A runtime *scalar*
    /// `f64` is bit-identical to `float` and reflects as `Float`; this variant appears where the
    /// width is physically reified — a packed list element or a declared-type reflection — so
    /// `List<f64>` is distinguishable from `List<float>` while their equal elements stay `==`.
    F64,
    /// A fixed-width integer `i8..i64`/`u8..u64` (packed-widths arc; variant name `IntN`). Like
    /// [`TypeRepr::F64`], a runtime *scalar* is erased to `Int` (Tier W) and reflects as `Int`; this
    /// variant carries the width where it is reified — a packed list element or a declared type — so
    /// `List<i32>` is distinguishable from `List<int>` while equal elements stay `==`.
    IntN {
        /// `true` for the `iN` family, `false` for `uN`.
        signed: bool,
        /// One of 8, 16, 32, 64.
        bits: u8,
    },
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
            // The scalars have no arguments. Exhaustive on purpose (no `_`): a new `TypeRepr`
            // variant must decide here whether the R3 matcher sees into it, rather than silently
            // inheriting "no arguments" — the same guard every `BuiltinTy` site carries.
            TypeRepr::Int
            | TypeRepr::Float
            | TypeRepr::F32
            | TypeRepr::F64
            | TypeRepr::IntN { .. }
            | TypeRepr::Bool
            | TypeRepr::Str
            | TypeRepr::Bytes
            | TypeRepr::Unit
            | TypeRepr::Dyn => Vec::new(),
            // A trait object's trait name is identity, not a type argument — `arg_matches`
            // compares it by name in its own arm.
            TypeRepr::DynTrait(_) => Vec::new(),
            // A function type's parameters/return and a union's members are structural
            // components, not R3 type arguments: neither is a narrowable tagged container, and
            // `arg_matches` handles unions member-wise in its own arms. Deliberately empty.
            TypeRepr::Fn(_, _) | TypeRepr::Union(_) => Vec::new(),
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
            TypeRepr::F64 => "F64",
            TypeRepr::IntN { .. } => "IntN",
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

    /// The **payload shape** of the `Type.*` variant this descriptor constructs. Paired with
    /// [`variant_name`](Self::variant_name) it is the full declaration of one prelude-enum
    /// variant, which is what the checker registers and what both backends materialize.
    pub fn adt_fields(&self) -> AdtFields {
        match self {
            TypeRepr::Int
            | TypeRepr::Float
            | TypeRepr::F32
            | TypeRepr::F64
            | TypeRepr::Bool
            | TypeRepr::Str
            | TypeRepr::Bytes
            | TypeRepr::Unit
            | TypeRepr::Dyn => AdtFields::None,
            // The fixed-width integer carries its `(bits, signed)` so a reflected `Type.IntN`
            // reports exactly which width it is (matched structurally by narrowing regardless).
            TypeRepr::IntN { .. } => AdtFields::IntWidth,
            TypeRepr::List(_) | TypeRepr::Set(_) | TypeRepr::Option(_) => AdtFields::Types(1),
            TypeRepr::Map(_, _) | TypeRepr::Result(_, _) => AdtFields::Types(2),
            TypeRepr::Enum(_, _)
            | TypeRepr::Struct(_, _)
            | TypeRepr::Class(_, _)
            | TypeRepr::Named(_, _) => AdtFields::NameAndArgs,
            TypeRepr::Fn(_, _) => AdtFields::ParamsAndRet,
            TypeRepr::Union(_) => AdtFields::TypeList,
            TypeRepr::DynTrait(_) => AdtFields::Name,
        }
    }
}

/// The payload shape of one `Type.*` prelude-enum variant — the closed vocabulary of field lists
/// the ADT uses, so the checker's registration is a projection of [`TypeRepr::adt_fields`] rather
/// than a hand-maintained parallel table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdtFields {
    /// No payload (the scalars).
    None,
    /// `n` recursive `Type` fields — a container's element / key / value types.
    Types(usize),
    /// `(name: string, args: List<Type>)` — the nominal variants.
    NameAndArgs,
    /// `(members: List<Type>)` — a union's members.
    TypeList,
    /// `(params: List<Type>, ret: Type)` — a function type.
    ParamsAndRet,
    /// `(name: string)` — a trait object's trait name.
    Name,
    /// `(bits: int, signed: bool)` — a fixed-width integer's width descriptor (packed-widths arc).
    IntWidth,
}

/// One sample [`TypeRepr`] per variant. Running each through the exhaustive
/// [`TypeRepr::variant_name`] / [`TypeRepr::adt_fields`] matches yields the prelude `Type` enum's
/// full declaration — so the checker registers the ADT from the reflection descriptor itself
/// instead of re-listing the variants and their arities in a table that can drift.
///
/// **Adding a [`TypeRepr`] variant**: both matches above will fail to compile until you handle it;
/// add its sample here at the same time, or the prelude enum will silently lack the variant.
pub fn type_adt_variants() -> Vec<TypeRepr> {
    let any = || Box::new(TypeRepr::Dyn);
    let name = || String::new();
    vec![
        TypeRepr::Int,
        TypeRepr::Float,
        TypeRepr::F32,
        TypeRepr::F64,
        TypeRepr::IntN {
            signed: true,
            bits: 32,
        },
        TypeRepr::Bool,
        TypeRepr::Str,
        TypeRepr::Bytes,
        TypeRepr::Unit,
        TypeRepr::Dyn,
        TypeRepr::List(any()),
        TypeRepr::Set(any()),
        TypeRepr::Option(any()),
        TypeRepr::Map(any(), any()),
        TypeRepr::Result(any(), any()),
        TypeRepr::Enum(name(), Vec::new()),
        TypeRepr::Struct(name(), Vec::new()),
        TypeRepr::Class(name(), Vec::new()),
        TypeRepr::Named(name(), Vec::new()),
        TypeRepr::Fn(Vec::new(), any()),
        TypeRepr::Union(Vec::new()),
        TypeRepr::DynTrait(name()),
    ]
}

/// A [`TypeRepr`] displays as its **Noeta surface spelling** — the source form a developer
/// recognizes: scalars by keyword (`int`, `string`, `void`), containers as `List<T>` / `Map<K, V>`,
/// optionals as `?T`, unions as `A | B`, function types as `(A, B) -> R`, and nominal types by name
/// with `<…>` type arguments. Shared by every human-facing surface (LSP hover, the debugger's
/// Variables view) so a type reads the same everywhere.
impl std::fmt::Display for TypeRepr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The fully-qualified nominal name, verbatim — the disambiguating form hover and the
        // debugger want.
        self.fmt_spelling(f, &|name| name)
    }
}

impl TypeRepr {
    /// The surface spelling with every nominal name shortened to its final `.`-segment — the
    /// in-scope short name a developer actually wrote (`geometry.vec.Vec2` → `Vec2`, recursively
    /// through type arguments: `List<geometry.vec.Vec2>` → `List<Vec2>`). Inlay type hints use this
    /// because they sit right next to source that already spells the type by its imported short
    /// name; [`Display`](std::fmt::Display) keeps the fully-qualified form for hover/debugger, where
    /// there is no adjacent code to disambiguate against.
    pub fn display_short(&self) -> String {
        struct Short<'a>(&'a TypeRepr);
        impl std::fmt::Display for Short<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0
                    .fmt_spelling(f, &|name| name.rsplit('.').next().unwrap_or(name))
            }
        }
        Short(self).to_string()
    }

    /// Render the Noeta surface spelling, mapping every nominal name (nominal types and `dyn Trait`)
    /// through `name`. The single source of truth for [`Display`] (identity mapping) and
    /// [`display_short`](Self::display_short) (final-segment mapping); all container/generic/function
    /// nesting recurses through here so the mapping reaches type arguments too.
    fn fmt_spelling(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        name: &dyn Fn(&str) -> &str,
    ) -> std::fmt::Result {
        // Comma-join a nested type list (`<…>` arguments, `(…)` parameters) under the same mapping.
        let join = |f: &mut std::fmt::Formatter<'_>, types: &[TypeRepr]| -> std::fmt::Result {
            for (i, t) in types.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                t.fmt_spelling(f, name)?;
            }
            Ok(())
        };
        match self {
            TypeRepr::Int => f.write_str("int"),
            TypeRepr::Float => f.write_str("float"),
            TypeRepr::F32 => f.write_str("f32"),
            TypeRepr::F64 => f.write_str("f64"),
            TypeRepr::IntN { signed, bits } => {
                write!(f, "{}{bits}", if *signed { 'i' } else { 'u' })
            }
            TypeRepr::Bool => f.write_str("bool"),
            TypeRepr::Str => f.write_str("string"),
            TypeRepr::Bytes => f.write_str("bytes"),
            TypeRepr::Unit => f.write_str("void"),
            TypeRepr::Dyn => f.write_str("dyn"),
            TypeRepr::DynTrait(t) => write!(f, "dyn {}", name(t)),
            TypeRepr::List(t) => {
                f.write_str("List<")?;
                t.fmt_spelling(f, name)?;
                f.write_str(">")
            }
            TypeRepr::Set(t) => {
                f.write_str("Set<")?;
                t.fmt_spelling(f, name)?;
                f.write_str(">")
            }
            TypeRepr::Option(t) => {
                f.write_str("?")?;
                // A nested optional needs the space: `??` lexes as the null-coalescing operator,
                // so `??int` is not a writable annotation — `? ?int` is (the spelling round-trip
                // oracle caught the bare form failing to re-parse).
                if matches!(**t, TypeRepr::Option(_)) {
                    f.write_str(" ")?;
                }
                t.fmt_spelling(f, name)
            }
            TypeRepr::Map(k, v) => {
                f.write_str("Map<")?;
                k.fmt_spelling(f, name)?;
                f.write_str(", ")?;
                v.fmt_spelling(f, name)?;
                f.write_str(">")
            }
            TypeRepr::Result(o, e) => {
                f.write_str("Result<")?;
                o.fmt_spelling(f, name)?;
                f.write_str(", ")?;
                e.fmt_spelling(f, name)?;
                f.write_str(">")
            }
            TypeRepr::Enum(n, args)
            | TypeRepr::Struct(n, args)
            | TypeRepr::Class(n, args)
            | TypeRepr::Named(n, args) => {
                f.write_str(name(n))?;
                if !args.is_empty() {
                    f.write_str("<")?;
                    join(f, args)?;
                    f.write_str(">")?;
                }
                Ok(())
            }
            TypeRepr::Fn(params, ret) => {
                f.write_str("(")?;
                join(f, params)?;
                f.write_str(") -> ")?;
                ret.fmt_spelling(f, name)
            }
            TypeRepr::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    m.fmt_spelling(f, name)?;
                }
                Ok(())
            }
        }
    }
}

/// Project a surface [`TypeRef`] onto a reflection [`TypeRepr`], **without kind information** (runtime
/// type-argument reflection, R3): a declared `struct`/`class`/`enum` maps to [`TypeRepr::Named`]
/// (the R3 matcher keys on the name, not the kind, so `Named("Box")` matches a value tagged
/// `Struct("Box")`). Built-in scalars/collections map to their lattice variant; a `?T` is `Option<T>`.
/// Used to turn a narrow target (`x is List<int>`) into the shape compared against a value's tag.
/// The [`TypeRepr`] of a built-in type constructor, reading its type arguments through `arg`.
/// `None` for the three abstract kind-types (`Enum`/`Struct`/`Class`), which have no reflection
/// descriptor of their own — no *value* is an `Enum`, so each caller resolves them through its own
/// nominal fallback (kind lookup / `TypeRepr::Named`).
///
/// Shared by [`ReflectionInfo::type_ref_repr`] and [`typeref_to_repr`]: they agree on the whole
/// vocabulary and differ only in what `arg` yields — bare `dyn` for the name-only mapper, the real
/// nested `TypeRef` reprs for the structural one.
///
/// A **declared** `f64`/`iN`/`uN` keeps its width here (`TypeRepr::F64`/`TypeRepr::IntN`), the
/// physically-meaningful reflection for a type annotation and a narrow target (packed-widths arc).
/// A runtime *scalar value* of one of these still erases to `Float`/`Int` (no boxing site to stamp
/// a width tag on), so `type_of` of a scalar reports the lattice variant — the deliberate split
/// between declared-type reflection (width-carrying) and value reflection (width-erased). Both
/// declared-type surfaces (`params_of` and the narrow matcher) route through [`BuiltinTy`] here, so
/// they cannot drift from each other.
fn builtin_repr(
    builtin: BuiltinTy,
    arg: impl Fn(usize) -> Box<TypeRepr>,
    top: bool,
) -> Option<TypeRepr> {
    Some(match builtin {
        BuiltinTy::Int => TypeRepr::Int,
        // A fixed width reifies only in **container-element position**, never as a top-level scalar
        // (packed-widths arc). At the top a declared `i32`/`u8`/`f64` erases to `Int`/`Float` — the
        // rule `params_of` and `type_of` agree on, since a scalar value carries no width tag. As a
        // list/map/option *element* it keeps its width, so `List<i32>` is distinguishable from
        // `List<int>` (the element is a physically distinct storage slot). `arg(_)` recurses in
        // element position, so the width survives at every depth below the top scalar.
        BuiltinTy::IntN { signed, bits } => {
            if top {
                TypeRepr::Int
            } else {
                TypeRepr::IntN { signed, bits }
            }
        }
        BuiltinTy::Float => TypeRepr::Float,
        BuiltinTy::F64 => {
            if top {
                TypeRepr::Float
            } else {
                TypeRepr::F64
            }
        }
        BuiltinTy::F32 => TypeRepr::F32,
        BuiltinTy::Bool => TypeRepr::Bool,
        BuiltinTy::Str => TypeRepr::Str,
        BuiltinTy::Bytes => TypeRepr::Bytes,
        BuiltinTy::Unit => TypeRepr::Unit,
        BuiltinTy::Dyn => TypeRepr::Dyn,
        BuiltinTy::List => TypeRepr::List(arg(0)),
        BuiltinTy::Set => TypeRepr::Set(arg(0)),
        BuiltinTy::Map => TypeRepr::Map(arg(0), arg(1)),
        BuiltinTy::Option => TypeRepr::Option(arg(0)),
        BuiltinTy::Result => TypeRepr::Result(arg(0), arg(1)),
        // `number` is a union of scalars, so it has no single reflected shape — and no *value* ever
        // has it (every number is an `int`, an `f32`, …). Like the abstract kind-types above, it is
        // a static-only type with no reflection descriptor rather than a wrong concrete one.
        BuiltinTy::Number => return None,
        BuiltinTy::KindEnum | BuiltinTy::KindStruct | BuiltinTy::KindClass => return None,
    })
}

/// The **top-level** reflection of a surface type: a bare scalar `i32`/`f64` erases to `Int`/`Float`
/// (declared-scalar erasure), while container elements keep their width. Used for a parameter's or
/// attribute's declared type, and kind-agnostic for nominals (the R3 matcher keys on the name).
pub fn typeref_to_repr(ty: &TypeRef) -> TypeRepr {
    typeref_repr_with(
        ty,
        &|name, args| TypeRepr::Named(name.to_string(), args),
        true,
    )
}

/// The reflection of a surface type already in **element position** — a narrow target's type
/// argument, e.g. the `i32` of `x is List<i32>` (packed-widths arc). A fixed width keeps its width so
/// the target matches a value's width-carrying tag; nominals stay kind-agnostic.
pub fn typeref_to_repr_arg(ty: &TypeRef) -> TypeRepr {
    typeref_repr_with(
        ty,
        &|name, args| TypeRepr::Named(name.to_string(), args),
        false,
    )
}

/// How a projection resolves a **nominal** type name — the one axis on which the two type-ref
/// converters differ. [`typeref_to_repr`] answers [`TypeRepr::Named`] unconditionally (the R3
/// narrow matcher keys on the name and is deliberately kind-tolerant); [`ReflectionInfo::typeref_repr`]
/// looks the name up in the type registry and answers `Struct`/`Class`/`Enum`.
///
/// It is a parameter rather than two copies of the walk because the copies drifted: the kind-aware
/// converter used to be a name-only lookup that dropped its type arguments, so an attribute
/// argument `List<int>` materialized as `Type.List(Type.Dyn)` while the same annotation reflected
/// through `params_of` kept the `int`. Both are now the same walk.
type NominalResolver<'a> = dyn Fn(&str, Vec<TypeRepr>) -> TypeRepr + 'a;

/// The [`TypeRepr`] of a **named** type reference — head name plus generic arguments — resolving
/// nominals through `nominal`. The single place surface generic application becomes a reflection
/// type: a built-in constructor reads its arguments positionally (a missing one is the `Dyn` top,
/// the inference hole the bare `list`/`map` spellings leave), and anything else is nominal with its
/// arguments carried through verbatim. Shared by [`ReflectionInfo::type_ref_repr`] (which has only
/// the name and args, never a whole [`TypeRef`]) and the [`TypeRef::Named`] arm of the walk.
fn named_repr(name: &str, args: &[TypeRef], nominal: &NominalResolver<'_>, top: bool) -> TypeRepr {
    // A type argument is always in element position (`top = false`), so a width inside it survives.
    let arg = |i: usize| match args.get(i) {
        Some(t) => Box::new(typeref_repr_with(t, nominal, false)),
        None => Box::new(TypeRepr::Dyn),
    };
    BuiltinTy::from_name_any(name)
        .and_then(|b| builtin_repr(b, arg, top))
        .unwrap_or_else(|| {
            nominal(
                name,
                args.iter()
                    .map(|a| typeref_repr_with(a, nominal, false))
                    .collect(),
            )
        })
}

/// Walk a surface [`TypeRef`] into a [`TypeRepr`], resolving nominal names through `nominal`. The
/// single converter behind both [`typeref_to_repr`] and [`ReflectionInfo::typeref_repr`]. `top`
/// distinguishes a bare scalar (declared width erases) from an element (width kept) — see
/// [`builtin_repr`]; every recursive position is an element, so `recur` passes `false`.
fn typeref_repr_with(ty: &TypeRef, nominal: &NominalResolver<'_>, top: bool) -> TypeRepr {
    let recur = |t: &TypeRef| typeref_repr_with(t, nominal, false);
    match ty {
        TypeRef::Union { members, .. } => TypeRepr::Union(members.iter().map(recur).collect()),
        TypeRef::Optional { inner, .. } => TypeRepr::Option(Box::new(recur(inner))),
        // A trait object reflects as `DynTrait(name)` — the dynamic top refined by its trait bound, so
        // reflection can recover which trait a parameter is bound to (service injection by interface).
        TypeRef::DynTrait { trait_name, .. } => TypeRepr::DynTrait(trait_name.clone()),
        TypeRef::Tuple { .. } => TypeRepr::Dyn,
        // A `Self::Name` projection is not statically a concrete type here (resolution is per-impl at
        // the checker); reflect it as the dynamic top, like a tuple (slice 1a).
        TypeRef::AssocProjection { .. } => TypeRepr::Dyn,
        TypeRef::Fn { params, ret, .. } => {
            TypeRepr::Fn(params.iter().map(recur).collect(), Box::new(recur(ret)))
        }
        TypeRef::Named { name, args, .. } => named_repr(name, args, nominal, top),
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
        | (F64, F64)
        | (Bool, Bool)
        | (Str, Str)
        | (Bytes, Bytes)
        | (Unit, Unit) => true,
        // Two fixed-width integers match iff they are the same width and signedness — this is what
        // makes `List<i32>` distinct from `List<i16>` and from `List<int>` (which is `Int`).
        (
            IntN {
                signed: es,
                bits: eb,
            },
            IntN {
                signed: as_,
                bits: ab,
            },
        ) => es == as_ && eb == ab,
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

/// The `ParamInfo` prelude struct's name — `{ name: string, type: Type, optional: bool, attrs:
/// List<dyn> }`, the element type of `params_of()`'s result list. `type` is the reflection `Type`
/// ADT value (the same ADT `type_of` returns), built from the parameter's declared type annotation;
/// `optional` reports whether the parameter declared a default, and so whether a call may omit it;
/// `attrs` holds the parameter's materialized `#[...]` attribute instances, in source order.
///
/// `attrs` exists so a signature-driven consumer can read a signature *once*: `params_of("build")`
/// yields each parameter beside its own metadata, which is what a CLI or router derivation actually
/// wants. The alternative — `attributes_of::<Arg>()` and re-joining its `target` strings back onto
/// the parameter list — works, and still does, but makes every such consumer re-implement the key
/// format. It is a view, not a second table: the values come from the same manifest rows
/// `attributes_of` returns, via [`ReflectionInfo::param_attributes_for`].
pub const PARAM_INFO: &str = "ParamInfo";

/// The built-in **test-metadata attributes** (object-model slice 6h) — prelude `@attribute` structs
/// the test runner reads off a `@test`/`@bench` fn: `#[Skip]` (zero fields, mark as skipped),
/// `#[Name("…")]` (display name), `#[Group("…")]` (category for `--group` filtering), `#[Data([…])]`
/// (parameterized rows). The single source of truth shared by the checker's prelude registration and
/// the runner that interprets them.
// D2b — the tier attributes live under their tier's namespace (no global attribute namespace), so
// these constants are the **qualified identity** every path shares: the loader rewrites a
// user-written `#[Skip]` (after `use std.test.{Skip}`) to this FQN, the reflection manifest carries
// it, and the runner reads it. A user must `use std.test` to apply them.
pub const TEST_ATTR_SKIP: &str = "std.test.Skip";
pub const TEST_ATTR_NAME: &str = "std.test.Name";
pub const TEST_ATTR_GROUP: &str = "std.test.Group";
pub const TEST_ATTR_DATA: &str = "std.test.Data";

/// The **tier-knob attribute** of the `bench` tier: `#[Bench(iterations: N)]` on a bench fn sets its
/// iteration count. A `@bench(iterations: N) { … }` block directive is distribution sugar — it
/// stamps this attribute onto each contained fn that does not already carry one (a per-fn attribute
/// wins over the block's). One mandatory `iterations: int` field; validated by the ordinary
/// attribute construction gate, read by the bench runner.
pub const TIER_ATTR_BENCH: &str = "std.bench.Bench";

/// The `doc` tier's attribute: activation with the `doc` tier live stamps `#[Doc("…")]` onto the
/// declaration a `@doc { … }` block documents (adjacency-resolved), giving runtime docstrings via
/// `attributes_of`. On a normal build the doc blocks strip at lowering and nothing is stamped, so
/// production carries no doc text. One mandatory `text: string` field.
pub const TIER_ATTR_DOC: &str = "std.doc.Doc";

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

/// The declaration-kind vocabulary an `@attribute(Kind, …)` placement list names — the directive
/// spellings the checker's `TargetKind::from_name` accepts, shared so its diagnostics help and IDE
/// completion can never drift from the accepted set (a checker test asserts lockstep).
pub const ATTRIBUTE_TARGET_KINDS: &[&str] = &[
    "Struct", "Class", "Enum", "Function", "Method", "Field", "Variant", "Param",
];

/// The prelude `FieldEntry` struct — the element type of `fields_of(value)`'s result (derive
/// layer 3): `{ name: string, value: dyn }`, one per field of a struct/class instance, in
/// declaration order. Registered like `ParamInfo`; both backends materialize the matching shape.
pub const FIELD_ENTRY: &str = "FieldEntry";

/// The prelude `FieldSpec` struct — the element type of the **type-level** field query
/// `field_specs_of::<T>()` / `field_specs_of(name)`: `{ name: string, type: Type, optional: bool }`,
/// one per declared field of a struct/class TYPE, in declaration order. Unlike [`FIELD_ENTRY`]
/// (which reflects an *instance*'s field *values*) this reflects the *declaration*, so `type` is the
/// field's declared type — **precise**, not the runtime-erased head `type_of` yields on a value —
/// and `optional` reports whether the field declared a default (so a dynamic constructor knows it
/// may omit it). Registered like `ParamInfo`; both backends materialize the matching shape.
pub const FIELD_SPEC: &str = "FieldSpec";

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
    /// The packed struct's type name — the nominal type of a materialized element. **Empty** marks a
    /// *bare-scalar* layout (packed-widths bare-scalar arc): a `List<i32>`/`List<u8>`/`List<f32>` whose
    /// element is a single sub-8-byte numeric with no struct wrapper. A scalar layout has exactly one
    /// (unnamed) field and materializes to a bare `Value` (`int`/`f32`), not a `Value::Object` — user
    /// types always have a non-empty name, so the emptiness is an unambiguous marker. Use
    /// [`PackedLayout::is_scalar`] rather than testing the string directly.
    pub type_name: String,
    /// The fields in declared (slot) order. A scalar layout ([`Self::is_scalar`]) holds exactly one.
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
    /// An explicit 64-bit float field `f64` (packed-widths arc) — 8 bytes, storage-identical to
    /// `Float` but a distinct kind so packed reflection reports `f64`.
    F64,
    /// A fixed-width integer field (packed-widths arc): `bits/8` bytes, `signed` deciding read-back
    /// extension. The compiled/runtime counterparts (`noeta_bytecode::PackedFieldDef::IntN`,
    /// `noeta_object::PackedKind::IntN`) carry the same pair.
    IntN {
        /// One of 8, 16, 32, 64.
        bits: u8,
        /// `true` for the `iN` family, `false` for `uN`.
        signed: bool,
    },
    Bool,
    /// A nested `@packed` struct, laid out contiguously in the parent's buffer.
    Struct(Box<PackedLayout>),
}

impl PackedLayout {
    /// A **bare-scalar** element layout (packed-widths bare-scalar arc): a single unnamed field of
    /// `kind`, no struct wrapper, so a `List<i32>`/`List<u8>`/`List<f32>` stores its elements as a flat
    /// `byte_width`-per-element buffer that materializes back to a bare `int`/`f32` (not a
    /// `Value::Object`). Row/column is moot for one field, so it is always row-major.
    pub fn scalar(kind: PackedKind) -> PackedLayout {
        PackedLayout {
            type_name: String::new(),
            fields: vec![PackedField {
                name: String::new(),
                kind,
            }],
            column: false,
        }
    }

    /// Whether this is a bare-scalar element layout (no nominal struct — a `List<i32>`/`List<f32>`),
    /// materializing to a bare `Value` rather than a `Value::Object`. See [`Self::type_name`].
    pub fn is_scalar(&self) -> bool {
        self.type_name.is_empty()
    }

    /// The number of machine words one value of this layout occupies — the sum of each field's width
    /// (a primitive is 1; a nested struct is its own `word_count`). Pre-Phase-3 every primitive is one
    /// 64-bit word; Phase 3 (`f32`) will narrow specific slots.
    pub fn word_count(&self) -> usize {
        self.fields
            .iter()
            .map(|f| match &f.kind {
                PackedKind::Int
                | PackedKind::Float
                | PackedKind::F32
                | PackedKind::F64
                | PackedKind::IntN { .. }
                | PackedKind::Bool => 1,
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
                PackedKind::Int | PackedKind::Float | PackedKind::F64 => 8,
                PackedKind::IntN { bits, .. } => (*bits as usize) / 8,
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

    fn record(type_name: &str, trait_name: &str) -> TraitImplRecord {
        TraitImplRecord {
            type_name: type_name.to_string(),
            trait_name: trait_name.to_string(),
        }
    }

    /// `traits_for` sorts and dedups; `type_implements` is the same rows read as a predicate — the
    /// two surfaces cannot disagree because they read one table.
    #[test]
    fn traits_for_is_sorted_deduped_and_agrees_with_type_implements() {
        let info = ReflectionInfo {
            trait_impls: vec![
                record("Dog", "Speaks"),
                record("Dog", "Comparable"),
                record("Dog", "Speaks"), // duplicate — one row survives the query
                record("Cat", "Purrs"),
            ],
            ..Default::default()
        };
        assert_eq!(info.traits_for("Dog"), vec!["Comparable", "Speaks"]);
        assert_eq!(info.traits_for("Cat"), vec!["Purrs"]);
        assert!(info.traits_for("Unknown").is_empty());
        assert!(info.type_implements("Dog", "Speaks"));
        assert!(!info.type_implements("Dog", "Purrs"));
        assert!(!info.type_implements("Unknown", "Speaks"));
    }

    /// `accumulate` supersedes a redeclared type's membership rows wholesale (REPL latest-wins) and
    /// leaves other types' rows in place — mirroring the `TypeInfo` purge.
    #[test]
    fn accumulate_supersedes_a_redeclared_types_trait_impls() {
        let mut base = ReflectionInfo {
            types: vec![TypeInfo {
                name: "Dog".to_string(),
                kind: TypeKind::Struct,
                fields: Vec::new(),
                field_types: Vec::new(),
                field_optional: Vec::new(),
                field_defaults: Vec::new(),
                variants: Vec::new(),
            }],
            trait_impls: vec![record("Dog", "Speaks"), record("Cat", "Purrs")],
            ..Default::default()
        };
        // The fragment redeclares `Dog` WITHOUT the impl: the old membership must not survive.
        let fragment = ReflectionInfo {
            types: vec![TypeInfo {
                name: "Dog".to_string(),
                kind: TypeKind::Struct,
                fields: Vec::new(),
                field_types: Vec::new(),
                field_optional: Vec::new(),
                field_defaults: Vec::new(),
                variants: Vec::new(),
            }],
            trait_impls: vec![record("Dog", "Fetches")],
            ..Default::default()
        };
        base.accumulate(fragment);
        assert_eq!(base.traits_for("Dog"), vec!["Fetches"]);
        assert_eq!(base.traits_for("Cat"), vec!["Purrs"]);
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

    /// A named `TypeRef` with the given generic arguments, spans elided.
    fn named(name: &str, args: Vec<TypeRef>) -> TypeRef {
        TypeRef::Named {
            name: name.to_string(),
            args,
            span: Span::new(0, 0),
        }
    }

    /// A registry holding one struct, so the kind-aware projection has something to classify.
    fn one_struct(name: &str) -> ReflectionInfo {
        ReflectionInfo {
            types: vec![TypeInfo {
                name: name.to_string(),
                kind: TypeKind::Struct,
                fields: Vec::new(),
                field_types: Vec::new(),
                field_optional: Vec::new(),
                field_defaults: Vec::new(),
                variants: Vec::new(),
            }],
            ..Default::default()
        }
    }

    /// A type reference's generic arguments reach the reflected type, recursively — the defect that
    /// made `#[Builds(target: List<int>)]` materialize as `Type.List(Type.Dyn)`. Both backends call
    /// this one method, so fixing it here fixes both; it was invisible to the differential precisely
    /// because they were identically wrong.
    #[test]
    fn type_ref_arguments_are_not_erased() {
        let info = ReflectionInfo::default();
        assert_eq!(
            info.type_ref_repr("List", &[named("int", vec![])]),
            TypeRepr::List(boxed(TypeRepr::Int))
        );
        assert_eq!(
            info.type_ref_repr(
                "Map",
                &[
                    named("string", vec![]),
                    named("List", vec![named("int", vec![])]),
                ]
            ),
            TypeRepr::Map(
                boxed(TypeRepr::Str),
                boxed(TypeRepr::List(boxed(TypeRepr::Int)))
            )
        );
        // No arguments still means the `Dyn` top — a bare `List` is an inference hole, not an error.
        assert_eq!(
            info.type_ref_repr("List", &[]),
            TypeRepr::List(boxed(TypeRepr::Dyn))
        );
    }

    /// Arguments and the nominal-kind classification hold *together*: a declared struct is
    /// `Type.Struct` at the head **and** in argument position. The two properties used to live in
    /// separate converters (`type_ref_repr` had the kind, `typeref_to_repr` had the arguments), and
    /// neither had both.
    #[test]
    fn kind_classification_survives_and_reaches_arguments() {
        let info = one_struct("Codec");
        assert_eq!(
            info.type_ref_repr("Codec", &[named("int", vec![])]),
            TypeRepr::Struct("Codec".to_string(), vec![TypeRepr::Int])
        );
        assert_eq!(
            info.type_ref_repr("List", &[named("Codec", vec![])]),
            TypeRepr::List(boxed(TypeRepr::Struct("Codec".to_string(), vec![])))
        );
        // An undeclared name has no kind, so it stays the honest `Named` fallback — still with its
        // arguments.
        assert_eq!(
            info.type_ref_repr("Opaque", &[named("int", vec![])]),
            TypeRepr::Named("Opaque".to_string(), vec![TypeRepr::Int])
        );
    }

    /// The two converters agree on **everything except the nominal kind** — the one axis they are
    /// meant to differ on ([`typeref_to_repr`] feeds the deliberately kind-tolerant R3 narrow
    /// matcher). Sharing one walk is what makes that the only difference.
    #[test]
    fn the_two_converters_differ_only_in_nominal_kind() {
        let info = one_struct("Codec");
        let ty = named("Map", vec![named("string", vec![]), named("List", vec![])]);
        assert_eq!(info.typeref_repr(&ty), typeref_to_repr(&ty));
        let nominal = named("Codec", vec![]);
        assert_eq!(
            info.typeref_repr(&nominal),
            TypeRepr::Struct("Codec".to_string(), vec![])
        );
        assert_eq!(
            typeref_to_repr(&nominal),
            TypeRepr::Named("Codec".to_string(), vec![])
        );
    }

    #[test]
    fn display_short_drops_the_qualifier() {
        // A bare nominal loses its module prefix; scalars and keywords are untouched.
        assert_eq!(
            TypeRepr::Struct("geometry.vec.Vec2".to_string(), vec![]).display_short(),
            "Vec2"
        );
        assert_eq!(TypeRepr::Int.display_short(), "int");
        // Display keeps the fully-qualified form for hover/debugger.
        assert_eq!(
            TypeRepr::Struct("geometry.vec.Vec2".to_string(), vec![]).to_string(),
            "geometry.vec.Vec2"
        );
    }

    #[test]
    fn display_short_reaches_nested_type_arguments() {
        // Shortening recurses through containers, generics, function types and unions.
        let vec2 = || TypeRepr::Struct("geometry.vec.Vec2".to_string(), vec![]);
        assert_eq!(TypeRepr::List(boxed(vec2())).display_short(), "List<Vec2>");
        assert_eq!(
            TypeRepr::Map(boxed(TypeRepr::Str), boxed(vec2())).display_short(),
            "Map<string, Vec2>"
        );
        assert_eq!(
            TypeRepr::Class("pkg.Box".to_string(), vec![vec2()]).display_short(),
            "Box<Vec2>"
        );
        assert_eq!(
            TypeRepr::Fn(vec![vec2()], boxed(TypeRepr::Option(boxed(vec2())))).display_short(),
            "(Vec2) -> ?Vec2"
        );
        assert_eq!(
            TypeRepr::Union(vec![vec2(), TypeRepr::Int]).display_short(),
            "Vec2 | int"
        );
    }
}
