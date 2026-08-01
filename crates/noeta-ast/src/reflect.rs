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
    ///
    /// **Derived, never primary**: it is exactly [`derive_roles`] of [`Self::manifest`] and
    /// [`Self::role_tags`], and both [`build`] and [`ReflectionInfo::accumulate`] recompute it that
    /// way. Materialized into the artifact rather than joined at run time because that is what
    /// `roles_of()` reads, but it carries no information the two inputs do not.
    pub roles: Vec<RoleRecord>,
    /// Which `@role(Enum.Variant)` tags each **attribute declaration** carries — the other half of
    /// the join [`Self::roles`] is.
    ///
    /// It is here, and not folded into the derived index, because the tag and the *use* of the
    /// attribute live in different declarations and therefore in different hot-swap fragments: a
    /// fragment carrying only `#[Page("/")] fn renderHome` re-declares the manifest entry while
    /// `@role(WebRole.Controller) struct Page` stays behind, unchanged, in the live session. With
    /// the join computed per fragment and stored as if it were primary data, `accumulate`'s purge
    /// dropped `renderHome`'s binding and the fragment had nothing to put back — reflection lost
    /// exactly the declaration the swap touched. Keeping the inputs and re-deriving makes the index
    /// a function of the whole accumulated session, which is what a cold start compares against.
    pub role_tags: Vec<RoleTagRecord>,
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
    /// incoming fragment does not touch are left in place; the fragment's own records land **where
    /// the records they supersede were**, and only a genuinely new name appends.
    ///
    /// That placement rule is the whole of the second half. Superseding a declaration is not the
    /// same as re-declaring it last: `attributes_of::<T>()` hands its manifest back in source order
    /// and callers pin that order, so purge-and-append moved the first annotated declaration behind
    /// every one the fragment never touched, and an ordered listing came out permuted by nothing
    /// more than which body a developer saved. Anchoring each incoming group at the position of the
    /// first record it supersedes keeps the accumulated tables in declaration order, which is the
    /// order a cold compile of the same source produces.
    pub fn accumulate(&mut self, fragment: ReflectionInfo) {
        // The declaration names this fragment (re)defines — a type it declares, or any attribute /
        // role target it carries. Their old records are superseded wholesale before the new ones land.
        // Owned rather than borrowed from `fragment`, because the fragment's own tables are moved
        // into the merges below while these are still being consulted.
        let redeclared: std::collections::HashSet<String> = fragment
            .types
            .iter()
            .map(|t| t.name.clone())
            .chain(fragment.manifest.iter().map(|a| a.target.clone()))
            .chain(fragment.roles.iter().map(|r| r.target.clone()))
            .collect();
        // Every callable this fragment (re)declares, by the target its parameters are keyed under.
        // A callable always emits a `ParamRecord` (`push_params` emits both renderings together), so
        // this set is exactly "the callables whose parameter lists this fragment redefines".
        let fragment_callables: std::collections::HashSet<String> =
            fragment.params.iter().map(|p| p.target.clone()).collect();
        // Param records are keyed by a callable's target (`fn` or `Type.method`); a redeclared
        // callable purges its old params. A plain fn or method carries no attribute, so its target
        // is not in `redeclared` (which is built from type names + attribute/role targets) — key the
        // purge on the target's declaration base (the type name before `.`, or the bare fn name) and
        // on the incoming fragment's own param targets, so redefining a callable supersedes its old
        // parameter list even when it bears no attribute.
        let param_bases: std::collections::HashSet<String> = fragment
            .params
            .iter()
            .map(|p| param_base(&p.target).to_string())
            .collect();

        supersede(
            &mut self.types,
            fragment.types,
            |t| redeclared.contains(t.name.as_str()),
            |t| t.name.clone(),
        );
        supersede(
            &mut self.manifest,
            fragment.manifest,
            |a| {
                match split_param_attr_target(&a.target) {
                    // A parameter row lives and dies with its callable's parameter list, not with
                    // its own key: redeclaring `fn build(target: string)` without the `#[Arg]` it
                    // used to carry must *drop* the old row, and the new fragment names no such
                    // target to supersede it. Keying the purge on the callable is the same move the
                    // `params` purge below makes, for the same reason — and it is why the parameter
                    // key is built to be splittable back into its callable at all.
                    Some((callable, _)) => {
                        fragment_callables.contains(callable)
                            || redeclared.contains(param_base(callable))
                    }
                    None => redeclared.contains(a.target.as_str()),
                }
            },
            // A callable's own row and its parameter rows are ONE group, anchored together: `build`
            // is emitted immediately before `build#target` / `build#release`, and splitting them
            // into separate anchors would let the parameter rows drift away from their callable.
            |a| manifest_group(&a.target).to_string(),
        );
        supersede(
            &mut self.params,
            fragment.params,
            |p| {
                let base = param_base(&p.target);
                redeclared.contains(base) || param_bases.contains(base)
            },
            |p| param_base(&p.target).to_string(),
        );
        // A redeclared *attribute's* role tags are superseded with it — re-declaring
        // `struct Page` without its `@role` must drop the role, and the fragment carries whatever
        // tags the new declaration has. Tags for attributes the fragment does not touch (the common
        // hot-swap case, and every native attribute re-supplied per install) stay put; identical
        // re-supplied tags are dropped rather than duplicated.
        supersede_set(
            &mut self.role_tags,
            fragment.role_tags,
            |t| redeclared.contains(t.attribute.as_str()),
            |t| t.attribute.clone(),
        );
        // A redeclared type's trait impls are superseded wholesale — the fragment's own records
        // (re-collected from its `impl`s/derives) land in their place, exactly like its `TypeInfo`.
        supersede_set(
            &mut self.trait_impls,
            fragment.trait_impls,
            |r| redeclared.contains(r.type_name.as_str()),
            |r| r.type_name.clone(),
        );
        drop(redeclared);
        drop(param_bases);
        drop(fragment_callables);
        // `roles` is derived, so it is *recomputed* rather than merged: the fragment's own join is
        // discarded (it could only see the tags its own declarations carried) and the index is
        // re-derived from the merged manifest and the merged tag table. This is what makes the
        // accumulated index equal a cold compile's — see [`ReflectionInfo::role_tags`].
        self.roles = derive_roles(&self.manifest, &self.role_tags);
    }

    /// The parameter list declared for `target`, or empty if the target names no known callable — the
    /// projection `params_of(target)` materializes for dependency injection.
    ///
    /// The program's own declarations answer first; a target none of them names falls through to the
    /// **native** lookup ([`native_reflect::native_param_record`](crate::native_reflect::native_param_record)),
    /// so a shipped stdlib callable is reported as the callable it is rather than as a typo. The
    /// program-first order is not incidental: it is the prelude-shadowing rule, and it is what the
    /// eager seeding's "only if absent" guard used to express.
    pub fn params_for(&self, target: &str) -> &[ParamSig] {
        self.params
            .iter()
            .find(|p| p.target == target)
            .or_else(|| crate::native_reflect::native_param_record(target))
            .map(|p| p.params.as_slice())
            .unwrap_or(&[])
    }

    /// The **return type** declared for `target`, or `None` if the target names no known callable —
    /// the projection `returns_of(target)` materializes into a `?Type`.
    ///
    /// Deliberately an `Option` where [`params_for`](Self::params_for) answers an empty slice for the
    /// same unknown target, and the difference is not an inconsistency: an empty parameter list is a
    /// *legitimate answer* (`fn tick(): void` really does take no parameters), so `params_of` can
    /// fold "unknown" into it without losing information, while there is no return type that means
    /// "this callable does not exist" — `void` is a real return type. Folding the two would make a
    /// typo in a target string indistinguishable from a `void` method, which is exactly the
    /// vanishing-route failure a reflection-driven framework must be able to detect. So the
    /// missing case gets its own `none`, and the caller has to look at it.
    ///
    /// Falls through to the native lookup on a miss, on the same terms as
    /// [`params_for`](Self::params_for) — one record answers both queries, so a callable present in
    /// one index is present in the other.
    pub fn returns_for(&self, target: &str) -> Option<&TypeRepr> {
        self.params
            .iter()
            .find(|p| p.target == target)
            .or_else(|| crate::native_reflect::native_param_record(target))
            .map(|p| &p.ret)
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

    /// The data attributes attached to one **field** of `type_name`, in source order — the field twin
    /// of [`param_attributes_for`](Self::param_attributes_for), and the join that makes
    /// `FieldSpec.attrs` a *view* of the attribute manifest rather than a second copy of it.
    ///
    /// `field_specs_of` materializes each field's attributes through here, and `attributes_of::<T>()`
    /// reads the very same rows off the same table; both reach the key through
    /// [`field_attr_target`], so the two surfaces cannot disagree about which attribute belongs to
    /// which field. A field carrying no attribute answers the empty list, exactly as an unannotated
    /// parameter does — an absence is reported as an empty list, never as a missing descriptor.
    pub fn field_attributes_for(&self, type_name: &str, field: &str) -> Vec<&AttributeRecord> {
        let key = field_attr_target(type_name, field);
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
    ///
    /// The program's own declarations answer first; a name none of them declares falls through to
    /// the **prelude and native** lookup
    /// ([`native_reflect::native_type_info`](crate::native_reflect::native_type_info)), so
    /// `Ordering` and `std.http.Framing` are as knowable to reflection as they are to the rest of
    /// the language. Program-first *is* the shadowing rule: a program that declares its own
    /// `Ordering` reflects its own.
    pub fn type_named(&self, name: &str) -> Option<&TypeInfo> {
        self.types
            .iter()
            .find(|t| t.name == name)
            .or_else(|| crate::native_reflect::native_type_info(name))
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
                // A `TypeInfo` written before `field_public` existed (or one assembled by a test
                // fixture) carries no bit for this field; default to public, which is what every
                // struct field and every `pub` class field is — so a missing bit widens nothing that
                // was not already open, it just fails to close a private one.
                public: info.field_public.get(i).copied().unwrap_or(true),
            })
            .collect()
    }

    /// The **type-level variant schema** of the declared enum `type_name` — one [`VariantSpecData`]
    /// per variant in declaration order — the data `variants_of::<T>()` / `variants_of(name)`
    /// materialize. The enum twin of [`Self::field_specs`], and deliberately the same contract: an
    /// unknown name, or a name that is a struct or class rather than an enum, yields the empty list,
    /// the same "nothing to report" answer `params_of` gives an unknown target, so a framework can
    /// probe a type name without a guard.
    ///
    /// The pair is what makes a walked type *knowable*. `field_specs_of` alone cannot tell an enum
    /// from a field-less struct — both answer with the empty list — so a schema builder that recursed
    /// into a `Type.Named(name, _)` emitted an empty object for an enum and was silently wrong.
    /// Asking both means the empty/empty case is the one honest "I know nothing about this name",
    /// and a non-empty variant list is the loud answer that was missing.
    ///
    /// Each variant's payload is reported as [`FieldSpecData`] — the very elements `field_specs`
    /// returns for a struct — because a payload *is* ordinary declared-field data (a positional
    /// payload carries a synthesized `_0`/`_1` name and its real type). Both backends read this one
    /// accessor, so the materialized `List<VariantSpec>` agrees across the differential by
    /// construction.
    pub fn variant_specs(&self, type_name: &str) -> Vec<VariantSpecData<'_>> {
        let Some(info) = self.type_named(type_name) else {
            return Vec::new();
        };
        if info.kind != TypeKind::Enum {
            return Vec::new();
        }
        info.variants
            .iter()
            .map(|variant| VariantSpecData {
                name: &variant.name,
                payload: variant
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, name)| FieldSpecData {
                        name,
                        ty: variant.field_types.get(i).unwrap_or(&TypeRepr::Dyn),
                        // A variant payload field declares no default — there is no syntax for one —
                        // so it can never be omitted from a construction. Reported through the same
                        // `FieldSpec` the struct side uses rather than a payload-only element type:
                        // one vocabulary, and `optional` says the true thing about a payload.
                        optional: false,
                        // A payload slot has no visibility syntax either: a variant is public with
                        // its enum, so every payload field is settable wherever the case is.
                        public: true,
                    })
                    .collect(),
                backing: variant.backing.as_ref(),
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

/// One `@role(Enum.Variant)` tag as it rides on the **attribute declaration** that confers it —
/// `@role(WebRole.Controller) struct Page` — before any declaration has been annotated with it.
/// [`derive_roles`] joins these with the attribute manifest to produce [`RoleRecord`]s.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RoleTagRecord {
    /// The attribute's name (e.g. `Page`), or a native attribute's qualified identity — matched
    /// against [`AttributeRecord::name`], which is the same spelling a linked application carries.
    pub attribute: String,
    /// The role's `@semantic` enum name (e.g. `Semantic`, `WebRole`).
    pub enum_name: String,
    /// The role's variant name (e.g. `EntryPoint`, `Controller`).
    pub variant: String,
}

