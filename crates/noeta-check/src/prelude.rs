//! **Prelude seeding**: the write-only registration passes that pre-populate the [`Checker`]'s
//! symbol tables before `collect` sees the program — built-in prelude types, extern-type traits,
//! extension-declared attributes, semantic roles, tiers, and the reflection `Type` enum. All
//! methods are `Checker` methods moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;
// The shared element-derivation abstraction (slice 1b): `AssocDerivation::apply` folds a native
// trait's derivation over an implementing type's element into a concrete `Type` for `trait_assoc`.
use crate::stdlib::DeriveApply;

impl Checker {
    /// Register built-in prelude types the checker must know regardless of the program. Run before
    /// `collect` so a user declaration of the same name shadows it (matching the backends, which
    /// register `Ordering` the same way). `Attributed<T> { target: string, value: T }` is the
    /// element type of `attributes_of::<T>()`'s result; it is an ordinary generic struct so member
    /// access (`a.target`, `a.value`) and `value`'s instantiation to `T` reuse the generic path.
    pub(crate) fn register_prelude(&mut self) {
        self.register_prelude_struct(noeta_ast::reflect::ATTRIBUTED);
        self.symbols.generic_types.insert(
            noeta_ast::reflect::ATTRIBUTED.to_string(),
            vec![synthetic::prelude_param("T")],
        );
        self.register_prelude_enums();
        self.register_semantic_prelude();
        self.register_tier_prelude();
        self.register_extension_attributes();
        self.seed_ext_enums();
        self.seed_ext_fielded();
        self.seed_ext_directives();
    }

    /// Seed every installed extension's declared **native classes** (native-extensibility S2) into
    /// the checker's symbol tables, keyed by **qualified identity** (`res.Handle`) — so a native
    /// class is indistinguishable from a `.noe` class to every downstream consumer (field typing,
    /// visibility E0035, mutability E0033, construction, reference-type semantics). Runs eagerly at
    /// prelude time, mirroring `register_extension_attributes` (which seeds `records`/`types`/
    /// `type_kinds`) plus the class-only tables the collect pass's `Stmt::Class` arm writes
    /// (`private_fields`, `mut_fields`). A user declaration of the same qualified name shadows it
    /// (collect runs after prelude).
    ///
    /// **Not** seeded into `destructor_classes`: a native class has no `.noe` `destruct` block — its
    /// destructor is the extern-handle field's Rust `Drop`, which the RC/cycle collector runs on
    /// collection unconditionally (heap free drops the field's box). Marking it in
    /// `destructor_classes` would falsely claim a language destructor and defer its destructor-free
    /// cycles to the exit reaper; leaving it out keeps mid-run cycle reclamation, and the field's
    /// `Drop` fires on every free path regardless (verified by the S2 leak oracle).
    pub(crate) fn seed_ext_fielded(&mut self) {
        // Snapshot the qualified declarations first so the immutable registry borrow is released
        // before the `&mut self` symbol-table writes (the enum seeder takes the same shape). One
        // seeder covers both native classes and native structs — `reg().fielded()` streams both,
        // and the only per-declaration difference is the `TypeKind` written off `ExtFielded::kind`
        // (a class is a reference type; a struct is a value type with structural equality).
        struct FieldedDecl {
            qualified: String,
            fields: Vec<(String, Type)>,
            private: HashMap<String, DeclSite>,
            muts: HashSet<String>,
            kind: noeta_types::TypeKind,
        }
        let decls: Vec<FieldedDecl> = self
            .reg()
            .fielded()
            .map(|cl| {
                let fields = cl
                    .fields
                    .iter()
                    .map(|f| (f.name.to_string(), stdlib::sig_to_type(self.reg(), &f.ty)))
                    .collect();
                // Fields default **private** (a class's `.noe` rule) — only `is_public` are exempt.
                // `None` for the declaration site: a native field is registered through the ABI,
                // not parsed, so there is no source line to point a label at.
                let private = cl
                    .fields
                    .iter()
                    .filter(|f| !f.is_public)
                    .map(|f| (f.name.to_string(), None))
                    .collect();
                let muts = cl
                    .fields
                    .iter()
                    .filter(|f| f.is_mut)
                    .map(|f| f.name.to_string())
                    .collect();
                let kind = match cl.kind {
                    noeta_ext_abi::FieldedKind::Class => noeta_types::TypeKind::Class,
                    noeta_ext_abi::FieldedKind::Struct => noeta_types::TypeKind::Struct,
                };
                FieldedDecl {
                    qualified: cl.qualified(),
                    fields,
                    private,
                    muts,
                    kind,
                }
            })
            .collect();
        for FieldedDecl {
            qualified,
            fields,
            private,
            muts,
            kind,
        } in decls
        {
            self.symbols.records.insert(qualified.clone(), fields);
            self.symbols.types.insert(qualified.clone());
            self.symbols.type_kinds.insert(qualified.clone(), kind);
            if !private.is_empty() {
                self.symbols
                    .private_fields
                    .insert(qualified.clone(), private);
            }
            if !muts.is_empty() {
                self.symbols.mut_fields.insert(qualified, muts);
            }
        }
    }

