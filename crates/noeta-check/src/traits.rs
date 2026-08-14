//! **Trait machinery**: `impl Trait` blocks (built-in and bundle), coherence, `@derive`
//! validation with field constraints, orderability/serializability classification, and the
//! generic-call instantiation + trait-bound enforcement (`satisfies`/module bounds). All
//! `Checker` methods moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;
use crate::stdlib::DeriveApply;

impl Checker {
    // ----- traits: impl coherence and derive validation (M1.8) -----

    /// Validate an in-body `impl Trait { ... }` block: the trait must be a known built-in, and the
    /// block must provide the trait's required method with the right arity. The impl's method
    /// *bodies* are checked separately (they are flattened into `ClassDecl::methods`).
    pub(crate) fn check_impl(&mut self, block: &ImplBlock) {
        // The implementor is the type whose body this block sits in — the current coloring type.
        let target = self.coloring.current_type.clone().unwrap_or_default();
        self.check_trait_impl(
            &target,
            block.trait_name.as_str(),
            &block.trait_args,
            block.trait_span,
            &block.methods,
            &block.assoc_bindings,
        );
    }

    /// The trait-side validation shared by in-body `impl` blocks and standalone `impl Trait for T`
    /// declarations: the trait must be a known built-in, and a non-marker trait must be given its
    /// required method with the right arity. (The orphan rule and the standalone-only body
    /// restriction are enforced by the caller, [`Self::check_standalone_impl`].)
    pub(crate) fn check_trait_impl(
        &mut self,
        target: &str,
        trait_name: &str,
        trait_args: &[noeta_ast::TypeRef],
        trait_span: Span,
        methods: &[FnDecl],
        assoc_bindings: &[(String, noeta_ast::TypeRef)],
    ) {
        // A method that implements a trait is part of the type's **public** surface, necessarily:
        // a trait is an outward contract, and anyone holding a `dyn Trait` — or a `<T: Trait>`
        // generic body — calls it. So it must SAY so (method-visibility arc). Required explicitly
        // rather than implied, on the same ground the rest of the language reads: a declaration
        // says what it means, and a reader of the `impl` block should not have to know which names
        // the trait happens to declare to know which of these methods are callable from outside.
        // The implied alternative also makes the visibility of a method change when a trait adds or
        // removes it, at a distance, with nothing at the impl site to read.
        //
        // Checked at the shared funnel, so an in-body `impl Trait { }` and a standalone
        // `impl Trait for T { }` state the same requirement in the same words for every trait,
        // built-in and user-declared alike.
        for m in methods.iter().filter(|m| !m.is_public) {
            self.error(
                DiagnosticCode::InvalidImpl,
                m.name_span,
                format!(
                    "`{}` implements `{trait_name}`, so it must be declared `pub`",
                    m.name
                ),
            )
            .help(format!(
                "a trait is an outward contract — anyone holding a `dyn {trait_name}` calls this \
                 method — so write `pub fn {}(…)`; methods are otherwise private by default",
                m.name
            ));
        }
        // A user-defined trait (L1, UT2): validate conformance against its declared contract —
        // instantiated at the impl's type arguments when the trait is generic (`impl
        // Cache<string>` checks `fn get(k: string): …`) — then return before the built-in
        // resolution below (which would otherwise report E0014).
        if let Some(decl) = self.symbols.user_traits.get(trait_name).cloned() {
            match noeta_ast::derive::instantiate_trait(&decl, trait_args) {
                Ok(instantiated) => {
                    self.check_user_trait_impl(
                        target,
                        instantiated.as_ref().unwrap_or(&decl),
                        trait_span,
                        methods,
                        assoc_bindings,
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
        // `From` is the one built-in whose `impl` carries a real type argument
        // (`impl From<Source>`); every other built-in impl is bare. The argument's resolution is
        // validated below (E0013 for a ghost name), and the resolved source was recorded at
        // collection ([`Self::record_from_impls`]).
        if trait_name == BuiltinTrait::From.name() {
            if trait_args.len() != 1 {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    trait_span,
                    "`From` takes exactly one type argument (`impl From<Source>`)".to_string(),
                );
                return;
            }
            self.check_type_ref(&trait_args[0]);
        } else if !trait_args.is_empty() {
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
        // No built-in trait declares an `async` method — the runtime invokes these protocols
        // (`to_string`, `compare`, `add`, `call`, …) for a value, not for a future. An `async`
        // implementation would hand the protocol a `Future<T>` where it reads a `T`, the same
        // contract break a user trait's `async` mismatch is (E0015, below), so it is refused for the
        // same reason: the caller's typing comes from the trait, never from the body.
        for m in methods.iter().filter(|m| m.is_async) {
            self.error(
                DiagnosticCode::InvalidImpl,
                m.name_span,
                format!(
                    "`{}` cannot be `async`: `{trait_name}` is a built-in trait",
                    m.name
                ),
            )
            .help(
                "the runtime invokes a built-in protocol for a value, not a future; drop `async` \
                 and await inside the caller instead",
            );
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
        // A built-in trait declares receiver-ness the same way a `.noe` trait does — with `static`
        // ([`BuiltinTrait::declares_static`]) — and it is enforced by the same rule, spelled once
        // (static-trait-methods arc). This USED to be a bespoke `if t == BuiltinTrait::From` that
        // scanned the impl body for `self` and reported in `From`'s own words; `From::from` is now
        // simply the one built-in whose contract says `static`, so the next static protocol
        // inherits the check instead of needing a second copy of it.
        if t.declares_static()
            && let Some(m) = methods.iter().find(|m| m.name == req_name)
            && m.body.iter().any(|s| s.mentions("self"))
        {
            self.error(
                DiagnosticCode::InvalidImpl,
                m.name_span,
                format!("`{req_name}` uses `self`, but trait `{trait_name}` declares it `static`"),
            )
            .help(format!(
                "a `static` method takes no receiver — build and return the new value from \
                 `{req_name}`'s parameters alone"
            ));
        }
        // `From` is the one built-in whose `impl` carries a type argument, so it is also the one
        // whose declared source must agree with the method's annotated parameter. That is a rule
        // about the trait's TYPE ARGUMENT, not about its receiver — the receiver half above is now
        // general.
        if t == BuiltinTrait::From
            && let Some(m) = methods.iter().find(|m| m.name == "from")
            && let (Some(arg), Some(param)) = (trait_args.first(), m.params.first())
        {
            let want = self.annot(arg);
            let got = self.annot_field(&param.ty);
            if !Self::sig_types_compatible(&want, &got) {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    param.name_span,
                    format!(
                        "`from` converts the declared source `{want}`, but its parameter is \
                         `{got}`"
                    ),
                );
            }
        }
        // `Validate`'s `validate` must return `Result<void, E>` where `E` is a plain `string` or any
        // `Error`-implementing type — the shape both the `?`-conversion path and the recipe-seam
        // auto-enforcement (validation arc slice 2) rely on. (Presence + arity 0 were checked
        // above; here we pin the return.)
        if t == BuiltinTrait::Validate
            && let Some(m) = methods.iter().find(|m| m.name == "validate")
        {
            let ret = self.annot_field(&m.ret);
            let ok_shape = match &ret {
                Type::Result(ok, err) if matches!(**ok, Type::Unit) => Some((**err).clone()),
                _ => None,
            };
            let err_ok = ok_shape.as_ref().is_some_and(|err| {
                matches!(err, Type::String) || self.satisfies(err, BuiltinTrait::Error)
            });
            if !err_ok {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`validate` must return `Result<void, E>` where `E` is `string` or a type \
                         implementing `Error`, found `{ret}`"
                    ),
                )
                .help(
                    "return `Ok()` when the value is well-formed and `Err(e)` with a `string` or \
                     `Error` payload when an invariant is violated",
                );
            }
        }
    }

    /// Validate a standalone `impl Trait for T {}` declaration. Three checks beyond the shared
    /// trait-side validation ([`Self::check_trait_impl`], also run): `T` must be a struct, class, or
    /// enum **the program declares**, not a built-in or an unresolved name (E0013); the **package
    /// orphan rule** ([`Self::check_package_orphan`], E0070); and the **built-in body restriction** —
    /// a *user* trait's standalone impl may carry method bodies (hoisted onto the target), but a
    /// built-in trait's must stay an empty-body marker (E0015).
    ///
    /// "The program declares" is the whole linked program, not the one file: the checker runs
    /// downstream of the linker, so an imported type is a declared type like any other and a module
    /// may implement a trait for a **sibling's** type. That stays true and is deliberately
    /// unrestricted — the orphan rule's boundary is the *package*, not the file, so cross-module
    /// impls within one package are as legal as they ever were. What the rule adds is that the
    /// package writing the impl must be the one that declares the trait or the type. Uniqueness —
    /// the other half of coherence — is enforced separately, together with the target's `@derive`s
    /// and in-body impls, in [`Self::check_coherence`] (E0027).
    pub(crate) fn check_standalone_impl(&mut self, decl: &ImplDecl, env: &mut Env) {
        // A dotted trait path is a method-bundle binding (kernel-methods K1) with its own
        // validation — bundle resolution, packed-target + constraint checks, conflict rules. But a
        // cross-package **user trait** is *also* dotted once qualified (`para.aether.Store`), so a
        // known user trait is never a bundle — check that first, or a dependency's standalone
        // `impl Trait for T` would be misread as a bundle impl.
        if decl.trait_name.as_str().contains('.')
            && !self
                .symbols
                .user_traits
                .contains_key(decl.trait_name.as_str())
        {
            self.check_bundle_impl(decl);
            // Override bodies are now permitted (ExtBundle→ExtTrait fold-in, slice 4): a kernel binding
            // may provide a method to override the trait's native default, so its bodies are checked
            // like any standalone-impl body (was forbidden — "empty body required" — under the bundle).
            self.check_standalone_impl_bodies(decl, env);
            return;
        }
        if !self.symbols.records.contains_key(decl.target.as_str())
            && !self.symbols.enums.contains_key(decl.target.as_str())
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
        self.check_package_orphan(decl);
        // A standalone `impl` with a method body is supported for **user traits** (L1, UT2 — its
        // methods are hoisted onto the target type by the loader). A built-in trait's standalone
        // impl is still marker-only: its operator/protocol methods live in the type's own body.
        if !decl.methods.is_empty()
            && !self
                .symbols
                .user_traits
                .contains_key(decl.trait_name.as_str())
        {
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
            decl.target.as_str(),
            decl.trait_name.as_str(),
            &decl.trait_args,
            decl.trait_span,
            &decl.methods,
            &decl.assoc_bindings,
        );
        self.check_standalone_impl_bodies(decl, env);
    }

    /// The **package orphan rule** (E0070): a standalone `impl Trait for Type` must live in the same
    /// package as the trait **or** as the type.
    ///
    /// Noeta links one whole program at a time, so coherence's *uniqueness* half is always
    /// answerable and needs no orphan rule to be decidable. This rule exists for a different reason:
    /// without it a **transitive dependency** can implement one package's trait for another
    /// package's type, and that behavior shows up in an application that imports both and mentions
    /// the implementing package nowhere — `t is dyn Speaks` silently becomes true and `t.speak()`
    /// runs code the author never wrote down. Two such packages in one graph then collide as an
    /// E0027 the end user cannot fix: they own neither impl, so they can remove neither. The cost of
    /// the feature is global and invisible; its value — attaching behavior to a foreign type — is
    /// already served ergonomically by the newtype (`@derive(Trait, via: field)`), which is what the
    /// help points at.
    ///
    /// **Provenance, not a heuristic.** The judgement reads the loader's per-source
    /// [`noeta_span::PackageMap`] ([`Checker::package_at`]). A `namespace` would be the tempting
    /// proxy and is the wrong one — it is declared per file with no required relationship to the
    /// package that shipped it, so it would both admit real orphans and reject legitimate
    /// same-package impls. Where provenance is *unknown* the rule stands down entirely: a
    /// single-file check, a REPL fragment, a synthesized program, and compile-time generated code
    /// are never judged.
    ///
    /// **What counts as "local".** The type is local when it is declared in a source belonging to
    /// the impl's package. The trait is local on the same terms — but a **built-in** trait
    /// (`Display`, `Comparable`, …) and a **native** trait seeded from the extension registry belong
    /// to no package at all, so neither can make an impl local; such an impl must live with its
    /// type. That is the same judgement Rust makes about `impl std::fmt::Display for ForeignType`.
    /// Method-bundle bindings (`impl vec.Kernels for T {}`) return before this point: they are a
    /// native kernel-binding mechanism with their own conflict rules, not trait impls.
    pub(crate) fn check_package_orphan(&mut self, decl: &ImplDecl) {
        // Unknown provenance ⇒ no judgement. Cloned so the `&mut self` diagnostic below does not
        // conflict with the borrow of `self.config.provenance.packages`.
        let Some(impl_pkg) = self.package_at(decl.span).cloned() else {
            return;
        };
        let type_span = self
            .symbols
            .type_decl_spans
            .get(decl.target.as_str())
            .copied();
        let Some(type_pkg) = type_span.and_then(|s| self.package_at(s)).cloned() else {
            return;
        };
        // Same package as the type: legal, whichever module either sits in.
        if impl_pkg == type_pkg {
            return;
        }
        // A trait that resolves to nothing at all is already E0014 at this same impl; reporting an
        // orphan on top would cascade *and* describe a trait that does not exist as "built into the
        // language". Only a real trait — a built-in or a declared one — is judged.
        if BuiltinTrait::from_name(decl.trait_name.as_str()).is_none()
            && !self
                .symbols
                .user_traits
                .contains_key(decl.trait_name.as_str())
        {
            return;
        }
        let trait_span = self.trait_decl_span(decl.trait_name.as_str());
        let trait_pkg = trait_span.and_then(|s| self.package_at(s)).cloned();
        // Same package as the trait: equally legal — that is the "or" in the rule.
        if trait_pkg.as_ref() == Some(&impl_pkg) {
            return;
        }
        let trait_name = decl.trait_name.as_str();
        let target = decl.target.as_str();
        // A trait belonging to no package is not "unknown" — it is a built-in or a native trait,
        // which no package can claim, so it can never make an impl local.
        let trait_where = match &trait_pkg {
            Some(pkg) => pkg.to_string(),
            None => "no package (it is built into the language, or provided by a native extension)"
                .to_string(),
        };
        // The impl HEADER (`Trait for Type`), not the whole block: a tight one-line caret, and the
        // same shape E0027 points at.
        let header = decl.trait_span.merge(decl.target_span);
        let d = self.error(
            DiagnosticCode::OrphanImpl,
            header,
            format!(
                "`impl {trait_name} for {target}` is an orphan: the trait comes from {trait_where}, \
                 the type from {type_pkg}, but this implementation is in {impl_pkg}"
            ),
        );
        d.label(header, format!("written in {impl_pkg}"));
        if let Some(span) = trait_span {
            d.label(span, format!("`{trait_name}` is declared in {trait_where}"));
        }
        if let Some(span) = type_span {
            d.label(span, format!("`{target}` is declared in {type_pkg}"));
        }
        // The concrete escape hatch, written out with this impl's own names — `@derive(T, via: f)`
        // is documented as "the newtype pattern without boilerplate", and it is the fix the author
        // actually needs, not a restatement of the rule.
        let short_trait = short_name(trait_name);
        let short_target = short_name(target);
        d.help(format!(
            "an `impl` must live in the same package as the trait or as the type. To give \
             `{short_target}` this behavior from here, wrap it in a type you own — the newtype \
             pattern, which `via:` writes for you:\n    \
             @derive({short_trait}, via: inner)\n    \
             class My{short_target} {{ pub inner: {short_target} }}"
        ));
    }

    /// Where the trait named `name` was **declared**, when that is a `.noe` `trait` declaration with
    /// a real span. `None` for a built-in trait (never in `user_traits`) and for a trait seeded from
    /// the extension registry (recorded in `native_traits`, and carrying a placeholder span that
    /// points at the entry source) — both belong to no package, which is exactly what the orphan
    /// rule needs to know.
    fn trait_decl_span(&self, name: &str) -> Option<Span> {
        if self.symbols.native_traits.contains(name) {
            return None;
        }
        self.symbols.user_traits.get(name).map(|d| d.name_span)
    }

    /// Type-check a standalone `impl Trait for T { … }`'s method **bodies**.
    ///
    /// Until this existed they were never checked at all. [`Self::check_trait_impl`] validates the
    /// *contract* — that the impl provides the trait's methods with matching arity and annotated
    /// types (E0015) — and the hoist that grafts the methods onto the target so they can be
    /// dispatched lives in the backends (`noeta_ir::hoist_standalone_impl_methods`). Nothing on the
    /// checking path walked the statements in between, so a body could be arbitrarily wrong
    /// (`return "not an int"` from a `fn f(): int`) and check clean, failing only at run time. The
    /// same body written in an *in-body* `impl Trait { }` block WAS checked, because those methods
    /// are flattened into the type's own `methods` — so the two spellings disagreed about whether
    /// your code was checked at all.
    ///
    /// A body is checked in exactly the context its in-body twin gets: the target's generic
    /// parameters in scope, `self` bound to the target type, and `current_type` set so the type's
    /// own private fields are visible (the type-scoped privacy rule).
    fn check_standalone_impl_bodies(&mut self, decl: &ImplDecl, env: &mut Env) {
        if decl.methods.is_empty() {
            return;
        }
        // An **orphan** target — not a record/enum this program declares — already produced E0013
        // above. Its bodies are checked all the same. This used to `return` early, to keep cascading
        // noise off the one real error, and that made the body-coverage gate
        // ([`Checker::verify_body_coverage`]) fire: the checker enumerated these bodies and then
        // never entered them. A debug build panicked outright ("the checker never visited these
        // bodies") on `impl T for Undeclared { fn m() { … } }` — a program a user writes by
        // misspelling a type name — and a release build silently left the body unchecked. The gate
        // exists precisely because "never looked at a body" is the failure that hides indefinitely,
        // so the answer is to look, not to exempt.
        //
        // What kept the noise down is kept: `self` binds to the gradual top rather than to a type we
        // know nothing about, so member access through it defers instead of erroring, and
        // `current_type` stays unset rather than naming a type that has no fields to be private.
        let known = self.symbols.records.contains_key(decl.target.as_str())
            || self.symbols.enums.contains_key(decl.target.as_str());
        let type_params = self
            .symbols
            .type_params
            .get(decl.target.as_str())
            .cloned()
            .unwrap_or_default();
        // `Self` binds to the target for the same reason and under the same condition `self` does:
        // an orphan target names no type, so binding `Self` to it would turn one misspelling into a
        // type error at every annotation that mentions it.
        let self_ty = known.then(|| self_type(decl.target.as_str(), &type_params));
        let saved_params = self.enter_type_body(self_ty.clone(), &type_params);
        let bindings = vec![("self".to_string(), self_ty.unwrap_or(Type::Unknown))];
        let saved_type = if known {
            self.coloring.current_type.replace(decl.target.to_string())
        } else {
            self.coloring.current_type.take()
        };
        for method in &decl.methods {
            self.check_fn(method, env, &bindings, TargetKind::Method);
        }
        self.coloring.current_type = saved_type;
        self.coloring.type_params = saved_params;
    }

    /// Validate that an `impl` of a user trait provides its contract (L1, UT2): every **required**
    /// (non-default) trait method must be present, and every method the impl provides — required or
    /// an override of a defaulted one — must match the trait's arity, its `async`-ness, and (when
    /// both sides annotate them) its parameter and return types. Default methods may be *omitted*
    /// (their fallback body lands in UT5). Extra methods beyond the trait are allowed (inherent
    /// methods). Shares the E0015 `InvalidImpl` code with the built-in path.
    fn check_user_trait_impl(
        &mut self,
        target: &str,
        decl: &noeta_ast::TraitDecl,
        trait_span: Span,
        methods: &[FnDecl],
        assoc_bindings: &[(String, noeta_ast::TypeRef)],
    ) {
        // A **native-derived** associated type (slice 1b) is **auto-supplied** at the user impl site
        // (slice 4): it is computed from the implementing type's element, not written per-impl, so the
        // coherence "must bind" check below treats it as if defaulted. Fold each derivation over the
        // target's `@packed` element into `trait_assoc[(target, trait)]` — so `Self::Name` in a native
        // trait method resolves for this concrete `T` exactly as an advertised native type's does — and
        // collect the names to exclude from the required set. (An empty `impl vec.Kernels for V3 {}`
        // stays empty AND `v.dot(w)` types as the derived `Wide`.)
        let derived: Vec<(String, noeta_ext_abi::AssocDerivation)> = self
            .symbols
            .native_derived_assoc
            .get(decl.name.as_str())
            .cloned()
            .unwrap_or_default();
        if !derived.is_empty()
            && let Some(elem) = self
                .packed_layout(&Type::Named(target.to_string(), Vec::new()))
                .and_then(|layout| stdlib::packed_elem_type(&layout))
        {
            let map: HashMap<String, Type> = derived
                .iter()
                .map(|(name, d)| (name.clone(), d.apply(&elem)))
                .collect();
            self.symbols
                .trait_assoc
                .insert((target.to_string(), decl.name.to_string()), map);
        }
        // Associated-type coherence (slice 1a): every associated type WITHOUT a default must be bound
        // by this impl. A defaulted associated type — or a native-derived one (auto-supplied above) —
        // may be omitted.
        for a in &decl.assoc_types {
            let auto_supplied = derived.iter().any(|(n, _)| n == &a.name);
            if a.default.is_none()
                && !auto_supplied
                && !assoc_bindings.iter().any(|(n, _)| n == &a.name)
            {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    trait_span,
                    format!(
                        "`impl {} for {}` must bind `type {}`",
                        decl.name, target, a.name
                    ),
                )
                .help(format!(
                    "the `{}` trait declares an associated `type {};` with no default",
                    decl.name, a.name
                ));
            }
        }
        for tm in &decl.methods {
            let req_name = &tm.sig.name;
            let Some(m) = methods.iter().find(|m| &m.name == req_name) else {
                // A default method is optional for an implementor; a required one is not.
                if !tm.has_default {
                    self.error(
                        DiagnosticCode::InvalidImpl,
                        trait_span,
                        format!("`impl {}` must define `fn {}`", decl.name, req_name),
                    )
                    .help(format!(
                        "the `{}` trait requires `fn {}`",
                        decl.name, req_name
                    ));
                }
                continue;
            };
            // A method the impl DOES provide is checked against the trait's signature whether or not
            // the trait defaults it. The `has_default` skip used to sit above this whole block, so an
            // *override* of a defaulted method was exempt from every conformance rule — it could take
            // different parameters, return a different type, or (see below) differ in `async`-ness,
            // while `dyn Trait` and every bound kept typing it by the trait's declaration.
            //
            // `async` is part of the contract, not a private implementation detail: the return type a
            // caller sees is `Future<T>` for an `async fn` and `T` otherwise, and every receiver form
            // — bound, trait object, concrete — types the call from *some* signature. If the two sides
            // may disagree, then typing a `dyn Trait` call by the trait's declaration is unsound (an
            // `async` declaration reached a synchronous body, or the reverse). Pinning it here is what
            // makes the trait-object typing above a promise the runtime keeps.
            if tm.sig.is_async != m.is_async {
                let (want, got) = if tm.sig.is_async {
                    ("async fn", "fn")
                } else {
                    ("fn", "async fn")
                };
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`{req_name}` is declared `{got}`, but trait `{}` declares it `{want}`",
                        decl.name
                    ),
                )
                .help(format!(
                    "an implementation must match the trait's `async`-ness — a call through \
                     `dyn {}` or a `<T: {}>` bound is typed from the trait's declaration",
                    decl.name, decl.name
                ));
            }
            // `static` is a contract term in exactly the sense `async` is, and it is checked in
            // exactly the same place (static-trait-methods arc). A trait that declares `static fn m`
            // promises **every** implementation answers `m` without a receiver, which is what lets a
            // generic body under a `<T: Trait>` bound write `T.m(…)` without consulting a single
            // implementor. An implementation whose body reaches for `self` breaks that promise, so
            // it is the same class of error as the wrong arity — reported here, at the body that is
            // wrong, rather than at whichever call site happens to pick this implementor.
            //
            // This runs for an OVERRIDE of a defaulted method too: the loop is over the trait's
            // methods and only a *missing* one is skipped, so overriding `static fn m` with a
            // `self`-using body is caught by the very same rule.
            if tm.sig.is_static && m.body.iter().any(|s| s.mentions("self")) {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`{req_name}` uses `self`, but trait `{}` declares it `static`",
                        decl.name
                    ),
                )
                .help(format!(
                    "a `static` method takes no receiver — a `<T: {}>` bound may call it as \
                     `T.{req_name}(…)`, where nothing binds `self`",
                    decl.name
                ));
            }
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
                // Resolve a `Self::Name` on either side against this impl's binding (slice 1a) so the
                // contract compares concrete types (`int` vs `int`), not two opaque projections.
                let want = self.assoc_resolved_type(&tp.ty, target, decl.name.as_str());
                let got = self.assoc_resolved_type(&ip.ty, target, decl.name.as_str());
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
            let want_ret = self.assoc_resolved_type(&tm.sig.ret, target, decl.name.as_str());
            let got_ret = self.assoc_resolved_type(&m.ret, target, decl.name.as_str());
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
        // Native structural `Self`-constraint (ExtBundle→ExtTrait convergence, slice 3): the third
        // capability a bundle had that a trait lacked. When this trait is a **native** trait carrying a
        // [`noeta_ext_abi::ExtTrait::self_constraint`], the implementing type must be a `@packed` struct
        // matching its [`noeta_ext_abi::PackedConstraint`] — the SAME shape check (and E0015 diagnostic)
        // `check_bundle_binding` runs for a bundle bind, through the shared helper. A `.noe` trait (or a
        // native trait with no constraint) records nothing in the table, so this is a no-op for it. The
        // `trait_span` locates the diagnostic — `check_user_trait_impl` has no separate target span.
        if let Some(constraint) = self
            .symbols
            .native_trait_self_constraints
            .get(decl.name.as_str())
            .copied()
        {
            self.check_packed_self_constraint(target, trait_span, decl.name.as_str(), &constraint);
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

    /// Resolve an associated type on an implementor (slice 1a): the concrete `Type` that `type_name`'s
    /// `impl trait_name` bound `assoc` to (its default when the impl omitted it), or `None` when there
    /// is no such binding. Reads `trait_assoc`, the collect-time table — the trait analogue of a
    /// bundle's element resolution.
    pub(crate) fn resolve_assoc(
        &self,
        type_name: &str,
        trait_name: &str,
        assoc: &str,
    ) -> Option<Type> {
        self.symbols
            .trait_assoc
            .get(&(type_name.to_string(), trait_name.to_string()))?
            .get(assoc)
            .cloned()
    }

    /// The concrete `Type` of a signature annotation, projecting a top-level `Self::Name` through the
    /// implementor's binding (slice 1a). A projection with no resolvable binding — or under `dyn`, or
    /// nested inside a composite — degrades to `Type::Unknown` (a gradual hole that defers), so a
    /// conformance comparison never falsely rejects on an unresolved projection.
    ///
    /// A bare `Self` resolves **at any depth**, unlike a projection: `fn spread(): List<Self>` is a
    /// perfectly ordinary contract, and the impl cannot meet it by spelling `Self` back (that is
    /// E0013), so a nested `Self` left unresolved is a contract no implementation could ever
    /// satisfy. The same [`subst_self`] the call sites use, so the impl side and the call side
    /// cannot drift about what `Self` means.
    fn assoc_resolved_type(
        &self,
        ty: &Option<noeta_ast::TypeRef>,
        target: &str,
        trait_name: &str,
    ) -> Type {
        if let Some(noeta_ast::TypeRef::AssocProjection { name, .. }) = ty {
            return self
                .resolve_assoc(target, trait_name, name)
                .unwrap_or(Type::Unknown);
        }
        // A bare `Self` in the contract means "the implementing type", so resolve it exactly as an
        // associated projection is resolved. Without this the impl is forced to spell the literal
        // word `Self` — and a signature written that way is *uncallable*, because the concrete
        // argument at the call site has nothing to unify a nominal `Self` against. A native trait
        // declaring `SigType::SelfTy` synthesizes into precisely this shape
        // (`stdlib::sig_type_ref`), so one fix serves both trait kinds.
        subst_self(
            self.annot_field(ty),
            &Type::Named(target.to_string(), Vec::new()),
        )
    }

    /// Validate a user-defined `trait` declaration (L1, UT1). The declaration was registered in
    /// pass 1; here we reject a name that collides with a built-in trait, a declared type, or an
    /// earlier `trait` of the same name, plus duplicated method signatures within the body. Default
    /// method bodies are accepted syntactically but not yet type-checked (UT5).
    pub(crate) fn check_trait_decl(&mut self, decl: &noeta_ast::TraitDecl, env: &mut Env) {
        // A user trait may not shadow a built-in trait name — an `impl`/bound naming it would be
        // ambiguous against the closed built-in set.
        if decl.name.as_str() == SELF_TYPE {
            // The same rule the type declarations get (`Checker::collect`): `Self` is the word for
            // the enclosing type, and a trait's own signatures are where it is used most.
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!("`{SELF_TYPE}` cannot be declared as a trait"),
            )
            .help(
                "`Self` names the implementing type inside a trait's own signatures — pick another \
                 name for this trait",
            );
        } else if BuiltinTrait::from_name(decl.name.as_str()).is_some() {
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!(
                    "`{}` is a built-in trait and cannot be redeclared",
                    decl.name
                ),
            );
        } else if self.symbols.types.contains(decl.name.as_str())
            || self.symbols.records.contains_key(decl.name.as_str())
            || self.symbols.enums.contains_key(decl.name.as_str())
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
            .get(decl.name.as_str())
            .is_some_and(|first| first.span != decl.span)
        {
            // A second `trait` of the same name; pass 1 kept the first.
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                decl.name_span,
                format!("trait `{}` is declared more than once", decl.name),
            );
        }
        // **An associated type declared in source is refused**, and a type parameter is the answer.
        //
        // The two say the same thing here — `trait Container { type Item }` and
        // `trait Container<Item>` both let each implementor fix the type, and the parameter form is
        // one concept instead of two. What makes associated types worth their weight elsewhere does
        // not apply: a type may implement a trait once (a second `impl` is a coherence conflict,
        // E0027), so there is no ambiguity for the associated form to resolve, and `T::Item` — the
        // projection through a bound that carries Rust's version — is not expressible, so the
        // parameter form is also strictly the more capable one.
        //
        // The mechanism itself stays, for the one thing only it can do: a **native** trait derives
        // its associated types from the implementing type's element (`AssocDerivation`), so
        // `impl vec.Kernels for V3 {}` binds `Self::Wide` and `Self::Float` with nothing for the
        // author to write. That path synthesizes its `TraitDecl` rather than parsing one, so it
        // never reaches here.
        for assoc in &decl.assoc_types {
            self.error(
                DiagnosticCode::InvalidTraitDeclaration,
                assoc.name_span,
                format!(
                    "trait `{}` cannot declare the associated type `{}` — declare a type \
                     parameter instead",
                    decl.name, assoc.name
                ),
            )
            .help(format!(
                "write `trait {}<{}>` and refer to `{}` directly in the method signatures; each \
                 implementor fixes it at its `impl {}<Concrete>`",
                decl.name, assoc.name, assoc.name, decl.name
            ));
        }
        // A trait accepts `#[...]` data attributes only; the `@`-directives are type-only and do
        // not apply to a trait (UT6). That is checked by the shared placement walk, which reaches
        // a trait through `Stmt::decorated` like every other decorated declaration — this used to
        // call it a second time, which would now report each misplacement twice.
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
            // `pub` on a trait's own method DECLARATION (method-visibility arc). Every method a
            // trait declares is the contract itself, so `pub` restates what `trait` already said —
            // and it would suggest, wrongly, that the unmarked ones are private. Refused rather
            // than accepted-and-ignored, for the same reason `static` is refused where the body
            // already decides: a modifier that means nothing is a modifier a reader must still
            // interpret. (The `impl` block is the other side of this — there `pub` is REQUIRED,
            // because there it distinguishes the method from an inherent one.)
            if m.sig.is_public {
                self.error(
                    DiagnosticCode::InvalidTraitDeclaration,
                    m.sig.name_span,
                    format!(
                        "`{}` cannot be declared `pub`: every method a `trait` declares is \
                         already its public contract",
                        m.sig.name
                    ),
                )
                .help(
                    "drop `pub` here — write it on the `impl` block's method instead, where it \
                     tells a reader the method is part of the type's surface",
                );
            }
            // A trait method's parameters carry `#[...]` attributes like any other callable's, and
            // reflection emits them: `build` keys a trait's signatures `Trait.method` and pushes
            // their parameter rows. So they must be validated, or an attribute that is not an
            // attribute struct at all could reach the manifest through a trait and nowhere else.
            //
            // Only for a method WITHOUT a default: a defaulted one is a real body, reached through
            // `check_fn` from `check_trait_default_bodies` below, which checks its parameters as
            // part of checking the callable. Checking here too would report each misplacement
            // twice — the same double-report the directive check above was restructured to avoid.
            if !m.has_default {
                self.check_param_attrs(&m.sig.params);
            }
            // A trait's REQUIRED-method set stays monomorphic (the pinned D3 boundary): a
            // per-method `<...>` on a trait method has no coherent instantiation site — the trait
            // is dispatched dynamically and each `impl` would have to agree on the method's own
            // parameters — so it is rejected here (E0058), not silently erased. Generic methods
            // live on concrete types (`class`/`struct`/`enum`), where the receiver pins the class's
            // parameters and the call pins the method's own.
            if !m.sig.type_params.is_empty() {
                self.error(
                    DiagnosticCode::InvalidTypeArguments,
                    m.sig.name_span,
                    format!(
                        "trait method `{}::{}` cannot declare its own type parameters",
                        decl.name, m.sig.name
                    ),
                )
                .help(
                    "a trait's method set stays monomorphic; put a generic method on a concrete \
                     type, or make the whole trait generic (`trait T<U> { ... }`)",
                );
            }
        }
        self.check_trait_default_bodies(decl, env);
    }

    /// Type-check a trait's **default method bodies** — the second hole the body-coverage ledger
    /// found, and unchecked for the same reason as the first: nothing walked them.
    ///
    /// A default body is ordinary user code that really runs (every implementor that omits the
    /// method inherits it), so leaving it unchecked meant a trait could ship a body that fails only
    /// once somebody declines to override it.
    ///
    /// It is checked **once, against the trait's own contract**, not once per implementor: `self` is
    /// bound to `dyn <Trait>`, so the body may rely on exactly what every implementor is guaranteed
    /// to provide — the trait's own methods — and nothing more. That is the honest scope. Checking
    /// per implementor instead would re-report one authoring mistake at every `impl` in the program
    /// and still not check a trait nobody has implemented yet.
    fn check_trait_default_bodies(&mut self, decl: &noeta_ast::TraitDecl, env: &mut Env) {
        let defaults: Vec<&noeta_ast::TraitMethod> =
            decl.methods.iter().filter(|m| m.has_default).collect();
        if defaults.is_empty() {
            return;
        }
        // In a default body `Self` is only known to be *some* implementor — which is exactly what
        // `self` is bound to here — so the two agree by construction, and `fn dup(): Self { return
        // self }` checks. An implementor's own `Self` is its concrete type, resolved where the two
        // signatures are compared ([`Checker::assoc_resolved_type`]).
        let self_ty = Type::DynTrait(decl.name.to_string());
        let saved_params = self.enter_type_body(Some(self_ty.clone()), &decl.type_params);
        let bindings = vec![("self".to_string(), self_ty)];
        // These bodies are the trait's OWN contract, the one place `static` is a legal declaration
        // (static-trait-methods arc) — so the general rejection in `check_fn` stands down here.
        let saved_contract = std::mem::replace(&mut self.coloring.in_trait_contract, true);
        for m in defaults {
            // A `static` declaration binds the default exactly as it binds an implementor's
            // override (E0015, `check_user_trait_impl`): a self-less default is what the promise
            // says, and a default reaching for `self` would be a contract the trait breaks in its
            // own body. Reported at the method's name, like every other conformance mismatch.
            if m.sig.is_static && m.sig.body.iter().any(|s| s.mentions("self")) {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.sig.name_span,
                    format!(
                        "`{}` is declared `static`, but its default body uses `self`",
                        m.sig.name
                    ),
                )
                .help(
                    "a `static` method takes no receiver — drop `static` from the declaration, or \
                     write a default that needs none",
                );
            }
            self.check_fn(&m.sig, env, &bindings, TargetKind::Method);
        }
        self.coloring.in_trait_contract = saved_contract;
        self.coloring.type_params = saved_params;
    }

    /// Validate a method-bundle binding `impl <module>.<Bundle> for T {}` (kernel-methods K1).
    /// The binding itself was recorded during collect (so method typing sees it regardless of
    /// statement order); this reports every impl-site violation: an unresolvable path, a
    /// non-empty body (the methods are native), a target that isn't a locally-declared `@packed`
    /// struct, a constraint mismatch (the shape check the raw-buffer kernels used to make at
    /// runtime, moved to compile time), and method-name conflicts with the target's own methods
    /// or an earlier binding's.
    pub(crate) fn check_bundle_impl(&mut self, decl: &ImplDecl) {
        let (module_ref, bundle_name) = decl
            .trait_name
            .as_str()
            .rsplit_once('.')
            .expect("dotted path");
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
        let Some((_, bundle)) = self.resolve_bundle_ref(decl.trait_name.as_str()) else {
            self.error(
                DiagnosticCode::UnknownTrait,
                decl.trait_span,
                format!("module `{module_ref}` has no method bundle `{bundle_name}`"),
            );
            return;
        };
        // The empty-body requirement was relaxed in the fold-in (slice 4): a kernel binding may now
        // carry an override body (checked by `check_standalone_impl_bodies` at the caller). The methods
        // are still native defaults — an empty `impl vec.Kernels for T {}` adopts every one.
        self.check_bundle_binding(
            decl.target.as_str(),
            decl.target_span,
            decl.trait_span,
            decl.trait_name.as_str(),
            bundle,
        );
    }

    /// Validate a bundle binding against its target and register its consequences — the shared
    /// core of the two spellings that bind a bundle: `impl <module>.<Bundle> for T {}`
    /// ([`Self::check_bundle_impl`]) and `@derive(<module>.<Bundle>)` ([`Self::check_derives`]).
    /// Because a `@derive(vec.Kernels)` is *exactly* `impl vec.Kernels for T {}`, both funnel here:
    /// the packed-target + constraint checks (the runtime shape check, moved to compile time,
    /// E0015), the flat-layout schema registration, and the method-name conflict rules. Resolution
    /// (module/bundle lookup, E0014) and the empty-body rule are the caller's — a derive has no
    /// body and its argument already resolved to `bundle`.
    ///
    /// `target_span` locates the packed/constraint diagnostics (the type being bound); `binding_span`
    /// is the binding site (the `impl`'s trait path or the `@derive` argument) — conflicts report
    /// there, and the textually-later binding carries the diagnostic.
    pub(crate) fn check_bundle_binding(
        &mut self,
        target: &str,
        target_span: Span,
        binding_span: Span,
        trait_name: &str,
        bundle: &'static noeta_ext_abi::ExtTrait,
    ) {
        // The structural `Self`-constraint now lives on the trait (slice 3, `ExtTrait::self_constraint`)
        // — the same `PackedConstraint` the `ExtBundle` carried, checked by the same core with the same
        // E0015 diagnostics. Every kernel trait declares one; a trait without one binds any type.
        if let Some(constraint) = &bundle.self_constraint
            && !self.check_packed_self_constraint(target, target_span, trait_name, constraint)
        {
            return;
        }
        // Conflicts, reported on the binding (the textually-later party). Receiver-aware: an
        // Element method lives on `T` — it may not collide with the target's own methods or
        // fields; a Bulk method lives on `List<T>` — it may not shadow a built-in list method
        // (the one namespace it joins). Cross-bundle collisions check within either kind.
        let mut conflicts: Vec<String> = Vec::new();
        for m in bundle.methods {
            match m.receiver {
                // A `Static` method joins the SAME namespace an `Element` one does — the target
                // type's methods — it is only reached without a receiver. So it collides with the
                // same names.
                noeta_ext_abi::BundleReceiver::Element | noeta_ext_abi::BundleReceiver::Static => {
                    if self
                        .symbols
                        .methods
                        .contains_key(&(target.to_string(), m.sig.name.to_string()))
                    {
                        conflicts.push(format!(
                            "`{target}` already declares a method `{}`",
                            m.sig.name
                        ));
                    }
                    if self
                        .symbols
                        .records
                        .get(target)
                        .is_some_and(|fields| fields.iter().any(|(f, _)| f == m.sig.name))
                    {
                        conflicts.push(format!(
                            "`{target}` already declares a field `{}`",
                            m.sig.name
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
        for earlier in self.symbols.bundle_impls.get(target).into_iter().flatten() {
            // Only bindings textually before this one (single-report discipline, like
            // `check_coherence`); skip this binding's own collect record.
            if earlier.bundle.name == bundle.name || earlier.span.start >= binding_span.start {
                continue;
            }
            for m in bundle.methods {
                if earlier
                    .bundle
                    .methods
                    .iter()
                    .any(|em| em.sig.name == m.sig.name)
                {
                    conflicts.push(format!(
                        "`{target}` already acquires `{}` from bundle `{}`",
                        m.sig.name, earlier.bundle.name
                    ));
                }
            }
        }
        for conflict in conflicts {
            self.error(
                DiagnosticCode::ConflictingTraitImpl,
                binding_span,
                format!("{conflict} — binding `{trait_name}` would make the name ambiguous"),
            );
        }
        // Unify the binding with the general trait machinery (guardrail): record it in
        // `user_trait_impls` too, so coherence/dedup see the kernel trait as implemented for `target`
        // exactly like a `.noe` trait — the typing index stays `bundle_impls` (the `List<Self>` dual
        // receiver), but the trait *identity* is recorded uniformly. Native-derived assoc types
        // (`Self::Wide`/`Self::Float`) are resolved directly from the trait at the call site
        // (`bundle_method_return`), so no `trait_assoc` fold is needed on this path.
        self.symbols
            .user_trait_impls
            .entry(target.to_string())
            .or_default()
            .entry(bundle.name.to_string())
            .or_default();
    }

    /// The **packed-`Self` shape check** shared by the two structural constraints in the
    /// ExtBundle→ExtTrait convergence: a bundle's [`noeta_ext_abi::ExtBundle::constraint`]
    /// ([`Self::check_bundle_binding`]) and a native trait's [`noeta_ext_abi::ExtTrait::self_constraint`]
    /// (slice 3, [`Self::check_user_trait_impl`]). Extracted verbatim from the bundle path so both
    /// spellings enforce the *same* rule with the *same* E0015 diagnostics: the target must be a
    /// locally-declared `@packed` struct ([`Self::packed_layout`]), its fields must satisfy the
    /// [`noeta_ext_abi::PackedConstraint`] ([`constraint_mismatch`]), and on success its flat layout is
    /// interned into [`Sites::bundle_schema_layouts`] so the compiler recovers the element width for the
    /// element-relative methods (a single struct value erases its field widths to boxed scalars at the
    /// seam; without this an integer vector's element op would not wrap at its width on the VM).
    ///
    /// Returns `true` when the constraint is satisfied (schema registered), `false` after emitting the
    /// packed-target or field-mismatch diagnostic. `constraint_owner` names the binding in the message
    /// (the bundle path passes its trait path, the trait path its trait name).
    pub(crate) fn check_packed_self_constraint(
        &mut self,
        target: &str,
        target_span: Span,
        constraint_owner: &str,
        constraint: &noeta_ext_abi::PackedConstraint,
    ) -> bool {
        let target_ty = Type::Named(target.to_string(), vec![]);
        let Some(layout) = self.packed_layout(&target_ty) else {
            self.error(
                DiagnosticCode::InvalidImpl,
                target_span,
                format!(
                    "`{target}` cannot bind `{constraint_owner}`: a method bundle binds to a `@packed` \
                     struct declared in this module"
                ),
            )
            .help("mark the target `@packed` — bundles are packed-operations method sets");
            return false;
        };
        if let Some(message) = constraint_mismatch(&layout, constraint) {
            self.error(
                DiagnosticCode::InvalidImpl,
                target_span,
                format!("`{target}` does not satisfy `{constraint_owner}`: {message}"),
            );
            return false;
        }
        self.sites.bundle_schema_layouts.push(layout);
        true
    }

    // (constraint_mismatch, the bundle-constraint comparison, is a free function below the impl.)

    /// Enforce **trait coherence** (overlap/uniqueness) on a single type: a trait may be
    /// implemented at most once — `From` once per source type it converts — counting both a
    /// `@derive(T)` directive and an `impl T { }` block as implementations. A second implementation
    /// of an already-implemented trait — whether
    /// `@derive(T)` twice, two `impl T` blocks, or a `@derive(T)` alongside an `impl T` — is
    /// reported as `E0027 ConflictingTraitImpl`, **labelling both sites**: the primary span on the
    /// later occurrence, a secondary label on the one it collides with. This keeps each
    /// `(type, trait)` pair single-implementation, so [`Self::satisfies`] and runtime dispatch are
    /// unambiguous.
    ///
    /// **Both sites, because the two can be in different files.** Coherence runs over the *linked*
    /// program, so the competing implementations may be two sibling modules — or two dependency
    /// packages — that never mention each other. Naming only the later one, and describing the
    /// other as "above", sent the reader looking up a file that does not contain it; the second
    /// label (rendered by `ariadne` against its own file, see `noeta_diagnostics::render_mapped`)
    /// is the only thing that makes the conflict locatable. [`ImplForm`] supplies each side's
    /// spelling, so the wording fits whichever pair actually collided rather than assuming the
    /// same-file `@derive`-vs-`impl` case.
    ///
    /// The orphan half of coherence is enforced separately: an in-body `impl` block can only name
    /// the type that owns it, and a standalone `impl Trait for T {}` must target a type the program
    /// declares and live in the same package as that type or as the trait
    /// ([`Self::check_standalone_impl`]). Records and enums carry no in-body `impl` blocks (pass an
    /// empty slice); `standalone` carries the `(trait, span)` of every standalone impl targeting
    /// this type.
    pub(crate) fn check_coherence(
        &mut self,
        derives: &[DeriveSpec],
        impls: &[ImplBlock],
        standalone: &[(String, Span)],
    ) {
        // Source order is derives, then in-body impls, then standalone impls: this scan reports the
        // textually-later duplicate and labels the one it collides with. Every trait is keyed by
        // name — except `From`, which is keyed by the SOURCE it converts ([`coherence_key`]),
        // because that is what a type may declare only once. Two `impl From<A>` blocks are the
        // ambiguity a `?` conversion must never see (two declared paths from one source into one
        // target) and collide here; `impl From<A>` beside `impl From<B>` declares two different
        // conversions and does not.
        let mut seen: HashMap<String, (Span, ImplForm)> = HashMap::new();
        let occurrences: Vec<(String, Span, ImplForm)> = derives
            .iter()
            .map(|d| (d.name.to_string(), d.span, ImplForm::Derive))
            .chain(impls.iter().map(|b| {
                (
                    coherence_key(b.trait_name.as_str(), &b.trait_args),
                    b.trait_span,
                    ImplForm::InBody,
                )
            }))
            .chain(
                standalone
                    .iter()
                    .map(|(name, span)| (name.clone(), *span, ImplForm::Standalone)),
            )
            .collect();
        for (name, span, form) in occurrences {
            match seen.get(&name) {
                Some((first_span, first_form)) => {
                    let (first_span, first_form) = (*first_span, *first_form);
                    self.error(
                        DiagnosticCode::ConflictingTraitImpl,
                        span,
                        format!("trait `{name}` is implemented more than once for this type"),
                    )
                    // The offending (later) site first, so `ariadne` groups it first and the
                    // rendered header carries the same file/line the primary span — and every
                    // non-rendered consumer of the diagnostic — reports.
                    .label(span, format!("implemented again here, {form}"))
                    .label(first_span, format!("first implemented here, {first_form}"))
                    // A conversion's key carries its source in angle brackets, which no trait NAME
                    // can contain — so this recognizes the one contest whose fix is not "implement
                    // it once" but "one per source".
                    .help(
                        if name.starts_with(&format!("{}<", BuiltinTrait::From.name())) {
                            format!(
                                "a type declares one conversion per source — remove one of the two \
                             `{name}` blocks, or merge them into a single one. A conversion from a \
                             DIFFERENT source is a different conversion and may sit beside this one"
                            )
                        } else {
                            format!(
                                "a type may implement each trait only once — remove one of the two \
                             implementations of `{name}`, or merge them into a single one"
                            )
                        },
                    );
                }
                None => {
                    seen.insert(name, (span, form));
                }
            }
        }
    }

    /// Coherence's **second** uniqueness rule, over method *names* rather than trait names: two
    /// traits a type implements may not each hand it a **default body** for one method, because a
    /// method table has one slot per name (there is no overloading — the same fact that limits a
    /// type to one `From`, `check_coherence`) and nothing in the source says which body belongs in
    /// it. E0027, like every other contest coherence settles.
    ///
    /// The rule is not new here; the *enforcement* is. The bundle spelling of the identical
    /// collision has always been rejected — "`Color` already acquires `add` from bundle `Kernels` —
    /// binding `vec.SatKernels` would make the name ambiguous" ([`Self::check_bundle_binding`]) —
    /// while the `.noe` trait spelling silently picked a winner by `HashMap` iteration order. That
    /// made a *checked* fact depend on the process: `p.hello()` typed from `Greet` in one run and
    /// from `Wave` in the next, so a program with two colliding defaults compiled green roughly
    /// half the time and failed E0007 the rest, while both backends' hoist always ran the
    /// textually-first body. Ambiguity is a diagnostic, never a guess (the derive-bridge rule,
    /// E0050, says the same thing about two candidate fields).
    ///
    /// The programmer resolves it the way the language already documents: **override the method**
    /// (a provided body wins over every default, so the slot has one owner again), or implement one
    /// of the two traits fewer. An override that both traits' signatures accept satisfies both.
    ///
    /// Reported here, in pass 2, because this is where the binding *sites* are known — the
    /// collision itself is found in pass 1, where "provided vs inherited" is decidable
    /// ([`crate::Symbols::trait_default_conflicts`]). The two spans are ordered by source position,
    /// so the later binding carries the diagnostic and the earlier one is labelled: the same
    /// two-label discipline duplicate impls get, and for the same reason — the rival
    /// implementations are routinely in different files.
    pub(crate) fn report_trait_default_conflicts(&mut self) {
        // Taken, not read: a session/REPL re-collects on every entry, and a conflict already
        // reported must not be reported again on the next one.
        let mut conflicts = std::mem::take(&mut self.symbols.trait_default_conflicts);
        conflicts.sort();
        conflicts.dedup();
        for conflict in conflicts {
            let (a, b) = &conflict.traits;
            let ty = &conflict.type_name;
            // A binding with no source site (a native type advertising the trait through its
            // registry declaration) cannot be blamed — there is nothing to point at.
            let (Some(a_span), Some(b_span)) = (
                self.symbols
                    .trait_impl_sites
                    .get(&(ty.clone(), a.clone()))
                    .copied(),
                self.symbols
                    .trait_impl_sites
                    .get(&(ty.clone(), b.clone()))
                    .copied(),
            ) else {
                continue;
            };
            let ((first, first_span), (later, later_span)) = if a_span.start <= b_span.start {
                ((a, a_span), (b, b_span))
            } else {
                ((b, b_span), (a, a_span))
            };
            let method = &conflict.method;
            self.error(
                DiagnosticCode::ConflictingTraitImpl,
                later_span,
                format!(
                    "`{ty}` already inherits a default `{method}` from trait `{first}` — \
                     implementing `{later}` would make the name ambiguous"
                ),
            )
            .label(
                later_span,
                format!("`{later}` also supplies a default `{method}`"),
            )
            .label(
                first_span,
                format!("`{first}`'s default `{method}` is inherited here"),
            )
            .help(format!(
                "a method table has one slot per name — give `{ty}` its own `fn {method}` (a \
                 provided method overrides every trait default, and one body can satisfy both \
                 traits), or implement only one of `{first}` and `{later}`"
            ));
        }
    }

    /// The `?` **failure-position rule** — the `Result` twin of [`Self::check_try_option`], plus the
    /// error-conversion rule layered on top of it.
    ///
    /// *Position* first: `?` on a `Result` early-returns the `Err`, so the enclosing function has to
    /// be able to return one. A declared return that is neither a `Result` nor deferring is
    /// **E0012**, exactly as the `Option` half is — the same rule, the same span, the same code.
    /// Without it, `fn work(): void { client.get(url)? }` checked clean, discarded the transport
    /// failure, and exited 0: the failure was unobservable and CI went green on a broken program. A
    /// declared return that defers (`dyn`, an inference hole, top-level code, an unannotated closure)
    /// still defers — an `Err` that reaches the top aborts there (E0069) rather than vanishing.
    ///
    /// *Conversion* second (error-ergonomics): a `?` whose `Err` payload type differs from the
    /// declared error type either **converts** through the target's declared `impl From<Source>` (the
    /// site is recorded for lowering — the one implicit conversion position in the language) or is
    /// `E0057`. That judgement runs only when both sides are resolved: a `dyn`/hole on either side or
    /// a type parameter in scope defers to runtime, and an assignable error (a union member, for
    /// instance) propagates unconverted. Exactly-one-path is by construction: sources are matched by
    /// type **equality**, and coherence admits at most one `From` impl per (target, source) pair, so
    /// no `?` site ever sees two candidate conversions — a target declaring several conversions has
    /// one per distinct source, and the propagated `Err` type equals at most one of them. The site
    /// records the conversion's method-table key beside the target, which is what tells lowering
    /// *which* of them to call.
    pub(crate) fn check_try_error(&mut self, err: &Type, span: Span) {
        let Type::Result(_, declared) = self.coloring.current_ret.clone() else {
            self.reject_try_position(err, span);
            return;
        };
        let declared = *declared;
        if err.defers_to_runtime() || declared.defers_to_runtime() {
            return;
        }
        if *err == declared || self.arg_assignable(err, &declared) {
            return;
        }
        // A side naming an in-scope type parameter is not yet a concrete type — defer to the
        // instantiation (gradual, like every other parameter-typed judgement).
        if !self.concrete_error_type(err) || !self.concrete_error_type(&declared) {
            return;
        }
        if let Type::Named(target, _) = &declared
            && let Some(conv) = self
                .symbols
                .from_impls
                .get(target)
                .and_then(|convs| convs.iter().find(|c| c.source == *err))
        {
            self.sites
                .try_conversion_sites
                .insert(span, (target.clone(), conv.method.clone()));
            return;
        }
        let d = self.error(
            DiagnosticCode::TryErrorMismatch,
            span,
            format!(
                "`?` propagates an `Err` of type `{err}`, but the function returns \
                 `Result<_, {declared}>` and `{declared}` declares no `From<{err}>` conversion"
            ),
        );
        if let Type::Named(target, _) = &declared {
            d.help(format!(
                "add `impl From<{err}> {{ fn from(value: {err}): {target} {{ … }} }}` inside \
                 `{target}`, or align the function's declared error type"
            ));
        } else {
            d.help("align the function's declared error type with the propagated error");
        }
    }

    /// The position half of [`Self::check_try_error`]: the enclosing function's declared return is
    /// not a `Result`, so the `Err` this `?` early-returns has nowhere to go. A return that defers
    /// (`dyn`, an inference hole, top-level code, an unannotated closure) still defers to runtime;
    /// anything else is E0012, naming the declaration that would make the propagation legal.
    fn reject_try_position(&mut self, err: &Type, span: Span) {
        let declared = self.coloring.current_ret.clone();
        if declared.defers_to_runtime() {
            return;
        }
        let d = self.error(
            DiagnosticCode::InvalidTry,
            span,
            format!(
                "`?` on a `Result` early-returns its `Err`, but this function returns `{declared}`"
            ),
        );
        // Name the concrete declaration that admits this very `Err`; an unresolved error payload
        // (`dyn`, a hole) can only be described generically.
        if err.defers_to_runtime() {
            d.help(
                "declare the return as `Result<T, E>` to propagate the failure (converting through \
                 `impl From<Source>` on `E` if the error types differ), or handle it here with \
                 `match` / `??`",
            );
        } else {
            d.help(format!(
                "declare the return as `Result<T, {err}>` to propagate the failure (or \
                 `Result<T, E>` with an `impl From<{err}>` on `E` to convert it), or handle it here \
                 with `match` / `??`"
            ));
        }
    }

    /// The `?` **absence-position rule**: `?` on an `Option` early-returns `none`, so the enclosing
    /// function has to be able to return one.
    ///
    /// Without this, `fn head(xs: List<string>): string { return xs.first()? }` type-checked clean
    /// and then handed the caller a `none` sitting in a slot the checker had promised was a
    /// `string` — the same shape of hole as a declared value holding a future. A declared return
    /// that defers (`dyn`, an inference hole, top-level code, an unannotated closure) still defers,
    /// and a `?T` return is exactly what the operator is for.
    pub(crate) fn check_try_option(&mut self, span: Span) {
        let declared = self.coloring.current_ret.clone();
        if declared.defers_to_runtime() || matches!(declared, Type::Option(_)) {
            return;
        }
        self.error(
            DiagnosticCode::InvalidTry,
            span,
            format!(
                "`?` on an `Option` early-returns `none`, but this function returns `{declared}`"
            ),
        )
        .help(
            "declare the return as `?T` to propagate the absence, or supply a value with \
             `?? <fallback>` — a `Result`-returning function has to match the `Option` and return \
             its own `Err`",
        );
    }

    /// Whether an error-position type is resolved enough for the `?` rule to judge: anything but a
    /// generic type parameter (those defer to the instantiation).
    ///
    /// This asked the in-scope NAME table and matched `Type::Named`, which is exactly the
    /// conflation this arc removes — and it was load-bearing: `fn f<E>(): Result<int, E>`
    /// propagating a concrete `Err` deferred only because `"E"` was in that table. As a lattice
    /// question the arm is direct, and the catch-all no longer silently swallows a parameter.
    fn concrete_error_type(&self, ty: &Type) -> bool {
        !matches!(ty, Type::Param(_))
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
            let Some(t) = BuiltinTrait::from_name(spec.name.as_str()) else {
                // A USER trait derives through the shared planner (UT5 + bridging + `via:`
                // delegation): defaults adopted wholesale, required members bridged onto the
                // type's own fields/methods, or the whole trait forwarded through a field.
                if let Some(decl) = self.symbols.user_traits.get(spec.name.as_str()).cloned() {
                    self.check_user_trait_derive(type_name, spec, &decl, fields, type_methods);
                    continue;
                }
                // A method-BUNDLE binding via derive (kernel-methods): `@derive(vec.Kernels)` is
                // *exactly* `impl vec.Kernels for T {}` — a bundle binding is shape-derived behavior,
                // so `@derive` is its natural home. A dotted name that resolves to a registered
                // `ExtBundle` (its module bound by a `use`) funnels into the SAME impl-site
                // validation — the packed-target + constraint check that yields the identical E0015
                // an empty `impl` would (`check_bundle_binding`); the binding itself was recorded in
                // `collect` beside the `impl` form. A bundle takes no member bindings, no `via:`, and
                // no type arguments — those belong to trait derives.
                if spec.name.as_str().contains('.')
                    && let Some((_, bundle)) = self.resolve_bundle_ref(spec.name.as_str())
                {
                    if let Some(b) = spec.bindings.first() {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            b.span,
                            format!(
                                "`{}: {}` — `{}` is a method bundle; it binds wholesale and takes \
                                 no member bindings",
                                b.member, b.target, spec.name
                            ),
                        );
                    } else if let Some((_, via_span)) = &spec.via {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            *via_span,
                            format!("`{}` is a method bundle; `via:` does not apply", spec.name),
                        );
                    } else if !spec.args.is_empty() {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            spec.span,
                            format!("method bundle `{}` takes no type arguments", spec.name),
                        );
                    } else {
                        // Same target as the impl form: the decorated type, at the derive argument's
                        // span (which is both where the shape error points and the binding site).
                        self.check_bundle_binding(
                            type_name,
                            spec.span,
                            spec.span,
                            spec.name.as_str(),
                            bundle,
                        );
                    }
                    continue;
                }
                // A NATIVE derive recipe (layer 4, `ExtDerive`): synthesizes handler forwards —
                // no bindings/via surface, plus the recipe's own optional shape validation.
                if let Some(ext) = self.reg().find_ext_derive(spec.name.as_str()) {
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
                        // The shared derivation (`noeta_ast::shape`), which also builds
                        // `DirectiveCtx::fields` for an `ExtDirective::expand` hook. One walk, so a
                        // derive recipe and an expansion hook in the same extension can never see
                        // the same declaration differently — and the spelling a recipe judges is the
                        // *declared* one (`List<int>`, `?User`) rather than a lattice rendering of
                        // it, which is what a recipe generating code from a field actually needs.
                        let shape = noeta_ast::shape::field_shape(fields);
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
                    noeta_ast::derive::plan_builtin_via(spec.name.as_str(), type_name, fields, spec)
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
                    let params: ParamSet = self
                        .symbols
                        .generic_types
                        .get(type_name)
                        .map(|ps| ps.iter().map(|p| p.id).collect())
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
                } else if t == BuiltinTrait::Error {
                    // The forward is `self.f.message()` — the via field's type must itself
                    // implement `Error`, or the delegation dispatches into nothing (the same
                    // judgement as a user-trait `via:`). A field typed as one of the deriving
                    // type's own generic parameters defers to the instantiation site.
                    let params: ParamSet = self
                        .symbols
                        .generic_types
                        .get(type_name)
                        .map(|ps| ps.iter().map(|p| p.id).collect())
                        .unwrap_or_default();
                    let field_ty = self
                        .symbols
                        .records
                        .get(type_name)
                        .and_then(|fs| fs.iter().find(|(n, _)| n == via_name))
                        .map(|(_, ty)| ty.clone());
                    if let Some(ty) = field_ty
                        && !mentions_param(&ty, &params)
                        && !self.satisfies(&ty, BuiltinTrait::Error)
                    {
                        self.error(
                            DiagnosticCode::UnderivableTrait,
                            *via_span,
                            format!(
                                "`via: {via_name}` forwards `message()` to the field, but its \
                                 type (`{ty}`) does not implement `Error`"
                            ),
                        )
                        .help(
                            "the field's type needs an `impl Error` (or its own \
                             `@derive(Error)`)",
                        );
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
                    "derivable traits are `Equatable`, `Comparable`, `Display`, `Error`, \
                         `Clone`, `Serialize<Format>`, `Deserialize<Format>`; mark attribute \
                         records with the `@attribute` directive",
                );
                continue;
            }
            // `@derive(Error)`'s synthesized `message()` returns `"${self}"` — the type's display
            // story — so the type must HAVE one: an `impl Display` (whose `to_string` the
            // rendering dispatches to) or a `@derive(Display)` (the structural rendering, opted
            // into). Without either, the "message" would be an accidental structural dump.
            if t == BuiltinTrait::Error {
                let ty = Type::Named(type_name.to_string(), Vec::new());
                if !self.satisfies(&ty, BuiltinTrait::Display) {
                    self.error(
                        DiagnosticCode::UnderivableTrait,
                        spec.span,
                        format!(
                            "cannot derive `Error` for `{type_name}`: it does not implement \
                             `Display`, so `message()` has no rendering to return"
                        ),
                    )
                    .help(
                        "add `@derive(Display)` or an `impl Display`, or delegate with \
                         `@derive(Error, via: <field>)`",
                    );
                }
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
                self.check_transient_fields_fillable(type_name, spec.span);
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
            // A syntactic check on the WRITTEN annotation (no lattice conversion happens here), so
            // this one legitimately compares spellings: it is asking whether the source wrote the
            // declaration's own parameter, and the declaration is the one that named it.
            let params = self
                .symbols
                .generic_types
                .get(type_name)
                .cloned()
                .unwrap_or_default();
            let satisfied = match &f.ty {
                Some(noeta_ast::TypeRef::Named { name, .. })
                    if params.iter().any(|p| p.name == name.as_str()) =>
                {
                    true // parameter-typed — deferred to the instantiation site
                }
                Some(noeta_ast::TypeRef::Named { name, .. }) => self
                    .symbols
                    .user_trait_impls
                    .get(name.as_str())
                    .is_some_and(|traits| traits.contains_key(decl.name.as_str())),
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

    /// Every `#[Transient]` field of a type deriving `Deserialize<Format>` must be fillable
    /// **without the wire** (E0050) — the rule that makes the round trip total.
    ///
    /// A transient field is absent from the encoded form by construction, so a decode of that form
    /// has nothing to read for it and must fill the slot from the declaration alone. Two things
    /// can: a `?T` (an absent optional is `none`, always), and a default the checker folded to a
    /// literal — which the decoder bakes and decodes through the field's own recipe, the same walk a
    /// supplied value would take. A default it could not fold (`= now()`, `= other()`) cannot,
    /// because the decoder is a data walk with no way to run an expression; neither can a literal
    /// default whose *type* has no JSON form (a `Set`, a tuple), since there is no `NativeOut` shape
    /// to fill the slot with.
    ///
    /// Asked here, at the declaration, rather than left to the first parse. A field marked transient
    /// and then never fillable is a program that encodes fine and fails to decode its own output —
    /// the failure would surface at whichever runtime first round-trips it, naming a field the
    /// author deliberately removed from the wire, which reads as a decoder bug rather than as the
    /// missing default it is.
    fn check_transient_fields_fillable(&mut self, type_name: &str, span: Span) {
        let Some(transient) = self.symbols.transient_fields.get(type_name).cloned() else {
            return;
        };
        let defaults = self.symbols.field_defaults.get(type_name);
        let offender = self.symbols.records.get(type_name).and_then(|fields| {
            fields
                .iter()
                .filter(|(fname, _)| transient.contains(fname))
                .find(|(fname, fty)| {
                    let optional = matches!(fty, Type::Option(_));
                    let bakeable = matches!(
                        defaults.and_then(|d| d.get(fname)),
                        Some(noeta_ext_abi::FieldDefault::Literal(_))
                    ) && self.type_to_recipe(fty).is_some();
                    !optional && !bakeable
                })
                .cloned()
        });
        if let Some((fname, fty)) = offender {
            self.error(
                DiagnosticCode::UnderivableTrait,
                span,
                format!(
                    "cannot derive `Deserialize` for `{type_name}`: field `{fname}` is \
                     `#[Transient]`, so a decode never reads it, and nothing can fill it"
                ),
            )
            .help(format!(
                "give `{fname}` a literal default (`{fname}: {fty} = …`) or make it optional \
                 (`?{fty}`) — a transient field is absent from the wire, so its value has to come \
                 from the declaration"
            ));
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
            let params: ParamSet = self
                .symbols
                .generic_types
                .get(type_name)
                .map(|ps| ps.iter().map(|p| p.id).collect())
                .unwrap_or_default();
            // A `#[Transient]` field is exempt from `Serialize`'s constraint: it is not part of the
            // serialized form, so whether it *has* one is not this derive's question. That is the
            // point of the marker — a class holding a live handle beside its data becomes
            // serializable by saying which field does not travel, rather than being refused whole.
            // `Comparable` is unaffected: ordering happens in-process, where every field is present.
            let transient = match t {
                BuiltinTrait::Serialize => self.symbols.transient_fields.get(type_name),
                _ => None,
            };
            let exempt = |fname: &String| transient.is_some_and(|t| t.contains(fname));
            let offender = if let Some(fields) = self.symbols.records.get(type_name) {
                fields
                    .iter()
                    .find(|(fname, ty)| {
                        !exempt(fname) && !mentions_param(ty, &params) && !ok(self, ty)
                    })
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
            | Type::String
            // A type parameter orders value-dependently, exactly like `dyn`: whether it does is a
            // property of the instantiation, judged at the instantiation site. (Callers that care
            // about the declaration itself — the derive checks — screen parameter-typed fields out
            // with `mentions_param` before asking.)
            | Type::Param(_)
            // Vacuous: no two values of the bottom type exist to compare, so nothing can
            // observe a missing ordering. Permissive, like `dyn`.
            | Type::Never => true,
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
                // A `#[Transient]` field is not part of the serialized form, so it does not have to
                // have one — the same exemption `check_derive_field_constraint` applies at the
                // declaration, applied here so a *containing* type is serializable too.
                let transient = self.symbols.transient_fields.get(name);
                let fields_ok = self.symbols.records.get(name).is_none_or(|fs| {
                    fs.iter()
                        .filter(|(fname, _)| !transient.is_some_and(|t| t.contains(fname)))
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
        arg_exprs: &[noeta_ast::CallArg],
        span: Span,
        recv_args: &[Type],
        supplied_at: &[usize],
        hidden_site: Option<(Span, String, ForwardSpelling)>,
        env: &mut Env,
    ) -> Type {
        // Seed with the receiver's type arguments (instance call); the call's own arguments then
        // refine any still-unbound parameters without overwriting the receiver's binding.
        let seed: Subst = generic
            .params
            .iter()
            .map(|(p, _)| p.id)
            .zip(recv_args.iter().cloned())
            .filter(|(_, t)| !t.defers_to_runtime())
            .collect();
        self.check_generic_call_seeded(
            name,
            generic,
            required,
            args,
            arg_exprs,
            span,
            seed,
            supplied_at,
            hidden_site,
            env,
        )
    }

    /// The seeded core of [`Self::check_generic_call`]: `seed` holds type-parameter bindings that
    /// **win** over anything the arguments would derive — the receiver's type arguments for an
    /// instance method call, or (poly-values F2) the EXPLICIT turbofish instantiations of
    /// `f::<T, ...>(args)`, which is what makes "explicit args win; a conflicting argument is the
    /// ordinary assignability error against the substituted parameter" true by construction
    /// (binding uses first-wins `or_insert`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_generic_call_seeded(
        &mut self,
        name: &str,
        generic: &GenericInfo,
        required: usize,
        args: &mut [Type],
        arg_exprs: &[noeta_ast::CallArg],
        span: Span,
        seed: Subst,
        supplied_at: &[usize],
        hidden_site: Option<(Span, String, ForwardSpelling)>,
        env: &mut Env,
    ) -> Type {
        let tps: ParamSet = generic.params.iter().map(|(p, _)| p.id).collect();
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
        let mut subst: Subst = seed;
        // Which parameter each argument fills. A named-argument call that SKIPS a defaulted
        // parameter (`f(1, c: 9)`) has already been compacted into parameter order, so argument
        // `i` is the `i`-th SUPPLIED parameter — not `raw_params[i]`. Checking it against
        // `raw_params[i]` bound the wrong type parameter and reported the skipped parameter's type
        // in the mismatch, which is what made a named argument over a defaulted one unusable on
        // any generic callable. Empty means the ordinary dense prefix, where the two coincide.
        let positions: Vec<usize> = if supplied_at.is_empty() {
            (0..generic.raw_params.len()).collect()
        } else {
            supplied_at.to_vec()
        };
        for (i, &p) in positions.iter().enumerate() {
            if i >= args.len() {
                // Omitted trailing defaults — already checked at the declaration.
                break;
            }
            let Some(raw) = generic.raw_params.get(p) else {
                break;
            };
            // A deferred closure argument finalizes against the raw parameter with everything
            // bound SO FAR substituted in — `fn each<T>(xs: List<T>, f: (T) -> unit)` has `T`
            // pinned by `xs` before `f` is looked at — and its now-known type (the inferred
            // return especially) then binds any parameter the earlier arguments did not
            // (`fn pick<T>(f: () -> T): T`).
            if let Some(arg) = arg_exprs.get(i)
                && let expr = &arg.value
                && self.is_deferred_arg(expr, env)
                && matches!(args[i], Type::Unknown)
            {
                let expected = subst_or_dyn(raw, &subst, &tps);
                // Absorb the (substituted) parameter type into the deferred argument — one shared
                // definition with the non-generic path, so the two agree by construction — and its
                // resolved type then binds any still-unbound type parameter below; a mismatched or
                // unguiding param synthesizes standalone (unchanged from the closure-only behavior).
                args[i] = self.absorb_deferred_arg(expr, Some(&expected), env);
            }
            let arg = args[i].clone();
            bind_type_params(raw, &arg, &tps, &mut subst);
            let expected = subst_or_dyn(raw, &subst, &tps);
            let arg = &arg;
            // A bare literal adapts into a fixed-width parameter here too (P-NUM-SYM) — whether the
            // parameter is a concrete `u8`/`f32`/`f64` or a type variable already bound to one
            // (`g(200u8, 200)` binds `T = u8`, so the second `200` narrows). Tried before the
            // type-based `arg_assignable`, exactly as in `check_args`.
            if let Some(a) = arg_exprs.get(i)
                && self.try_adapt_literal(&a.value, &expected).is_some()
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
        self.enforce_type_param_bounds(name, &generic.params, &subst, &tps, span);
        // A call of a **forwarding** generic (poly-values F2b, composite slots D2a) must supply
        // its hidden type-argument slots: substitute the call's instantiation into each slot
        // TEMPLATE and resolve the result into a table entry (concrete — the whole composite is
        // interned statically, so the runtime never constructs a recipe) or a pass-through of the
        // enclosing fn's own matching slot (a template still mentioning the caller's parameters).
        // `hidden_site` is the whole-call span lowering keys on, paired with the KEY the callee's
        // slot layout is recorded under — a bare `fn` name, or `Type.method` for a method (Axis A),
        // which is why it is not simply `name`: two classes may declare `load`. `None` at a call
        // that has no channel to supply them through. The third element is how this call was
        // SPELLED, which is what the pre-pass's reach is a property of — see [`ForwardSpelling`],
        // and the E0058 below that is the only consumer.
        if let Some((call_span, key, spelling)) = hidden_site
            && let Some(fwd) = self.symbols.forwarding.get(key.as_str()).cloned()
            // A poisoned callee (diverging slot set, D2a) already carries the one clear error at
            // its declaration; resolving its partial slots here would only cascade noise.
            && !self.symbols.forwarding_poisoned.contains(key.as_str())
        {
            let mut hidden = Vec::with_capacity(fwd.len());
            for slot in &fwd {
                let sigma = apply_subst(&slot.template, &subst);
                // A callee parameter the call leaves unbound (or pins only to `dyn`/a hole)
                // cannot fill a call-site-typed slot — the instantiation must be explicit.
                if let Some(open) = params_mentioned(&slot.template, &tps)
                    .into_iter()
                    .find(|p| {
                        subst
                            .get(&p.id)
                            .is_none_or(|t| t.defers_to_runtime() || t.contains_unknown())
                    })
                {
                    // A parameter of the enclosing **type** (the leading `class_params` of the
                    // composed list) cannot be spelled with a turbofish here — a method's `::<…>`
                    // names the METHOD's own parameters, never its class's, which come from the
                    // receiver's type arguments. So "supply it explicitly" is a dead end for one and
                    // the fix for the other, and each says which it is.
                    let class_param = generic
                        .params
                        .iter()
                        .take(generic.class_params)
                        .any(|(p, _)| *p == open);
                    let help = if class_param {
                        format!(
                            "a self-less member reads `{open}` from the CALL's instantiation, and \
                             nothing here determines one — annotate the position this call flows \
                             into so its result type pins `{open}` (`x: Type<Something> = …`)"
                        )
                    } else {
                        format!("supply it explicitly: `{name}::<...>(...)`")
                    };
                    self.error(
                        DiagnosticCode::CannotInfer,
                        span,
                        format!(
                            "cannot infer type parameter `{open}` of `{name}`, which determines \
                             a call-site-typed result"
                        ),
                    )
                    .help(help);
                    continue;
                }
                if self.mentions_in_scope_param(&sigma) {
                    // A pass-through: the substituted template mentions the CALLER's own
                    // parameters, so the caller's matching slot (computed by the same fixpoint)
                    // is forwarded onward.
                    match self
                        .coloring
                        .current_forwarding
                        .iter()
                        .position(|t| t == &sigma)
                    {
                        Some(j) => hidden.push(noeta_ext_abi::HiddenArg::Forward(j as u32)),
                        // No matching slot in this body. What to say depends entirely on how the
                        // call was SPELLED ([`ForwardSpelling`]), because the pre-pass that builds
                        // the slot table is syntactic — it is the spelling, not the resolved
                        // callee, that decided whether a slot could be registered. Getting this
                        // wrong is worse than silence: the previous single message claimed
                        // forwarding lives in top-level generic functions only (it has worked from
                        // generic methods since the generic-forwarding arc, and fires *inside* a
                        // top-level fn in the inferred case) and advised a turbofish the failing
                        // source had usually already spelled.
                        None => {
                            let d = self.error(
                                DiagnosticCode::InvalidTypeArguments,
                                span,
                                match spelling {
                                    ForwardSpelling::CompoundReceiver => format!(
                                        "cannot forward `{sigma}` into `{name}` here: a compound \
                                         receiver is typed by checking, while the slots a body \
                                         forwards through are computed before it — forwarding \
                                         reaches a call spelled on a BARE NAME \
                                         (`json.try_parse::<{sigma}>`, `self.{name}::<...>`, \
                                         `Type.{name}::<...>`), from a top-level generic `fn` and \
                                         a generic method alike"
                                    ),
                                    ForwardSpelling::Inferred => format!(
                                        "cannot forward `{sigma}` into `{name}` here: this body \
                                         carries no forwarding slot for `{sigma}` — one is \
                                         registered from an EXPLICIT turbofish, never from an \
                                         instantiation the arguments or the expected type inferred"
                                    ),
                                    ForwardSpelling::Turbofish => format!(
                                        "cannot forward `{sigma}` into `{name}` here: `{name}` \
                                         forwards the slot `{sigma}`, and this body carries no \
                                         matching one — a body carries exactly the slots its own \
                                         forwarded sites spell"
                                    ),
                                },
                            );
                            match spelling {
                                // Verified: `r = self.inner; r.load::<T>(text)` compiles and
                                // decodes per instantiation — a bare-name receiver is the spelling
                                // the pre-pass registers a slot from.
                                ForwardSpelling::CompoundReceiver => {
                                    d.help(format!(
                                        "bind the receiver to a local and call on that name: \
                                         `r = <receiver>;` then `r.{name}::<...>(...)`"
                                    ));
                                }
                                ForwardSpelling::Inferred => {
                                    d.help(format!(
                                        "spell the instantiation with an explicit turbofish \
                                         (`{name}::<...>`) so `{sigma}` is recognized as forwarded"
                                    ));
                                }
                                // The turbofish is already spelled on a name the pre-pass sees, so
                                // there is no route to point at — a help here would only repeat
                                // what the source does. Say nothing rather than something false.
                                ForwardSpelling::Turbofish => {}
                            }
                        }
                    }
                    continue;
                }
                hidden.push(self.intern_type_arg(&sigma, slot, name, span));
            }
            self.sites.hidden_arg_sites.insert(call_span, hidden);
        }
        subst_or_dyn(&generic.raw_ret, &subst, &tps)
    }

    /// Whether `t` mentions any type parameter currently in scope — the guard that keeps a
    /// composite instantiation (`List<T>`) out of a forwarded hidden slot (only the bare
    /// parameter passes through).
    pub(crate) fn mentions_in_scope_param(&self, t: &Type) -> bool {
        mentions_param(t, &self.scope_param_ids())
    }

    /// Enforce a polymorphic callable's declared **trait bounds** against a resolved substitution:
    /// each bound type parameter that the substitution pins to a concrete type must satisfy its
    /// bounds (`E0025`), exactly as a generic call enforces them. Shared by the call path
    /// ([`Self::check_generic_call`]) and the value-position instantiation
    /// ([`Self::instantiate_fn_value`], F1 poly-values), so the two judgments cannot drift.
    pub(crate) fn enforce_type_param_bounds(
        &mut self,
        name: &str,
        params: &[(ParamRef, Vec<BoundReq>)],
        subst: &Subst,
        tps: &ParamSet,
        span: Span,
    ) {
        for (param, bounds) in params {
            let pname = &param.name;
            let Some(concrete) = subst.get(&param.id) else {
                continue; // unconstrained by the arguments — nothing concrete to check against
            };
            for bound in bounds {
                // A type parameter that is itself in scope satisfies whatever its OWN declaration
                // bounds it by — there is no `impl` to look up, the bound is the declaration. This
                // is what licenses a generic type's methods to call each other: inside
                // `struct Agent<P: Provider>`, `self.other()` substitutes the callee's `P` with the
                // receiver's `P`, and the callee also demands `P: Provider`.
                if self.in_scope_param_satisfies(concrete, bound, subst, tps) {
                    continue;
                }
                // A user-defined trait bound (L1, UT3): satisfied iff `concrete` has a recorded
                // `impl` of it — and, for an INSTANTIATED bound (`T: Keyed<int>`), an impl at that
                // instantiation. A bound argument may mention a sibling parameter (`<K, T:
                // Keyed<K>>`), so the call's own substitution applies first; a parameter the
                // arguments leave unbound erases to `dyn` and defers.
                if self.symbols.user_traits.contains_key(&bound.name) {
                    let want: Vec<Type> = bound
                        .args
                        .iter()
                        .map(|a| subst_or_dyn(a, subst, tps))
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
                let bound = &bound.name;
                let satisfied = match BuiltinTrait::from_name(bound) {
                    Some(t) => self.satisfies(concrete, t),
                    None => {
                        self.satisfies_user_trait(concrete, bound, &[])
                            || self.native_type_advertises(concrete, bound)
                    }
                };
                if !satisfied {
                    let help =
                        format!("`{concrete}` must `@derive` or `impl {bound}` to be used here");
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
    }

    /// Whether `concrete` is a **type parameter currently in scope** whose own declaration already
    /// carries `bound`.
    ///
    /// A parameter is not a nominal type, so it can never have a recorded `impl` — its bounds *are*
    /// its declaration. Without this, a generic type's methods could not call one another: in
    /// `struct Agent<P: Provider>`, `self.drive(…)` substitutes the callee's `P` with the caller's
    /// `P` and then failed the callee's `P: Provider` bound, reporting "type `P` does not satisfy
    /// the bound `Provider`" against the very declaration that states it does. The same applies to
    /// built-in bounds (`<T: Comparable>` calling another `<T: Comparable>` helper).
    ///
    /// An instantiated bound must match argument-wise (`T: Keyed<int>` does not license a call
    /// demanding `Keyed<string>`), with the call's own substitution applied to the callee's bound
    /// arguments first, exactly as the nominal path does.
    fn in_scope_param_satisfies(
        &self,
        concrete: &Type,
        bound: &BoundReq,
        subst: &Subst,
        tps: &ParamSet,
    ) -> bool {
        let Type::Param(p) = concrete else {
            return false;
        };
        let Some(declared) = self.param_bounds(p) else {
            return false;
        };
        let want: Vec<Type> = bound
            .args
            .iter()
            .map(|a| subst_or_dyn(a, subst, tps))
            .collect();
        declared.iter().any(|d| {
            d.name == bound.name
                && (want.is_empty()
                    || (d.args.len() == want.len()
                        && d.args
                            .iter()
                            .zip(&want)
                            .all(|(a, b)| bound_arg_matches(a, b))))
        })
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
            if !self.has_builtin_trait(n, t) {
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
    /// Whether a **native** type advertises `bound` in its own `ExtType.traits` declaration.
    ///
    /// The import-gated `user_trait_impls` table cannot answer this: `seed_ext_traits` seeds a
    /// native trait only when the program `use`s it, which is right for *naming* one (`impl Widget`
    /// needs the import like any other name) and wrong for a **bound on a native signature**. A
    /// program calling `synced_signal(crdt.gcounter(), …)` never names `Mergeable` — the bound is
    /// the extension's own business — so requiring an import to satisfy it would make every such
    /// call site import traits it does not mention. The registry knows the advertisement outright,
    /// so ask it there.
    fn native_type_advertises(&self, ty: &Type, bound: &str) -> bool {
        let Type::Named(name, _) = ty else {
            return false;
        };
        let qualified = self
            .imports
            .extern_types
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        self.reg()
            .find_type_qualified(&qualified)
            .is_some_and(|t| t.traits.contains(&bound))
    }

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
                // A **native** type's impls are recorded under its qualified identity
                // (`user_trait_impls["para.crdt.GSet"]["Mergeable"]`), while a signature names it
                // by the short spelling its `ExtType` declares (`GSet`). Resolve through the
                // import map on a miss, so a native type advertising a native trait satisfies the
                // bound the same way a user type does.
                let impls = self.symbols.user_trait_impls.get(n).or_else(|| {
                    self.imports
                        .extern_types
                        .get(n)
                        .and_then(|qualified| self.symbols.user_trait_impls.get(qualified))
                });
                let Some(impl_args) = impls.and_then(|impls| impls.get(bound)) else {
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
            // A bound on a native signature names EITHER a built-in trait or a trait the extension
            // itself declares (`para.crdt.Mergeable`). Resolving only the built-in set and skipping
            // the rest silently drops the bound — which is how `synced_signal(42, "t")` briefly
            // type-checked when `Mergeable` stopped being built-in. Both kinds are checked here.
            let satisfied = match BuiltinTrait::from_name(bound) {
                Some(t) => self.satisfies(&concrete, t),
                None => {
                    self.satisfies_user_trait(&concrete, bound, &[])
                        || self.native_type_advertises(&concrete, bound)
                }
            };
            if satisfied {
                continue;
            }
            let help = format!("`{concrete}` must `@derive` or `impl {bound}`");
            self.error(
                DiagnosticCode::TraitBoundNotSatisfied,
                span,
                format!("type `{concrete}` does not satisfy the bound `{bound}`"),
            )
            .help(help);
        }
    }
}

/// The **short** form of a link-qualified name (`b.thing.Thing` → `Thing`) — what the author wrote
/// and what a code sketch in a diagnostic must use, since the qualified form is the linker's
/// spelling and is not valid in a declaration.
fn short_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
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

/// **What a type may implement only once** — the key [`Checker::check_coherence`] counts
/// occurrences under, and the name its diagnostic calls the contest by.
///
/// For every trait that is the trait's own name. `From` is the exception, and the only one: it is
/// the single built-in whose `impl` carries a type argument, and that argument is part of what is
/// being implemented — `impl From<HttpError>` and `impl From<JsonError>` are two different
/// conversions into one target, not two implementations of one contract. Keying `From` on the
/// source is what lets a type declare one conversion per source while a repeated source stays the
/// conflict it is (E0027).
///
/// A **generic user trait** is deliberately still keyed by name: `impl Cache<string>` beside
/// `impl Cache<int>` would hand the type two `get`s with no way to choose between them at a call
/// site, which is the ambiguity coherence exists to refuse. `From` escapes that because its call
/// sites carry the source type — a `?`'s propagated `Err`, an argument's type — and so can say
/// which conversion they mean.
pub(crate) fn coherence_key(trait_name: &str, trait_args: &[noeta_ast::TypeRef]) -> String {
    if trait_name == BuiltinTrait::From.name()
        && let [source] = trait_args
    {
        return format!("{trait_name}<{}>", noeta_ast::shape::type_source(source));
    }
    trait_name.to_string()
}

/// How one implementation of a trait was **written** — the three spellings [`Checker::check_coherence`]
/// counts as implementations, so a collision report can name what actually collided instead of
/// guessing at the `@derive`-vs-`impl` pair. Two standalone impls in two modules are the common
/// cross-file conflict, and neither is a `@derive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImplForm {
    /// A `@derive(Trait)` directive on the type's declaration.
    Derive,
    /// An `impl Trait { … }` block inside the type's own body.
    InBody,
    /// A standalone `impl Trait for Type { … }` declaration, which may live in another module
    /// (or, before the package orphan rule, another package) entirely.
    Standalone,
}

impl std::fmt::Display for ImplForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ImplForm::Derive => "by a `@derive`",
            ImplForm::InBody => "by an `impl` block in the type's body",
            ImplForm::Standalone => "by a standalone `impl … for …`",
        })
    }
}