/// Join the attribute manifest with the role tags: every declaration bearing a role-tagged
/// attribute is indexed `(target, enum, variant)`, in manifest order. Identical entries (two
/// attributes conferring the same role on one declaration) are de-duplicated while preserving that
/// order.
///
/// The **whole** definition of [`ReflectionInfo::roles`], called from [`build`] on a program and
/// from [`ReflectionInfo::accumulate`] on the merged session state — so a session that has absorbed
/// a hot-swap fragment holds the same index a cold compile of the same source produces, rather than
/// whatever the last fragment happened to be able to re-derive on its own.
pub fn derive_roles(manifest: &[AttributeRecord], tags: &[RoleTagRecord]) -> Vec<RoleRecord> {
    let mut roles: Vec<RoleRecord> = Vec::new();
    for entry in manifest {
        for tag in tags.iter().filter(|t| t.attribute == entry.name) {
            let record = RoleRecord {
                target: entry.target.clone(),
                target_span: entry.target_span,
                enum_name: tag.enum_name.clone(),
                variant: tag.variant.clone(),
            };
            if !roles.contains(&record) {
                roles.push(record);
            }
        }
    }
    roles
}

/// One callable's declared **signature** — a top-level fn or a method — keyed by the same target
/// convention as the attribute manifest (a bare fn name, or a qualified `Type.method`). `params_of()`
/// materializes the parameters into a `List<ParamInfo>` (each `{ name: string, type: Type, optional:
/// bool }`); `returns_of()` materializes [`ParamRecord::ret`] into a `?Type`.
///
/// Parameters and return type live in ONE record because they are one declaration: a callable cannot
/// be present in the parameter index and absent from the return index, so the two queries can never
/// disagree about which callables exist.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ParamRecord {
    /// The callable's target: a top-level fn's bare name, or a method's qualified `Type.method` name.
    pub target: String,
    /// The declared parameters, in source order.
    pub params: Vec<ParamSig>,
    /// The declared **return type** as a reflection [`TypeRepr`] — what `returns_of(target)`
    /// surfaces, and what a framework deriving a response schema from a controller method needs.
    ///
    /// A callable that declares no return type records [`TypeRepr::Unit`], the same repr the
    /// explicit `void`/`unit` spelling maps to: the two spellings mean one thing, so reflection must
    /// not make them distinguishable. (A named fn must declare a return type anyway — E0022 — so an
    /// absent one only reaches here from a program that never runs.) Deliberately NOT `Dyn`: `Dyn` is
    /// the honest answer for an *unannotated parameter*, whose type is genuinely unknown, whereas an
    /// omitted return type is a known thing spelled by omission.
    pub ret: TypeRepr,
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

/// The group an attribute-manifest row belongs to for latest-wins *placement*: a parameter row
/// (`build#release`) groups with its callable (`build`), everything else with itself.
///
/// A callable's own row is emitted immediately before its parameter rows, so they must move as one
/// block when the callable is superseded — giving the parameter rows their own anchor would let
/// them drift away from the declaration they describe the moment anything between them changed.
fn manifest_group(target: &str) -> &str {
    split_param_attr_target(target)
        .map(|(callable, _)| callable)
        .unwrap_or(target)
}

/// Merge `fragment`'s records into `base`, **in place**: the positions the superseded records held
/// are the positions the incoming ones take, filled in the **fragment's** order, and records whose
/// key superseded nothing append in the fragment's order too.
///
/// `purged` says whether a record already in `base` is superseded by this fragment; `key` names the
/// declaration a record belongs to, over `base` and `fragment` alike.
///
/// The in-place rule exists because these tables are **ordered surfaces**: `attributes_of` hands the
/// manifest back in declaration order and callers pin it, so appending a superseded declaration's
/// records permuted a listing on nothing more than which body was edited last.
///
/// **Which slot each incoming group gets is the fragment's answer, not the base's**, and that is
/// the other half of the same rule. The positions are the base's — one per key the fragment
/// actually re-supplies, in base order — but they are handed out by walking the *fragment*, so a
/// fragment that re-declares its groups in a different order than the session holds them re-orders
/// the table rather than re-landing every group where it already was. Pinning each group to its own
/// old slot made a declaration reorder unobservable: the differ built the fragment and the merge
/// undid it (`plans/parallel-path-audit.md` §14). A fragment carrying ONE group has one slot and
/// therefore cannot move anything — which is exactly the body-edit case the in-place rule is for,
/// preserved by construction rather than by a second code path.
///
/// A key the fragment carries that the base has never seen is **buffered onto the next slot**
/// instead of appending: a declaration inserted between two existing ones lands between them, the
/// way a cold compile of the same source has it. With nothing after it (the ordinary "append a
/// function at the end of the file" edit), the buffer flushes at the end, which is the same append
/// as before.
///
/// Ordering discipline for the append path: the fragment's records keep their relative order
/// whatever their keys, because a whole-program compile is *this same merge onto an empty table* —
/// nothing is anchored, everything appends, and the result must be the builder's output verbatim.
/// Batching by key would quietly re-sort a table like `trait_impls`, whose rows legitimately
/// interleave two types.
fn supersede<T, F, G>(base: &mut Vec<T>, fragment: Vec<T>, purged: F, key: G)
where
    F: Fn(&T) -> bool,
    G: Fn(&T) -> String,
{
    merge(base, fragment, purged, key, |_, _| false)
}

/// [`supersede`] for a **set-like** table — one re-supplied in full on every install (the native
/// registry's trait impls and role tags): an appended record the merged table already holds is
/// dropped rather than duplicated, so a long session does not grow a copy per install. The
/// distinction is not cosmetic — a *sequence*-like table (the attribute manifest) may legitimately
/// hold two equal rows, and silently collapsing them would be a second reflection bug.
fn supersede_set<T, F, G>(base: &mut Vec<T>, fragment: Vec<T>, purged: F, key: G)
where
    T: PartialEq,
    F: Fn(&T) -> bool,
    G: Fn(&T) -> String,
{
    merge(base, fragment, purged, key, |merged: &[T], record: &T| {
        merged.contains(record)
    })
}

