//! **Prelude seeding**: the write-only registration passes that pre-populate the [`Checker`]'s
//! symbol tables before `collect` sees the program — built-in prelude types, extern-type traits,
//! extension-declared attributes, semantic roles, tiers, and the reflection `Type` enum. All
//! methods are `Checker` methods moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;

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
        self.seed_extern_type_traits();
    }

    /// Seed the built-in traits that **extern types** declare through the extension registry (p2p
    /// P2) into the trait-impl table — the extern-type analogue of processing a user type's
    /// `@derive`/`impl`. This is what makes `satisfies(GCounter, Mergeable)` true, so a
    /// `T: Mergeable` bound accepts a CRDT. Runs at prelude time, once, from every construction
    /// path; only registry-declared (intrinsic) traits appear here, so it cannot let a user type
    /// masquerade as one.
    pub(crate) fn seed_extern_type_traits(&mut self) {
        for ext in self.reg().extensions() {
            for ty in ext.types() {
                if !ty.traits.is_empty() {
                    // Keyed by the **qualified identity** (`para.crdt.GCounter` once the para-p2p
                    // package is installed) the checker stores in `Type::Named`, so a `T: Mergeable`
                    // bound resolves against the same string.
                    self.record_trait_impls(&ty.qualified(), ty.traits.iter().copied());
                }
            }
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
            let fields: Vec<(String, Type)> = attr
                .fields
                .iter()
                .map(|f| (f.name.to_string(), attr_field_type(f.ty)))
                .collect();
            self.symbols.types.insert(attr.name.to_string());
            self.symbols.records.insert(attr.name.to_string(), fields);
            self.symbols
                .type_kinds
                .insert(attr.name.to_string(), noeta_types::TypeKind::Struct);
            // Mark `@attribute` (bare — attachable anywhere) so the E0029 capability gate passes.
            self.record_attribute(attr.name, Some(&[]));
            let optional: HashSet<String> = attr
                .fields
                .iter()
                .filter(|f| f.default.is_some())
                .map(|f| f.name.to_string())
                .collect();
            if !optional.is_empty() {
                self.symbols
                    .attribute_optional_fields
                    .insert(attr.name.to_string(), optional);
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
        // `ParamInfo { name: string, type: Type }` — the element type of `params_of()`'s result.
        // `type` is the prelude `Type` enum (the same ADT `type_of` returns), built from the
        // parameter's declared type annotation. Registered like any prelude struct, so a user
        // declaration of the same name shadows it.
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
        let ty = || Type::Named("Type".to_string(), Vec::new());
        let list_of_ty = || Type::List(Box::new(ty()));
        let mut variants = Vec::new();
        for name in [
            "Int", "Float", "F32", "Bool", "String", "Bytes", "Unit", "Dyn",
        ] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: Vec::new(),
            });
        }
        for name in ["List", "Set", "Option"] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: vec![ty()],
            });
        }
        for name in ["Map", "Result"] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: vec![ty(), ty()],
            });
        }
        // The three nominal kinds + the unknown-kind `Named` fallback all carry `(name, args)`.
        for name in ["Enum", "Struct", "Class", "Named"] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: vec![Type::String, list_of_ty()],
            });
        }
        variants.push(VariantInfo {
            name: "Fn".to_string(),
            fields: vec![list_of_ty(), ty()],
        });
        variants.push(VariantInfo {
            name: "Union".to_string(),
            fields: vec![list_of_ty()],
        });
        // A trait object `dyn Trait` carries its trait name — so `params_of` can recover the interface
        // a parameter is bound to (service injection). A bare `dyn` param is still `Type.Dyn`.
        variants.push(VariantInfo {
            name: "DynTrait".to_string(),
            fields: vec![Type::String],
        });
        self.symbols.types.insert("Type".to_string());
        self.symbols.enums.insert("Type".to_string(), variants);
        self.symbols
            .type_kinds
            .insert("Type".to_string(), noeta_types::TypeKind::Enum);
    }
}