    /// Seed the **imported** native traits (native-extensibility S3) into the user-trait tables —
    /// the trait analogue of `seed_ext_classes`, but run at **collect time** (import-aware), not at
    /// prelude, and keyed by the imported **short** name.
    ///
    /// A native trait slots into the checker's user-trait machinery (`symbols.user_traits` /
    /// `user_trait_impls`), which is keyed by the source-written **short** name everywhere (an
    /// `impl NativeTrait for T`, a `T: NativeTrait` bound, a `dyn NativeTrait` all name the trait by
    /// the short spelling), exactly like a `.noe` trait and the built-in traits. So — unlike a native
    /// enum/class, which is keyed by qualified identity because its *values* must unify — the trait
    /// entry is keyed by the short name and gated by the `use`: this seeds only names present in
    /// `imports.extern_types` that resolve to a native trait (`use fx.Widget`). Bare `impl Widget`
    /// without the `use` resolves nothing, exactly like a missing import.
    ///
    /// Runs **after** the collect pass's `Stmt::Trait` walk and uses `.or_insert` throughout, so a
    /// user `trait Widget` (collected first) **shadows** the same-short-named native trait —
    /// consistent with S1/S2's "user shadows native".
    ///
    /// It also seeds the **3b dynamic-dispatch coercion channel**: for each native type advertising
    /// this trait (a name in its [`ExtType::traits`] list matching the trait), it records
    /// `user_trait_impls[native_type_qualified][short_trait] = []`, so a native value typed
    /// `Type::Named("fx.Button")` coerces to `dyn Widget` (`assignable`/`implements_trait`) and its
    /// method call dispatches through the existing extern-method seam — no runtime change. The
    /// advertiser loop is written over a generic `(qualified_type, trait_names)` source, so a future
    /// `ExtClass` gaining a `traits` field joins it without a redesign (Option A ships ExtType only).
    pub(crate) fn seed_ext_traits(&mut self) {
        // Snapshot the import aliases first so the immutable registry borrow is released before the
        // `&mut self` symbol-table writes (the enum/class seeders take the same shape).
        struct TraitSeed {
            local: String,
            decl: noeta_ast::TraitDecl,
            impls: Vec<String>,
            /// The trait's native-derived associated types (slice 1b): `(name, derivation)` per
            /// `ExtAssocType`, applied over each implementing type's element into `trait_assoc`.
            assoc: Vec<(&'static str, noeta_ext_abi::AssocDerivation)>,
            /// The trait's structural `Self`-constraint (slice 3): recorded into
            /// `native_trait_self_constraints` when this native trait wins the `user_traits` slot,
            /// then enforced at the user `impl` site.
            self_constraint: Option<noeta_ext_abi::PackedConstraint>,
        }
        let aliases: Vec<(String, String)> = self
            .imports
            .extern_types
            .iter()
            .map(|(l, q)| (l.clone(), q.clone()))
            .collect();
        let seeds: Vec<TraitSeed> = aliases
            .iter()
            .filter_map(|(local, qualified)| {
                // A **dotted** local (`vec.Kernels`) is a module-namespace projection of a kernel
                // trait, NOT a short-name type import (`Packable`). Kernel traits are the migrated
                // bundle surface (ExtBundle→ExtTrait fold-in, slice 4): they bind ONLY through the
                // module-qualified `impl vec.Kernels for T {}` spelling, resolved via
                // `resolve_bundle_ref`/`bundle_impls`, and must NOT be seeded as short-name
                // `user_traits` (that would make the collect bundle-binding loop skip them, and route
                // their methods through the general trait path, which has no notion of the `List<Self>`
                // bulk receiver). Only short-name imports seed the general native-trait machinery.
                if local.contains('.') {
                    return None;
                }
                let reg = self.reg();
                let tr = reg.find_trait_qualified(qualified)?;
                let decl = synth_trait_decl(reg, tr, local);
                // Native declarations advertising this trait — a non-built-in name in `ExtType.traits`
                // (Pass 1), `ExtFielded.traits` (Pass 2b, a class OR a struct), OR `ExtEnum.traits`
                // (Slice C) matching the trait (short or qualified spelling). `record_trait_impls`
                // drops non-built-in names (they can't satisfy a built-in bound), so this is the one
                // channel that records a native declaration's native-trait impl. Written over every
                // kind so an ExtType, a class, a struct, and an enum advertiser seed the same
                // `user_trait_impls[qualified][trait]` uniformly — the coercion channel is
                // representation-agnostic (its receiver is an extern value, a fielded object, OR an
                // enum value, each dispatched by its own native seam at runtime).
                let advertises =
                    |traits: &[&str]| traits.iter().any(|t| *t == tr.name || tr.is_qualified(t));
                let type_impls = reg
                    .extensions()
                    .iter()
                    .flat_map(|ext| ext.types())
                    .filter(|ty| advertises(ty.traits))
                    .map(|ty| ty.qualified());
                let fielded_impls = reg
                    .fielded()
                    .filter(|cl| advertises(cl.traits))
                    .map(|cl| cl.qualified());
                let enum_impls = reg
                    .enums()
                    .filter(|en| advertises(en.traits))
                    .map(|en| en.qualified());
                let impls: Vec<String> =
                    type_impls.chain(fielded_impls).chain(enum_impls).collect();
                // The trait's native-derived associated types (slice 1b) — snapshot name + derivation
                // under the registry borrow, applied per-impl below.
                let assoc: Vec<(&'static str, noeta_ext_abi::AssocDerivation)> = tr
                    .assoc_types
                    .iter()
                    .map(|a| (a.name, a.derivation))
                    .collect();
                Some(TraitSeed {
                    local: local.clone(),
                    decl,
                    impls,
                    assoc,
                    self_constraint: tr.self_constraint,
                })
            })
            .collect();
        for TraitSeed {
            local,
            decl,
            impls,
            assoc,
            self_constraint,
        } in seeds
        {
            // A user `trait <local>` collected first wins (shadow ordering).
            let native_won = !self.symbols.user_traits.contains_key(&local);
            self.symbols
                .user_traits
                .entry(local.clone())
                .or_insert(decl);
            // Remember that this name is a *registry* trait when it actually took the slot. Its
            // synthesized declaration carries a placeholder `Span::new(0, 0)` — which points at the
            // ENTRY source — so the package orphan rule must not read a package off it; a native
            // trait belongs to no package the checker can see. A shadowed one is a real `.noe`
            // trait with a real span, so it is deliberately not recorded.
            if native_won {
                self.symbols.native_traits.insert(local.clone());
            }
            // The trait's structural `Self`-constraint (slice 3) is recorded ONLY when the native
            // trait actually occupies the slot — a same-named user `trait` shadows it and carries no
            // such constraint, so recording it under a shadow would enforce a phantom shape.
            if native_won && let Some(constraint) = self_constraint {
                self.symbols
                    .native_trait_self_constraints
                    .insert(local.clone(), constraint);
            }
            // Which of this trait's associated types are **native-derived** (slice 1b / auto-supply,
            // slice 4): a USER `impl <local> for T {}` must NOT be required to bind these — they are
            // computed from `T`'s element, not written per-impl — so `check_user_trait_impl` treats
            // them as auto-supplied (folding the derivation over `T`'s element into `trait_assoc`).
            if native_won && !assoc.is_empty() {
                self.symbols.native_derived_assoc.insert(
                    local.clone(),
                    assoc.iter().map(|(n, d)| (n.to_string(), *d)).collect(),
                );
            }
            for ty in impls {
                // Native-derived associated types (slice 1b): fold each `AssocDerivation` over this
                // implementing type's uniform `@packed` element into `trait_assoc[(type, trait)]` —
                // the SAME table slice 1a's `.noe` `type Name = …` bindings land in, so a native
                // trait method's `Self::Name` resolves on a concrete receiver unchanged. Computed
                // (immutable `packed_layout`/element read) BEFORE the `&mut` symbol writes below. A
                // non-`@packed` implementor (no uniform element) records nothing — the projection then
                // stays a gradual hole rather than a wrong concrete type.
                let derived: Option<HashMap<String, Type>> = if assoc.is_empty() {
                    None
                } else {
                    self.packed_layout(&Type::Named(ty.clone(), Vec::new()))
                        .and_then(|layout| stdlib::packed_elem_type(&layout))
                        .map(|elem| {
                            assoc
                                .iter()
                                .map(|(name, derivation)| {
                                    (name.to_string(), derivation.apply(&elem))
                                })
                                .collect()
                        })
                };
                self.symbols
                    .user_trait_impls
                    .entry(ty.clone())
                    .or_default()
                    .entry(local.clone())
                    .or_default();
                if let Some(map) = derived {
                    self.symbols.trait_assoc.insert((ty, local.clone()), map);
                }
            }
        }
    }

    /// Seed every installed extension's declared **native enums** (native-extensibility S1) into the
    /// checker's symbol tables, keyed by **qualified identity** (`std.http.SameSite`) — so a native
    /// enum is indistinguishable from a `.noe` enum to every downstream consumer (exhaustiveness
    /// E0011, variant-pattern binding, `is`/construction typing). Runs eagerly at prelude time
    /// (unlike the lazily-resolved extern types), because those consumers read `symbols.enums`
    /// directly, before any lookup. Mirrors the collect pass's `Stmt::Enum` arm and the built-in
    /// `register_cancelled` / `register_type_enum`; a user declaration of the same qualified name
    /// would shadow it (collect runs after prelude).
    pub(crate) fn seed_ext_enums(&mut self) {
        // Snapshot the qualified declarations first so the immutable registry borrow is released
        // before the `&mut self` symbol-table writes (the reactive/tier seeders take the same shape).
        let decls: Vec<(String, Vec<VariantInfo>)> = self
            .reg()
            .enums()
            .map(|en| {
                let qualified = en.qualified();
                let variants = en
                    .variants
                    .iter()
                    .map(|v| VariantInfo {
                        // A variant's payload types (accurate, like a struct's fields) — one source
                        // of truth for pattern-binding and the `Send`/destructor classifiers. A
                        // fieldless or backed variant has none.
                        name: v.name.to_string(),
                        fields: v
                            .fields
                            .iter()
                            .map(|f| stdlib::sig_to_type(self.reg(), f))
                            .collect(),
                        // A native enum is never backed — the ABI declares cases, not wire values.
                        backing: None,
                    })
                    .collect();
                (qualified, variants)
            })
            .collect();
        for (qualified, variants) in decls {
            self.symbols.enums.insert(qualified.clone(), variants);
            self.symbols.types.insert(qualified.clone());
            self.symbols
                .type_kinds
                .insert(qualified, noeta_types::TypeKind::Enum);
        }
    }

    /// Seed the **built-in directives** native fielded + enum types carry (native type-declaration
    /// unification, Slice D) into the same `Symbols` tables the checker's collect pass writes from a
    /// `.noe` type's `Decorators`. A native type bypasses the AST placement gate (E0054), so
    /// (kind, directive) legality is enforced at assembly by [`Registry::validate`]; this pass trusts
    /// that and performs only the table write:
    ///
    /// - [`ExtTypeDirective::Validated`] (struct/class) → `validated_types`, keyed by **qualified**
    ///   identity — a native record is qualified-keyed, and the E0060 construction gate compares the
    ///   constructed type's *resolved* name, which is qualified for a native type (so this matches
    ///   what the gate sees, unlike collect.rs's short user key). This installs only the static
    ///   construction ban; validation *runs* iff the type also advertises `traits:["Validate"]`
    ///   (answered by [`Checker::has_builtin_trait`], so `satisfies(Validate)` is true) and
    ///   carries a reachable `validate` method — both on the type's existing channels.
    /// - [`ExtTypeDirective::Semantic`] (enum) → `semantic_enums`, keyed by qualified identity, so a
    ///   native enum is a legal role vocabulary for `@role(Enum.Variant)` exactly like a `.noe`
    ///   `@semantic` enum.
    ///
    /// Runs at prelude time after the fielded/enum seeders (whose records this keys against); a
    /// `@validated` type is `satisfies(Validate)` through [`Checker::has_builtin_trait`], which reads
    /// the registry directly and so needs no ordering against this pass.
    pub(crate) fn seed_ext_directives(&mut self) {
        // Snapshot the qualified declarations + their directives under the immutable registry borrow,
        // then release it before the `&mut self` symbol-table writes (every native seeder's shape).
        let fielded: Vec<(String, &'static [noeta_ext_abi::ExtTypeDirective])> = self
            .reg()
            .fielded()
            .map(|f| (f.qualified(), f.directives))
            .collect();
        let enums: Vec<(String, &'static [noeta_ext_abi::ExtTypeDirective])> = self
            .reg()
            .enums()
            .map(|en| (en.qualified(), en.directives))
            .collect();
        for (qualified, directives) in fielded {
            for d in directives {
                match d {
                    noeta_ext_abi::ExtTypeDirective::Validated => {
                        self.symbols.validated_types.insert(qualified.clone());
                    }
                    // `@attribute` on a native fielded struct → the same `attributes` opt-in (E0029)
                    // and, when placement is restricted, the same `attachable` gate (E0030) a `.noe`
                    // `@attribute(Kind, …)` struct seeds — keyed on the qualified identity, with the
                    // struct's fields (seeded by the fielded seeder) as its construction contract. One
                    // enforcement path: a native `@attribute` is indistinguishable from a `.noe` one.
                    noeta_ext_abi::ExtTypeDirective::Attribute(targets) => {
                        self.symbols.attributes.insert(qualified.clone());
                        if !targets.is_empty() {
                            self.symbols.attachable.insert(
                                qualified.clone(),
                                targets.iter().map(|t| attr_target_kind(*t)).collect(),
                            );
                        }
                    }
                    noeta_ext_abi::ExtTypeDirective::Semantic => {}
                    // `@role` (Slice D3) seeds **no** `Symbols` table: a native role is surfaced purely
                    // by `reflect::build` joining the tags (projected via `Registry::native_roles`)
                    // against in-program attribute applications, not by a checker membership write.
                    // `Registry::validate` enforces its couplings at assembly.
                    noeta_ext_abi::ExtTypeDirective::Role(_) => {}
                    // `@packed` (Slice E1) → the same `packed_structs` (and `column_structs` for a
                    // column layout) membership a `.noe` `@packed` struct seeds in `collect.rs`, keyed
                    // on the **qualified** identity (a native record is qualified-keyed in `records`, so
                    // `packed_layout` resolves its fields there and a source `List<Pt>` literal packs
                    // flat on both backends). `Registry::validate` has already enforced struct-only + the
                    // all-packable-field rule (the native E0038 analogue), so this is a pure table write.
                    noeta_ext_abi::ExtTypeDirective::Packed(layout) => {
                        self.symbols.packed_structs.insert(qualified.clone());
                        if let noeta_ext_abi::PackedLayoutKind::Column = layout {
                            self.symbols.column_structs.insert(qualified.clone());
                        }
                    }
                }
            }
        }
        for (qualified, directives) in enums {
            for d in directives {
                if let noeta_ext_abi::ExtTypeDirective::Semantic = d {
                    self.symbols.semantic_enums.insert(qualified.clone());
                }
            }
        }
    }

    /// Whether a **native declaration** advertises the built-in trait `t` through the extension
    /// registry (p2p P2; unified across kinds in native-extensibility Slice C) — the native
    /// analogue of a user type's `@derive`/`impl`, and what makes `satisfies(Uuid, Comparable)`
    /// true so a `T: Comparable` bound accepts a native type.
    ///
    /// Resolved **on the lookup**, against the `&'static` registry, rather than pre-written into
    /// `symbols.trait_impls` at prelude time: the answer is a pure function of the registry, so
    /// seeding it merely paid for every native declaration in the process before the program was
    /// looked at. It is asked in exactly two places ([`Checker::satisfies`] and the `to_json`
    /// member rule), both through [`Checker::has_builtin_trait`], so there is one place that
    /// decides and no eager copy for it to drift from.
    ///
    /// `name` is the **qualified identity** (`para.crdt.GCounter`) the checker stores in
    /// `Type::Named`, matching the identity every native kind is keyed on elsewhere. Every native
    /// kind is consulted — extern types, fielded (class/struct), and enums — so a native class,
    /// struct, or enum declaring a built-in trait actually satisfies it, not only an `ExtType` (the
    /// pre-Slice-C latent gap). Only registry-declared (intrinsic) traits answer here, so it cannot
    /// let a user type masquerade as one; a name that is not a `BuiltinTrait` (a native
    /// [`ExtTrait`]) is dead data for this question and is recorded by
    /// [`Checker::seed_ext_traits`] instead.
    fn native_declares_builtin_trait(&self, name: &str, t: BuiltinTrait) -> bool {
        use noeta_ext_abi::NominalType;
        // The shared reading, so what the checker admits and what the runtimes serve is one
        // question rather than three: both backends seed their ordering flag through the same
        // predicate.
        let advertises = |traits: &'static [&'static str]| {
            noeta_ext_abi::registry::declares_builtin_trait(traits, t.name())
        };
        let reg = self.reg();
        reg.extensions()
            .iter()
            .flat_map(|ext| ext.types())
            .any(|ty| ty.is_qualified(name) && advertises(ty.traits))
            || reg
                .fielded()
                .any(|f| f.is_qualified(name) && advertises(f.traits))
            || reg
                .enums()
                .any(|en| en.is_qualified(name) && advertises(en.traits))
    }

    /// Whether the nominal type `name` implements the built-in trait `t` — the **one** membership
    /// question, over the program's own `@derive`/`impl` declarations *and* the registry's.
    ///
    /// The two are a union, not a precedence: a program declaring its own type under a native
    /// type's qualified identity used to merge into the same `symbols.trait_impls` entry the
    /// prelude seeding had already written, so both contributions counted. They still do.
    pub(crate) fn has_builtin_trait(&self, name: &str, t: BuiltinTrait) -> bool {
        self.symbols
            .trait_impls
            .get(name)
            .is_some_and(|ts| ts.contains(&t))
            || self.native_declares_builtin_trait(name, t)
    }

    /// Register every installed extension's declared **prelude attributes** (tier-extensions
    /// port) — std's core unit ships the test-metadata set (`Skip`/`Name`/`Group`/`Data`/`Timeout`),
    /// `bench`'s knob (`Bench { iterations: int }`), and the doc tier's text carrier
    /// (`Doc { text: string }`); a third-party extension's attributes register identically. Each
    /// is an ordinary struct (fields validated by the construction gate) marked `@attribute` (so
    /// the capability gate E0029 passes); the runners read them off a fn's `attrs`. Registered
    /// like any prelude type, so a user declaration of the same name shadows it. A field carrying
    /// a declaration default is optional at construction (`Skip.reason` = `""`, so both `#[Skip]`
    /// and `#[Skip("flaky")]` construct); the materialization default flows into the reflection
    /// artifact at compile time.
    pub(crate) fn register_extension_attributes(&mut self) {
        for attr in self.reg().ext_attributes() {
            // Key every table on the attribute's **qualified identity** (`std.test.Skip`), the same
            // identity it projects under through `nominal_types` — so a consumer's `use std.test.Skip`
            // resolves `Skip → std.test.Skip` and D2a's gate binds it, while a bare `#[Skip]` with no
            // `use` is the checker's E0029 (there is no global attribute namespace).
            let qualified = attr.qualified();
            let fields: Vec<(String, Type)> = attr
                .fields
                .iter()
                .map(|f| (f.name.to_string(), attr_field_type(f.ty)))
                .collect();
            self.symbols.types.insert(qualified.clone());
            self.symbols.records.insert(qualified.clone(), fields);
            self.symbols
                .type_kinds
                .insert(qualified.clone(), noeta_types::TypeKind::Struct);
            // Mark `@attribute` so the E0029 capability gate passes, and — when the declaration
            // restricts its placement — seed the same `attachable` set an `@attribute(Field, …)`
            // struct seeds, so E0030 fires on a misplaced native attribute exactly as on a `.noe`
            // one. An empty `targets` is "attachable anywhere", which is what every tier-metadata
            // attribute wants (a `#[Name("…")]` is meaningful wherever its runner looks).
            self.record_attribute(&qualified, Some(&[]));
            if !attr.targets.is_empty() {
                self.symbols.attachable.insert(
                    qualified.clone(),
                    attr.targets.iter().map(|t| attr_target_kind(*t)).collect(),
                );
            }
            let optional: HashSet<String> = attr
                .fields
                .iter()
                .filter(|f| f.default.is_some())
                .map(|f| f.name.to_string())
                .collect();
            if !optional.is_empty() {
                self.symbols
                    .attribute_optional_fields
                    .insert(qualified.clone(), optional);
            }
        }
    }

    /// Register the prelude structs that ride alongside the prelude *enums* — `RoleBinding`,
    /// `ParamInfo`, `FieldEntry`, `FieldSpec`, `VariantSpec`. (The enums themselves, `Semantic`
    /// included, come from the one shared table in
    /// [`register_prelude_enums`](Self::register_prelude_enums).)
    ///
    /// Each registers like any prelude type, so a user declaration of the same name shadows it and
    /// the backends materialize the matching shapes. What each field *is*, and why, is stated once —
    /// in the shared declaration table these read ([`noeta_ast::reflect::prelude_structs`]), which
    /// now carries the field types the hand-written lists here used to.
    pub(crate) fn register_semantic_prelude(&mut self) {
        self.register_prelude_struct(noeta_ast::reflect::ROLE_BINDING);
        self.register_prelude_struct(noeta_ast::reflect::PARAM_INFO);
        self.register_prelude_struct(noeta_ast::reflect::FIELD_ENTRY);
        self.register_prelude_struct(noeta_ast::reflect::FIELD_SPEC);
        self.register_prelude_struct(noeta_ast::reflect::VARIANT_SPEC);
    }

    /// Register one **prelude struct** from the shared declaration table
    /// ([`noeta_ast::reflect::prelude_structs`]) — the same table both backends materialize and
    /// construct these values from.
    ///
    /// Its field *types* used to come from the caller, because the checker lattice is not visible to
    /// `noeta-ast`. That split is what made the prelude structs invisible to reflection: the field
    /// names were shared, the types were not, and reflection — which cannot see the lattice either —
    /// had nothing to report, so `field_specs_of("FieldSpec")` and `variants_of("FieldSpec")` were
    /// both empty about the very types a reflection consumer walks while reflecting. The table now
    /// carries a closed [`PreludeStructFieldTy`](noeta_ast::reflect::PreludeStructFieldTy)
    /// vocabulary and [`prelude_struct_field_type`] projects it onto the lattice, so the checker and
    /// reflection can no more disagree about a prelude struct's field *types* than about its field
    /// *names*.
    ///
    /// Registered like any prelude type (before `collect`), so a user declaration of the same name
    /// shadows it. Panics for a name the table does not carry: every call site passes one of its
    /// constants, so a miss is a programming error, and every program's first check runs this.
    fn register_prelude_struct(&mut self, name: &str) {
        let decl = noeta_ast::reflect::prelude_struct(name)
            .unwrap_or_else(|| panic!("`{name}` is not a prelude struct"));
        let types: Vec<Type> = decl
            .field_types
            .iter()
            .map(|t| prelude_struct_field_type(*t))
            .collect();
        self.symbols.types.insert(name.to_string());
        self.symbols
            .type_kinds
            .insert(name.to_string(), noeta_types::TypeKind::Struct);
        self.symbols.records.insert(
            name.to_string(),
            decl.fields.into_iter().zip(types).collect(),
        );
    }

    /// Register the prelude `TierRoot` struct (tier-providers T2) — the element type of the roots
    /// list a declared tier's runner receives: `fn runner(roots: List<TierRoot>): void`. One root
    /// per activated fn: its `name` (for the runner's report) and `run`, the fn itself as a
    /// first-class `() -> void` value (in-process reflected-handle dispatch — the runner calls
    /// `root.run()`). Knob values are not carried here: the stamped config attribute is read via
    /// `attributes_of::<Config>()`, whose `target` matches `name`. Registered like any prelude
    /// type, so a user declaration shadows it.
    pub(crate) fn register_tier_prelude(&mut self) {
        self.register_prelude_struct(noeta_ast::reflect::TIER_ROOT);
        // Its text-tier counterpart (text-tiers arc): one `TierText` per activated verbatim body.
        self.register_prelude_struct(noeta_ast::reflect::TIER_TEXT);
    }

    /// Register **every prelude enum** — `Ordering`, `Cancelled`, `Semantic`, `Layout`, and the
    /// reflection `Type` ADT — from the one shared declaration table,
    /// [`noeta_ast::reflect::prelude_enums`], which both backends also seed their runtime type
    /// environments from. One table is the point: while the checker and the two backends each kept
    /// their own list, the three disagreed in both directions — `Type`/`Semantic`/`Layout`/
    /// `Cancelled` type-checked and then aborted with E0005 because neither backend had them, and
    /// `Ordering` skipped the exhaustiveness rule (E0011) because the checker did not.
    ///
    /// Each registers like any prelude type — before `collect`, so a user declaration of the same
    /// name shadows it — and the enums that are role vocabularies (`Semantic`) additionally join
    /// `semantic_enums`, so `@role(Semantic.EntryPoint)` resolves.
    pub(crate) fn register_prelude_enums(&mut self) {
        for decl in noeta_ast::reflect::prelude_enums() {
            let variants: Vec<VariantInfo> = decl
                .variants
                .iter()
                .map(|v| VariantInfo {
                    name: v.name.clone(),
                    fields: v
                        .fields
                        .iter()
                        .map(|f| prelude_field_type(decl.name, *f))
                        .collect(),
                    // No prelude enum is backed — their cases are their wire values.
                    backing: None,
                })
                .collect();
            self.symbols.types.insert(decl.name.to_string());
            self.symbols.enums.insert(decl.name.to_string(), variants);
            self.symbols
                .type_kinds
                .insert(decl.name.to_string(), noeta_types::TypeKind::Enum);
            if decl.semantic {
                self.symbols.semantic_enums.insert(decl.name.to_string());
            }
        }
    }
}

/// Map one prelude-enum payload field's declared shape onto the checker lattice. `enum_name` is the
/// enum the field belongs to, so the two recursive arms (`Type.List(inner: Type)`,
/// `Type.Union(members: List<Type>)`) name the right type without the table having to spell it.
fn prelude_field_type(enum_name: &str, field: noeta_ast::reflect::PreludeFieldTy) -> Type {
    use noeta_ast::reflect::PreludeFieldTy as F;
    let this = || Type::Named(enum_name.to_string(), Vec::new());
    match field {
        F::SelfEnum => this(),
        F::ListOfSelf => Type::List(Box::new(this())),
        F::Str => Type::String,
        F::Int => Type::Int,
        F::Bool => Type::Bool,
    }
}

/// Map one prelude **struct** field's declared shape onto the checker lattice — the struct twin of
/// [`prelude_field_type`], and the lattice half of the projection whose reflection half is
/// [`PreludeStructFieldTy::repr`](noeta_ast::reflect::PreludeStructFieldTy::repr). One vocabulary,
/// two lattices: this is what replaced the hand-written `&[Type]` lists at the registration sites, so
/// a prelude struct's field types are stated once and every consumer — the checker, both backends'
/// materializations, and now reflection — reads that one statement.
fn prelude_struct_field_type(field: noeta_ast::reflect::PreludeStructFieldTy) -> Type {
    use noeta_ast::reflect::PreludeStructFieldTy as F;
    match field {
        F::Str => Type::String,
        F::Bool => Type::Bool,
        F::Dyn => Type::Dyn,
        // The prelude `Type` ADT enum — the same type `type_of` returns.
        F::TypeAdt => Type::Named(noeta_ast::reflect::TYPE_ENUM.to_string(), Vec::new()),
        // The abstract `Enum` kind: any enum. A lattice type of its own, which is why the vocabulary
        // spells it separately from a named enum — a role binding's role may come from any
        // `@semantic` enum, not one fixed one.
        F::EnumKind => Type::Kind(noeta_types::TypeKind::Enum),
        F::Struct(n) => Type::Named(n.to_string(), Vec::new()),
        // This struct's OWN type parameter — a real parameter in the lattice, taking the reserved
        // synthetic identity `generic_types` registers it under, so an instantiation substitutes
        // it by identity exactly like a user type's.
        F::Param(n) => Type::Param(synthetic::prelude_param(n)),
        F::List(inner) => Type::List(Box::new(prelude_struct_field_type(*inner))),
        F::Option(inner) => Type::Option(Box::new(prelude_struct_field_type(*inner))),
        F::VoidFn => Type::Fn {
            params: Vec::new(),
            ret: Box::new(Type::Unit),
        },
    }
}

/// Synthesize a [`noeta_ast::TraitDecl`] from a native [`registry::ExtTrait`] (native-extensibility
/// S3) — the declarative surface `seed_ext_traits` seeds into `symbols.user_traits`, so a native
/// trait is indistinguishable from a `.noe` `trait` to `check_user_trait_impl` (E0015),
/// `enforce_type_param_bounds` (E0025), and the `dyn`-method result typing. The decl is named by the
/// imported **short** `local` name (alias-safe: the source writes that spelling); its methods' `sig`
/// carries AST `TypeRef` types via the `SigType → TypeRef` reverse map (`stdlib::sig_to_typeref` /
/// `ret_to_typeref`), because the user-trait checkers read those through `field_type`, exactly as
/// they read a `.noe` trait's.
fn synth_trait_decl(
    reg: &noeta_ext_abi::registry::Registry,
    tr: &noeta_ext_abi::ExtTrait,
    local: &str,
) -> noeta_ast::TraitDecl {
    use noeta_ast::{Decorators, FnDecl, Param, TraitDecl, TraitMethod};
    use noeta_span::Span;
    let sp = Span::new(0, 0);
    let methods = tr
        .methods
        .iter()
        .map(|m| {
            let params = m
                .sig
                .params
                .iter()
                .enumerate()
                .map(|(i, p)| Param {
                    attrs: Vec::new(),
                    // A native signature carries no parameter names; only the count and types are
                    // load-bearing for the contract check, so positional placeholders suffice.
                    name: format!("_{i}"),
                    name_span: sp,
                    ty: Some(stdlib::sig_to_typeref(reg, p)),
                    default: None,
                    span: sp,
                    positional: false,
                })
                .collect();
            let sig = FnDecl {
                name: noeta_ast::Name::canonical(m.sig.name),
                name_span: sp,
                is_public: true,
                type_params: Vec::new(),
                params,
                ret: Some(stdlib::ret_to_typeref(reg, &m.sig.ret)),
                attrs: Vec::new(),
                directives: Vec::new(),
                is_dev_tier: false,
                is_async: false,
                // A native trait declares receiver-ness the same way a `.noe` trait does — through
                // its ABI method's receiver marker (static-trait-methods, ABI half). `Static` is the
                // "no receiver" case `BundleReceiver` could not express before.
                is_static: matches!(m.receiver, noeta_ext_abi::BundleReceiver::Static),
                tier: None,
                captures: Vec::new(),
                body: Vec::new(),
                span: sp,
            };
            TraitMethod {
                sig,
                has_default: m.has_default,
            }
        })
        .collect();
    // Native associated types (ExtBundle→ExtTrait convergence, slice 1b): faithfully carry each
    // `ExtAssocType`'s name into the `TraitDecl` so a native trait is indistinguishable from a `.noe`
    // one to the coherence machinery. Their concrete values are **derived** (not per-impl `type Name
    // = …` bindings), so the synthesized decl carries no default `TypeRef`; `seed_ext_traits` folds
    // each `AssocDerivation` over the implementing type's element straight into `trait_assoc`.
    let assoc_types = tr
        .assoc_types
        .iter()
        .map(|a| noeta_ast::AssocTypeDecl {
            name: a.name.to_string(),
            name_span: sp,
            default: None,
            span: sp,
        })
        .collect();
    TraitDecl {
        name: noeta_ast::Name::canonical(local),
        name_span: sp,
        is_public: true,
        type_params: Vec::new(),
        methods,
        assoc_types,
        decorators: Decorators::default(),
        span: sp,
    }
}
