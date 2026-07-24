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
        self.symbols.types.insert("Attributed".to_string());
        self.symbols.records.insert(
            "Attributed".to_string(),
            vec![
                ("target".to_string(), Type::String),
                (
                    "value".to_string(),
                    Type::Named("T".to_string(), Vec::new()),
                ),
            ],
        );
        self.symbols
            .generic_types
            .insert("Attributed".to_string(), vec!["T".to_string()]);
        self.symbols
            .type_kinds
            .insert("Attributed".to_string(), noeta_types::TypeKind::Struct);
        self.register_type_enum();
        self.register_cancelled();
        self.register_semantic_prelude();
        self.register_tier_prelude();
        self.register_extension_attributes();
        self.seed_native_builtin_traits();
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
            private: HashSet<String>,
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
                let private = cl
                    .fields
                    .iter()
                    .filter(|f| !f.is_public)
                    .map(|f| f.name.to_string())
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
    /// `Type::Named("fx.Button")` coerces to `dyn Widget` (`assignable`/`type_impls_trait`) and its
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
            // The trait's structural `Self`-constraint (slice 3) is recorded ONLY when the native
            // trait actually occupies the slot — a same-named user `trait` shadows it and carries no
            // such constraint, so recording it under a shadow would enforce a phantom shape.
            if native_won && let Some(constraint) = self_constraint {
                self.symbols
                    .native_trait_self_constraints
                    .insert(local.clone(), constraint);
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
    ///   (seeded by [`Checker::seed_native_builtin_traits`], so `satisfies(Validate)` is true) and
    ///   carries a reachable `validate` method — both on the type's existing channels.
    /// - [`ExtTypeDirective::Semantic`] (enum) → `semantic_enums`, keyed by qualified identity, so a
    ///   native enum is a legal role vocabulary for `@role(Enum.Variant)` exactly like a `.noe`
    ///   `@semantic` enum.
    ///
    /// Runs at prelude time after the fielded/enum seeders (whose records this keys against) and after
    /// [`Checker::seed_native_builtin_traits`] (so a `@validated` type is already `satisfies(Validate)`).
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

    /// Seed the built-in traits that **native declarations** advertise through the extension registry
    /// (p2p P2; unified across kinds in native-extensibility Slice C) into the trait-impl table — the
    /// native analogue of processing a user type's `@derive`/`impl`. This is what makes
    /// `satisfies(GCounter, Mergeable)` true, so a `T: Mergeable` bound accepts a CRDT. Iterates
    /// **every** native kind — extern types, fielded (class/struct), and enums — so a native class,
    /// struct, or enum declaring a built-in trait (`traits: ["Comparable"]`) actually satisfies it,
    /// not only an [`ExtType`] (the pre-Slice-C latent gap). Runs at prelude time, once, from every
    /// construction path; only registry-declared (intrinsic) traits appear here, so it cannot let a
    /// user type masquerade as one. [`Checker::record_trait_impls`] filters its input to the closed
    /// `BuiltinTrait` set, so feeding it a mixed `traits` list is safe — a non-built-in name (a
    /// native [`ExtTrait`]) is dropped here and recorded by [`Checker::seed_ext_traits`] instead.
    pub(crate) fn seed_native_builtin_traits(&mut self) {
        // Snapshot (qualified identity, declared traits) over every native kind while the immutable
        // registry borrow is live, then release it before the `&mut self` `record_trait_impls`
        // writes. Keyed by the **qualified identity** (`para.crdt.GCounter` once the para-p2p package
        // is installed) the checker stores in `Type::Named`, so a `T: Mergeable` bound resolves
        // against the same string.
        let decls: Vec<(String, &'static [&'static str])> = {
            let reg = self.reg();
            let types = reg
                .extensions()
                .iter()
                .flat_map(|ext| ext.types())
                .map(|ty| (ty.qualified(), ty.traits));
            let fielded = reg.fielded().map(|f| (f.qualified(), f.traits));
            let enums = reg.enums().map(|en| (en.qualified(), en.traits));
            types
                .chain(fielded)
                .chain(enums)
                .filter(|(_, traits)| !traits.is_empty())
                .collect()
        };
        for (qualified, traits) in decls {
            self.record_trait_impls(&qualified, traits.iter().copied());
        }
    }

    /// Register every installed extension's declared **prelude attributes** (tier-extensions
    /// port) — std's core unit ships the test-metadata quartet (`Skip`/`Name`/`Group`/`Data`),
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
            // Mark `@attribute` (bare — attachable anywhere) so the E0029 capability gate passes.
            self.record_attribute(&qualified, Some(&[]));
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

    /// Register the prelude `Semantic` enum and `RoleBinding` struct. `Semantic` is the language's
    /// built-in role vocabulary (every variant payload-free, so matchable bare) and is implicitly
    /// `@semantic`, so `@role(Semantic.EntryPoint)` is always valid; a user promotes any enum to the
    /// same status with `@semantic`. `RoleBinding { target: string, role: Enum }` is the element type
    /// of `roles_of()`'s result — `role` is the abstract `Enum` kind because a binding's role may be
    /// any `@semantic` enum, not a single fixed type. Both register like any prelude type, so a user
    /// declaration of the same name shadows them and the backends materialize the matching shapes.
    pub(crate) fn register_semantic_prelude(&mut self) {
        let variants = noeta_ast::reflect::SEMANTIC_VARIANTS
            .iter()
            .map(|name| VariantInfo {
                name: name.to_string(),
                fields: Vec::new(),
            })
            .collect();
        self.symbols
            .types
            .insert(noeta_ast::reflect::SEMANTIC_ENUM.to_string());
        self.symbols
            .enums
            .insert(noeta_ast::reflect::SEMANTIC_ENUM.to_string(), variants);
        self.symbols.type_kinds.insert(
            noeta_ast::reflect::SEMANTIC_ENUM.to_string(),
            noeta_types::TypeKind::Enum,
        );
        self.symbols
            .semantic_enums
            .insert(noeta_ast::reflect::SEMANTIC_ENUM.to_string());
        self.symbols.type_kinds.insert(
            noeta_ast::reflect::ROLE_BINDING.to_string(),
            noeta_types::TypeKind::Struct,
        );
        self.symbols
            .types
            .insert(noeta_ast::reflect::ROLE_BINDING.to_string());
        self.symbols.records.insert(
            noeta_ast::reflect::ROLE_BINDING.to_string(),
            vec![
                ("target".to_string(), Type::String),
                ("role".to_string(), Type::Kind(noeta_types::TypeKind::Enum)),
            ],
        );
        // `ParamInfo { name: string, type: Type, optional: bool, attrs: List<dyn> }` — the element
        // type of `params_of()`'s result. `type` is the prelude `Type` enum (the same ADT `type_of`
        // returns), built from the parameter's declared type annotation; `optional` is true when the
        // parameter declared a default, so a signature-driven consumer can tell a required parameter
        // from one a call may omit; `attrs` holds the parameter's `#[...]` attribute instances.
        //
        // `attrs` is `List<dyn>` because a parameter's attributes are heterogeneous — `#[Arg]` and
        // `#[Sensitive]` are different structs — so there is no one element type to name. A consumer
        // recovers the one it cares about by narrowing (`if a is Arg { … }`), the same way it would
        // with any `dyn`. Registered like any prelude struct, so a user declaration of the same name
        // shadows it.
        self.symbols.type_kinds.insert(
            noeta_ast::reflect::PARAM_INFO.to_string(),
            noeta_types::TypeKind::Struct,
        );
        self.symbols
            .types
            .insert(noeta_ast::reflect::PARAM_INFO.to_string());
        self.symbols.records.insert(
            noeta_ast::reflect::PARAM_INFO.to_string(),
            vec![
                ("name".to_string(), Type::String),
                (
                    "type".to_string(),
                    Type::Named(noeta_ast::reflect::TYPE_ENUM.to_string(), Vec::new()),
                ),
                ("optional".to_string(), Type::Bool),
                ("attrs".to_string(), Type::List(Box::new(Type::Dyn))),
            ],
        );
        // `FieldEntry { name: string, value: dyn }` — the element type of `fields_of()`'s result
        // (derive layer 3). Registered like `ParamInfo`; shadowable like any prelude type.
        self.symbols.type_kinds.insert(
            noeta_ast::reflect::FIELD_ENTRY.to_string(),
            noeta_types::TypeKind::Struct,
        );
        self.symbols
            .types
            .insert(noeta_ast::reflect::FIELD_ENTRY.to_string());
        self.symbols.records.insert(
            noeta_ast::reflect::FIELD_ENTRY.to_string(),
            vec![
                ("name".to_string(), Type::String),
                ("value".to_string(), Type::Dyn),
            ],
        );
        // `FieldSpec { name: string, type: Type, optional: bool }` — the element type of the
        // TYPE-level `field_specs_of::<T>()` / `field_specs_of(name)` query. The type-level twin of
        // `FieldEntry`: `type` is the field's declared type as the same `Type` ADT `type_of` returns
        // (precise, from the declaration — not a value's erased head), and `optional` reports whether
        // the field declared a default. Registered like `ParamInfo`; shadowable like any prelude type.
        self.symbols.type_kinds.insert(
            noeta_ast::reflect::FIELD_SPEC.to_string(),
            noeta_types::TypeKind::Struct,
        );
        self.symbols
            .types
            .insert(noeta_ast::reflect::FIELD_SPEC.to_string());
        self.symbols.records.insert(
            noeta_ast::reflect::FIELD_SPEC.to_string(),
            vec![
                ("name".to_string(), Type::String),
                (
                    "type".to_string(),
                    Type::Named(noeta_ast::reflect::TYPE_ENUM.to_string(), Vec::new()),
                ),
                ("optional".to_string(), Type::Bool),
            ],
        );
        // `Layout { Row, Column }` — the storage-layout vocabulary `@packed(Layout.…)` names.
        // Directive vocabulary like `Semantic` (the parser resolves the argument syntactically);
        // registered so hover/completion/docs see one authoritative enum, and shadowable like any
        // prelude type. Not role-eligible: it stays out of `semantic_enums`.
        let layout_variants = noeta_ast::reflect::LAYOUT_VARIANTS
            .iter()
            .map(|name| VariantInfo {
                name: name.to_string(),
                fields: Vec::new(),
            })
            .collect();
        self.symbols
            .types
            .insert(noeta_ast::reflect::LAYOUT_ENUM.to_string());
        self.symbols
            .enums
            .insert(noeta_ast::reflect::LAYOUT_ENUM.to_string(), layout_variants);
        self.symbols.type_kinds.insert(
            noeta_ast::reflect::LAYOUT_ENUM.to_string(),
            noeta_types::TypeKind::Enum,
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
        let name = noeta_ast::reflect::TIER_ROOT.to_string();
        self.symbols.types.insert(name.clone());
        self.symbols
            .type_kinds
            .insert(name.clone(), noeta_types::TypeKind::Struct);
        self.symbols.records.insert(
            name,
            vec![
                ("name".to_string(), Type::String),
                (
                    "run".to_string(),
                    Type::Fn {
                        params: Vec::new(),
                        ret: Box::new(Type::Unit),
                    },
                ),
            ],
        );
        // Its text-tier counterpart (text-tiers arc): one `TierText` per activated verbatim body.
        let name = noeta_ast::reflect::TIER_TEXT.to_string();
        self.symbols.types.insert(name.clone());
        self.symbols
            .type_kinds
            .insert(name.clone(), noeta_types::TypeKind::Struct);
        self.symbols.records.insert(
            name,
            vec![
                ("target".to_string(), Type::String),
                ("text".to_string(), Type::String),
            ],
        );
    }

    /// Register the prelude `Cancelled` enum (Track A.8) — the typed marker `h.join()` returns as
    /// the `Err` of its `Result<T, Cancelled>` when the joined task was cancelled. A single
    /// payload-free variant (`Cancelled.Cancelled`), so it is matchable bare and `Send` — modeled
    /// on `Ordering`. Registered like any prelude type, so a user declaration of the same name
    /// shadows it and both backends materialize the matching shape.
    pub(crate) fn register_cancelled(&mut self) {
        self.symbols.types.insert("Cancelled".to_string());
        self.symbols.enums.insert(
            "Cancelled".to_string(),
            vec![VariantInfo {
                name: "Cancelled".to_string(),
                fields: Vec::new(),
            }],
        );
        self.symbols
            .type_kinds
            .insert("Cancelled".to_string(), noeta_types::TypeKind::Enum);
    }

    /// Register the prelude `Type` enum — the ADT `type_of` returns, mirroring the type lattice so
    /// reflected types are pattern-matchable (`match type_of(x) { Type.List(e) => … }`). It is a
    /// recursive enum: payload-carrying variants reference `Type` itself.
    pub(crate) fn register_type_enum(&mut self) {
        use noeta_ast::reflect::AdtFields;
        let ty = || Type::Named("Type".to_string(), Vec::new());
        let list_of_ty = || Type::List(Box::new(ty()));
        // The variant list is *derived* from `TypeRepr` rather than re-listed here: the reflection
        // descriptor is what both backends materialize a `Type` value from, so the enum the checker
        // registers must be exactly its shape. A hand-kept parallel table could disagree — this one
        // cannot, and adding a `TypeRepr` variant fails to compile in `noeta-ast` until it is
        // handled there. Order is preserved (variant ordinals are baked into compiled programs).
        let variants: Vec<VariantInfo> = noeta_ast::reflect::type_adt_variants()
            .iter()
            .map(|repr| VariantInfo {
                name: repr.variant_name().to_string(),
                fields: match repr.adt_fields() {
                    AdtFields::None => Vec::new(),
                    AdtFields::Types(n) => (0..n).map(|_| ty()).collect(),
                    // The three nominal kinds + the unknown-kind `Named` fallback.
                    AdtFields::NameAndArgs => vec![Type::String, list_of_ty()],
                    AdtFields::TypeList => vec![list_of_ty()],
                    AdtFields::ParamsAndRet => vec![list_of_ty(), ty()],
                    // A trait object `dyn Trait` carries its trait name — so `params_of` can recover
                    // the interface a parameter is bound to (service injection). A bare `dyn` param
                    // is still `Type.Dyn`.
                    AdtFields::Name => vec![Type::String],
                    // `Type.IntN(bits: int, signed: bool)` — a fixed-width integer's descriptor.
                    AdtFields::IntWidth => vec![Type::Int, Type::Bool],
                },
            })
            .collect();
        self.symbols.types.insert("Type".to_string());
        self.symbols.enums.insert("Type".to_string(), variants);
        self.symbols
            .type_kinds
            .insert("Type".to_string(), noeta_types::TypeKind::Enum);
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
                })
                .collect();
            let sig = FnDecl {
                name: m.sig.name.to_string(),
                name_span: sp,
                is_public: true,
                type_params: Vec::new(),
                params,
                ret: Some(stdlib::ret_to_typeref(reg, &m.sig.ret)),
                attrs: Vec::new(),
                directives: Vec::new(),
                is_dev_tier: false,
                is_async: false,
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
        name: local.to_string(),
        name_span: sp,
        is_public: true,
        type_params: Vec::new(),
        methods,
        assoc_types,
        decorators: Decorators::default(),
        span: sp,
    }
}