fn merge<T, F, G, D>(base: &mut Vec<T>, fragment: Vec<T>, purged: F, key: G, already_held: D)
where
    F: Fn(&T) -> bool,
    G: Fn(&T) -> String,
    D: Fn(&[T], &T) -> bool,
{
    // The incoming records, in fragment order; each is taken as it is placed. Every key set below
    // is a `Vec` for its order plus a `HashSet` for its membership: the ordinary whole-program
    // compile is this merge onto an EMPTY base with thousands of distinct keys, and a linear
    // `contains` per record would make a cold `noeta run` quadratic in its own declarations.
    let mut incoming: Vec<Option<T>> = fragment.into_iter().map(Some).collect();
    // The fragment's keys, in the fragment's own order — the order it wants the table to hold.
    let mut fragment_keys: Vec<String> = Vec::new();
    let mut fragment_key_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in incoming.iter().flatten() {
        let k = key(record);
        if fragment_key_set.insert(k.clone()) {
            fragment_keys.push(k);
        }
    }
    // The slots: the first superseded record of each key the fragment actually re-supplies, in base
    // order. A key whose records are purged with nothing to replace them (a callable re-declared
    // without the attribute it used to carry) opens no slot — its rows simply go, and the keys that
    // do arrive must not shift into the hole it leaves.
    let mut slot_count = 0usize;
    let mut slot_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for record in base.iter() {
        if !purged(record) {
            continue;
        }
        let k = key(record);
        if fragment_key_set.contains(&k) && slot_set.insert(k) {
            slot_count += 1;
        }
    }
    // What each slot emits, assigned by walking the fragment: the i-th slot (in base order) takes
    // the i-th key the fragment re-supplies (in fragment order), preceded by any keys new to the
    // base that the fragment declared just before it.
    let mut emitted: Vec<Vec<String>> = vec![Vec::new(); slot_count];
    let mut pending: Vec<String> = Vec::new();
    let mut next = 0usize;
    for k in &fragment_keys {
        if slot_set.contains(k) {
            emitted[next].append(&mut pending);
            emitted[next].push(k.clone());
            next += 1;
        } else {
            pending.push(k.clone());
        }
    }
    let mut merged: Vec<T> = Vec::with_capacity(base.len());
    let mut anchored: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut slot = 0usize;
    for record in base.drain(..) {
        if !purged(&record) {
            merged.push(record);
            continue;
        }
        // The first record a key supersedes opens that key's slot — which the assignment above may
        // have given to a *different* key's records. The remaining superseded records are dropped;
        // their replacements have already been placed.
        let k = key(&record);
        if anchored.insert(k.clone()) && slot_set.contains(&k) {
            for group in &emitted[slot] {
                for held in incoming.iter_mut() {
                    if held.as_ref().is_some_and(|r| key(r) == *group) {
                        merged.push(held.take().expect("just matched Some"));
                    }
                }
            }
            slot += 1;
        }
    }
    for record in incoming.into_iter().flatten() {
        if !already_held(&merged, &record) {
            merged.push(record);
        }
    }
    *base = merged;
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

/// The attribute-manifest key of one **field** of `type_name` — the qualified `Type.field` spelling,
/// mirroring the `Type.method` and `Enum.Variant` conventions, so a `#[Column(…)]` on a property
/// surfaces distinctly per owner.
///
/// Unlike a parameter's key this needs no separator of its own: a field name cannot collide with a
/// method name on the same type, so the dotted member spelling is already unambiguous — which is why
/// it is the key `attributes_of::<T>()` has always reported for a field, and why the
/// `FieldSpec.attrs` join reads the same rows a consumer used to re-join by hand.
pub fn field_attr_target(type_name: &str, field: &str) -> String {
    format!("{type_name}.{field}")
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
    /// Whether each field is **publicly settable**, parallel to `fields` (empty for enums). A value
    /// `struct`'s fields are always public; a reference `class`'s default private with a per-field
    /// `pub` opt-in (object-model slice 2d) — so this is the checker's `symbols.private_fields`
    /// inverted, and a native fielded type's registry `ExtField::is_public`.
    ///
    /// Reflection carries it because the reflective construction door needs it. The checker enforces
    /// privacy at a *literal* (E0035) off a symbol table no runtime door can see, so without this bit
    /// `construct("Box", {"secret": 9})` set a field a source-written `Box { secret: 9 }` could not —
    /// reflection minting a value the declaration forbids. Read by [`plan_construct`] /
    /// [`plan_construct_named`], which refuse a private field by name.
    pub field_public: Vec<bool>,
    /// Each field's **literal default** (object-model slice 6i), parallel to `fields`: `Some` when
    /// the field declared `name: T = <literal>`, `None` for a mandatory field or a non-literal
    /// default. Used to fill an omitted optional field when materializing an attribute instance, so
    /// `attributes_of` reports the declared default rather than a placeholder.
    pub field_defaults: Vec<Option<AttrValue>>,
    /// Variants in declaration order (enums; empty otherwise).
    pub variants: Vec<VariantInfo>,
}

/// An enum variant's reflectable shape: its name, its payload fields, and — for a backed enum — the
/// literal value backing it.
///
/// A **positional** payload (`Leaf(User)`, `Pair(string, int)`) reaches here as an ordinary field
/// with a synthesized `_0`/`_1` name and its declared type in `field_types`, because that is how the
/// AST stores it. So this is the same (name, declared type) pairing [`TypeInfo::fields`] /
/// [`TypeInfo::field_types`] carry for a struct, and [`ReflectionInfo::variant_specs`] projects it
/// through the same [`FieldSpecData`] the struct-side `field_specs_of` reports — one payload
/// vocabulary rather than an enum-shaped special case.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VariantInfo {
    pub name: String,
    /// Payload field names in declaration order (`_0`, `_1`, … for a positional payload); empty for
    /// a fieldless variant.
    pub fields: Vec<String>,
    /// Each payload field's declared type as a reflection [`TypeRepr`], parallel to `fields`. An
    /// unannotated payload field is [`TypeRepr::Dyn`]. Captured from the DECLARATION, so — like
    /// [`TypeInfo::field_types`] — it is precise: `Many(List<int>)` is `List(Int)`, not `List(Dyn)`.
    pub field_types: Vec<TypeRepr>,
    /// The **backing value** of this variant in a backed enum (`enum Status: string { Pending =
    /// "pending" }`), folded through the shared [`fold_const_expr`]; `None` for a plain enum's
    /// variant, and for a backed variant whose value is not a literal. Reported by `variants_of` so
    /// a schema derived from a backed enum can emit the wire values rather than the variant names.
    pub backing: Option<AttrValue>,
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
    // Attribute name → its `@role(Enum.Variant)` tags, harvested from the attribute declarations
    // themselves; joined with the manifest below so every *use* of a role-tagged attribute is
    // indexed. One entry per (attribute, role) pair — an attribute may carry several roles. Kept in
    // the artifact beside the join it feeds, because the tag and the use are separable declarations
    // (see [`ReflectionInfo::role_tags`]).
    let mut role_tags: Vec<RoleTagRecord> = Vec::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Struct(decl) => {
                push_attrs(
                    &mut manifest,
                    decl.name.as_str(),
                    decl.name_span,
                    &decl.decorators.attrs,
                );
                push_field_attrs(&mut manifest, decl.name.as_str(), &decl.fields);
                // A role tag rides on the attribute struct; record each (validated) `Enum.Variant`
                // so every declaration the attribute annotates inherits it. A malformed `@role`
                // never reaches a runnable program (the checker rejects it).
                if let Some(roles) = decl.decorators.role.as_ref() {
                    for tag in roles {
                        role_tags.push(RoleTagRecord {
                            attribute: decl.name.to_string(),
                            enum_name: tag.enum_name.to_string(),
                            variant: tag.variant.clone(),
                        });
                    }
                }
                // A method's attributes are keyed by its qualified `Struct.method` name, exactly as
                // the class and enum arms key theirs — the checker validates a struct method's
                // `#[...]` through the same `check_fn`, so omitting the record here made a
                // well-formed `#[Get("/users")]` on a struct method type-check and then vanish from
                // `attributes_of::<Get>()` with no diagnostic.
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, method.name_span, &method.attrs);
                    push_params(&mut manifest, &mut params, target, method);
                }
                types.push(TypeInfo {
                    name: decl.name.to_string(),
                    kind: TypeKind::Struct,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    field_types: field_types(&decl.fields),
                    field_optional: field_optional(&decl.fields),
                    field_public: field_public(TypeKind::Struct, &decl.fields),
                    field_defaults: field_defaults(&decl.fields),
                    variants: Vec::new(),
                });
            }
            Stmt::Class(decl) => {
                push_attrs(
                    &mut manifest,
                    decl.name.as_str(),
                    decl.name_span,
                    &decl.decorators.attrs,
                );
                push_field_attrs(&mut manifest, decl.name.as_str(), &decl.fields);
                // A method's attributes are keyed by its qualified `Class.method` name, so a
                // `#[...]` on a method surfaces distinctly from the same name on another class.
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.name, method.name);
                    push_attrs(&mut manifest, &target, method.name_span, &method.attrs);
                    push_params(&mut manifest, &mut params, target, method);
                }
                types.push(TypeInfo {
                    name: decl.name.to_string(),
                    kind: TypeKind::Class,
                    fields: decl.fields.iter().map(|f| f.name.clone()).collect(),
                    field_types: field_types(&decl.fields),
                    field_optional: field_optional(&decl.fields),
                    field_public: field_public(TypeKind::Class, &decl.fields),
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
                push_attrs(
                    &mut manifest,
                    decl.name.as_str(),
                    decl.name_span,
                    &decl.attrs,
                );
                push_params(&mut manifest, &mut params, decl.name.to_string(), decl);
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
                    decl.name.as_str(),
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
                        &method.sig,
                    );
                }
            }
            Stmt::Enum(decl) => {
                push_attrs(
                    &mut manifest,
                    decl.name.as_str(),
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
                    push_params(&mut manifest, &mut params, target, method);
                }
                types.push(TypeInfo {
                    name: decl.name.to_string(),
                    kind: TypeKind::Enum,
                    fields: Vec::new(),
                    field_types: Vec::new(),
                    field_optional: Vec::new(),
                    field_public: Vec::new(),
                    field_defaults: Vec::new(),
                    variants: variant_infos(&decl.variants),
                });
            }
            // A **standalone** `impl Trait for T { … }`'s methods, keyed by the same `Type.method`
            // convention, so an attribute on one is discoverable and joins with `params_of`.
            //
            // This is the one method carrier the walk above cannot reach: an *in-body* `impl Trait
            // { … }` block's methods are flattened by the parser into the type's own `methods` (and
            // so are already scanned through the struct/class/enum arms), but a standalone impl is
            // its own top-level statement. Its bodies are checked and its methods really dispatch,
            // so an `#[...]` on one type-checked and then silently did not exist — exactly the
            // asymmetry the `Stmt::Struct` arm had before it gained its own `push_attrs`.
            Stmt::Impl(decl) => {
                for method in &decl.methods {
                    let target = format!("{}.{}", decl.target, method.name);
                    push_attrs(&mut manifest, &target, method.name_span, &method.attrs);
                    push_params(&mut manifest, &mut params, target, method);
                }
            }
            _ => {}
        }
    }
    // Native `@role`-bearing attributes (Slice D3): merge the registry-assembled tags into
    // `role_tags` keyed by the attribute's qualified identity — the identity a linked native
    // attribute application carries in the manifest — so the join below treats a native role-bearing
    // attribute exactly like a `.noe` one. Empty for the pure `.noe` path (byte-identical result).
    for (attr, tags) in native_roles {
        for (enum_name, variant) in tags {
            role_tags.push(RoleTagRecord {
                attribute: attr.clone(),
                enum_name: enum_name.clone(),
                variant: variant.clone(),
            });
        }
    }
    let roles = derive_roles(&manifest, &role_tags);
    ReflectionInfo {
        manifest,
        types,
        roles,
        role_tags,
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
            push(records, type_name, canon(block.trait_name.as_str()));
        }
        for spec in derives {
            // A native derive recipe implements no trait; every other derive a runnable program
            // carries names a real trait (built-in, user, or native — the checker gated the rest).
            if !is_recipe(spec.name.as_str()) {
                push(records, type_name, canon(spec.name.as_str()));
            }
        }
    };
    for stmt in &program.stmts {
        match stmt {
            Stmt::Impl(decl) => push(
                &mut records,
                decl.target.as_str(),
                canon(decl.trait_name.as_str()),
            ),
            Stmt::Struct(d) => body(
                &mut records,
                d.name.as_str(),
                &d.impls,
                &d.decorators.derives,
            ),
            Stmt::Class(d) => body(
                &mut records,
                d.name.as_str(),
                &d.impls,
                &d.decorators.derives,
            ),
            Stmt::Enum(d) => body(
                &mut records,
                d.name.as_str(),
                &d.impls,
                &d.decorators.derives,
            ),
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

/// Record one callable's **signature** — its parameters (in both of the renderings they have) and
/// its declared return type — from one walk.
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
///
/// The return type rides here for the same reason: it is part of the *same declaration*, so taking
/// the whole [`crate::FnDecl`] (rather than a loose parameter slice) means a caller cannot record a
/// callable's parameters while forgetting its return type. Shared by every callable arm of [`build`],
/// so a top-level fn, a struct/class/enum method, and a trait method signature all surface their
/// signature identically.
fn push_params(
    manifest: &mut Vec<AttributeRecord>,
    params: &mut Vec<ParamRecord>,
    target: String,
    decl: &crate::FnDecl,
) {
    for p in &decl.params {
        push_attrs(
            manifest,
            &param_attr_target(&target, &p.name),
            p.name_span,
            &p.attrs,
        );
    }
    params.push(ParamRecord {
        target,
        params: param_sigs(&decl.params),
        ret: fn_ret_repr(decl),
    });
}

/// A callable's declared return type as a reflection [`TypeRepr`] — the one place the "no declared
/// return type" case is decided, so every callable arm of [`build`] answers it the same way. See
/// [`ParamRecord::ret`] for why the absent case is [`TypeRepr::Unit`] and not [`TypeRepr::Dyn`].
fn fn_ret_repr(decl: &crate::FnDecl) -> TypeRepr {
    decl.ret
        .as_ref()
        .map(typeref_to_repr)
        .unwrap_or(TypeRepr::Unit)
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

/// Project one enum's variants into their reflectable shape — the data `variants_of` materializes.
///
/// A payload field's declared type goes through the very same `typeref_to_repr` [`field_types`] uses,
/// so a variant payload and a struct field report a declared type identically; a positional payload
/// needs no special case because the parser already stored its type in the type slot under a
/// synthesized `_0`/`_1` name. A backed variant's value folds through [`fold_const_expr`], the one
/// definition of "a literal" the whole manifest shares.
fn variant_infos(variants: &[crate::VariantDecl]) -> Vec<VariantInfo> {
    variants
        .iter()
        .map(|v| VariantInfo {
            name: v.name.clone(),
            fields: v.fields.iter().map(|f| f.name.clone()).collect(),
            field_types: v
                .fields
                .iter()
                .map(|f| f.ty.as_ref().map(typeref_to_repr).unwrap_or(TypeRepr::Dyn))
                .collect(),
            backing: v.backed_value.as_ref().and_then(fold_const_expr),
        })
        .collect()
}

/// Whether each field declared a default, parallel to the field list — the optionality a dynamic
/// constructor reads. Any default expression counts (not only a literal one), matching the runtime
/// default thunks both backends compile per field.
fn field_optional(fields: &[FieldDecl]) -> Vec<bool> {
    fields.iter().map(|f| f.default.is_some()).collect()
}

/// Whether each field is publicly settable, parallel to the field list — the visibility
/// [`TypeInfo::field_public`] carries, and the same rule the checker's `collect` pass applies to
/// `symbols.private_fields`: a value **`struct`**'s fields are always public (there is nothing to
/// opt into), a reference **`class`**'s default private with a per-field `pub` opt-in (object-model
/// slice 2d). Keyed off `kind` rather than off `is_public` alone, because a struct's field carries
/// `is_public: false` for want of the keyword and *is* public regardless — reading the raw bit would
/// make `construct` refuse every struct field in the language.
fn field_public(kind: TypeKind, fields: &[FieldDecl]) -> Vec<bool> {
    match kind {
        TypeKind::Class => fields.iter().map(|f| f.is_public).collect(),
        TypeKind::Struct | TypeKind::Enum => fields.iter().map(|_| true).collect(),
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
/// which both backends materialize into a prelude `FieldSpec { name, type, optional, attrs }` value.
///
/// The `attrs` slot is **not** carried here, and deliberately so: a field's attributes live in the
/// manifest, keyed by [`field_attr_target`], and both backends join them at materialization through
/// [`ReflectionInfo::field_attributes_for`] — the same shape the parameter half takes, where
/// [`ParamSig`] likewise carries no attributes and `params_of` joins them. Keeping the join out of
/// the borrowed view is what makes `FieldSpec.attrs` a *view* of the manifest rather than a second
/// copy of it, so `attributes_of::<T>()` and `field_specs_of` cannot come to disagree.
#[derive(Debug)]
pub struct FieldSpecData<'a> {
    pub name: &'a str,
    pub ty: &'a TypeRepr,
    pub optional: bool,
    /// Whether the field may be set from outside its type — [`TypeInfo::field_public`] for this
    /// field. Read by [`plan_construct`] / [`plan_construct_named`] and deliberately **not**
    /// materialized into the prelude `FieldSpec`: the surface question a schema consumer asks is
    /// "what shape is this type", and the construction door answers "may I set this" by refusing.
    pub public: bool,
}

/// One variant of a type-level enum schema — the borrowed view [`ReflectionInfo::variant_specs`]
/// returns, which both backends materialize into a prelude `VariantSpec { name, payload, backing }`
/// value.
///
/// The variant's own `#[…]` attributes are deliberately **absent**: they are already keyed in the
/// manifest under the qualified `Enum.Variant` target, and `attributes_of::<T>()` answers "what is
/// annotated on this variant" for every consumer. What the struct half gained (`FieldSpec.attrs`)
/// does not apply here, because a variant is not walked as a *member of a signature* — it is the
/// field and parameter descriptors that a schema deriver walks side by side, and those two are the
/// pair that had to agree. A variant **payload** slot materializes an empty `attrs` list, which is
/// the true answer: a payload slot has no attribute syntax to carry one.
#[derive(Debug)]
pub struct VariantSpecData<'a> {
    pub name: &'a str,
    /// The variant's payload as ordinary declared-field data, in declaration order; empty for a
    /// fieldless variant.
    pub payload: Vec<FieldSpecData<'a>>,
    /// The literal value backing this variant in a backed enum, or `None` for a plain enum.
    pub backing: Option<&'a AttrValue>,
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

/// A scalar wire value handed to `Enum.from(v)` / `Enum.try_from(v)` — the neutral probe both
/// backends build from their own runtime value before asking [`variant_for_wire`] which case it
/// names, so the two cannot disagree about what matches.
#[derive(Debug, Clone, PartialEq)]
pub enum WireProbe {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl WireProbe {
    /// Whether this probe is the backing value `backing`. Numeric widening follows the language
    /// lattice, exactly as the JSON decode door's tag matching does (`int <: float`): an integer
    /// probe selects a `float`-backed case, and a fractional probe never selects an `int`-backed one.
    fn selects_backing(&self, backing: &AttrValue) -> bool {
        match (self, backing) {
            (WireProbe::Str(a), AttrValue::Str(b)) => a == b,
            (WireProbe::Int(a), AttrValue::Int(b)) => a == b,
            (WireProbe::Float(a), AttrValue::Float(b)) => a == b,
            (WireProbe::Int(a), AttrValue::Float(b)) => (*a as f64) == *b,
            (WireProbe::Bool(a), AttrValue::Bool(b)) => a == b,
            _ => false,
        }
    }
}

/// Which payload-free case of an enum a wire value names, given each case's `(name, backing)` in
/// declaration order — the shared decision behind `Enum.from` / `Enum.try_from` in both backends.
///
/// **Backing first, across every case; then case name, across every case.** A backed enum's backing
/// is the value its JSON Schema advertises and the value a real document carries, so a wire-facing
/// conversion has to read it that way — reading it any other way is exactly what left `Enum.from`
/// unusable on untrusted input despite looking like the door for it. The case name stays accepted as
/// a second pass, so a plain enum is unchanged (its cases have no backing, so the first pass matches
/// nothing) and every program that already spelled a backed enum's case name keeps working.
///
/// The two passes are ordered rather than interleaved precisely so that an enum backing one case with
/// another case's name resolves by *backing* — deterministically, and identically in both backends —
/// rather than by whichever happened to come first in the declaration.
///
/// A **payload-carrying** case is never selected: there is no payload to supply, so callers exclude
/// it from `cases` rather than have it matched and then built wrong.
pub fn variant_for_wire(cases: &[(&str, Option<&AttrValue>)], probe: &WireProbe) -> Option<usize> {
    cases
        .iter()
        .position(|(_, backing)| backing.is_some_and(|b| probe.selects_backing(b)))
        .or_else(|| {
            cases
                .iter()
                .position(|(name, _)| matches!(probe, WireProbe::Str(s) if s == name))
        })
}

/// What a `construct(name, fields)` target string names — the shared resolution both backends run,
/// so the two agree on every accept, every reject, and every message.
#[derive(Debug)]
pub enum ConstructTarget<'a> {
    /// A declared struct or class: build it by name from its field schema.
    Fielded,
    /// An `Enum.Variant` spelling: build that case, with `payload` as its construction schema.
    Variant {
        /// The enum's own name, as the built value carries it (qualified under a `namespace`, exactly
        /// as a source-written `Enum.Variant` is).
        enum_name: &'a str,
        variant: &'a str,
        /// The variant's declaration index — what both backends stamp on the value, so a derived
        /// `Comparable` orders a constructed case identically to a literal one.
        index: u32,
        /// The variant's payload as ordinary declared-field data, in declaration order; empty for a
        /// fieldless case. These are the very [`FieldSpecData`]s `variants_of` reports, fed to the
        /// very [`plan_construct`] a struct's fields go through.
        payload: Vec<FieldSpecData<'a>>,
    },
    /// The name is not constructible; the string is the ready-to-surface message.
    Rejected(String),
}

/// Resolve a `construct(name, …)` target string against the reflection artifact.
///
/// **A payload-carrying variant is spelled `"Enum.Variant"`, and its `fields` argument is the
/// variant's payload.** That is the whole convention, and it falls out of what the surrounding
/// surface already reports rather than being invented for `construct`:
///
/// * `variants_of(name)` gives each variant's payload as a `List<FieldSpec>` — the identical element
///   type `field_specs_of(name)` gives a struct's fields, positional payloads included (they carry
///   their synthesized `_0`/`_1` names). So a caller reads a schema from one query and hands the
///   matching values straight to the other, in either of `construct`'s two shapes: a `List<dyn>` in
///   declaration order, or a `Map<string, dyn>` keyed by those payload names.
/// * `"Enum.Variant"` is how the case is written in source, and how the attribute manifest already
///   keys a variant target — the same `Type.member` convention `attributes_of` uses. Nothing new is
///   introduced to name a member.
///
/// The alternative — a bare `"Enum"` with the case name smuggled in as the first field value — was
/// rejected because it makes the field list mean two different things depending on position, and
/// leaves no spelling at all for a *fieldless* case that reads like the payload-carrying one.
///
/// A **bare enum name** is therefore a rejection, but a teaching one: it names the spelling that
/// would have worked. Resolution tries the whole string as a type first, so a namespaced type name
/// (which contains dots itself) is never mistaken for an `Enum.Variant` pair.
pub fn resolve_construct_target<'a>(info: &'a ReflectionInfo, name: &str) -> ConstructTarget<'a> {
    if let Some(t) = info.type_named(name) {
        return match t.kind {
            TypeKind::Struct | TypeKind::Class => ConstructTarget::Fielded,
            TypeKind::Enum => ConstructTarget::Rejected(format!(
                "`{name}` is an enum: name the variant to construct, as in \
                 `construct(\"{name}.{}\", […])`",
                t.variants
                    .first()
                    .map(|v| v.name.as_str())
                    .unwrap_or("Variant")
            )),
        };
    }
    // Not a type name of its own, so try the `Enum.Variant` reading: split at the LAST dot, since a
    // namespaced enum's own name carries the earlier ones.
    if let Some((enum_name, variant)) = name.rsplit_once('.')
        && let Some(t) = info.type_named(enum_name)
        && t.kind == TypeKind::Enum
    {
        return match t.variants.iter().position(|v| v.name == variant) {
            Some(index) => ConstructTarget::Variant {
                enum_name: &t.name,
                variant: &t.variants[index].name,
                index: index as u32,
                payload: variant_payload_specs(&t.variants[index]),
            },
            None => ConstructTarget::Rejected(format!("`{enum_name}` has no variant `{variant}`")),
        };
    }
    ConstructTarget::Rejected(format!("`{name}` is not a constructible type"))
}

/// The built-in `Validate` trait's name, as the membership table [`ReflectionInfo::trait_impls`]
/// records it (a built-in trait keeps its bare name; a `.noe` in-body `impl Validate`, a standalone
/// `impl Validate for T`, and a native type's ABI-advertised `traits: ["Validate"]` all land here).
pub const VALIDATE_TRAIT: &str = "Validate";

/// Whether a `construct` of `type_name` must run the built value's own `validate()` before handing it
/// back — the shared decision both backends read, so the two cannot disagree about which
/// constructions are validated.
///
/// **`construct` is a decode door, so it validates like one.** `json.try_parse::<T>` / `from_bytes`
/// re-enter a type's `validate()` on the freshly-built value via `TypeRecipe::has_validator`, and that
/// is what makes them exempt from the `@validated` construction ban (E0060) rather than a hole in it:
/// they build directly *and* enforce the invariant. `construct` builds directly from untrusted
/// data the same way, and used to skip the check — so `construct("Email", ["nope"])` handed back an
/// `Email` whose own declaration says it cannot exist, the exact value `json.try_parse` refuses.
///
/// The condition is **implementing `Validate`**, not carrying `@validated` — the identical condition
/// `type_to_recipe` computes `has_validator` from (`satisfies(Validate)`). `@validated` decides where
/// a *literal* may be written; the validator's presence decides whether a data door enforces it, and
/// a type that implements `Validate` without the decorator is already validated by `json.try_parse`.
///
/// Read off the same [`ReflectionInfo::trait_impls`] table `traits_of(value)` and the precise
/// `x is dyn Validate` read, so "does this type validate" has one answer across every surface.
pub fn construct_validates(info: &ReflectionInfo, type_name: &str) -> bool {
    info.type_implements(type_name, VALIDATE_TRAIT)
}

/// One variant's payload as [`FieldSpecData`]s — the same projection [`ReflectionInfo::variant_specs`]
/// reports, reused here so `construct` validates a payload against exactly the schema `variants_of`
/// advertises for it.
fn variant_payload_specs(variant: &VariantInfo) -> Vec<FieldSpecData<'_>> {
    variant
        .fields
        .iter()
        .enumerate()
        .map(|(i, name)| FieldSpecData {
            name,
            ty: variant.field_types.get(i).unwrap_or(&TypeRepr::Dyn),
            // A payload field has no syntax for a default, so it can never be omitted.
            optional: false,
            // Nor for visibility: a variant's payload is public with the case that carries it.
            public: true,
        })
        .collect()
}

/// Where each payload field's value sits in a **named** construction's `provided` list, in payload
/// declaration order — the ordering step an enum needs and a struct does not (an object is filled by
/// field name, an enum's payload is positional).
///
/// Shared rather than mirrored in each backend because it decides the built value's slot order, which
/// is precisely the kind of glue the two have silently disagreed on before. Every payload field is
/// present by the time this runs: payload fields are never optional, so [`plan_construct_named`] has
/// already rejected any omission.
pub fn plan_variant_payload_order(
    payload: &[FieldSpecData<'_>],
    provided: &[String],
) -> Vec<usize> {
    payload
        .iter()
        .filter_map(|spec| provided.iter().position(|n| n == spec.name))
        .collect()
}

/// Validate one supplied value against the field it will fill — the single type check both
/// [`plan_construct`] and [`plan_construct_named`] run, so the positional and named forms of
/// `construct` cannot accept different things.
///
/// Two field kinds are enforced, and the boundary is what the declaration actually pins down:
///
/// * a **concrete scalar** field (`int`/`float`/`bool`/`string`/`bytes`, widths erased) must get a
///   value of that scalar kind;
/// * a **nominal** field (a declared struct, class or enum) must get a value of that same nominal.
///   This was the hole: only the scalar half was checked, so `construct("Outer", {"inner": {"a": 2}})`
///   stored a raw `Map` in a `Inner`-typed field, answered `Ok`, survived `.as<Outer>()` — where
///   `type_of` on the field then reported the *declared* type, actively misdescribing the value — and
///   aborted at the first field read with `no field `b` on map`, several layers from its cause.
///
/// Everything else passes: a `dyn`, a `dyn Trait` (a distinct repr, so an implementor is never
/// rejected), an `Option`/collection, a bare type parameter. The callee's own typing is the backstop
/// there, mirroring how `invoke` treats an unconstrained parameter.
///
/// Names are compared **kind-agnostically**: a declared field type reaches reflection as
/// `TypeRepr::Named("Inner", …)` while a value classifies as `TypeRepr::Struct("Inner", …)`, and both
/// are qualified consistently under a `namespace`. Generic arguments are deliberately not compared —
/// they are erased on some runtime paths, and the head name is what makes the difference between a
/// real instance and a map wearing its slot.
fn check_field_value(
    type_name: &str,
    spec: &FieldSpecData<'_>,
    repr: &TypeRepr,
) -> Result<(), String> {
    if let Some(expected) = enforced_scalar(spec.ty) {
        let got = value_repr_name(repr);
        if got != expected {
            return Err(format!(
                "field `{}` of `{type_name}` expects {expected}, got {got}",
                spec.name
            ));
        }
        return Ok(());
    }
    if let Some(expected) = nominal_name(spec.ty)
        && nominal_name(repr) != Some(expected)
    {
        return Err(format!(
            "field `{}` of `{type_name}` expects {expected}, got {}",
            spec.name,
            value_repr_name(repr)
        ));
    }
    Ok(())
}

/// Refuse setting a **private** field through a reflective construction — the E0035 rule the checker
/// enforces at a source literal, applied at the reflective door that has the same effect.
///
/// A `class` field not declared `pub` (and a native fielded type's `is_public: false` field) is
/// visible only inside the declaring type's own methods, so `Box { secret: 9 }` written outside the
/// class is a compile error. `construct("Box", {"secret": 9})` is the same construction spelled
/// reflectively, and it used to succeed — reflection minting a value the declaration forbids.
///
/// **Context-free, deliberately.** The checker's gate relaxes inside the declaring type's own
/// methods and inside a dev-tier (`@test`) body; a runtime door knows neither its caller's type nor
/// its tier, so it refuses everywhere. That is the conservative half of the asymmetry — the sites the
/// checker exempts are exactly the sites where the *literal* is available, so nothing that was
/// expressible becomes inexpressible.
///
/// Only a **supplied** field is refused. An omitted private field with a default is left to the
/// construction path's own default thunk, which is precisely what a source literal outside the class
/// does too (an omitted field is not in the literal, so E0035 never looks at it).
///
/// The wording is the checker's own (`Checker::report_private_field` with the `Set` verb) so the two
/// doors do not describe one rule two ways.
fn check_field_visible(type_name: &str, spec: &FieldSpecData<'_>) -> Result<(), String> {
    if spec.public {
        return Ok(());
    }
    Err(format!(
        "cannot set private field `{}` of `{type_name}` from outside it",
        spec.name
    ))
}

/// The head **name** of a nominal type repr — a declared struct/class/enum, or the kind-agnostic
/// `Named` a declaration site produces — and `None` for everything else (scalars, collections,
/// `Option`, `dyn`, `dyn Trait`, a function type). The one place the two spellings of "a declared
/// type" are collapsed, so a declaration's repr and a value's repr are comparable.
fn nominal_name(ty: &TypeRepr) -> Option<&str> {
    match ty {
        TypeRepr::Named(n, _)
        | TypeRepr::Struct(n, _)
        | TypeRepr::Class(n, _)
        | TypeRepr::Enum(n, _) => Some(n),
        _ => None,
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
///   * a value supplied for a **private** field (see [`check_field_visible`] — the E0035 rule the
///     checker enforces at a literal, applied at the reflective door);
///   * a value whose runtime scalar kind disagrees with a concrete-scalar field type, or whose
///     nominal head disagrees with a declared struct/class/enum field type;
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
            // Visibility before typing: whether a value of the right type may be supplied at all is
            // the prior question, so a private field reads as private rather than as a type error.
            check_field_visible(type_name, spec)?;
            check_field_value(type_name, spec, &value_reprs[i])?;
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
///   * a provided name that is a **private** field (see [`check_field_visible`]);
///   * a provided value whose runtime scalar kind disagrees with a concrete-scalar field type, or
///     whose nominal head disagrees with a declared struct/class/enum field type;
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
        // Visibility before typing, as in the positional form.
        check_field_visible(type_name, spec)?;
        check_field_value(type_name, spec, repr)?;
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

/// The plan for a **named** by-name invocation: how a `Map<string, dyn>` of arguments becomes the
/// positional argument list a call prologue expects, plus the supplied mask that tells the callee
/// which of its defaults to run. Produced by [`plan_invoke_named`].
#[derive(Debug, PartialEq, Eq)]
pub struct NamedCall {
    /// The supplied mask, indexed over the callee's **declared parameters** (a receiver, where there
    /// is one, is not a parameter — the VM shifts the mask up by one at the call site, exactly as the
    /// compiler does for a statically-named method call). `None` when the named arguments happen to
    /// fill a dense prefix of the parameters, which the ordinary count-based prefix rule already
    /// describes — the same "only carry a mask when it says something new" rule lowering applies, so
    /// a reordering-only named call stays on every prefix-assuming fast path.
    pub supplied: Option<u64>,
    /// For each argument the callee will receive, in ascending parameter order, the index into the
    /// caller's `provided` list it comes from.
    pub order: Vec<usize>,
}

/// The number of declared parameters a **skipping** call can address by name. The supplied mask is
/// one `u64`, and a method's mask is shifted up by one to make room for the receiver at bit 0, so the
/// tighter of the two capacities is 63 — applied uniformly, because one bound that is always right
/// beats two that differ by call kind and diverge between the backends (the tree-walker never
/// shifts, so parameter 63 of a method used to work there and silently misplace its value in the VM).
///
/// Only *skipping* is limited: a call that fills a dense prefix of the parameters needs no mask and
/// is unaffected at any arity, and so is a pure reordering.
pub const MASKED_PARAM_LIMIT: usize = 63;

/// Plan a **named** dynamic invocation: bind a set of `provided` argument names against a callable's
/// declared `params`, in the same shape [`plan_construct_named`] binds field names against a type's
/// field schema. `callee` names the callable for the error messages — the same target string
/// `params_of` takes, so an error points at the thing you would reflect.
///
/// `declared_arity` is the parameter count the *callee itself* declares, cross-checked against
/// `params` (which comes from the reflection artifact). The two disagree only when the artifact holds
/// no signature for this callable at all — a global bound to a closure that was never declared as a
/// top-level `fn`, say — and binding names against a signature that is not the callee's would place
/// arguments on the wrong parameters. That case is refused in the checker's own words rather than
/// guessed at.
///
/// Deliberately **no argument type-checking**, where [`plan_construct_named`] does check a scalar
/// field against its declared type: `invoke`'s positional form has never type-checked its arguments
/// (the callee's own typing is the backstop), so checking them here would make the very same call
/// succeed positionally and fail by name. Errors, all ready-to-surface messages:
///   * a provided name that is not a parameter of the callable;
///   * a parameter that is neither provided nor defaulted;
///   * a skipping call that names a parameter past [`MASKED_PARAM_LIMIT`].
pub fn plan_invoke_named(
    callee: &str,
    params: &[ParamSig],
    declared_arity: usize,
    provided: &[String],
) -> Result<NamedCall, String> {
    if params.len() != declared_arity {
        return Err(format!("`{callee}` does not take named arguments"));
    }
    let mut binding: Vec<Option<usize>> = vec![None; params.len()];
    for (i, name) in provided.iter().enumerate() {
        let Some(p) = params.iter().position(|s| &s.name == name) else {
            return Err(format!("`{callee}` has no parameter `{name}`"));
        };
        binding[p] = Some(i);
    }
    for (p, spec) in params.iter().enumerate() {
        if binding[p].is_none() && !spec.optional {
            return Err(format!(
                "missing required parameter `{}` of `{callee}`",
                spec.name
            ));
        }
    }
    let order: Vec<usize> = binding.iter().flatten().copied().collect();
    // A dense prefix is exactly what an ordinary short argument list means, so it needs no mask —
    // and staying off the mask keeps `invoke("f", {"a": 1})` byte-identical to `invoke("f", [1])`.
    if binding[..order.len()].iter().all(Option::is_some) {
        return Ok(NamedCall {
            supplied: None,
            order,
        });
    }
    // This call skips a parameter, so the mask is load-bearing and every parameter it supplies must
    // fit in it. Checked over the *supplied* indices rather than merely where the first hole falls: a
    // bit that does not fit is dropped from the mask, and the argument then lands on whichever
    // parameter the shortened bit-count points at — a wrong value, with nothing said.
    if let Some(p) = binding
        .iter()
        .rposition(Option::is_some)
        .filter(|p| *p >= MASKED_PARAM_LIMIT)
    {
        return Err(format!(
            "`{callee}` skips a parameter, so it cannot also name `{}` — only the first \
             {MASKED_PARAM_LIMIT} parameters can be named by a skipping call",
            params[p].name
        ));
    }
    let mask = binding
        .iter()
        .enumerate()
        .filter(|(_, b)| b.is_some())
        .fold(0u64, |m, (p, _)| m | (1 << p));
    Ok(NamedCall {
        supplied: Some(mask),
        order,
    })
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
    /// `never` — the **bottom type**, the declared return of a function that does not return.
    ///
    /// It appears only in *declared* positions a signature reflection reads back (`returns_of` on
    /// `os.exit`), never as the tag of a value: no value has this type, which is the whole point.
    /// It is reflected rather than folded into `Unit`/`Named("never")` for the reason
    /// [`crate::BuiltinTy`]'s module docs give — a built-in the reflection decoder does not know
    /// reappears as a nominal type of the same name, and then a *parameter* and a *value* of the
    /// same type disagree.
    ///
    /// **Appended last** in [`type_adt_variants`] on purpose: the prelude `Type` enum's variant
    /// ordinals are baked into compiled programs, so a new variant may only be added at the end.
    Never,
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

/// The runtime abort message for a `type_name::<T>()` whose receiver carries no recorded type
/// argument for `T` — shared by both backends so the two cannot word it differently.
///
/// This is deliberately an abort and not an empty/`"dyn"` answer. The whole point of the surface is
/// to hand a *name* to something that keys on it (a table, a route, a decoder); a plausible-looking
/// wrong name would travel silently, which is exactly the failure the reflected type tag exists to
/// prevent. The instantiation is missing because the value was built where the checker could not
/// determine it, and the fix is at that construction site.
pub fn missing_type_arg_message(type_name: &str, param: &str) -> String {
    let short = crate::short_type_name(type_name);
    format!(
        "`type_name::<{param}>()`: this `{short}` records no type argument for `{param}` \u{2014} build it at a concrete instantiation (`x: {short}<Something> = {short}.new(\u{2026})`), so the construction site can record one"
    )
}

/// The runtime abort message for a call that reaches a **forwarding generic** without supplying
/// its type arguments — shared by both backends so the two cannot word it differently.
///
/// A forwarding generic (poly-values F2b) declares leading type-argument slots, and a checked call
/// by name always fills them from the call node's own type-argument channel. The entry points that
/// cannot are the ones with no static callee type for the checker to resolve against: a `dyn`
/// receiver, a method/bound handle, `invoke`, or a first-class value of the function. Those bind
/// arguments positionally, so proceeding would lay a *value* argument into a type-argument slot and
/// read it as a type-table index — a silently wrong answer.
///
/// So this aborts instead, exactly as `missing_type_arg_message` does on the receiver channel: the
/// instantiation is unknowable at this call, and the fix belongs at the call site that lost it.
pub fn no_instantiation_message(callee: &str, declared: usize, supplied: usize) -> String {
    // The callee is named exactly as it traces — `Type.method` for a method — rather than
    // shortened: which `load` this is, is the first thing you need to know.
    format!(
        "`{callee}` forwards {declared} type argument(s), but no instantiation reaches here \u{2014} this call supplies {supplied} (a `dyn` receiver, a handle, `invoke`, or a first-class value carries none); call it by name at a concrete instantiation, so the call site can record one"
    )
}

impl TypeRepr {
    /// This type's **head name**, the string `type_name::<T>()` yields — the counterpart of
    /// [`crate::TypeRef::head_name`] on the runtime side, so a name read off a value's reflected tag
    /// and one folded from a written type reference agree.
    ///
    /// A nominal reports its (qualified) declared name; a built-in reports its surface spelling.
    /// A container reports its constructor (`List<int>` → `"List"`), matching how `TypeRef::head_name`
    /// answers the written `List<int>`. The forms a bare name cannot spell — an optional, a union, a
    /// function type — have no written head to agree with; they report their constructor name too,
    /// which is more use to a caller than the empty string `TypeRef::head_name` gives them.
    pub fn head_name(&self) -> String {
        match self {
            TypeRepr::Int => "int".to_string(),
            TypeRepr::Float => "float".to_string(),
            TypeRepr::F32 => "f32".to_string(),
            TypeRepr::F64 => "f64".to_string(),
            TypeRepr::IntN { signed, bits } => {
                format!("{}{bits}", if *signed { "i" } else { "u" })
            }
            TypeRepr::Bool => "bool".to_string(),
            TypeRepr::Str => "string".to_string(),
            TypeRepr::Bytes => "bytes".to_string(),
            TypeRepr::Unit => "unit".to_string(),
            TypeRepr::Dyn => "dyn".to_string(),
            TypeRepr::Never => "never".to_string(),
            TypeRepr::DynTrait(t) => t.clone(),
            TypeRepr::List(_) => "List".to_string(),
            TypeRepr::Set(_) => "Set".to_string(),
            TypeRepr::Option(_) => "Option".to_string(),
            TypeRepr::Map(..) => "Map".to_string(),
            TypeRepr::Result(..) => "Result".to_string(),
            TypeRepr::Enum(n, _)
            | TypeRepr::Struct(n, _)
            | TypeRepr::Class(n, _)
            | TypeRepr::Named(n, _) => n.clone(),
            TypeRepr::Fn(..) => "Fn".to_string(),
            TypeRepr::Union(_) => "Union".to_string(),
        }
    }

    /// The **head name of type argument `index`**, or `None` when this type carries no such
    /// argument — the read `type_name::<T>()` performs against a receiver's reflected tag. `None` is
    /// the honest "this value does not record that" and the callers turn it into an abort; it is
    /// never folded into a placeholder name.
    pub fn type_arg_name(&self, index: usize) -> Option<String> {
        self.type_args().get(index).map(|t| t.head_name())
    }

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
            | TypeRepr::Dyn
            // The bottom type has no arguments and never will: it is uninhabited, so there is
            // nothing inside it for the R3 matcher to see into.
            | TypeRepr::Never => Vec::new(),
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
            TypeRepr::Never => "Never",
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
            | TypeRepr::Dyn
            | TypeRepr::Never => AdtFields::None,
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

impl AdtFields {
    /// This payload shape as the positional [`PreludeFieldTy`] list [`prelude_enums`] carries — the
    /// projection that lets the `Type` entry of the prelude table be *derived* from
    /// [`TypeRepr::adt_fields`] instead of re-listed beside it.
    pub fn field_types(self) -> Vec<PreludeFieldTy> {
        use PreludeFieldTy as F;
        match self {
            AdtFields::None => Vec::new(),
            AdtFields::Types(n) => vec![F::SelfEnum; n],
            AdtFields::NameAndArgs => vec![F::Str, F::ListOfSelf],
            AdtFields::TypeList => vec![F::ListOfSelf],
            AdtFields::ParamsAndRet => vec![F::ListOfSelf, F::SelfEnum],
            AdtFields::Name => vec![F::Str],
            AdtFields::IntWidth => vec![F::Int, F::Bool],
        }
    }
}

/// One sample [`TypeRepr`] per variant. Running each through the exhaustive
/// [`TypeRepr::variant_name`] / [`TypeRepr::adt_fields`] matches yields the prelude `Type` enum's
/// full declaration — so the checker registers the ADT from the reflection descriptor itself
/// instead of re-listing the variants and their arities in a table that can drift.
///
/// **Adding a [`TypeRepr`] variant**: both matches above will fail to compile until you handle it;
/// add its sample here at the same time, or the prelude enum will silently lack the variant.
pub fn type_adt_variants() -> Vec<TypeRepr> {
    type_adt_variants_with(AdtHead::DEFAULT)
}

/// [`type_adt_variants`], with `head`'s payload stamped into the samples whose head name is spelled
/// *from* their payload — the nominals, the trait object, and the fixed-width integer.
/// `type_adt_variants()` passes [`AdtHead::DEFAULT`] (no sample stands for a particular type);
/// [`adt_head_name`] passes what it read off a runtime `Type` value, so the sample it selects answers
/// that value's own head rather than a placeholder.
///
/// Parameterizing the one list is deliberate: a separate "which variants spell their head from their
/// payload" table would be a second statement of the same fact, free to drift from this one.
fn type_adt_variants_with(head: AdtHead<'_>) -> Vec<TypeRepr> {
    let any = || Box::new(TypeRepr::Dyn);
    let name = || head.name.to_string();
    let (bits, signed) = head.int_width.unwrap_or((32, true));
    vec![
        TypeRepr::Int,
        TypeRepr::Float,
        TypeRepr::F32,
        TypeRepr::F64,
        TypeRepr::IntN { signed, bits },
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
        // APPENDED LAST, and every future variant must be too: the prelude `Type` enum's variant
        // ordinals come from this order and are baked into compiled programs.
        TypeRepr::Never,
    ]
}

/// The name of the prelude `Type` enum's **head-name accessor** — `type_of(x).name()`. Spelled to
/// match the `name` field every other reflected descriptor carries (`FieldSpec.name`,
/// `ParamInfo.name`, `VariantSpec.name`); it is a zero-argument method rather than a field because
/// `Type` is an enum, and an enum's accessor surface is a method (`.value()` on a backed enum).
///
/// One constant because three places key on it — the checker's built-in method table and both
/// backends' method dispatch — and a typo in one of the three would be a backend divergence.
pub const TYPE_NAME_METHOD: &str = "name";

/// The payload fields of a runtime prelude `Type` value that its own **head name** is spelled from —
/// everything [`adt_head_name`] needs beyond the variant tag, and nothing more, so a backend answers
/// `.name()` by reading at most two payload slots instead of decoding the whole type tree.
///
/// Only two cases contribute anything: a nominal (or trait object), whose head *is* its declared
/// `name` payload, and `Type.IntN(bits, signed)`, whose head is spelled from the width. Every other
/// case's head is its constructor, so [`AdtHead::DEFAULT`] answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtHead<'a> {
    /// The leading `name: string` payload of a nominal (`Type.Struct`/`Class`/`Enum`/`Named`) or a
    /// `Type.DynTrait`. Empty for every other case, which ignores it.
    pub name: &'a str,
    /// A `Type.IntN(bits, signed)`'s width descriptor. `None` for every other case — and for an
    /// `IntN` whose `bits` payload is not a `u8`, which is not a width any type has; that answers the
    /// ADT's declared default width rather than a truncated one.
    pub int_width: Option<(u8, bool)>,
}

impl AdtHead<'_> {
    /// The head payload of no particular type — an empty nominal name and no width. What
    /// [`type_adt_variants`] stamps into its samples.
    pub const DEFAULT: AdtHead<'static> = AdtHead {
        name: "",
        int_width: None,
    };
}

/// The **head name** of a runtime prelude `Type` enum value, from its `variant` tag and the payload
/// fields that head is spelled from ([`AdtHead`]).
///
/// This is the value-side door onto [`TypeRepr::head_name`], which stays the single statement of what
/// a type's head is called: the tag selects the sample descriptor [`type_adt_variants`] already
/// carries — stamped with `head`, so a nominal answers its own name and an `IntN` its own width — and
/// the answer is that descriptor's `head_name`. A mapping keyed directly on variant strings would be
/// a second copy of that table, free to drift from it; this cannot.
///
/// `None` only when `variant` names no `Type` case at all. Every case answers a name, which is the
/// point of the surface: the hand-rolled `match type_of(v) { Type.Class(n, _) => n, … _ => "" }` a
/// consumer writes instead answers the empty string for every shape its match forgot, and that empty
/// name then travels into a table or a route.
pub fn adt_head_name(variant: &str, head: AdtHead<'_>) -> Option<String> {
    type_adt_variants_with(head)
        .iter()
        .find(|repr| repr.variant_name() == variant)
        .map(TypeRepr::head_name)
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
            TypeRepr::Never => f.write_str("never"),
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
        // The bottom type reflects as itself. It reaches here only from a *declared* position — a
        // `never` return read back by `returns_of` — never from a value's tag, because no value has
        // it. Folding it into `Unit` would report `os.exit` as returning `void`, which is exactly
        // the lie this reflection exists to state precisely.
        BuiltinTy::Never => TypeRepr::Never,
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
        TypeRef::DynTrait { trait_name, .. } => TypeRepr::DynTrait(trait_name.to_string()),
        TypeRef::Tuple { .. } => TypeRepr::Dyn,
        // A `Self::Name` projection is not statically a concrete type here (resolution is per-impl at
        // the checker); reflect it as the dynamic top, like a tuple (slice 1a).
        TypeRef::AssocProjection { .. } => TypeRepr::Dyn,
        TypeRef::Fn { params, ret, .. } => {
            TypeRepr::Fn(params.iter().map(recur).collect(), Box::new(recur(ret)))
        }
        TypeRef::Named { name, args, .. } => named_repr(name.as_str(), args, nominal, top),
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
/// (parameterized rows), and `#[Timeout(seconds)]` (raise or disable this test's per-test deadline).
/// The single source of truth shared by the checker's prelude registration and the runner that
/// interprets them.
// D2b — the tier attributes live under their tier's namespace (no global attribute namespace), so
// these constants are the **qualified identity** every path shares: the loader rewrites a
// user-written `#[Skip]` (after `use std.test.{Skip}`) to this FQN, the reflection manifest carries
// it, and the runner reads it. A user must `use std.test` to apply them.
pub const TEST_ATTR_SKIP: &str = "std.test.Skip";
pub const TEST_ATTR_NAME: &str = "std.test.Name";
pub const TEST_ATTR_GROUP: &str = "std.test.Group";
pub const TEST_ATTR_DATA: &str = "std.test.Data";
/// `#[Timeout(seconds)]` — the per-test deadline override. The runner bounds every test by a suite
/// default (`noeta test --timeout`, else the runner's own); this attribute raises it for the one test
/// that legitimately needs longer, and `#[Timeout(0)]` opts that test out of the bound entirely.
pub const TEST_ATTR_TIMEOUT: &str = "std.test.Timeout";

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
///
/// `attrs` is the exact counterpart of [`PARAM_INFO`]'s, for the exact reason that one exists: a
/// schema-deriving library walks a callable's parameters with `params_of` and a type's fields with
/// `field_specs_of`, and the two are meant to be **the same walk producing the same bytes**. While
/// only the parameter descriptor carried its annotation they could not be — the field door had to
/// re-join `attributes_of::<T>()`'s `"<Type>.<field>"` targets by hand, once per field, and every
/// such consumer re-implemented the key format. It is a view, not a second table: the values come
/// from the same manifest rows `attributes_of` returns, via
/// [`ReflectionInfo::field_attributes_for`], and an unannotated field reports an empty list rather
/// than an absence.
pub const FIELD_SPEC: &str = "FieldSpec";

/// The prelude `VariantSpec` struct — the element type of the **type-level** enum query
/// `variants_of::<T>()` / `variants_of(name)`: `{ name: string, payload: List<FieldSpec>, backing:
/// ?dyn }`, one per declared variant of an enum TYPE, in declaration order. The enum twin of
/// [`FIELD_SPEC`], and it reuses `FieldSpec` for the payload rather than introducing a second
/// member-shape vocabulary: a variant payload is ordinary declared-field data. `backing` is the
/// variant's value in a backed enum (`some("pending")` / `some(3)`) and `none` for a plain enum.
/// Registered like `FieldSpec`; both backends materialize the matching shape.
pub const VARIANT_SPEC: &str = "VariantSpec";

/// The prelude `Layout` enum's name — the storage-layout vocabulary `@packed` takes
/// (`@packed(Layout.Column)`). Like [`SEMANTIC_ENUM`] it is directive vocabulary, not a runtime
/// value: the parser resolves the argument syntactically, and the prelude registers the enum so
/// tooling (hover, completion, docs) sees one authoritative declaration.
pub const LAYOUT_ENUM: &str = "Layout";

/// The `Layout.*` variants, in declaration order, mirroring [`PackedLayout`](crate::PackedLayout):
/// `Row` (AoS, the bare-`@packed` default) and `Column` (SoA, P-SIMD). The single source of truth
/// the parser validates `@packed(Layout.…)` against and completion offers.
pub const LAYOUT_VARIANTS: &[&str] = &["Row", "Column"];

/// The `Ordering` prelude enum's name — what `.compare()` returns and derived `Comparable` orders
/// by. Namable like any other prelude enum, so `Ordering.Less` is constructible, not only receivable.
pub const ORDERING_ENUM: &str = "Ordering";

/// The `Ordering.*` variants, in declaration order. The order is load-bearing (`Less < Equal <
/// Greater`): it is the variant index derived `Comparable` compares by, and both backends bake it
/// into the values `.compare()` returns.
pub const ORDERING_VARIANTS: &[&str] = &["Less", "Equal", "Greater"];

/// The `Cancelled` prelude enum's name (Track A.8) — the `Err` payload of `h.join(): Result<T,
/// Cancelled>`. A one-variant marker enum, namable and constructible like the rest.
pub const CANCELLED_ENUM: &str = "Cancelled";

/// The `Cancelled.*` variants — the single `Cancelled` marker case.
pub const CANCELLED_VARIANTS: &[&str] = &["Cancelled"];

/// The declared type of one prelude-enum variant's payload field — the closed vocabulary
/// [`PreludeEnum`] needs to describe every prelude variant. Deliberately tiny: the prelude enums
/// are fixed declarations, and this is exactly the set their payloads use. A consumer that types
/// fields (the checker) maps each arm onto its own lattice; a consumer that only lays out slots
/// (both backends) needs nothing but the field *count*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreludeFieldTy {
    /// The enum itself — a recursive `Type` field (`Type.List(inner: Type)`).
    SelfEnum,
    /// `List<Self>` — a recursive list field (`Type.Union(members: List<Type>)`).
    ListOfSelf,
    Str,
    Int,
    Bool,
}

/// One prelude enum variant: its name and its positional payload fields (empty for a fieldless
/// case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreludeVariant {
    pub name: String,
    pub fields: Vec<PreludeFieldTy>,
}

impl PreludeVariant {
    /// The variant's positional **slot names**, synthesized `_0`, `_1`, … — the same convention a
    /// native enum's payload uses ([`crate`]-external `ext_enum_type_info`), and for the same
    /// reason: a prelude variant's payload is positional, so only the slot *count* is load-bearing.
    /// Enum equality, matching, and display all compare by enum name + variant + positional data,
    /// never by field name.
    pub fn field_names(&self) -> Vec<String> {
        (0..self.fields.len()).map(|i| format!("_{i}")).collect()
    }

    /// The variant's payload as reflection [`TypeRepr`]s, parallel to [`Self::field_names`] —
    /// `enum_name` is the enum the variant belongs to, so the two recursive arms resolve to it.
    /// The reflection twin of the checker's `prelude_field_type`, and the reason
    /// [`prelude_type_infos`] can report a prelude variant's payload as precisely as a `.noe`
    /// enum's.
    pub fn field_reprs(&self, enum_name: &str) -> Vec<TypeRepr> {
        self.fields.iter().map(|f| f.repr(enum_name)).collect()
    }
}

impl PreludeFieldTy {
    /// This payload field's declared type as a reflection [`TypeRepr`], given the enum it belongs
    /// to. The recursive arms name that enum (`Type.List(inner: Type)` → `Type.Enum("Type", [])`),
    /// exactly as the checker's lattice projection does — one vocabulary, two lattices.
    pub fn repr(self, enum_name: &str) -> TypeRepr {
        let this = || TypeRepr::Enum(enum_name.to_string(), Vec::new());
        match self {
            PreludeFieldTy::SelfEnum => this(),
            PreludeFieldTy::ListOfSelf => TypeRepr::List(Box::new(this())),
            PreludeFieldTy::Str => TypeRepr::Str,
            PreludeFieldTy::Int => TypeRepr::Int,
            PreludeFieldTy::Bool => TypeRepr::Bool,
        }
    }
}

/// A **prelude enum**: one of the enums the language itself declares, which a program can name,
/// match on, and construct without declaring anything.
///
/// This is the *one* registration every consumer reads — the checker's symbol tables, the
/// tree-walker's global scope, and the compiler's type environment. Before it existed each of the
/// three carried its own hand-written variant list, and the lists disagreed: only `Ordering` was
/// seeded into the two backends, so `Type.Unit` / `Semantic.TrustBoundary` / `Layout.Row` /
/// `Cancelled.Cancelled` type-checked and then aborted with E0005 at run time; and only `Ordering`
/// was *missing* from the checker's enum table, so a non-exhaustive `match` on it passed the
/// exhaustiveness rule and aborted at run time instead. A single table cannot drift that way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreludeEnum {
    /// The enum's name, as a program spells it.
    pub name: &'static str,
    /// Its variants, in declaration order — the order that fixes each variant's index, which is
    /// what derived `Comparable` orders by and what both backends bake into a constructed value.
    pub variants: Vec<PreludeVariant>,
    /// Whether the enum is implicitly `@semantic` — role-eligible, so `@role(Enum.Variant)` names
    /// it. True only for [`SEMANTIC_ENUM`].
    pub semantic: bool,
}

impl PreludeEnum {
    /// The variant of this name, or `None` if the enum has no such case.
    pub fn variant(&self, name: &str) -> Option<&PreludeVariant> {
        self.variants.iter().find(|v| v.name == name)
    }

    /// The variant's **declaration index** — derived `Comparable`'s primary key.
    pub fn variant_index(&self, name: &str) -> Option<u32> {
        self.variants
            .iter()
            .position(|v| v.name == name)
            .map(|i| i as u32)
    }
}

/// Every prelude enum, in registration order — the one table the checker and both backends build
/// their `Ordering` / `Cancelled` / `Semantic` / `Layout` / `Type` declarations from.
///
/// `Option` and `Result` are deliberately absent: they are constructed through the `none` / `some`
/// / `Ok` / `Err` prelude bindings rather than by naming the enum, so there is no `Option.none`
/// spelling for this table to make work.
///
/// The `Type` entry is *derived* from [`type_adt_variants`] rather than listed, so adding a
/// [`TypeRepr`] variant cannot leave the constructible ADT one case short.
pub fn prelude_enums() -> Vec<PreludeEnum> {
    let fieldless = |names: &[&str]| -> Vec<PreludeVariant> {
        names
            .iter()
            .map(|name| PreludeVariant {
                name: (*name).to_string(),
                fields: Vec::new(),
            })
            .collect()
    };
    vec![
        PreludeEnum {
            name: ORDERING_ENUM,
            variants: fieldless(ORDERING_VARIANTS),
            semantic: false,
        },
        PreludeEnum {
            name: CANCELLED_ENUM,
            variants: fieldless(CANCELLED_VARIANTS),
            semantic: false,
        },
        PreludeEnum {
            name: SEMANTIC_ENUM,
            variants: fieldless(SEMANTIC_VARIANTS),
            semantic: true,
        },
        PreludeEnum {
            name: LAYOUT_ENUM,
            variants: fieldless(LAYOUT_VARIANTS),
            semantic: false,
        },
        PreludeEnum {
            name: TYPE_ENUM,
            variants: type_adt_variants()
                .iter()
                .map(|repr| PreludeVariant {
                    name: repr.variant_name().to_string(),
                    fields: repr.adt_fields().field_types(),
                })
                .collect(),
            semantic: false,
        },
    ]
}

/// Every type **the language itself declares** as a reflectable [`TypeInfo`] — the projection of
/// [`prelude_enums`] and [`prelude_structs`] that makes them answer the type-level reflection queries
/// (`variants_of`, `field_specs_of`) the way every other declared type does.
///
/// [`build`] walks a *program*, and these thirteen declarations belong to the language, not to the
/// program — so before this projection existed they were absent from the artifact entirely, and
/// `variants_of("Ordering")` / `field_specs_of("Ordering")` were **both** empty. By the pair rule
/// those two queries are documented under (see [`ReflectionInfo::variant_specs`]), both-empty is
/// the one honest "I know nothing about this name" — so reflection reported a type the language
/// itself declares, and `Ordering.try_from("Less")` accepts, as unknown. A framework walking a
/// `type_of` result saw `Type.Enum("Ordering", [])` and then got no cases for it, which is the same
/// silently-wrong outcome `variants_of` was introduced to remove, one level up.
///
/// The **structs** were invisible the same way, and one level worse: `FieldSpec` and `VariantSpec`
/// are the types a reflection consumer walks *while* reflecting, so a schema deriver that recursed
/// into its own result type got the both-empty answer about it. They are not symmetric with the enums
/// — a prelude struct's field *types* used to be stated at the checker's registration sites, where
/// neither reflection nor the backends could see them — so [`PreludeStruct`] carries them now
/// ([`PreludeStructFieldTy`]) and each consumer projects that one statement onto its own vocabulary.
/// Every field is mandatory and none carries a default: a prelude struct has no default syntax, and
/// reporting a default the declaration does not have would misdescribe what `construct` requires.
///
/// Derived from the one shared table rather than re-listed, for the reason the table exists: the
/// checker, both backends, and the compiler already read it, and reflection was the last consumer
/// that did not. Seeded into the artifact by `noeta_check::extend_reflection`, which skips any name
/// the program itself declares — so a user's own `enum Ordering` or `struct FieldEntry` shadows the
/// prelude one here exactly as it does everywhere else.
pub fn prelude_type_infos() -> Vec<TypeInfo> {
    let enums = prelude_enums().into_iter().map(|decl| TypeInfo {
        name: decl.name.to_string(),
        kind: TypeKind::Enum,
        // An enum declares no fields; `field_specs_of` on one reports the empty list, and it is
        // `variants_of` that carries the schema. The pair is what distinguishes it from a
        // field-less struct.
        fields: Vec::new(),
        field_types: Vec::new(),
        field_optional: Vec::new(),
        field_public: Vec::new(),
        field_defaults: Vec::new(),
        variants: decl
            .variants
            .iter()
            .map(|v| VariantInfo {
                name: v.name.clone(),
                fields: v.field_names(),
                field_types: v.field_reprs(decl.name),
                // No prelude enum is backed — their cases are their wire values, the same
                // statement `register_prelude_enums` makes on the checker side.
                backing: None,
            })
            .collect(),
    });
    let structs = prelude_structs().into_iter().map(|decl| TypeInfo {
        name: decl.name.to_string(),
        kind: TypeKind::Struct,
        field_types: decl.field_types.iter().map(|t| t.repr()).collect(),
        // No prelude struct field declares a default, so none may be omitted at construction — the
        // mandatory/no-default pair `plan_construct` enforces.
        field_optional: decl.fields.iter().map(|_| false).collect(),
        // A prelude struct is a value struct, so every field is public — the same statement
        // `field_public` makes about a `.noe` struct, and what keeps `construct("FieldEntry", …)`
        // (the prelude-structs-constructible surface) working through the visibility gate.
        field_public: decl.fields.iter().map(|_| true).collect(),
        field_defaults: decl.fields.iter().map(|_| None).collect(),
        fields: decl.fields,
        // A struct declares no variants; `variants_of` on one reports the empty list, and the
        // non-empty `field_specs_of` is what says "a struct, and here is its schema".
        variants: Vec::new(),
    });
    enums.chain(structs).collect()
}

/// The `Attributed<T>` prelude struct's name — `{ target: string, value: T }`, the element type of
/// `attributes_of::<T>()`'s result.
pub const ATTRIBUTED: &str = "Attributed";

/// The declared type of one prelude **struct** field — the closed vocabulary [`PreludeStruct`] needs
/// to describe every prelude record's fields, and the struct twin of [`PreludeFieldTy`].
///
/// Deliberately tiny and closed, for the same reason its enum counterpart is: the prelude structs are
/// eight fixed declarations, and this is exactly the set their fields use. A consumer that types
/// fields (the checker) maps each arm onto its own lattice; reflection maps each onto a [`TypeRepr`]
/// ([`PreludeStructFieldTy::repr`]). Recursion goes through `&'static` rather than `Box` so the whole
/// vocabulary stays `Copy` and a declaration can be written as a `const`, matching how the extension
/// ABI spells its own signature types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreludeStructFieldTy {
    Str,
    Bool,
    /// The dynamic top — the honest type of a field that genuinely holds anything
    /// (`FieldEntry.value` is one field's value, of whatever type that field has).
    Dyn,
    /// The prelude `Type` ADT ([`TYPE_ENUM`]) — a *reflected type as data*, which is what the `type`
    /// field of `ParamInfo` / `FieldSpec` carries.
    TypeAdt,
    /// The abstract `Enum` **kind**: any enum, not one named enum. `RoleBinding.role` is this, because
    /// a role binding's role may come from any `@semantic` enum. It reflects as [`TypeRepr::Dyn`],
    /// which is not a hole invented here but the same answer the checker's own lattice→reflection
    /// projection gives an abstract kind — the value in such a field carries its own concrete enum
    /// tag, and the declaration genuinely names no single type.
    EnumKind,
    /// Another prelude struct, by name (`VariantSpec.payload` is a `List` of [`FIELD_SPEC`]).
    Struct(&'static str),
    /// This struct's own **type parameter** (`Attributed<T>.value`), by name — reflected as the
    /// kind-agnostic `Named`, exactly as a `.noe` generic struct's `T`-typed field is.
    Param(&'static str),
    List(&'static PreludeStructFieldTy),
    Option(&'static PreludeStructFieldTy),
    /// `() -> void` — a first-class function value taking nothing and returning nothing
    /// (`TierRoot.run`, the activated fn a tier runner calls).
    VoidFn,
}

impl PreludeStructFieldTy {
    /// This field's declared type as a reflection [`TypeRepr`] — the projection that lets
    /// [`prelude_type_infos`] report a prelude struct's schema as precisely as a `.noe` struct's, and
    /// the reflection twin of the checker's `prelude_struct_field_type`.
    ///
    /// A nominal reflects **kind-agnostically** ([`TypeRepr::Named`]), which is what a declared
    /// struct/class/enum annotation reflects as everywhere else in a declared position — so one type
    /// in a field's type slot reads the same whether the struct was declared by the language or by a
    /// program.
    pub fn repr(self) -> TypeRepr {
        match self {
            PreludeStructFieldTy::Str => TypeRepr::Str,
            PreludeStructFieldTy::Bool => TypeRepr::Bool,
            PreludeStructFieldTy::Dyn => TypeRepr::Dyn,
            PreludeStructFieldTy::TypeAdt => TypeRepr::Named(TYPE_ENUM.to_string(), Vec::new()),
            PreludeStructFieldTy::EnumKind => TypeRepr::Dyn,
            PreludeStructFieldTy::Struct(n) => TypeRepr::Named(n.to_string(), Vec::new()),
            PreludeStructFieldTy::Param(n) => TypeRepr::Named(n.to_string(), Vec::new()),
            PreludeStructFieldTy::List(inner) => TypeRepr::List(Box::new(inner.repr())),
            PreludeStructFieldTy::Option(inner) => TypeRepr::Option(Box::new(inner.repr())),
            PreludeStructFieldTy::VoidFn => TypeRepr::Fn(Vec::new(), Box::new(TypeRepr::Unit)),
        }
    }
}

/// A **prelude struct**: one of the record types the language declares for you, and the *only*
/// statement of its field list and field types. Everything that builds or registers one reads it from
/// here — the checker's records, both backends' `attributes_of` / `roles_of` / `params_of` /
/// `fields_of` / `field_specs_of` materializations, and the type environments that make a
/// source-written literal construct. Before the table those field lists were hand-copied at eight
/// sites across two backends, and neither backend registered the types at all, so `FieldEntry { name:
/// "a", value: 1 }` type-checked and aborted with E0005 at run time — the struct half of the
/// prelude-enum hole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreludeStruct {
    /// The struct's name, as a program spells it.
    pub name: &'static str,
    /// Its fields, in **slot order** — the order a materialized instance's values are built in, and
    /// the order a shape interns them under, so a materialized value and a constructed one are the
    /// same value.
    pub fields: Vec<String>,
    /// Each field's **declared type**, positionally parallel to [`Self::fields`].
    ///
    /// These lived at the checker's eight `register_prelude_struct` call sites — the field *names*
    /// came from this shared table and the field *types* from the caller, because the checker's
    /// lattice is not visible from `noeta-ast`. The consequence was that reflection, which can see
    /// neither, had no field types to report at all: `field_specs_of("FieldSpec")` and
    /// `variants_of("FieldSpec")` were **both** empty, the pair that means "I know nothing about this
    /// name", about the very types a reflection consumer walks *while* reflecting. Carrying a closed
    /// [`PreludeStructFieldTy`] vocabulary here instead lets each consumer project it onto its own
    /// vocabulary (the checker onto its lattice, reflection onto [`TypeRepr`]) from one statement of
    /// the declaration — the same move [`prelude_enums`] made for the enum half, and it deletes the
    /// hand-written lists and the drift they invited.
    pub field_types: Vec<PreludeStructFieldTy>,
}

/// Every prelude struct, in registration order. The counterpart of [`prelude_enums`].
///
/// `ParamInfo` and `FieldSpec` are registered like the rest even though a source literal cannot
/// currently spell them (their `type` field collides with the `type` keyword in struct-literal
/// position): the registration is what makes their materialization read one field list rather than
/// two, and the day the literal becomes spellable it constructs. `VariantSpec` has no such collision
/// and is spellable today. (`construct("ParamInfo", …)` reaches them regardless — it keys on the
/// schema this table states, not on the literal syntax.)
///
/// Each field's type is stated here with the reasoning that used to sit at the checker's registration
/// sites, because this is now the one place it is said:
///
/// * `Attributed<T> { target: string, value: T }` — the element type of `attributes_of::<T>()`. An
///   ordinary generic struct, so `a.value`'s instantiation to `T` reuses the generic path.
/// * `RoleBinding { target: string, role: Enum }` — the element type of `roles_of()`. `role` is the
///   abstract `Enum` kind because a binding's role may be any `@semantic` enum, not a single type.
/// * `ParamInfo { name: string, type: Type, optional: bool, attrs: List<dyn> }` — the element type of
///   `params_of()`. `type` is the parameter's declared type as the same `Type` ADT `type_of` returns;
///   `optional` is true when the parameter declared a default, so a signature-driven consumer can
///   tell a required parameter from one a call may omit; `attrs` is `List<dyn>` because a parameter's
///   attributes are heterogeneous (`#[Arg]` and `#[Sensitive]` are different structs), so there is no
///   one element type to name — a consumer recovers the one it wants by narrowing.
/// * `FieldEntry { name: string, value: dyn }` — the element type of `fields_of()`, a value-level
///   view, so `value` is whatever the field holds.
/// * `FieldSpec { name: string, type: Type, optional: bool, attrs: List<dyn> }` — the element type of
///   the TYPE-level `field_specs_of`. The type-level twin of `FieldEntry`: `type` is the field's
///   declared type (precise, from the declaration — not a value's erased head). `attrs` is the
///   member half of what `ParamInfo.attrs` is for a parameter, `List<dyn>` for the same reason
///   (a field's attributes are heterogeneous, so there is no one element type to name).
/// * `VariantSpec { name: string, payload: List<FieldSpec>, backing: ?dyn }` — the element type of
///   `variants_of`, the enum twin of `FieldSpec`. `payload` reuses `FieldSpec` because a variant
///   payload IS ordinary declared field data, so the two halves of the type-level surface share one
///   member vocabulary; `backing` is `?dyn` because a backed enum's value may be a string or an int.
/// * `TierRoot { name: string, run: () -> void }` — one activated fn per root: its name, and the fn
///   itself as a first-class value the runner calls.
/// * `TierText { target: string, text: string }` — its verbatim-body twin.
pub fn prelude_structs() -> Vec<PreludeStruct> {
    use PreludeStructFieldTy as F;
    let s = |name: &'static str, fields: &[(&str, PreludeStructFieldTy)]| PreludeStruct {
        name,
        fields: fields.iter().map(|(n, _)| (*n).to_string()).collect(),
        field_types: fields.iter().map(|(_, t)| *t).collect(),
    };
    vec![
        s(ATTRIBUTED, &[("target", F::Str), ("value", F::Param("T"))]),
        s(ROLE_BINDING, &[("target", F::Str), ("role", F::EnumKind)]),
        s(
            PARAM_INFO,
            &[
                ("name", F::Str),
                ("type", F::TypeAdt),
                ("optional", F::Bool),
                ("attrs", F::List(&F::Dyn)),
            ],
        ),
        s(FIELD_ENTRY, &[("name", F::Str), ("value", F::Dyn)]),
        s(
            FIELD_SPEC,
            &[
                ("name", F::Str),
                ("type", F::TypeAdt),
                ("optional", F::Bool),
                ("attrs", F::List(&F::Dyn)),
            ],
        ),
        s(
            VARIANT_SPEC,
            &[
                ("name", F::Str),
                ("payload", F::List(&F::Struct(FIELD_SPEC))),
                ("backing", F::Option(&F::Dyn)),
            ],
        ),
        s(TIER_ROOT, &[("name", F::Str), ("run", F::VoidFn)]),
        s(TIER_TEXT, &[("target", F::Str), ("text", F::Str)]),
    ]
}

/// The prelude struct of this name, or `None` when the name is not one.
pub fn prelude_struct(name: &str) -> Option<PreludeStruct> {
    prelude_structs().into_iter().find(|s| s.name == name)
}

/// The fields of the prelude struct `name`, in slot order — the lookup every materialization site
/// uses instead of re-listing them. Panics for a name that is not a prelude struct: every call site
/// passes one of the constants above, so a miss is a programming error, not a runtime condition.
pub fn prelude_struct_fields(name: &str) -> Vec<String> {
    prelude_struct(name)
        .unwrap_or_else(|| panic!("`{name}` is not a prelude struct"))
        .fields
}

/// The **declaration index** of `variant` in the prelude enum `enum_name`, or `None` when the pair
/// names no prelude variant (a user enum, or `Option`/`Result`). Both backends stamp this onto the
/// values they materialize — a reflected `type_of(…)` value and a source-written `Type.Int` then
/// order identically under derived `Comparable`, instead of one of them being unordered.
pub fn prelude_variant_index(enum_name: &str, variant: &str) -> Option<u32> {
    prelude_enums()
        .iter()
        .find(|e| e.name == enum_name)
        .and_then(|e| e.variant_index(variant))
}

/// The positional **slot names** of `variant` in the prelude enum `enum_name`, or `None` when the
/// pair names no prelude variant. The shape-building counterpart of [`prelude_variant_index`], so a
/// materialized prelude enum value and a source-constructed one intern the *same* shape.
pub fn prelude_variant_field_names(enum_name: &str, variant: &str) -> Option<Vec<String>> {
    prelude_enums()
        .iter()
        .find(|e| e.name == enum_name)
        .and_then(|e| e.variant(variant))
        .map(|v| v.field_names())
}

/// Push each field's `#[...]` attributes, keyed by the qualified `Type.field` name (mirroring the
/// `Type.method` convention), so a `#[Column(...)]` on a property surfaces distinctly per owner.
fn push_field_attrs(manifest: &mut Vec<AttributeRecord>, type_name: &str, fields: &[FieldDecl]) {
    for field in fields {
        let target = field_attr_target(type_name, &field.name);
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
            name: attr.name.to_string(),
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

    fn sig(name: &str, optional: bool) -> ParamSig {
        ParamSig {
            name: name.to_string(),
            ty: TypeRepr::Int,
            optional,
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| (*n).to_string()).collect()
    }

    /// The named-invoke planner's three shapes. A **gap** — the whole point of the named form, and
    /// the one thing a positional list cannot say — carries a mask; a pure **reordering** and a
    /// dense **prefix** do not, because the ordinary count rule already describes them and staying
    /// off the mask keeps every prefix-assuming fast path applying.
    #[test]
    fn plan_invoke_named_masks_only_a_skipping_call() {
        let params = [sig("a", false), sig("b", true), sig("c", true)];
        // `{a, c}` skips `b`: parameters 0 and 2, so bits 0 and 2.
        let gap = plan_invoke_named("f", &params, 3, &names(&["a", "c"])).expect("plans");
        assert_eq!(gap.supplied, Some(0b101));
        assert_eq!(gap.order, vec![0, 1]);
        // A reordering fills a prefix; `order` permutes the caller's list into parameter order.
        let reordered = plan_invoke_named("f", &params, 3, &names(&["b", "a"])).expect("plans");
        assert_eq!(reordered.supplied, None);
        assert_eq!(reordered.order, vec![1, 0]);
        // A prefix is an ordinary short argument list.
        let prefix = plan_invoke_named("f", &params, 3, &names(&["a"])).expect("plans");
        assert_eq!(prefix.supplied, None);
        assert_eq!(prefix.order, vec![0]);
    }

    /// Every rejection is a message, never a panic — `invoke`'s contract is that resolution failures
    /// are values. The wording mirrors `plan_construct_named`'s field-side equivalents.
    #[test]
    fn plan_invoke_named_rejects_in_constructs_vocabulary() {
        let params = [sig("a", false), sig("b", true)];
        assert_eq!(
            plan_invoke_named("f", &params, 2, &names(&["a", "nope"])),
            Err("`f` has no parameter `nope`".to_string())
        );
        assert_eq!(
            plan_invoke_named("f", &params, 2, &names(&["b"])),
            Err("missing required parameter `a` of `f`".to_string())
        );
        // A signature that is not the callee's (a global holding an undeclared closure) is refused
        // rather than bound against, which would place arguments on the wrong parameters.
        assert_eq!(
            plan_invoke_named("f", &params, 3, &names(&["a"])),
            Err("`f` does not take named arguments".to_string())
        );
    }

    /// The mask is one `u64` (a method's shifted up by one for the receiver), so a **skipping** call
    /// cannot name a parameter past the limit. Checked over the parameters the call *supplies*, not
    /// over where its first hole falls: an out-of-range bit is simply dropped, and the argument then
    /// lands on whichever parameter the shortened bit-count points at — a silently wrong value.
    /// A dense prefix carries no mask, so it is unaffected at any arity.
    #[test]
    fn plan_invoke_named_bounds_a_skipping_call_by_what_it_supplies() {
        let mut params = vec![sig("a", false)];
        for i in 1..80 {
            params.push(sig(&format!("p{i}"), true));
        }
        let far = format!("p{}", MASKED_PARAM_LIMIT);
        assert_eq!(
            plan_invoke_named("f", &params, params.len(), &names(&["a", &far])),
            Err(format!(
                "`f` skips a parameter, so it cannot also name `{far}` — only the first \
                 {MASKED_PARAM_LIMIT} parameters can be named by a skipping call"
            ))
        );
        // The last in-range parameter still plans, and its bit is the top one that fits.
        let near = format!("p{}", MASKED_PARAM_LIMIT - 1);
        let ok =
            plan_invoke_named("f", &params, params.len(), &names(&["a", &near])).expect("in range");
        assert_eq!(ok.supplied, Some(1u64 | (1u64 << (MASKED_PARAM_LIMIT - 1))));
        // A dense prefix over the same wide signature needs no mask at all.
        let prefix: Vec<String> = std::iter::once("a".to_string())
            .chain((1..70).map(|i| format!("p{i}")))
            .collect();
        let dense = plan_invoke_named("f", &params, params.len(), &prefix).expect("prefix");
        assert_eq!(dense.supplied, None);
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

    /// The deliberate asymmetry between the two signature queries, pinned as data: `params_for`
    /// folds an unknown target into the empty slice (an empty parameter list is a legitimate
    /// answer), while `returns_for` keeps it as `None` — a `void` callable answers `Some(Unit)`, so
    /// collapsing the two would make a mistyped target indistinguishable from a `void` method.
    #[test]
    fn returns_for_distinguishes_a_void_callable_from_an_unknown_one() {
        let info = ReflectionInfo {
            params: vec![
                ParamRecord {
                    target: "tick".to_string(),
                    params: Vec::new(),
                    ret: TypeRepr::Unit,
                },
                ParamRecord {
                    target: "Api.list".to_string(),
                    params: Vec::new(),
                    ret: TypeRepr::List(boxed(TypeRepr::Str)),
                },
            ],
            ..Default::default()
        };
        // A `void` callable and an unknown one both have no parameters — `params_of` cannot tell
        // them apart, which is exactly why `returns_of` must.
        assert!(info.params_for("tick").is_empty());
        assert!(info.params_for("nope").is_empty());
        assert_eq!(info.returns_for("tick"), Some(&TypeRepr::Unit));
        assert_eq!(
            info.returns_for("Api.list"),
            Some(&TypeRepr::List(boxed(TypeRepr::Str)))
        );
        assert_eq!(info.returns_for("nope"), None);
        assert_eq!(info.returns_for("Api.missing"), None);
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
                field_public: Vec::new(),
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
                field_public: Vec::new(),
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

    /// `accumulate` re-derives the role index from the merged manifest and the merged **tag** table,
    /// so a fragment that re-declares an annotated function without re-declaring the attribute that
    /// tags it keeps the binding — and the manifest keeps the position it superseded, rather than
    /// moving to the end of an ordered surface callers read back.
    #[test]
    fn accumulate_re_derives_roles_and_supersedes_in_place() {
        let attr = |target: &str, name: &str| AttributeRecord {
            target: target.to_string(),
            target_span: Span::empty_at(0),
            name: name.to_string(),
            args: Vec::new(),
        };
        let mut base = ReflectionInfo {
            manifest: vec![attr("greet", "Page"), attr("Api.list", "Page")],
            role_tags: vec![RoleTagRecord {
                attribute: "Page".to_string(),
                enum_name: "WebRole".to_string(),
                variant: "Controller".to_string(),
            }],
            ..Default::default()
        };
        base.roles = derive_roles(&base.manifest, &base.role_tags);
        assert_eq!(base.roles.len(), 2);

        // The fragment is a body edit of `greet` alone: its manifest row, and NOT the `@role`-tagged
        // `struct Page` that confers the role — that declaration is unchanged and stayed behind.
        let fragment = ReflectionInfo {
            manifest: vec![attr("greet", "Page")],
            ..Default::default()
        };
        base.accumulate(fragment);

        let targets: Vec<&str> = base.manifest.iter().map(|a| a.target.as_str()).collect();
        assert_eq!(
            targets,
            vec!["greet", "Api.list"],
            "the superseded row lands where it was, not after the rows the fragment never touched"
        );
        let bindings: Vec<(&str, &str)> = base
            .roles
            .iter()
            .map(|r| (r.target.as_str(), r.variant.as_str()))
            .collect();
        assert_eq!(
            bindings,
            vec![("greet", "Controller"), ("Api.list", "Controller")],
            "the role the unchanged attribute confers survives the swap of what it annotates"
        );
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
            name: crate::Name::written(name),
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
                field_public: Vec::new(),
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

    /// **Totality is the surface's whole point**: every prelude `Type` case answers a non-empty head
    /// name through [`adt_head_name`], and the answer is exactly its descriptor's
    /// [`TypeRepr::head_name`]. A new `TypeRepr` variant that forgot its name — the hole a consumer's
    /// hand-rolled `match … _ => ""` has — fails here.
    #[test]
    fn every_type_adt_case_answers_a_head_name() {
        let nominal = AdtHead {
            name: "app.storage.Todo",
            int_width: Some((8, false)),
        };
        for repr in type_adt_variants() {
            let variant = repr.variant_name();
            let answered = adt_head_name(variant, nominal)
                .unwrap_or_else(|| panic!("`Type.{variant}` answers no head name"));
            assert!(
                !answered.is_empty(),
                "`Type.{variant}` answered the empty head name"
            );
            // The tag alone (no payload) is answerable too — every case's head is either its
            // constructor or its payload, never nothing.
            assert!(adt_head_name(variant, AdtHead::DEFAULT).is_some());
        }
        // The cases whose head is spelled FROM their payload report that payload; the rest ignore it
        // and report their constructor — the same answer `TypeRepr::head_name` gives the descriptor.
        fn named(n: &str) -> AdtHead<'_> {
            AdtHead {
                name: n,
                ..AdtHead::DEFAULT
            }
        }
        assert_eq!(
            adt_head_name("Struct", named("app.Todo")).as_deref(),
            Some("app.Todo")
        );
        assert_eq!(
            adt_head_name("Class", named("app.Todo")).as_deref(),
            Some("app.Todo")
        );
        assert_eq!(
            adt_head_name("Enum", named("app.Todo")).as_deref(),
            Some("app.Todo")
        );
        assert_eq!(
            adt_head_name("Named", named("id.Uuid")).as_deref(),
            Some("id.Uuid")
        );
        assert_eq!(
            adt_head_name("DynTrait", named("Greet")).as_deref(),
            Some("Greet")
        );
        assert_eq!(
            adt_head_name("List", named("ignored")).as_deref(),
            Some("List")
        );
        assert_eq!(
            adt_head_name("Int", named("ignored")).as_deref(),
            Some("int")
        );
        assert_eq!(
            adt_head_name("String", named("ignored")).as_deref(),
            Some("string")
        );
        assert_eq!(adt_head_name("Fn", named("ignored")).as_deref(), Some("Fn"));
        assert_eq!(
            adt_head_name("Union", named("ignored")).as_deref(),
            Some("Union")
        );
        // A fixed-width integer is spelled from its own width, not from the sample's.
        let width = |bits: u8, signed: bool| AdtHead {
            int_width: Some((bits, signed)),
            ..AdtHead::DEFAULT
        };
        assert_eq!(
            adt_head_name("IntN", width(8, false)).as_deref(),
            Some("u8")
        );
        assert_eq!(
            adt_head_name("IntN", width(64, true)).as_deref(),
            Some("i64")
        );
        assert_eq!(
            adt_head_name("IntN", AdtHead::DEFAULT).as_deref(),
            Some("i32")
        );
        // A tag that names no case is the honest `None`, never a placeholder name.
        assert_eq!(adt_head_name("NoSuchCase", AdtHead::DEFAULT), None);
    }
}
