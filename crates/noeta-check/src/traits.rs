//! **Trait machinery**: `impl Trait` blocks (built-in and bundle), coherence, `@derive`
//! validation with field constraints, orderability/serializability classification, and the
//! generic-call instantiation + trait-bound enforcement (`satisfies`/module bounds). All
//! `Checker` methods moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;

impl Checker {
    // ----- traits: impl coherence and derive validation (M1.8) -----

    /// Validate an in-body `impl Trait { ... }` block: the trait must be a known built-in, and the
    /// block must provide the trait's required method with the right arity. The impl's method
    /// *bodies* are checked separately (they are flattened into `ClassDecl::methods`).
    pub(crate) fn check_impl(&mut self, block: &ImplBlock) {
        self.check_trait_impl(
            &block.trait_name,
            &block.trait_args,
            block.trait_span,
            &block.methods,
        );
    }

    /// The trait-side validation shared by in-body `impl` blocks and standalone `impl Trait for T`
    /// declarations: the trait must be a known built-in, and a non-marker trait must be given its
    /// required method with the right arity. (The orphan rule and the standalone-only body
    /// restriction are enforced by the caller, [`Self::check_standalone_impl`].)
    pub(crate) fn check_trait_impl(
        &mut self,
        trait_name: &str,
        trait_args: &[noeta_ast::TypeRef],
        trait_span: Span,
        methods: &[FnDecl],
    ) {
        // A user-defined trait (L1, UT2): validate conformance against its declared contract —
        // instantiated at the impl's type arguments when the trait is generic (`impl
        // Cache<string>` checks `fn get(k: string): …`) — then return before the built-in
        // resolution below (which would otherwise report E0014).
        if let Some(decl) = self.symbols.user_traits.get(trait_name).cloned() {
            match noeta_ast::derive::instantiate_trait(&decl, trait_args) {
                Ok(instantiated) => {
                    self.check_user_trait_impl(
                        instantiated.as_ref().unwrap_or(&decl),
                        trait_span,
                        methods,
                    );
                }
                Err(e) => {
                    let d = self.error(DiagnosticCode::InvalidImpl, trait_span, e.message);
                    if let Some(h) = e.help {
                        d.help(h);
                    }
                }
            }
            return;
        }
        if !trait_args.is_empty() {
            self.error(
                DiagnosticCode::InvalidImpl,
                trait_span,
                format!("`{trait_name}` takes no type arguments"),
            );
        }
        let Some(t) = BuiltinTrait::from_name(trait_name) else {
            // A dotted path in an in-body impl is a method-bundle reference (kernel-methods K1)
            // used in the wrong position — bundles bind through the standalone form only.
            if trait_name.contains('.') {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    trait_span,
                    "a method bundle binds with a standalone impl, not an in-body block"
                        .to_string(),
                )
                .help(format!(
                    "write `impl {trait_name} for <Type> {{}}` at the top level"
                ));
                return;
            }
            self.error(
                DiagnosticCode::UnknownTrait,
                trait_span,
                format!("unknown trait `{trait_name}`"),
            )
            .help("only built-in traits can be implemented (e.g. `Add`, `Equatable`, `Display`)");
            return;
        };
        if t.intrinsic() {
            self.error(
                DiagnosticCode::InvalidImpl,
                trait_span,
                format!("`{trait_name}` cannot be implemented"),
            )
            .help(format!(
                "`{trait_name}` is a built-in capability satisfied only by the runtime types that \
                 provide it (e.g. the CRDT types for `Mergeable`), not by a user `impl`"
            ));
            return;
        }
        let Some((req_name, req_arity)) = t.required_method() else {
            return; // a marker trait (e.g. `Clone`, `Attribute`) imposes no hand-written method
        };
        match methods.iter().find(|m| m.name == req_name) {
            None => {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    trait_span,
                    format!("`impl {trait_name}` must define `fn {req_name}`"),
                )
                .help(format!(
                    "the `{trait_name}` trait requires the `{req_name}` method"
                ));
            }
            // An arity of `None` is not pinned by the registry (`Callable`'s `call` takes whatever
            // the object needs — `obj(args)` forwards the call site's arguments).
            Some(m) if req_arity.is_some_and(|arity| m.params.len() != arity) => {
                let arity = req_arity.expect("guarded by is_some_and");
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`{req_name}` must take {arity} parameter(s), found {}",
                        m.params.len()
                    ),
                );
            }
            Some(_) => {}
        }
    }

    /// Validate a standalone `impl Trait for T {}` declaration. Two checks beyond the shared
    /// trait-side validation ([`Self::check_trait_impl`], also run): the **orphan rule** — `T` must
    /// be a struct/class/enum declared in this module, not a built-in or a `use`-imported name
    /// (E0013) — and the **built-in body restriction** — a *user* trait's standalone impl may
    /// carry method bodies (hoisted onto the target), but a built-in trait's must stay an
    /// empty-body marker (E0015). Coherence is enforced together with the target's
    /// `@derive`s/in-body impls in [`Self::check_coherence`].
    pub(crate) fn check_standalone_impl(&mut self, decl: &ImplDecl) {
        // A dotted trait path is a method-bundle binding (kernel-methods K1) with its own
        // validation — bundle resolution, packed-target + constraint checks, conflict rules. But a
        // cross-package **user trait** is *also* dotted once qualified (`para.aether.Store`), so a
        // known user trait is never a bundle — check that first, or a dependency's standalone
        // `impl Trait for T` would be misread as a bundle impl.
        if decl.trait_name.contains('.') && !self.symbols.user_traits.contains_key(&decl.trait_name)
        {
            self.check_bundle_impl(decl);
            return;
        }
        if !self.symbols.records.contains_key(&decl.target)
            && !self.symbols.enums.contains_key(&decl.target)
        {
            self.error(
                DiagnosticCode::UnknownType,
                decl.target_span,
                format!(
                    "cannot implement a trait for `{}`: it is not a record, class, or enum \
                         declared in this module",
                    decl.target
                ),
            )
            .help(
                "a standalone `impl` may only target a type you declare — implement the trait \
                     where the type is defined",
            );
        }
        // A standalone `impl` with a method body is supported for **user traits** (L1, UT2 — its
        // methods are hoisted onto the target type by the loader). A built-in trait's standalone
        // impl is still marker-only: its operator/protocol methods live in the type's own body.
        if !decl.methods.is_empty() && !self.symbols.user_traits.contains_key(&decl.trait_name) {
            self.error(
                DiagnosticCode::InvalidImpl,
                decl.span,
                "a standalone `impl` with methods is not yet supported for a built-in trait",
            )
            .help(
                "only an empty-body capability impl (e.g. `impl Serialize for X {}`) is \
                     supported here; write trait methods inside the type's own `class` body",
            );
        }
        self.check_trait_impl(
            &decl.trait_name,
            &decl.trait_args,
            decl.trait_span,
            &decl.methods,
        );
    }

    /// Validate that an `impl` of a user trait provides its contract (L1, UT2): every **required**
    /// (non-default) trait method must be present with matching arity and — when both sides annotate
    /// them — matching parameter and return types. Default methods may be omitted (their fallback
    /// body lands in UT5). Extra methods beyond the trait are allowed (inherent methods). Shares the
    /// E0015 `InvalidImpl` code with the built-in path.
    fn check_user_trait_impl(
        &mut self,
        decl: &noeta_ast::TraitDecl,
        trait_span: Span,
        methods: &[FnDecl],
    ) {
        for tm in &decl.methods {
            if tm.has_default {
                continue; // a default method is optional for an implementor
            }
            let req_name = &tm.sig.name;
            let Some(m) = methods.iter().find(|m| &m.name == req_name) else {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    trait_span,
                    format!("`impl {}` must define `fn {}`", decl.name, req_name),
                )
                .help(format!(
                    "the `{}` trait requires `fn {}`",
                    decl.name, req_name
                ));
                continue;
            };
            if m.params.len() != tm.sig.params.len() {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`{}` must take {} parameter(s) to satisfy trait `{}`, found {}",
                        req_name,
                        tm.sig.params.len(),
                        decl.name,
                        m.params.len()
                    ),
                );
                continue;
            }
            for (i, (tp, ip)) in tm.sig.params.iter().zip(&m.params).enumerate() {
                let want = field_type(&tp.ty, &self.imports.extern_types);
                let got = field_type(&ip.ty, &self.imports.extern_types);
                if !Self::sig_types_compatible(&want, &got) {
                    self.error(
                        DiagnosticCode::InvalidImpl,
                        ip.name_span,
                        format!(
                            "parameter {} of `{}` is `{got}`, but trait `{}` declares `{want}`",
                            i + 1,
                            req_name,
                            decl.name,
                        ),
                    );
                }
            }
            let want_ret = field_type(&tm.sig.ret, &self.imports.extern_types);
            let got_ret = field_type(&m.ret, &self.imports.extern_types);
            if !Self::sig_types_compatible(&want_ret, &got_ret) {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`{}` returns `{got_ret}`, but trait `{}` declares `{want_ret}`",
                        req_name, decl.name,
                    ),
                );
            }
        }
    }

    /// Two signature types conform if either side is unannotated (`Unknown`) or `dyn` — those defer
    /// — or they are equal. Deliberately structural-equality, not subtyping: a trait method's
    /// contract is its exact signature (UT2).
    fn sig_types_compatible(want: &Type, got: &Type) -> bool {
        matches!(want, Type::Unknown | Type::Dyn)
            || matches!(got, Type::Unknown | Type::Dyn)
            || want == got
    }

    /// Validate a user-defined `trait` declaration (L1, UT1). The declaration was registered in
    /// pass 1; here we reject a name that collides with a built-in trait, a declared type, or an
    /// earlier `trait` of the same name, plus duplicated method signatures within the body. Default
    /// method bodies are accepted syntactically but not yet type-checked (UT5).
    pub(crate) fn check_trait_decl(&mut self, decl: &noeta_ast::TraitDecl) {
        // A user trait may not shadow a built-in trait name — an `impl`/bound naming it would be
        // ambiguous against the closed built-in set.
        if BuiltinTrait::from_name(&decl.name).is_some() {
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!(
                    "`{}` is a built-in trait and cannot be redeclared",
                    decl.name
                ),
            );
        } else if self.symbols.types.contains(&decl.name)
            || self.symbols.records.contains_key(&decl.name)
            || self.symbols.enums.contains_key(&decl.name)
        {
            // A trait and a type sharing a name would make `dyn {name}` / `{name}` ambiguous.
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!(
                    "`{}` is already declared as a type; a trait cannot reuse the name",
                    decl.name
                ),
            );
        } else if self
            .symbols
            .user_traits
            .get(&decl.name)
            .is_some_and(|first| first.span != decl.span)
        {
            // A second `trait` of the same name; pass 1 kept the first.
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!("trait `{}` is declared more than once", decl.name),
            );
        }
        // A trait accepts `#[...]` data attributes only; the `@`-directives are type-only and do not
        // apply to a trait (UT6). Report the first offender.
        let bad_directive = if !decl.derives.is_empty() {
            Some("@derive")
        } else if decl.attribute.is_some() {
            Some("@attribute")
        } else if decl.role.is_some() {
            Some("@role")
        } else if decl.semantic.is_some() {
            Some("@semantic")
        } else if decl.packed.is_some() {
            Some("@packed")
        } else {
            None
        };
        if let Some(directive) = bad_directive {
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!("`{directive}` does not apply to a trait `{}`", decl.name),
            )
            .help(
                "a trait accepts only `#[...]` data attributes; `@derive`/`@attribute`/`@role`/\
                 `@semantic`/`@packed` are for data types",
            );
        }
        // Duplicate method signatures within the trait body.
        let mut seen: HashSet<&str> = HashSet::new();
        for m in &decl.methods {
            if !seen.insert(m.sig.name.as_str()) {
                self.error(
                    DiagnosticCode::InvalidTraitDeclaration,
                    m.sig.name_span,
                    format!(
                        "trait `{}` declares method `{}` more than once",
                        decl.name, m.sig.name
                    ),
                );
            }
        }
    }

    /// Validate a method-bundle binding `impl <module>.<Bundle> for T {}` (kernel-methods K1).
    /// The binding itself was recorded during collect (so method typing sees it regardless of
    /// statement order); this reports every impl-site violation: an unresolvable path, a
    /// non-empty body (the methods are native), a target that isn't a locally-declared `@packed`
    /// struct, a constraint mismatch (the shape check the raw-buffer kernels used to make at
    /// runtime, moved to compile time), and method-name conflicts with the target's own methods
    /// or an earlier binding's.
    pub(crate) fn check_bundle_impl(&mut self, decl: &ImplDecl) {
        let (module_ref, bundle_name) = decl.trait_name.rsplit_once('.').expect("dotted path");
        if !self.imports.modules.contains_key(module_ref) {
            self.error(
                DiagnosticCode::UnknownTrait,
                decl.trait_span,
                format!(
                    "unknown module `{module_ref}` in bundle path `{}`",
                    decl.trait_name
                ),
            )
            .help("bind the module first — e.g. `use std.{vec}` brings `vec` into scope");
            return;
        }
        let Some((_, bundle)) = self.resolve_bundle_ref(&decl.trait_name) else {
            self.error(
                DiagnosticCode::UnknownTrait,
                decl.trait_span,
                format!("module `{module_ref}` has no method bundle `{bundle_name}`"),
            );
            return;
        };
        if !decl.methods.is_empty() {
            self.error(
                DiagnosticCode::InvalidImpl,
                decl.span,
                "a bundle binding takes an empty body — its methods are native",
            )
            .help(format!(
                "`impl {} for {} {{}}` acquires the bundle's methods as the extension declares them",
                decl.trait_name, decl.target
            ));
        }
        let target_ty = Type::Named(decl.target.clone(), vec![]);
        let Some(layout) = self.packed_layout(&target_ty) else {
            self.error(
                DiagnosticCode::InvalidImpl,
                decl.target_span,
                format!(
                    "`{}` cannot bind `{}`: a method bundle binds to a `@packed` struct declared \
                     in this module",
                    decl.target, decl.trait_name
                ),
            )
            .help("mark the target `@packed` — bundles are packed-operations method sets");
            return;
        };
        if let Some(message) = constraint_mismatch(&layout, &bundle.constraint) {
            self.error(
                DiagnosticCode::InvalidImpl,
                decl.target_span,
                format!(
                    "`{}` does not satisfy `{}`: {message}",
                    decl.target, decl.trait_name
                ),
            );
            return;
        }
        // Conflicts, reported on the binding (the textually-later party). Receiver-aware: an
        // Element method lives on `T` — it may not collide with the target's own methods or
        // fields; a Bulk method lives on `List<T>` — it may not shadow a built-in list method
        // (the one namespace it joins). Cross-bundle collisions check within either kind.
        let mut conflicts: Vec<String> = Vec::new();
        for m in bundle.methods {
            match m.receiver {
                noeta_ext_abi::BundleReceiver::Element => {
                    if self
                        .symbols
                        .methods
                        .contains_key(&(decl.target.clone(), m.sig.name.to_string()))
                    {
                        conflicts.push(format!(
                            "`{}` already declares a method `{}`",
                            decl.target, m.sig.name
                        ));
                    }
                    if self
                        .symbols
                        .records
                        .get(&decl.target)
                        .is_some_and(|fields| fields.iter().any(|(f, _)| f == m.sig.name))
                    {
                        conflicts.push(format!(
                            "`{}` already declares a field `{}`",
                            decl.target, m.sig.name
                        ));
                    }
                }
                noeta_ext_abi::BundleReceiver::Bulk => {
                    if stdlib::method_return(
                        self.reg(),
                        &Type::List(Box::new(Type::Dyn)),
                        m.sig.name,
                    )
                    .is_some()
                    {
                        conflicts.push(format!("`{}` is a built-in list method", m.sig.name));
                    }
                }
            }
        }
        for earlier in self
            .symbols
            .bundle_impls
            .get(&decl.target)
            .into_iter()
            .flatten()
        {
            // Only bindings textually before this one (single-report discipline, like
            // `check_coherence`); skip this binding's own collect record.
            if earlier.bundle.name == bundle_name || earlier.span.start >= decl.trait_span.start {
                continue;
            }
            for m in bundle.methods {
                if earlier.bundle.method(m.sig.name).is_some() {
                    conflicts.push(format!(
                        "`{}` already acquires `{}` from bundle `{}`",
                        decl.target, m.sig.name, earlier.bundle.name
                    ));
                }
            }
        }
        for conflict in conflicts {
            self.error(
                DiagnosticCode::ConflictingTraitImpl,
                decl.trait_span,
                format!(
                    "{conflict} — binding `{}` would make the name ambiguous",
                    decl.trait_name
                ),
            );
        }
    }

    // (constraint_mismatch, the bundle-constraint comparison, is a free function below the impl.)

    /// Enforce **trait coherence** (overlap/uniqueness) on a single type: a trait may be
    /// implemented at most once, counting both a `@derive(T)` directive and an `impl T { }` block
    /// as implementations. A second implementation of an already-implemented trait — whether
    /// `@derive(T)` twice, two `impl T` blocks, or a `@derive(T)` alongside an `impl T` — is
    /// reported as `E0027 ConflictingTraitImpl`, pointing at the later occurrence and naming where
    /// the first one is. This keeps each `(type, trait)` pair single-implementation, so
    /// [`Self::satisfies`] and runtime dispatch are unambiguous.
    ///
    /// The orphan half of coherence is enforced separately: an in-body `impl` block can only name
    /// the type that owns it, and a standalone `impl Trait for T {}` is required (in
    /// [`Self::check_standalone_impl`]) to target a type declared in the same module — so a trait
    /// is still only ever implemented for a local type, and every trait is a built-in. Records and
    /// enums carry no in-body `impl` blocks (pass an empty slice); `standalone` carries the
    /// `(trait, span)` of every standalone impl targeting this type.
    pub(crate) fn check_coherence(
        &mut self,
        derives: &[DeriveSpec],
        impls: &[ImplBlock],
        standalone: &[(String, Span)],
    ) {
        // Source order is derives, then in-body impls, then standalone impls: this scan reports the
        // textually-later duplicate and names where the first one is.
        let mut seen: HashMap<&str, Span> = HashMap::new();
        let occurrences = derives
            .iter()
            .map(|d| (d.name.as_str(), d.span))
            .chain(impls.iter().map(|b| (b.trait_name.as_str(), b.trait_span)))
            .chain(standalone.iter().map(|(name, span)| (name.as_str(), *span)));
        for (name, span) in occurrences {
            match seen.get(name) {
                Some(_first) => {
                    self.error(
                        DiagnosticCode::ConflictingTraitImpl,
                        span,
                        format!("trait `{name}` is implemented more than once for this type"),
                    )
                    .help(format!(
                        "`{name}` is already implemented above; a type may implement each trait \
                         only once (via one `@derive` or one `impl` block, not both)"
                    ));
                }
                None => {
                    seen.insert(name, span);
                }
            }
        }
    }

    /// Validate the `@derive(...)` directives on a declaration: every named trait must be a known
    /// *derivable* built-in, with the right number of generic type arguments, and a generic derive's
    /// arguments must resolve. The compiler synthesizes the listed impls from the type's fields,
    /// parameterized by the arguments (e.g. `Serialize<Json>`'s format). The only parameterized
    /// derivable trait today is `Serialize<Format>`.
    pub(crate) fn check_derives(
        &mut self,
        type_name: &str,
        derives: &[DeriveSpec],
        fields: &[noeta_ast::FieldDecl],
        type_methods: &[FnDecl],
    ) {
        for spec in derives {
            let Some(t) = BuiltinTrait::from_name(&spec.name) else {
                // A USER trait derives through the shared planner (UT5 + bridging + `via:`
                // delegation): defaults adopted wholesale, required members bridged onto the
                // type's own fields/methods, or the whole trait forwarded through a field.
                if let Some(decl) = self.symbols.user_traits.get(&spec.name).cloned() {
                    self.check_user_trait_derive(type_name, spec, &decl, fields, type_methods);
                    continue;
                }
                // A NATIVE derive recipe (layer 4, `ExtDerive`): synthesizes handler forwards —
                // no bindings/via surface, plus the recipe's own optional shape validation.
                if let Some(ext) = self.reg().find_ext_derive(&spec.name) {
                    if let Some(b) = spec.bindings.first() {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            b.span,
                            format!(
                                "`{}: {}` — `{}` is a native derive with a fixed recipe; it takes \
                                 no member bindings",
                                b.member, b.target, spec.name
                            ),
                        );
                    } else if let Some((_, via_span)) = &spec.via {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            *via_span,
                            format!("`{}` is a native derive; `via:` does not apply", spec.name),
                        );
                    } else if let Some(validate) = ext.validate {
                        let shape: Vec<(String, String)> = fields
                            .iter()
                            .map(|f| {
                                let ty = field_type(&f.ty, &self.imports.extern_types);
                                (f.name.clone(), ty.to_string())
                            })
                            .collect();
                        if let Some(message) = validate(type_name, &shape) {
                            self.error(DiagnosticCode::UnderivableTrait, spec.span, message);
                        }
                    }
                    continue;
                }
                self.error(
                    DiagnosticCode::UnknownTrait,
                    spec.span,
                    format!("unknown trait `{}` in `@derive(...)`", spec.name),
                );
                continue;
            };
            // Layer-2 delegation on a built-in (`@derive(Comparable, via: amount)`): validated by
            // the shared template planner; the field-wise recipe machinery (arity, formats, the
            // E0050 field constraint) does not apply — the synthesized method carries the behavior.
            if let Some((via_name, via_span)) = &spec.via {
                if let Err(e) =
                    noeta_ast::derive::plan_builtin_via(&spec.name, type_name, fields, spec)
                {
                    let d = self.error(DiagnosticCode::UnderivableTrait, spec.span, e.message);
                    if let Some(h) = e.help {
                        d.help(h);
                    }
                } else if t == BuiltinTrait::Comparable {
                    // The forward is `self.f.compare(other.f)` — a via field whose type can
                    // NEVER order (the same judgement the field-wise recipe applies) would only
                    // fail at the first runtime comparison; reject it at the declaration. A via
                    // field mentioning one of the type's own generic parameters is deferred to
                    // the instantiation site instead (`satisfies` judges the substituted via
                    // field — S4's `via:` twin), exactly like the field-wise recipe's deferral.
                    let params: Vec<String> = self
                        .symbols
                        .generic_types
                        .get(type_name)
                        .cloned()
                        .unwrap_or_default();
                    let field_ty = self
                        .symbols
                        .records
                        .get(type_name)
                        .and_then(|fs| fs.iter().find(|(n, _)| n == via_name))
                        .map(|(_, ty)| ty.clone());
                    if let Some(ty) = field_ty
                        && !mentions_param(&ty, &params)
                        && !self.type_orderable(&ty, &mut Vec::new())
                    {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            *via_span,
                            format!("`via: {via_name}` has no ordering (`{ty}`)"),
                        )
                        .help("delegate `Comparable` through a field whose type orders");
                    }
                }
                continue;
            }
            if let Some(b) = spec.bindings.first() {
                self.error(
                    DiagnosticCode::UnderivableTrait,
                    b.span,
                    format!(
                        "`{}: {}` — member bindings apply to user-trait derives; `{}` is a \
                         built-in with a fixed recipe",
                        b.member, b.target, spec.name
                    ),
                )
                .help("use `via: <field>` to delegate a built-in trait through a field");
                continue;
            }
            if !t.derivable() {
                self.error(
                        DiagnosticCode::UnknownTrait,
                        spec.span,
                        format!("`{}` is not a derivable trait", spec.name),
                    )
                    .help(
                        "derivable traits are `Equatable`, `Comparable`, `Display`, `Clone`, \
                         `Serialize<Format>`, `Deserialize<Format>`; mark attribute records with the \
                         `@attribute` directive",
                    );
                continue;
            }
            // Generic arity: `Serialize` requires one type argument (`Serialize<Json>`); every other
            // derivable trait is nullary.
            let arity = t.generic_arity();
            if spec.args.len() != arity {
                let msg = if arity == 0 {
                    format!("`{}` takes no type arguments", spec.name)
                } else {
                    format!(
                        "`{}` takes {arity} type argument(s), found {}",
                        spec.name,
                        spec.args.len()
                    )
                };
                self.error(DiagnosticCode::UnknownTrait, spec.span, msg).help(
                        "`Serialize`/`Deserialize` are `@derive(Serialize<Json>)` / \
                         `@derive(Deserialize<Json>)`; the other derivable traits take no arguments",
                    );
                continue;
            }
            // `Serialize`/`Deserialize`'s argument is a serialization **format** (a blessed token, not a
            // general type), validated against the format vocabulary rather than the type namespace.
            if spec.name == "Serialize" || spec.name == "Deserialize" {
                self.check_serialize_format(&spec.args[0]);
            }
            // `Deserialize<Json>` (L2.2 DI): the type must decode from JSON — a non-generic value struct
            // all of whose fields are themselves decodable. `type_to_recipe` answers exactly that (it
            // returns `None` for a class/enum/generic, or a struct with an undecodable field), so it is
            // both the field-constraint check and the recipe the runtime registry needs. On success the
            // `(type_name, recipe)` pair is recorded for the backends to bake; on failure it is E0050.
            if spec.name == "Deserialize" {
                match self.type_to_recipe(&Type::Named(type_name.to_string(), Vec::new())) {
                    Some(recipe) => {
                        self.sites
                            .deserialize_recipes
                            .push((type_name.to_string(), recipe));
                    }
                    None => {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            spec.span,
                            format!(
                                "cannot derive `Deserialize<Json>` for `{type_name}`: it has a \
                                 field (or a shape) that cannot be decoded from JSON"
                            ),
                        )
                        .help(
                            "`Deserialize<Json>` is derivable for a value struct whose fields are all \
                             JSON-decodable (numbers, `bool`, `string`, `Option`, `List`, \
                             string-keyed `Map`, or another such struct)",
                        );
                    }
                }
            }
            self.check_derive_field_constraint(type_name, t, spec.span);
        }
    }

    /// Validate a `@derive(<UserTrait>)` through the shared planner (UT5 + derive layers 1+2):
    /// defaults adopt, explicit `member: target` bindings and name/unique-type deduction bridge the
    /// required methods, `via: field` forwards the whole trait. A plan failure is an E0050 carrying
    /// the planner's candidate list; a `via:` field whose type does not itself implement the trait
    /// is also E0050 (the forward would dispatch into nothing). A via field typed as one of the
    /// deriving type's own generic parameters defers to the instantiation site instead
    /// (`satisfies_user_trait` judges the substituted via field — S4's `via:` twin).
    fn check_user_trait_derive(
        &mut self,
        type_name: &str,
        spec: &DeriveSpec,
        decl: &noeta_ast::TraitDecl,
        fields: &[noeta_ast::FieldDecl],
        type_methods: &[FnDecl],
    ) {
        if let Err(e) = noeta_ast::derive::plan_user_trait_derive(decl, fields, type_methods, spec)
        {
            let d = self.error(DiagnosticCode::UnderivableTrait, spec.span, e.message);
            if let Some(h) = e.help {
                d.help(h);
            }
            return;
        }
        // `via:` forwards into the field's own implementation — that implementation must exist.
        if let Some((via, via_span)) = &spec.via
            && let Some(f) = fields.iter().find(|f| f.name == *via)
        {
            let params: Vec<String> = self
                .symbols
                .generic_types
                .get(type_name)
                .cloned()
                .unwrap_or_default();
            let satisfied = match &f.ty {
                Some(noeta_ast::TypeRef::Named { name, .. }) if params.contains(name) => {
                    true // parameter-typed — deferred to the instantiation site
                }
                Some(noeta_ast::TypeRef::Named { name, .. }) => self
                    .symbols
                    .user_trait_impls
                    .get(name)
                    .is_some_and(|traits| traits.contains_key(&decl.name)),
                Some(noeta_ast::TypeRef::DynTrait { trait_name, .. }) => trait_name == &decl.name,
                _ => false,
            };
            if !satisfied {
                self.error(
                    DiagnosticCode::UnderivableTrait,
                    *via_span,
                    format!(
                        "`via: {via}` forwards `{}` calls to the field, but its type does not \
                         implement `{}`",
                        decl.name, decl.name
                    ),
                )
                .help(format!(
                    "the field's type needs an `impl {} …` (or a `dyn {}` field type)",
                    decl.name, decl.name
                ));
            }
        }
    }

    /// The field constraint behind a derive (E0050): every field (struct/class) or variant payload
    /// (enum) must be able to support the derived behavior at runtime — `Comparable` needs an
    /// ordering, `Serialize` needs a JSON form. Rejects only what can **never** work (a `List` field
    /// has no ordering under any values); value-dependent cases (`dyn`, unions, extern contracts)
    /// stay permitted and defer to the runtime, and a field mentioning one of the type's own generic
    /// parameters is deferred to the instantiation site (conditional derive). Runs after `collect`,
    /// so forward references resolve.
    pub(crate) fn check_derive_field_constraint(
        &mut self,
        type_name: &str,
        t: BuiltinTrait,
        span: Span,
    ) {
        let ok = |checker: &Self, ty: &Type| match t {
            BuiltinTrait::Comparable => checker.type_orderable(ty, &mut Vec::new()),
            BuiltinTrait::Serialize => checker.type_serializable(ty, &mut Vec::new()),
            _ => true,
        };
        if matches!(t, BuiltinTrait::Comparable | BuiltinTrait::Serialize) {
            let params: Vec<String> = self
                .symbols
                .generic_types
                .get(type_name)
                .cloned()
                .unwrap_or_default();
            let offender = if let Some(fields) = self.symbols.records.get(type_name) {
                fields
                    .iter()
                    .find(|(_, ty)| !mentions_param(ty, &params) && !ok(self, ty))
                    .map(|(fname, ty)| (format!("field `{fname}`"), ty.clone()))
            } else {
                self.symbols.enums.get(type_name).and_then(|variants| {
                    variants.iter().find_map(|v| {
                        v.fields
                            .iter()
                            .find(|ty| !mentions_param(ty, &params) && !ok(self, ty))
                            .map(|ty| (format!("variant `{}`'s payload", v.name), ty.clone()))
                    })
                })
            };
            if let Some((place, ty)) = offender {
                let (need, fix) = match t {
                    BuiltinTrait::Comparable => ("no ordering", "remove `Comparable`"),
                    _ => ("no serialized form", "remove `Serialize`"),
                };
                self.error(
                    DiagnosticCode::UnderivableTrait,
                    span,
                    format!(
                        "cannot derive `{}` for `{type_name}`: {place} has type `{ty}`, which has {need}",
                        t.name(),
                    ),
                )
                .help(format!("{fix}, or change {place} to a supported type"));
            }
        }
    }

    /// Whether a value of type `ty` has a defined ordering at runtime — the static mirror of the
    /// runtime `compare_primitive`/`compare_field` pair (which recurses into nested struct/class
    /// fields and enum payloads **structurally**, regardless of the nested type's own derives).
    /// `false` only for kinds that can never order (containers, tuples, `bytes`, functions);
    /// value-dependent kinds (`dyn`, holes, unions, extern contracts like `Uuid`) are permissive.
    /// `visited` guards recursive nominals, like [`Self::is_send`].
    pub(crate) fn type_orderable(&self, ty: &Type, visited: &mut Vec<String>) -> bool {
        match ty {
            Type::Unknown
            | Type::Dyn
            | Type::DynTrait(_)
            | Type::Int
            | Type::Float
            | Type::F32
            | Type::F64
            | Type::IntN { .. }
            | Type::Bool
            | Type::String => true,
            // A union's members order value-dependently (two ints do, an int and a string don't);
            // like `dyn`, that is the runtime's call, not a statically-impossible ordering.
            Type::Union(_) => true,
            // `?T` / `Result<T, E>` are the prelude enums — ordered by variant then payload (the
            // same structural rule as a named enum), so orderability follows the payloads.
            Type::Option(e) => self.type_orderable(e, visited),
            Type::Result(a, b) => {
                self.type_orderable(a, visited) && self.type_orderable(b, visited)
            }
            Type::Named(name, args) => match self.symbols.type_kinds.get(name) {
                Some(noeta_types::TypeKind::Struct) | Some(noeta_types::TypeKind::Class) => {
                    if visited.iter().any(|v| v == name) {
                        return true; // recursive nominal — covered by the outer frame
                    }
                    visited.push(name.clone());
                    let subst = self.type_arg_subst(name, args);
                    let ordered = self.symbols.records.get(name).is_none_or(|fs| {
                        fs.iter()
                            .all(|(_, t)| self.type_orderable(&apply_subst(t, &subst), visited))
                    });
                    visited.pop();
                    ordered
                }
                Some(noeta_types::TypeKind::Enum) => {
                    if visited.iter().any(|v| v == name) {
                        return true;
                    }
                    visited.push(name.clone());
                    let subst = self.type_arg_subst(name, args);
                    let ordered = self.symbols.enums.get(name).is_none_or(|vs| {
                        vs.iter().all(|v| {
                            v.fields
                                .iter()
                                .all(|t| self.type_orderable(&apply_subst(t, &subst), visited))
                        })
                    });
                    visited.pop();
                    ordered
                }
                // An unknown-kind nominal (an extern type like `Uuid`, or the prelude `Ordering`)
                // orders through its runtime contract — permissive, the contract decides.
                None => true,
            },
            // No runtime ordering exists for these under any values.
            Type::Unit
            | Type::Bytes
            | Type::List(_)
            | Type::Map(..)
            | Type::Set(_)
            | Type::Tuple(_)
            | Type::Fn { .. }
            | Type::Kind(_) => false,
        }
    }

    /// Whether a value of type `ty` has a JSON form — the field constraint behind
    /// `@derive(Serialize<Json>)`. Only function values can never serialize; containers and
    /// nominals recurse (with the [`Self::is_send`]-style `visited` guard); everything
    /// value-dependent stays permissive.
    pub(crate) fn type_serializable(&self, ty: &Type, visited: &mut Vec<String>) -> bool {
        match ty {
            Type::Fn { .. } | Type::Kind(_) => false,
            Type::List(e) | Type::Set(e) | Type::Option(e) => self.type_serializable(e, visited),
            Type::Map(k, v) | Type::Result(k, v) => {
                self.type_serializable(k, visited) && self.type_serializable(v, visited)
            }
            Type::Tuple(elems) | Type::Union(elems) => {
                elems.iter().all(|e| self.type_serializable(e, visited))
            }
            Type::Named(name, args) => {
                if visited.iter().any(|v| v == name) {
                    return true;
                }
                visited.push(name.clone());
                let subst = self.type_arg_subst(name, args);
                let fields_ok = self.symbols.records.get(name).is_none_or(|fs| {
                    fs.iter()
                        .all(|(_, t)| self.type_serializable(&apply_subst(t, &subst), visited))
                }) && self.symbols.enums.get(name).is_none_or(|vs| {
                    vs.iter().all(|v| {
                        v.fields
                            .iter()
                            .all(|t| self.type_serializable(&apply_subst(t, &subst), visited))
                    })
                });
                visited.pop();
                fields_ok
            }
            _ => true,
        }
    }

    /// Validate a `Serialize<Format>` derive's format argument: it must be one of the blessed
    /// formats (`Json`). A non-format type — `Serialize<int>`, `Serialize<List<int>>` — or an unknown
    /// name is `E0013`.
    pub(crate) fn check_serialize_format(&mut self, arg: &TypeRef) {
        let ok = matches!(
            arg,
            TypeRef::Named { name, args, .. }
                if args.is_empty() && noeta_types::SERIALIZE_FORMATS.contains(&name.as_str())
        );
        if !ok {
            self.error(
                DiagnosticCode::UnknownType,
                arg.span(),
                "expected a serialization format".to_string(),
            )
            .help(format!(
                "the formats are {}",
                noeta_types::SERIALIZE_FORMATS.join(", ")
            ));
        }
    }

    /// Instantiate and check a generic function call. Binds each type parameter from the argument
    /// types (left to right, first concrete argument wins), checks every argument against its
    /// substituted parameter type (`E0007`), enforces each parameter's trait bounds (`E0025`), and
    /// returns the substituted result type (any type parameter the arguments left unbound erases to
    /// `dyn`). Arity mismatch is reported exactly as a non-generic call's.
    /// `recv_args` seeds the substitution for an **instance** method call: the receiver's type
    /// arguments are bound to the class's type parameters positionally (`box: Box<int>` → `T=int`),
    /// so the method's result is precise and its bounds enforced against the receiver's instantiation.
    /// Empty for a free function or a static call (the arguments alone instantiate the parameters).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_generic_call(
        &mut self,
        name: &str,
        generic: &GenericInfo,
        required: usize,
        args: &mut [Type],
        arg_exprs: &[Expr],
        span: Span,
        recv_args: &[Type],
        env: &mut Env,
    ) -> Type {
        let tps: HashSet<String> = generic.params.iter().map(|(n, _)| n.clone()).collect();
        if args.len() < required || args.len() > generic.raw_params.len() {
            let expected = if required == generic.raw_params.len() {
                format!("{}", generic.raw_params.len())
            } else {
                format!("between {required} and {}", generic.raw_params.len())
            };
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{name}` expects {expected} argument(s), found {}",
                    args.len()
                ),
            );
            return erase_type_params(generic.raw_ret.clone(), &tps);
        }
        // Seed with the receiver's type arguments (instance call); the call's own arguments then
        // refine any still-unbound parameters without overwriting the receiver's binding.
        let mut subst: HashMap<String, Type> = generic
            .params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(recv_args.iter().cloned())
            .filter(|(_, t)| !t.defers_to_runtime())
            .collect();
        for (i, raw) in generic.raw_params.iter().enumerate() {
            if i >= args.len() {
                // Omitted trailing defaults — already checked at the declaration.
                break;
            }
            // A deferred closure argument finalizes against the raw parameter with everything
            // bound SO FAR substituted in — `fn each<T>(xs: List<T>, f: (T) -> unit)` has `T`
            // pinned by `xs` before `f` is looked at — and its now-known type (the inferred
            // return especially) then binds any parameter the earlier arguments did not
            // (`fn pick<T>(f: () -> T): T`).
            if let Some(expr) = arg_exprs.get(i)
                && is_deferred_literal_arg(expr)
                && matches!(args[i], Type::Unknown)
            {
                let expected = erase_type_params(apply_subst(raw, &subst), &tps);
                // Absorb the (substituted) parameter type into the deferred literal — a `Fn` into a
                // closure, a `List`/`Map` into a container literal — so its resolved type then binds
                // any still-unbound type parameter below; a mismatched or unguiding param synthesizes
                // standalone (unchanged from the closure-only behavior).
                args[i] = match (expr, &expected) {
                    (Expr::Closure { .. }, Type::Fn { .. }) => self.check(expr, &expected, env),
                    (Expr::List { .. } | Expr::Map { .. }, Type::List(_) | Type::Map(..)) => {
                        self.check(expr, &expected, env)
                    }
                    _ => self.synth(expr, env),
                };
            }
            let arg = args[i].clone();
            bind_type_params(raw, &arg, &tps, &mut subst);
            let expected = apply_subst(raw, &subst);
            let arg = &arg;
            // A bare literal adapts into a fixed-width parameter here too (P-NUM-SYM) — whether the
            // parameter is a concrete `u8`/`f32`/`f64` or a type variable already bound to one
            // (`g(200u8, 200)` binds `T = u8`, so the second `200` narrows). Tried before the
            // type-based `arg_assignable`, exactly as in `check_args`.
            if let Some(expr) = arg_exprs.get(i)
                && self.try_adapt_literal(expr, &expected).is_some()
            {
                continue;
            }
            if !self.arg_assignable(arg, &expected) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{expected}`"),
                );
            }
        }
        for (pname, bounds) in &generic.params {
            let Some(concrete) = subst.get(pname) else {
                continue; // unconstrained by the arguments — nothing concrete to check against
            };
            for bound in bounds {
                // A user-defined trait bound (L1, UT3): satisfied iff `concrete` has a recorded
                // `impl` of it — and, for an INSTANTIATED bound (`T: Keyed<int>`), an impl at that
                // instantiation. A bound argument may mention a sibling parameter (`<K, T:
                // Keyed<K>>`), so the call's own substitution applies first; a parameter the
                // arguments leave unbound erases to `dyn` and defers.
                if self.symbols.user_traits.contains_key(&bound.name) {
                    let want: Vec<Type> = bound
                        .args
                        .iter()
                        .map(|a| erase_type_params(apply_subst(a, &subst), &tps))
                        .collect();
                    if !self.satisfies_user_trait(concrete, &bound.name, &want) {
                        let shown = bound_display(&bound.name, &want);
                        self.error(
                            DiagnosticCode::TraitBoundNotSatisfied,
                            span,
                            format!(
                                "type `{concrete}` does not satisfy the bound `{shown}` on type \
                                 parameter `{pname}` of `{name}`"
                            ),
                        )
                        .help(format!("`{concrete}` must `impl {shown}` to be used here"));
                    }
                    continue;
                }
                // Bounds on a collected signature are validated trait names (E0014 otherwise); a
                // non-built-in, non-user name is unreachable here, so skip rather than falsely report.
                let Some(t) = BuiltinTrait::from_name(&bound.name) else {
                    continue;
                };
                let bound = &bound.name;
                if !self.satisfies(concrete, t) {
                    let help = if t.intrinsic() {
                        format!(
                            "`{bound}` is a built-in capability — only the runtime types that \
                             provide it (the CRDT types for `Mergeable`) satisfy this bound"
                        )
                    } else {
                        format!("`{concrete}` must `@derive` or `impl {bound}` to be used here")
                    };
                    self.error(
                        DiagnosticCode::TraitBoundNotSatisfied,
                        span,
                        format!(
                            "type `{concrete}` does not satisfy the bound `{bound}` on type \
                                 parameter `{pname}` of `{name}`"
                        ),
                    )
                    .help(help);
                }
            }
        }
        erase_type_params(apply_subst(&generic.raw_ret, &subst), &tps)
    }

    /// Whether `ty` satisfies the built-in trait `trait_name`. A `dyn`/inference-hole satisfies
    /// every bound (deferred to runtime / no information — never a false positive). A user type
    /// satisfies a trait it `@derive`s or `impl`s; a built-in type satisfies the traits the
    /// backends actually dispatch for it ([`builtin_satisfies`]).
    pub(crate) fn satisfies(&self, ty: &Type, t: BuiltinTrait) -> bool {
        if ty.defers_to_runtime() {
            return true;
        }
        if let Type::Named(n, args) = ty {
            if !self
                .symbols
                .trait_impls
                .get(n)
                .is_some_and(|s| s.contains(&t))
            {
                return false;
            }
            // A **generic** derive is conditional (derive-soundness S4): `Box<T>` deriving
            // `Comparable` is satisfied only when the *instantiated* field types are orderable
            // (`Box<int>` yes, `Box<List<int>>` no) — the instantiation-site twin of the E0050
            // declaration check, which deferred parameter-typed fields to here. The predicates
            // substitute the arguments into the field types, so a non-generic type (already
            // validated at its declaration) passes trivially. A hand-written `impl` is
            // unconditional — the author wrote the body, no field constraint applies.
            if !args.is_empty()
                && self
                    .symbols
                    .derived_traits
                    .get(n)
                    .is_some_and(|s| s.contains(&t))
            {
                // A `via:` derive's condition is the **via field's** alone — delegation exists
                // precisely so sibling fields don't constrain the trait (S4's `via:` twin).
                if let Some(field) = self.via_field(n, t.name()) {
                    return match t {
                        BuiltinTrait::Comparable => self
                            .field_type_at(n, args, &field)
                            .is_none_or(|ft| self.type_orderable(&ft, &mut Vec::new())),
                        _ => true,
                    };
                }
                return match t {
                    BuiltinTrait::Comparable => self.type_orderable(ty, &mut Vec::new()),
                    BuiltinTrait::Serialize => self.type_serializable(ty, &mut Vec::new()),
                    _ => true,
                };
            }
            return true;
        }
        builtin_satisfies(ty, t)
    }

    /// The `via:` field through which `type_name`'s derive of `trait_name` delegates, if that
    /// membership came from a `via:` derive at all.
    fn via_field(&self, type_name: &str, trait_name: &str) -> Option<String> {
        self.symbols
            .via_derives
            .get(type_name)?
            .iter()
            .find(|(t, _)| t == trait_name)
            .map(|(_, f)| f.clone())
    }

    /// A named type's field type at the given instantiation: the declared field type with the
    /// instance's type arguments substituted for the declaration's parameters.
    fn field_type_at(&self, type_name: &str, args: &[Type], field: &str) -> Option<Type> {
        let ty = self
            .symbols
            .records
            .get(type_name)?
            .iter()
            .find(|(n, _)| n == field)
            .map(|(_, t)| t.clone())?;
        let subst = self.type_arg_subst(type_name, args);
        Some(apply_subst(&ty, &subst))
    }

    /// Whether `ty` implements the user trait named `bound` (L1, UT3) — and, when `want` is
    /// non-empty, at that demanded instantiation (`T: Keyed<int>` is satisfied only by an `impl
    /// Keyed<int>`; an empty `want` is a bare bound, any instantiation). Only a named user type
    /// can — via a recorded in-body or standalone `impl` (`user_trait_impls`). A
    /// `dyn`/inference-hole defers to runtime (never a false negative); a built-in/primitive type
    /// never implements a user trait.
    fn satisfies_user_trait(&self, ty: &Type, bound: &str, want: &[Type]) -> bool {
        self.satisfies_user_trait_inner(ty, bound, want, &mut Vec::new())
    }

    /// The recursive worker: `visited` guards a recursive nominal reached through a `via:` chain
    /// (covered by the outer frame — the same convention as [`Self::type_orderable`]).
    fn satisfies_user_trait_inner(
        &self,
        ty: &Type,
        bound: &str,
        want: &[Type],
        visited: &mut Vec<String>,
    ) -> bool {
        if ty.defers_to_runtime() {
            return true;
        }
        match ty {
            // A trait object carries the trait, not an instantiation — permissive on `want`
            // (dispatch is by name at runtime; there is nothing static to hold the args against).
            Type::DynTrait(t) => t == bound,
            Type::Named(n, args) => {
                let Some(impl_args) = self
                    .symbols
                    .user_trait_impls
                    .get(n)
                    .and_then(|impls| impls.get(bound))
                else {
                    return false;
                };
                // An instantiated bound demands an impl at that instantiation (argument-wise,
                // with `dyn`/holes deferring on either side).
                if !want.is_empty()
                    && (impl_args.len() != want.len()
                        || impl_args
                            .iter()
                            .zip(want)
                            .any(|(a, b)| !bound_arg_matches(a, b)))
                {
                    return false;
                }
                // A GENERIC type whose membership came from a `via:` derive is conditional on the
                // substituted via field implementing the trait itself (S4's `via:` twin) — the
                // instantiation-site side of the declaration check, which deferred a
                // parameter-typed via field to here.
                if !args.is_empty()
                    && !visited.iter().any(|v| v == n)
                    && let Some(field) = self.via_field(n, bound)
                    && let Some(ft) = self.field_type_at(n, args, field.as_str())
                {
                    visited.push(n.clone());
                    let ok = self.satisfies_user_trait_inner(&ft, bound, want, visited);
                    visited.pop();
                    return ok;
                }
                true
            }
            _ => false,
        }
    }

    /// Enforce the **trait bounds** on a registry function's bounded type variables (p2p P2): each
    /// bound var is bound to a concrete type by the call's arguments (`module_var_bounds`), and that
    /// type must satisfy the named trait or it is `E0025`. An undetermined var yields nothing
    /// (gradual). This is the registry-call twin of the user-generic bound check.
    pub(crate) fn check_module_bounds(
        &mut self,
        module: &str,
        func: &str,
        args: &[Type],
        span: Span,
    ) {
        for (concrete, bound) in stdlib::module_var_bounds(self.reg(), module, func, args) {
            let Some(t) = BuiltinTrait::from_name(bound) else {
                continue;
            };
            if self.satisfies(&concrete, t) {
                continue;
            }
            let help = if t.intrinsic() {
                format!(
                    "`{bound}` is a built-in capability — only the runtime types that provide it \
                     (the CRDT types `GCounter`/`PnCounter`/`GSet`) satisfy this bound"
                )
            } else {
                format!("`{concrete}` must `@derive` or `impl {bound}`")
            };
            self.error(
                DiagnosticCode::TraitBoundNotSatisfied,
                span,
                format!("type `{concrete}` does not satisfy the bound `{bound}`"),
            )
            .help(help);
        }
    }
}

/// Whether a recorded impl argument satisfies a demanded bound argument: exact type equality,
/// with a `dyn`/inference-hole on either side deferring to the runtime (never a false negative).
fn bound_arg_matches(have: &Type, want: &Type) -> bool {
    have.defers_to_runtime() || want.defers_to_runtime() || have == want
}

/// Render a bound for a diagnostic: the bare name, or `Name<args>` for an instantiated bound.
fn bound_display(name: &str, args: &[Type]) -> String {
    if args.is_empty() {
        name.to_string()
    } else {
        let args: Vec<String> = args.iter().map(Type::to_string).collect();
        format!("{name}<{}>", args.join(", "))
    }
}
