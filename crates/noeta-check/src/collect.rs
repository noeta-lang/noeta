//! **Pass 0/1 — collection**: resolve every `use` import ([`Checker::collect_imports`]) and walk
//! the program registering every declaration into the symbol tables ([`Checker::collect`]),
//! plus the per-declaration recording helpers (optional fields, derives, trait impls, attribute
//! opt-ins). All `Checker` methods moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;

impl Checker {
    /// Pass 0: resolve every `use` import before any declaration is collected, so an annotation in a
    /// signature (`fn f(x: Uuid)`) sees the import map regardless of source order. Populates the four
    /// import channels: native **modules** (`use std.{json}`), selective module **functions**
    /// (`use std.math.sqrt`), **extern types** (`use std.id.Uuid [as Alias]` → [`Self::extern_types`]),
    /// and **user-type** imports (everything else → [`Self::types`]). The extern-type case is tried
    /// before the selective-function case: `use std.id.Uuid` names a *type* in the `id` unit, not a
    /// function, so it must not fall into the "module has no function" error.
    /// Record `local` as a name bindings in `span`'s own source may not shadow (E0059), described by
    /// `what` — see [`Symbols::source_statics`] for why the index is per-source.
    fn note_static(&mut self, span: Span, local: &str, what: &'static str) {
        self.symbols
            .source_statics
            .entry(span.source)
            .or_default()
            .insert(local.to_string(), what);
    }

    pub(crate) fn collect_imports(&mut self, program: &Program) {
        use noeta_ext_abi::registry::UseKind;
        for stmt in &program.stmts {
            let Stmt::Use {
                path, names, span, ..
            } = stmt
            else {
                continue;
            };
            let use_span = *span;
            for name in names {
                let local = name.local().to_string();
                // One shared classifier decides what every `use` target binds — so the checker, the
                // compiler, and the eval reference never diverge on whether a name is a module, a
                // namespace group, a member function, a type, or an error (the check/run divergence
                // this closes). `UnknownUnderRoot` stays lenient in this slice (except the existing
                // member-function-miss diagnostic); slice 2 tightens it to a hard E0019.
                let kind = self.reg().classify_use(path, &name.name);
                // Every `use` binds its local name **in this file**, so a later binding of the same
                // name there is E0059 — and a binding of that name in some *other* module of the
                // merged program is not this import's business. See `Symbols::source_statics`.
                match &kind {
                    UseKind::UnknownUnderRoot => {}
                    UseKind::Module(_) | UseKind::Namespace(_) => {
                        self.note_static(use_span, &local, "an imported module")
                    }
                    UseKind::MemberFn { .. } => {
                        self.note_static(use_span, &local, "a top-level function")
                    }
                    _ => self.note_static(use_span, &local, "an imported type"),
                }
                match kind {
                    UseKind::Module(qualified) => {
                        // Expose the module's own extern types under the bound name, exactly as the
                        // namespace arm below does for a group. Without this, `use para.db` +
                        // `db.Connection` left the annotation as a bare `Type::Named("db.Connection")`
                        // that never unified with the `para.db.Connection` a native returns — E0007
                        // "`Connection` is not assignable to `Connection`", two identities behind one
                        // short name.
                        //
                        // The divergence was invisible for a long time because it depends on whether
                        // the `use` target is a namespace group or a leaf module: `std.http` is a
                        // group (parent of `.client`/`.server`) and took the arm below, while
                        // `para.db` is a concrete module and took this one. Same spelling, different
                        // answer — so a package-provided extern type behaved unlike a std one for a
                        // reason no user could see.
                        for (rel, qualified_ty) in self.reg().namespace_types(&qualified) {
                            self.imports
                                .extern_types
                                .insert(format!("{local}.{rel}"), qualified_ty);
                        }
                        self.imports.modules.insert(local, qualified);
                    }
                    UseKind::Namespace(prefix) => {
                        // Expose the group's types under the bound name so a dotted annotation
                        // (`http.Response`, aliased `h.Response`) resolves like the group's modules
                        // resolve for a call — mapping `<local>.<rel>` to the type's qualified
                        // identity, the same channel a leaf `use std.http.Response` import uses.
                        for (rel, qualified) in self.reg().namespace_types(&prefix) {
                            self.imports
                                .extern_types
                                .insert(format!("{local}.{rel}"), qualified);
                        }
                        self.imports.namespaces.insert(local, prefix);
                    }
                    UseKind::ExternType(qualified) => {
                        // An **extern-type** import (`use std.id.Uuid`, `use std.metrics.Counter as
                        // C`): bind the local name (alias or short) to its qualified identity — the
                        // annotation resolver keys on this.
                        self.imports.extern_types.insert(local, qualified);
                    }
                    UseKind::ExtEnum(qualified) => {
                        // A native-**enum** import (`use std.http.SameSite`, native-extensibility
                        // S1): bind the local name to the enum's qualified identity through the SAME
                        // `extern_types` channel — an annotation `SameSite` then resolves to the
                        // qualified `Type::Named` the enum was seeded under, so a native fn returning
                        // it unifies by identity and a `match` over it is exhaustive.
                        self.imports.extern_types.insert(local, qualified);
                    }
                    UseKind::ExtClass(qualified) | UseKind::ExtStruct(qualified) => {
                        // A native **fielded-type** import — a class (`use res.Handle`,
                        // native-extensibility S2) or a value struct (`use pkg.Point`, fielded
                        // unification): bind the local name to the type's qualified identity through
                        // the SAME `extern_types` channel — an annotation `Handle`/`Point` and a
                        // construction `Handle { ... }` / `Point { ... }` then resolve to the
                        // qualified `Type::Named` the type was seeded under
                        // (`symbols.records`/`private_fields`/`mut_fields`/`type_kinds`), so a native
                        // fn returning it unifies by identity, field access types, and E0035/E0033
                        // fire. Class vs struct semantics diverge only downstream (at materialize),
                        // keyed off the seeded `TypeKind`; the checker binding is identical.
                        self.imports.extern_types.insert(local, qualified);
                    }
                    UseKind::ExtTrait(qualified) => {
                        // A native-**trait** import (`use fx.Widget`, native-extensibility S3): record
                        // the short→qualified alias through the SAME `extern_types` channel — so a
                        // `dyn Widget` annotation / a native method's `Widget`-typed signature resolve
                        // to the qualified identity (`qualified_extern`). The user-trait table entry
                        // (`user_traits["Widget"]`) is seeded from this alias by `seed_ext_traits`,
                        // AFTER the `Stmt::Trait` walk, so a user `trait Widget` shadows it.
                        self.imports.extern_types.insert(local, qualified);
                    }
                    UseKind::MemberFn { module, func } => {
                        self.imports.imported_fns.insert(local, (module, func));
                    }
                    UseKind::UnknownUnderRoot => {
                        // A known extension root is fully enumerable, so a target that resolves to no
                        // module / namespace / member / type is a genuine error — not an opaque stub
                        // (this is the check/run divergence: `use std.{http}` used to slip through to
                        // an opaque type and fail only at run/`--native`). A member miss on a real
                        // module reads as "has no member"; anything else names nothing under the root.
                        let module = path.join(".");
                        let message =
                            if path.len() >= 2 && self.reg().find_module(&module).is_some() {
                                format!("module `{module}` has no member `{}`", name.name)
                            } else {
                                format!(
                                    "`{}` is not a module, namespace, or type in `{module}`",
                                    name.name
                                )
                            };
                        let candidates = self.reg().import_candidates(path);
                        let suggestion = noeta_diagnostics::closest(
                            &name.name,
                            candidates.iter().map(String::as_str),
                        )
                        .map(str::to_string);
                        let diag = self.error(DiagnosticCode::UnresolvedImport, name.span, message);
                        if let Some(s) = suggestion {
                            diag.help(format!("did you mean `{s}`?"));
                        }
                    }
                    UseKind::UserImport => {
                        self.symbols.types.insert(local);
                    }
                }
            }
        }
    }

    /// Register one METHOD's signature under `(type_name, method)` — the shared worker of the
    /// struct/class/enum collection arms (they previously triplicated this verbatim). The
    /// registered signature is erased over BOTH the enclosing type's parameters and the method's
    /// OWN (generic methods, poly-deferrals D3); the `GenericInfo` composes them — the class's
    /// parameters first (`class_params` many, seeded positionally by the receiver's type
    /// arguments), then the method's own (filled by turbofish/arguments/expectation).
    fn collect_method_sig(&mut self, type_name: &str, m: &FnDecl, type_params: &[TypeParam]) {
        self.collect_method_sig_classified(type_name, m, type_params, false);
    }

    /// [`Self::collect_method_sig`] with control over the receiver classification.
    ///
    /// `trait_provided` says whether a **trait's interface** supplies this method — an `impl Trait`
    /// block's own method, in-body or standalone. It changes what a *self-less* body means: an
    /// inherent one is an associated function ([`Receiver::Static`], `T.m(…)` only), a trait's
    /// is reachable either way ([`Receiver::Either`]), because the trait's contract puts it in the
    /// instance interface — `dyn Trait` dispatches it on a value — while its body needs no receiver.
    /// A body that *does* read `self` is [`Receiver::Instance`] either way; the trait cannot conjure
    /// a receiver for it, and calling such a method as `T.m(…)` aborts at run time.
    ///
    /// This flag used to be `classify_instance`, and `false` meant "record nothing" — leaving the
    /// third state to the accident of two call sites disagreeing about the default. Recording it
    /// makes the standalone spelling say what it means, and incidentally closes the case that
    /// accident got wrong: a standalone impl whose body reads `self`, called as `T.m(…)`, checked
    /// clean and then died with "no field `x` on unit".
    fn collect_method_sig_classified(
        &mut self,
        type_name: &str,
        m: &FnDecl,
        type_params: &[TypeParam],
        trait_provided: bool,
    ) {
        let uses_self = m.body.iter().any(|s| s.mentions("self"));
        let receiver = if trait_provided {
            Receiver::trait_method(uses_self)
        } else {
            Receiver::inherent(uses_self)
        };
        // Method visibility, recorded at the ONE funnel every kind's methods pass through
        // (struct/class/enum inherent bodies, in-body `impl` blocks, standalone `impl`s) — so the
        // rule cannot be spelled once per kind and drift. A method is private unless declared
        // `pub`; a TRAIT-supplied one is public by construction (`trait_provided`), because the
        // trait's contract is what puts it on the outward surface.
        if !trait_provided && !m.is_public {
            self.symbols
                .private_methods
                .entry(type_name.to_string())
                .or_default()
                .insert(m.name.to_string());
        } else {
            // A later registration WINS over an earlier one for the same key (an `impl` method
            // over an inherent of the same name), so a public one must clear a private entry
            // rather than leave it standing.
            if let Some(set) = self.symbols.private_methods.get_mut(type_name) {
                set.remove(m.name.as_str());
            }
        }
        self.symbols
            .method_receiver
            .insert((type_name.to_string(), m.name.to_string()), receiver);
        let xt = &self.imports.extern_types;
        // The type's parameters, then the method's own LAYERED OVER them: a method `<T>` inside a
        // class `<T>` shadows, so an annotation in this signature resolves to the METHOD's `T`.
        // Both remain in `generic.params` below — they are different parameters with different
        // identities, and each is seeded from its own channel (the receiver's type arguments for
        // the class's, the turbofish/arguments for the method's).
        let type_scope = param_scope(type_params, xt);
        let scope = extend_param_scope(&type_scope, &m.type_params, xt);
        let type_generics: Vec<(ParamRef, Vec<BoundReq>)> = type_params
            .iter()
            .map(|p| (param_ref(p), bound_reqs(&p.bounds, xt, &type_scope)))
            .collect();
        let own_generics: Vec<(ParamRef, Vec<BoundReq>)> = m
            .type_params
            .iter()
            .map(|p| (param_ref(p), bound_reqs(&p.bounds, xt, &scope)))
            .collect();
        // Erasure quantifies over BOTH lists — including a class parameter the method shadowed,
        // which the signature cannot name but which costs nothing to include and would be a silent
        // gap if the shadowing rule ever changed.
        let mut tps = param_ids(type_params);
        tps.extend(param_ids(&m.type_params));
        let raw_params: Vec<Type> = m.params.iter().map(|p| param_type(p, xt, &scope)).collect();
        let raw_ret = async_return(
            m.ret
                .as_ref()
                .map(|t| from_ref_q(t, xt, &scope))
                .unwrap_or(Type::Unknown),
            m.is_async,
        );
        let params = raw_params
            .iter()
            .cloned()
            .map(|t| erase_type_params(t, &tps))
            .collect();
        let ret = erase_type_params(raw_ret.clone(), &tps);
        let generic =
            (!type_generics.is_empty() || !own_generics.is_empty()).then(|| GenericInfo {
                class_params: type_generics.len(),
                params: type_generics.into_iter().chain(own_generics).collect(),
                raw_params,
                raw_ret,
            });
        self.symbols.methods.insert(
            (type_name.to_string(), m.name.to_string()),
            FnSig {
                params,
                param_names: m.params.iter().map(|p| p.name.clone()).collect(),
                ret,
                required: required_params(&m.params),
                generic,
            },
        );
    }

    /// Pass 1: register every top-level declaration so forward references resolve before any
    /// body is checked. Mirrors the compiler's "register types first" pass.
    pub(crate) fn collect(&mut self, program: &Program) {
        // Hoist top-level value-binding names (F1): a function body may reference a global
        // declared textually later, so they are all "known" to the unknown-name gate.
        for stmt in &program.stmts {
            match stmt {
                Stmt::Binding { name, .. } => {
                    self.symbols.global_binding_names.insert(name.clone());
                }
                Stmt::Destructure { targets, .. } => {
                    for (name, _) in targets {
                        self.symbols.global_binding_names.insert(name.clone());
                    }
                }
                _ => {}
            }
        }
        // Hoist every NESTED fn declaration's name (sealed-fn model): a nested fn's name is an
        // item of its enclosing body, so recursion/sibling calls resolve even inside sealed
        // bodies. Walk each top-level fn/method body for `Stmt::Fn` at any depth.
        for stmt in &program.stmts {
            collect_nested_fn_names(stmt, true, &mut self.symbols.nested_fn_names);
        }
        // Every top-level declaration, as a name bindings **in its own file** may not shadow
        // (E0059). Under its *local* spelling: the loader qualifies a package module's declarations
        // (`desk.tools.find_order`), and what a binding could collide with is the last segment, which
        // is how the source spells it. Recorded after the imports pass so a declaration's own word
        // wins over an import's for the same name.
        // Two top-level declarations under one name (E0020). The registration below is a map
        // insert, so the second silently replaces the first — and then the compiler looks up a
        // method of the *first* class in the second's now-empty table and panics on the missing
        // key. The name is also simply ambiguous: nothing downstream can say which declaration a
        // reference meant.
        //
        // Scoped to a single **source**, which is what makes this rule right rather than merely
        // strict. Two declarations in one file are unambiguously a mistake. Two in different files
        // are not this pass's business: the loader qualifies a package module's declarations and
        // has its own E0020 for the cases that do collide, and a REPL session legitimately
        // *redeclares* a type at a later prompt — each entry is its own source, and the session
        // oracle holds a whole-program re-check of the accumulated entries to the same verdict.
        //
        // Keyed on the FULL name rather than the local spelling for the same reason.
        let mut declared: HashMap<(&str, noeta_span::SourceId), Span> = HashMap::new();
        for stmt in &program.stmts {
            let (name, span, what) = match stmt {
                Stmt::Fn(d) => (d.name.as_str(), d.span, "a top-level function"),
                Stmt::Struct(d) => (d.name.as_str(), d.span, "a type"),
                Stmt::Class(d) => (d.name.as_str(), d.span, "a type"),
                Stmt::Enum(d) => (d.name.as_str(), d.span, "a type"),
                _ => continue,
            };
            let local = name.rsplit('.').next().unwrap_or(name).to_string();
            self.note_static(span, &local, what);
            if let Some(first) = declared.insert((name, span.source), span) {
                self.error(
                    DiagnosticCode::NameCollision,
                    span,
                    format!("`{local}` is already declared in this module"),
                )
                .label(first, "the first declaration is here")
                .help("rename one of them — a reference to the name is otherwise ambiguous");
            }
        }
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(r) => {
                    // Field types resolve against the type's OWN parameters, so a `T`-typed field
                    // is a `Type::Param` a later instantiation substitutes by identity.
                    let scope = param_scope(&r.type_params, &self.imports.extern_types);
                    let fields = r
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                field_type(&f.ty, &self.imports.extern_types, &scope),
                            )
                        })
                        .collect();
                    self.symbols.records.insert(r.name.to_string(), fields);
                    // Where this type was declared — the span whose `SourceId` the package orphan
                    // rule resolves the type's package from.
                    self.symbols
                        .type_decl_spans
                        .insert(r.name.to_string(), r.name_span);
                    if let Some(directive) = &r.decorators.packed {
                        self.symbols.packed_structs.insert(r.name.to_string());
                        if directive.layout == noeta_ast::PackedLayout::Column {
                            self.symbols.column_structs.insert(r.name.to_string());
                        }
                    }
                    if r.decorators.validated.is_some() {
                        self.symbols.validated_types.insert(r.name.to_string());
                    }
                    // A struct's `mut` fields are assignable via `x.f = v` (value-semantic, so the
                    // write is a copy-on-write rebind). Register them exactly as for a class; the
                    // binding-`mut` requirement that distinguishes the two is a slice-2 refinement.
                    let muts: HashSet<String> = r
                        .fields
                        .iter()
                        .filter(|f| f.mut_field)
                        .map(|f| f.name.clone())
                        .collect();
                    if !muts.is_empty() {
                        self.symbols.mut_fields.insert(r.name.to_string(), muts);
                    }
                    self.symbols.types.insert(r.name.to_string());
                    self.symbols
                        .type_kinds
                        .insert(r.name.to_string(), noeta_types::TypeKind::Struct);
                    self.record_optional_fields(r.name.as_str(), &r.fields);
                    // A struct satisfies a trait it `@derive`s or in-body `impl`s — the same
                    // chain a class/enum records. (The impls half was missing here: a struct's
                    // `impl Comparable` never registered, so bounds falsely rejected it.)
                    self.record_trait_impls(
                        r.name.as_str(),
                        r.decorators
                            .derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(r.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.record_derived(r.name.as_str(), &r.decorators.derives);
                    self.record_from_impls(r.name.as_str(), &r.impls);
                    self.record_attribute(r.name.as_str(), r.decorators.attribute.as_deref());
                    self.symbols.generic_types.insert(
                        r.name.to_string(),
                        r.type_params.iter().map(param_ref).collect(),
                    );
                    // The same parameters WITH bounds, for checking a standalone `impl`'s bodies.
                    self.symbols
                        .type_params
                        .insert(r.name.to_string(), r.type_params.clone());
                    // Record each struct method's signature + instance classification, exactly as
                    // for a class (this closed a long-standing gap: struct associated calls —
                    // `B.new(1)` — previously typed as a hole because struct methods were never
                    // registered; prelude-redesign EX.2 needs the classification for all kinds).
                    // Inherent methods classify from their bodies; an `impl Trait { … }` block's do
                    // not (see `collect_method_sig_classified`). Registration order matches the
                    // flattened walk this replaces — inherent first, so a same-named impl method
                    // still wins.
                    for m in &r.methods {
                        self.collect_method_sig(r.name.as_str(), m, &r.type_params);
                    }
                    for (m, provided) in impl_block_methods(&r.impls) {
                        self.collect_method_sig_classified(
                            r.name.as_str(),
                            m,
                            &r.type_params,
                            provided,
                        );
                    }
                    self.bake_impl_assoc(r.name.as_str(), &r.impls, &r.type_params);
                }
                Stmt::Class(c) => {
                    // Field types resolve against the type's OWN parameters, so a `T`-typed field
                    // is a `Type::Param` a later instantiation substitutes by identity.
                    let scope = param_scope(&c.type_params, &self.imports.extern_types);
                    let fields = c
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                field_type(&f.ty, &self.imports.extern_types, &scope),
                            )
                        })
                        .collect();
                    self.symbols.records.insert(c.name.to_string(), fields);
                    self.symbols
                        .type_decl_spans
                        .insert(c.name.to_string(), c.name_span);
                    if c.decorators.validated.is_some() {
                        self.symbols.validated_types.insert(c.name.to_string());
                    }
                    let muts: HashSet<String> = c
                        .fields
                        .iter()
                        .filter(|f| f.mut_field)
                        .map(|f| f.name.clone())
                        .collect();
                    if !muts.is_empty() {
                        self.symbols.mut_fields.insert(c.name.to_string(), muts);
                    }
                    // Class fields default **private**; only those declared `pub` are public
                    // (object-model slice 2d). Struct fields are always public, so structs never
                    // register here.
                    let private: HashSet<String> = c
                        .fields
                        .iter()
                        .filter(|f| !f.is_public)
                        .map(|f| f.name.clone())
                        .collect();
                    if !private.is_empty() {
                        self.symbols
                            .private_fields
                            .insert(c.name.to_string(), private);
                    }
                    self.symbols.types.insert(c.name.to_string());
                    self.symbols
                        .type_kinds
                        .insert(c.name.to_string(), noeta_types::TypeKind::Class);
                    // A class with a `destruct { ... }` block seeds destruct-reachability (Phase 3.2b).
                    if c.destructor.is_some() {
                        self.symbols.destructor_classes.insert(c.name.to_string());
                    }
                    // A class satisfies a trait it `@derive`s or `impl`s; record both for bound
                    // enforcement (the `impl`/`derive` *names* are validated elsewhere).
                    self.record_trait_impls(
                        c.name.as_str(),
                        c.decorators
                            .derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(c.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.record_derived(c.name.as_str(), &c.decorators.derives);
                    self.record_from_impls(c.name.as_str(), &c.impls);
                    // Record each method's signature (class methods and impl-block methods alike),
                    // so `obj.method(...)` resolves to a concrete type and its arguments are
                    // checked. The class's generic parameters are erased to `dyn` (erased at
                    // runtime, they accept any argument).
                    self.symbols.generic_types.insert(
                        c.name.to_string(),
                        c.type_params.iter().map(param_ref).collect(),
                    );
                    // The same parameters WITH bounds, for checking a standalone `impl`'s bodies.
                    self.symbols
                        .type_params
                        .insert(c.name.to_string(), c.type_params.clone());
                    for m in &c.methods {
                        self.collect_method_sig(c.name.as_str(), m, &c.type_params);
                    }
                    for (m, provided) in impl_block_methods(&c.impls) {
                        self.collect_method_sig_classified(
                            c.name.as_str(),
                            m,
                            &c.type_params,
                            provided,
                        );
                    }
                    self.bake_impl_assoc(c.name.as_str(), &c.impls, &c.type_params);
                }
                Stmt::Enum(e) => {
                    // As for a struct's fields: a payload naming the enum's `T` is a parameter.
                    let scope = param_scope(&e.type_params, &self.imports.extern_types);
                    let variants = e
                        .variants
                        .iter()
                        .map(|v| VariantInfo {
                            name: v.name.clone(),
                            // A variant's payload types, read from the annotation exactly as a
                            // struct's field types are (R2b): one source of truth for
                            // enum-construction type-argument inference, the `Send` classifier, and
                            // destructor-relevance. This needed a `variant_field_type` helper that
                            // rebuilt a positional payload's type out of the `Param`'s *name*; the
                            // parser puts it in `ty` now, so the plain field rule reaches both forms.
                            fields: v
                                .fields
                                .iter()
                                .map(|v| field_type(&v.ty, &self.imports.extern_types, &scope))
                                .collect(),
                            // A backed variant's literal, through the one `fold_const_expr` the
                            // reflection manifest also folds with — so the backing a decode recipe
                            // matches on and the backing `variants_of` reports are the same value,
                            // not two independent readings of the same declaration.
                            backing: v
                                .backed_value
                                .as_ref()
                                .and_then(noeta_ast::reflect::fold_const_expr),
                        })
                        .collect();
                    self.symbols.enums.insert(e.name.to_string(), variants);
                    self.symbols.types.insert(e.name.to_string());
                    self.symbols
                        .type_decl_spans
                        .insert(e.name.to_string(), e.name_span);
                    self.symbols
                        .type_kinds
                        .insert(e.name.to_string(), noeta_types::TypeKind::Enum);
                    // `@semantic` makes the enum role-eligible (its fieldless variants may be named
                    // by `@role(Enum.Variant)`); recorded for the post-collect role-validation pass.
                    if e.decorators.semantic.is_some() {
                        self.symbols.semantic_enums.insert(e.name.to_string());
                    }
                    // An enum satisfies a trait it `@derive`s or `impl`s (its in-body blocks are
                    // uniform with a class's — object-model slice 3); record both so an operator
                    // trait (`impl Add`, `impl Comparable`, …) is accepted on an enum operand.
                    self.record_trait_impls(
                        e.name.as_str(),
                        e.decorators
                            .derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(e.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.record_derived(e.name.as_str(), &e.decorators.derives);
                    self.record_from_impls(e.name.as_str(), &e.impls);
                    self.symbols.generic_types.insert(
                        e.name.to_string(),
                        e.type_params.iter().map(param_ref).collect(),
                    );
                    // The same parameters WITH bounds, for checking a standalone `impl`'s bodies.
                    self.symbols
                        .type_params
                        .insert(e.name.to_string(), e.type_params.clone());
                    // Record each enum method's signature (inherent + impl-block, the unified body —
                    // object-model slice 3) under `(Enum, method)`, exactly like a class's, so an
                    // instance call `status.label()` and an associated call `Status.parse(s)` resolve
                    // to a concrete type. The enum's generic parameters are erased to `dyn`.
                    for m in &e.methods {
                        self.collect_method_sig(e.name.as_str(), m, &e.type_params);
                    }
                    for (m, provided) in impl_block_methods(&e.impls) {
                        self.collect_method_sig_classified(
                            e.name.as_str(),
                            m,
                            &e.type_params,
                            provided,
                        );
                    }
                    self.bake_impl_assoc(e.name.as_str(), &e.impls, &e.type_params);
                }
                Stmt::Fn(f) => {
                    // The registered signature is **erased** (generic parameters → `dyn`): the
                    // arity check and the non-generic fast path use it. A generic function also
                    // carries un-erased `GenericInfo` so a call site can instantiate it precisely
                    // and enforce its bounds (S4.2); a non-generic function carries `None`.
                    let xt = &self.imports.extern_types;
                    let scope = param_scope(&f.type_params, xt);
                    let tps = param_ids(&f.type_params);
                    let raw_params: Vec<Type> =
                        f.params.iter().map(|p| param_type(p, xt, &scope)).collect();
                    // An `async fn f(): T` call produces `Future<T>` (Track A); wrap before erasure so
                    // the erased signature and the generic instantiation both carry the future.
                    let raw_ret = async_return(
                        f.ret
                            .as_ref()
                            .map(|t| from_ref_q(t, xt, &scope))
                            .unwrap_or(Type::Unknown),
                        f.is_async,
                    );
                    let params = raw_params
                        .iter()
                        .cloned()
                        .map(|t| erase_type_params(t, &tps))
                        .collect();
                    let ret = erase_type_params(raw_ret.clone(), &tps);
                    let generic = (!f.type_params.is_empty()).then(|| GenericInfo {
                        params: f
                            .type_params
                            .iter()
                            .map(|p| (param_ref(p), bound_reqs(&p.bounds, xt, &scope)))
                            .collect(),
                        class_params: 0,
                        raw_params,
                        raw_ret,
                    });
                    self.symbols.functions.insert(
                        f.name.to_string(),
                        FnSig {
                            params,
                            param_names: f.params.iter().map(|p| p.name.clone()).collect(),
                            ret,
                            required: required_params(&f.params),
                            generic,
                        },
                    );
                }
                // A `use std.{json, …}` import binds a Ring 2 module value (tracked in `modules`);
                // any other imported name (whether the linker merged its declaration or left an
                // opaque stub) is a legal referent for an annotation — registered as a known type.
                // `use` imports are resolved up front in `collect_imports` (pass 0), so the import
                // map is ready before any signature annotation is resolved.
                Stmt::Use { .. } => {}
                // A standalone `impl Trait for T {}` registers `T` as satisfying the trait (for
                // bound/gate checks) and records the occurrence so the target's coherence check
                // counts it. Validity (orphan rule, trait, body) is checked in pass 2.
                Stmt::Impl(decl) => {
                    self.record_trait_impls(
                        decl.target.as_str(),
                        std::iter::once(decl.trait_name.as_str()),
                    );
                    self.symbols
                        .standalone_impls
                        .entry(decl.target.to_string())
                        .or_default()
                        .push((decl.trait_name.to_string(), decl.trait_span));
                }
                // A user-defined trait (L1) is registered up front so forward references (an `impl`
                // or `<T: Trait>` bound textually above the `trait`) resolve. A duplicate declaration
                // keeps the first; pass 2 (`check_trait_decl`) reports the collision.
                Stmt::Trait(t) => {
                    self.symbols
                        .user_traits
                        .entry(t.name.to_string())
                        .or_insert_with(|| t.clone());
                }
                _ => {}
            }
        }
        // Seed the imported **native traits** (native-extensibility S3) into `user_traits` /
        // `user_trait_impls` now — AFTER the `Stmt::Trait` walk above, so a user `trait` of the same
        // short name (already in `user_traits`) shadows the native one (`.or_insert`), and BEFORE the
        // impl-collection loop below, so an `impl NativeTrait for T` is recognized (UT2) and a native
        // type's advertised impl backs the `dyn NativeTrait` coercion (3b).
        self.seed_ext_traits();
        // Record which user traits each type implements (L1, UT2), from standalone `impl`s,
        // in-body `impl`s, and `@derive(UserTrait)` (a fully-defaulted trait adopted wholesale —
        // `check_derives` enforces the fully-defaulted part). Done after the main walk so every
        // `trait` is registered regardless of source order. The basis for UT3 bound satisfaction
        // and UT4 `dyn Trait` coercion.
        for stmt in &program.stmts {
            let (type_name, impls, derives): (&str, &[noeta_ast::ImplBlock], &[DeriveSpec]) =
                match stmt {
                    Stmt::Impl(decl)
                        if self
                            .symbols
                            .user_traits
                            .contains_key(decl.trait_name.as_str()) =>
                    {
                        let args: Vec<Type> = decl
                            .trait_args
                            .iter()
                            .map(|t| from_ref_q(t, &self.imports.extern_types, &ParamScope::new()))
                            .collect();
                        self.symbols
                            .user_trait_impls
                            .entry(decl.target.to_string())
                            .or_default()
                            .entry(decl.trait_name.to_string())
                            .or_insert(args);
                        self.symbols
                            .trait_impl_sites
                            .entry((decl.target.to_string(), decl.trait_name.to_string()))
                            .or_insert(decl.trait_span);
                        continue;
                    }
                    Stmt::Struct(d) => (d.name.as_str(), &d.impls, &d.decorators.derives),
                    Stmt::Class(d) => (d.name.as_str(), &d.impls, &d.decorators.derives),
                    Stmt::Enum(d) => (d.name.as_str(), &d.impls, &d.decorators.derives),
                    _ => continue,
                };
            for (trait_name, trait_args, site) in impls
                .iter()
                .map(|b| (&b.trait_name, b.trait_args.as_slice(), b.trait_span))
                .chain(derives.iter().map(|d| (&d.name, d.args.as_slice(), d.span)))
            {
                if self.symbols.user_traits.contains_key(trait_name.as_str()) {
                    let args: Vec<Type> = trait_args
                        .iter()
                        .map(|t| from_ref_q(t, &self.imports.extern_types, &ParamScope::new()))
                        .collect();
                    self.symbols
                        .user_trait_impls
                        .entry(type_name.to_string())
                        .or_default()
                        .entry(trait_name.to_string())
                        .or_insert(args);
                    self.symbols
                        .trait_impl_sites
                        .entry((type_name.to_string(), trait_name.to_string()))
                        .or_insert(site);
                }
            }
        }
        // Associated-type bindings per implementor (slice 1a): fold each impl's `type Name = T;`
        // bindings over the trait's defaulted associated types into `trait_assoc[(type, trait)]`.
        // Done after the `user_trait_impls` walk so every trait is registered; the basis for
        // projecting `Self::Name` in a method signature to the implementor's concrete type.
        for stmt in &program.stmts {
            match stmt {
                Stmt::Impl(d) => self.record_assoc_bindings(
                    d.target.as_str(),
                    d.trait_name.as_str(),
                    &d.assoc_bindings,
                ),
                Stmt::Struct(d) => {
                    for b in &d.impls {
                        self.record_assoc_bindings(
                            d.name.as_str(),
                            b.trait_name.as_str(),
                            &b.assoc_bindings,
                        );
                    }
                }
                Stmt::Class(d) => {
                    for b in &d.impls {
                        self.record_assoc_bindings(
                            d.name.as_str(),
                            b.trait_name.as_str(),
                            &b.assoc_bindings,
                        );
                    }
                }
                Stmt::Enum(d) => {
                    for b in &d.impls {
                        self.record_assoc_bindings(
                            d.name.as_str(),
                            b.trait_name.as_str(),
                            &b.assoc_bindings,
                        );
                    }
                }
                _ => {}
            }
        }
        // Derive bridging/delegation (layers 1+2): a derive's *planned* methods — required-member
        // bridges, `via:` forwards, builtin `via:` templates — register their signatures, from
        // the same shared planner the backends' hoist materializes, so what the checker types and
        // what runs can never drift. Runs BEFORE the generic UT5 defaults loop below: a `via:`
        // forward replaces the trait's default wholesale (delegation dispatches into the field's
        // implementation), and registration keeps the first entry per name. A plan error is
        // ignored here; `check_derives` reports it.
        for stmt in &program.stmts {
            let (type_name, fields, methods, derives): (
                &str,
                &[FieldDecl],
                &[noeta_ast::FnDecl],
                &[DeriveSpec],
            ) = match stmt {
                Stmt::Struct(d) => (
                    d.name.as_str(),
                    &d.fields,
                    &d.methods,
                    &d.decorators.derives,
                ),
                Stmt::Class(d) => (
                    d.name.as_str(),
                    &d.fields,
                    &d.methods,
                    &d.decorators.derives,
                ),
                Stmt::Enum(d) => (d.name.as_str(), &[], &d.methods, &d.decorators.derives),
                _ => continue,
            };
            // The ONE cascade (`noeta_ast::derive::plan_derive`), which the backends' hoist also
            // runs. It used to be restated here and in `noeta_ir::lower` as two structurally
            // identical chains that nothing forced to agree — and they had already drifted, one
            // testing `BuiltinTrait::Error.name()` and the other a bare `"Error"`.
            let ctx = CheckerDeriveContext {
                user_traits: &self.symbols.user_traits,
                registry: self.reg(),
            };
            let plans: Vec<Vec<noeta_ast::FnDecl>> = derives
                .iter()
                .filter_map(|spec| {
                    noeta_ast::derive::plan_derive(&ctx, spec, type_name, fields, methods)
                })
                // A plan error is ignored here; `check_derives` reports it.
                .filter_map(|planned| planned.ok())
                .collect();
            let type_name = type_name.to_string();
            for m in plans.iter().flatten() {
                self.register_synth_method(&type_name, m);
            }
        }
        // A STANDALONE `impl Trait for T { … }`'s method signatures. The in-body `impl` half was
        // already folded into each type's own method walk above (`.impls` chained into `methods`);
        // this closes the other half, which the surface has carried unfinished since standalone
        // impls first parsed ("runtime dispatch … is a later slice" — dispatch landed, this did not).
        //
        // Without it the methods dispatch correctly at runtime (the loader hoists them onto the
        // target) while the checker never learns their signatures, so the call typed as a hole and
        // NOTHING was checked: `d.same("nope")` against `fn same(other: int): bool` checked clean,
        // ran, and printed `false` — a wrong answer rather than a diagnostic.
        //
        // Placement is load-bearing. AFTER the type walk, so `symbols.type_params` already carries
        // the target's parameters (stored there for exactly this purpose). BEFORE the UT5
        // default-fallback below, whose `register_synth_method` skips an already-registered key —
        // so a method the impl really provides wins over the trait's default.
        for stmt in &program.stmts {
            let Stmt::Impl(d) = stmt else { continue };
            let type_params = self
                .symbols
                .type_params
                .get(d.target.as_str())
                .cloned()
                .unwrap_or_default();
            // Mirror the in-body path's `Self::Name` projection (slice 1a, `bake_impl_assoc`): a
            // signature written against an associated type resolves to this impl's binding for it,
            // so a concrete receiver types against the implementor's type rather than a hole.
            let assoc: HashMap<&str, &TypeRef> = d
                .assoc_bindings
                .iter()
                .map(|(n, t)| (n.as_str(), t))
                .collect();
            let provided = trait_supplies_instance_interface(d.trait_name.as_str());
            for m in &d.methods {
                if assoc.is_empty() {
                    self.collect_method_sig_classified(
                        d.target.as_str(),
                        m,
                        &type_params,
                        provided,
                    );
                } else {
                    let resolved = subst_self_assoc_in_fn(m, &assoc);
                    self.collect_method_sig_classified(
                        d.target.as_str(),
                        &resolved,
                        &type_params,
                        provided,
                    );
                }
            }
        }
        // GENERIC-trait impls (in-body and standalone) register their INSTANTIATED omitted
        // defaults' signatures — `impl Cache<string>` registers `fn get(k: string): …` — so the
        // member calls type concretely. The non-generic case is covered by the name-set loop
        // below; an arity mismatch registers nothing (`check_trait_impl` reports it).
        //
        // This walk is `program.stmts`, so where a *generic* trait's default collides with another
        // trait's the winner is the textually-first one — arbitrary, but stable, and the same one
        // the backends' hoist takes. The ambiguity rule below (E0027) therefore does not extend
        // here: it exists to replace a per-process coin flip, and there is none to replace.
        for stmt in &program.stmts {
            let mut register =
                |type_name: &str,
                 trait_name: &str,
                 args: &[noeta_ast::TypeRef],
                 provided: &[noeta_ast::FnDecl]| {
                    if args.is_empty() {
                        return;
                    }
                    let Some(tr) = self.symbols.user_traits.get(trait_name).cloned() else {
                        return;
                    };
                    let Ok(Some(concrete)) = noeta_ast::derive::instantiate_trait(&tr, args) else {
                        return;
                    };
                    for tm in concrete.methods.iter().filter(|tm| {
                        tm.has_default && !provided.iter().any(|m| m.name == tm.sig.name)
                    }) {
                        self.register_synth_method(type_name, &tm.sig);
                    }
                };
            match stmt {
                Stmt::Struct(d) => {
                    for b in &d.impls {
                        register(
                            d.name.as_str(),
                            b.trait_name.as_str(),
                            &b.trait_args,
                            &b.methods,
                        );
                    }
                }
                Stmt::Class(d) => {
                    for b in &d.impls {
                        register(
                            d.name.as_str(),
                            b.trait_name.as_str(),
                            &b.trait_args,
                            &b.methods,
                        );
                    }
                }
                Stmt::Enum(d) => {
                    for b in &d.impls {
                        register(
                            d.name.as_str(),
                            b.trait_name.as_str(),
                            &b.trait_args,
                            &b.methods,
                        );
                    }
                }
                Stmt::Impl(d) => {
                    register(
                        d.target.as_str(),
                        d.trait_name.as_str(),
                        &d.trait_args,
                        &d.methods,
                    );
                }
                _ => {}
            }
        }
        // Trait default-body routes (ExtBundle→ExtTrait convergence, slice 2): before the UT5
        // fallback below, record which `(type, method)` pairs a NATIVE trait's default-body dispatch
        // answers — a defaulted method the type neither declares nor overrides. Done here, with the AST
        // impl bodies in reach, because "omitted vs provided" is not decidable from `symbols.methods`.
        self.seed_native_trait_defaults(program);
        // Default-method fallback (UT5): a trait method the implementor omits falls back to the
        // trait's default body (the backends hoist it via `hoist_standalone_impl_methods`), so its
        // SIGNATURE registers here — member calls on the implementing type resolve and type it. A
        // method the type provides itself wins (already registered above); a generic trait's
        // defaults are excluded (per-implementor substitution — deferred with generic-trait
        // derivation). A **native** trait carrying a default-body dispatch (slice 2) is also excluded:
        // its omitted defaults route through `native_trait_default_sites` (a native body, no hoisted
        // `.noe` signature to register — a synth one would misclassify a no-`self` body as an
        // associated fn), and a native/user override resolves through its own real method instead.
        let native_default_traits: HashSet<String> = self
            .imports
            .extern_types
            .iter()
            .filter(|(_, q)| {
                self.reg()
                    .find_trait_qualified(q)
                    .is_some_and(|t| t.dispatch.is_some())
            })
            .map(|(local, _)| local.clone())
            .collect();
        // Sorted, because a `HashMap` walk here decided *which trait* an omitted method was typed
        // from whenever two of them defaulted the same name — per process, so one source file
        // checked green in one run and red in the next. Two traits contending for one slot is now a
        // diagnostic (recorded below, reported as E0027 in pass 2), so this order never picks a
        // winner between rival defaults; it is what keeps the pass itself reproducible.
        let mut bindings: Vec<(String, Vec<String>)> = self
            .symbols
            .user_trait_impls
            .iter()
            .map(|(ty, traits)| {
                let mut names: Vec<String> = traits.keys().cloned().collect();
                names.sort();
                (ty.clone(), names)
            })
            .collect();
        bindings.sort();
        // Which trait supplied each `(type, method)` slot *by default* — a second supplier is the
        // ambiguity, and it is the programmer's to resolve (an override, or one implementation
        // fewer). A slot filled by a real method (the type's own, an impl body, a derive's bridge)
        // is not in here, so overriding the method silences this by construction.
        let mut supplied_by: HashMap<(String, String), String> = HashMap::new();
        for (type_name, trait_names) in bindings {
            for trait_name in trait_names {
                if native_default_traits.contains(&trait_name) {
                    continue;
                }
                let Some(decl) = self.symbols.user_traits.get(&trait_name).cloned() else {
                    continue;
                };
                if !decl.type_params.is_empty() {
                    continue;
                }
                for tm in decl.methods.iter().filter(|tm| tm.has_default) {
                    let key = (type_name.clone(), tm.sig.name.to_string());
                    if let Some(first) = supplied_by.get(&key) {
                        self.symbols
                            .trait_default_conflicts
                            .push(crate::TraitDefaultConflict {
                                type_name: type_name.clone(),
                                method: tm.sig.name.to_string(),
                                traits: (first.clone(), trait_name.clone()),
                            });
                        continue;
                    }
                    // `register_synth_method` is a no-op on a slot that is already filled — by the
                    // type's own method, an impl body, a derive's bridge, or a generic trait's
                    // instantiated default (all registered above, all in source order). Such a slot
                    // has a real owner, so this default is inert and nothing is contested; record
                    // ownership only where this default actually took an empty slot.
                    let taken = self.symbols.methods.contains_key(&key);
                    self.register_synth_method(&type_name, &tm.sig);
                    if !taken {
                        supplied_by.insert(key, trait_name.clone());
                    }
                }
            }
        }
        // Method-bundle bindings (kernel-methods K1) resolve after the whole collect walk, so a
        // binding is visible to method typing regardless of where the `impl`/`@derive` sits relative
        // to the `use` that binds its module. The TWO spellings register identically: a standalone
        // `impl <module>.<Bundle> for T {}` and a `@derive(<module>.<Bundle>)` on the type — the
        // latter is exactly the former (a bundle binding is shape-derived behavior, so `@derive` is
        // its natural home). Resolution failures stay silent here — pass 2 reports them at the site
        // (`check_bundle_impl` / `check_derives`). A dotted name that is a known user trait is never
        // a bundle (a cross-package trait `para.aether.Store` is dotted once qualified).
        for stmt in &program.stmts {
            // `(target, bundle-path, site span)` for every binding this statement contributes.
            let candidates: Vec<(&str, &str, Span)> = match stmt {
                Stmt::Impl(decl) => {
                    vec![(
                        decl.target.as_str(),
                        decl.trait_name.as_str(),
                        decl.trait_span,
                    )]
                }
                Stmt::Struct(d) => bundle_derive_candidates(d.name.as_str(), &d.decorators.derives),
                Stmt::Class(d) => bundle_derive_candidates(d.name.as_str(), &d.decorators.derives),
                Stmt::Enum(d) => bundle_derive_candidates(d.name.as_str(), &d.decorators.derives),
                _ => continue,
            };
            for (target, trait_name, span) in candidates {
                if self.symbols.user_traits.contains_key(trait_name) {
                    continue;
                }
                let Some((module, bundle)) = self.resolve_bundle_ref(trait_name) else {
                    continue;
                };
                let bindings = self
                    .symbols
                    .bundle_impls
                    .entry(target.to_string())
                    .or_default();
                // A duplicate binding of the same bundle is a coherence error (reported there);
                // don't double-record it, or method typing would see each method twice. This also
                // dedups a type that writes BOTH `@derive(vec.Kernels)` and `impl vec.Kernels for
                // T {}` — one `bundle_impls` entry, and `check_coherence` flags the duplicate.
                if !bindings
                    .iter()
                    .any(|b| b.module == module && b.bundle.name == bundle.name)
                {
                    bindings.push(BoundBundle {
                        module,
                        bundle,
                        span,
                    });
                }
            }
        }
    }

    /// Resolve a dotted trait path (`vec.Kernels`) to its registered kernel **trait** (ExtBundle→ExtTrait
    /// fold-in, slice 4): everything before the last dot is a bound module name (`use std.{vec}`), the
    /// last segment the trait. `None` when the module binding or the trait doesn't exist — the impl-site
    /// check reports.
    ///
    /// **Surface adapter, NOT a second mechanism.** The kernel traits were `ExtBundle`s until the
    /// fold-in; this resolves the *module-qualified spelling* (`vec.Kernels`) — the surface a bundle bind
    /// was written in — to the one native `ExtTrait`, which the checker then treats through the ordinary
    /// trait machinery (its `self_constraint`, `assoc_types`, `dispatch`). The returned `String` is the
    /// qualified module (`std.vec`), which equals the trait's `namespace` (so `find_trait_in_module`
    /// matches it) and, appended with the trait name, its `qualified()` runtime-dispatch identity.
    pub(crate) fn resolve_bundle_ref(
        &self,
        trait_name: &str,
    ) -> Option<(String, &'static noeta_ext_abi::ExtTrait)> {
        let (module_ref, trait_short) = trait_name.rsplit_once('.')?;
        let qualified = self.imports.modules.get(module_ref)?;
        let tr = self.reg().find_trait_in_module(qualified, trait_short)?;
        Some((qualified.clone(), tr))
    }

    /// Record which of a type's `fields` carry a default (`name: T = …`) — and so are **optional** in
    /// an attribute construction (object-model slice 6i). Used by the construction gate to omit a
    /// defaulted field without an E0009.
    ///
    /// The same walk records each default's **decode** classification (json-defaults) into
    /// `symbols.field_defaults`, so the one "this field declared a default" reading feeds both the
    /// construction gate and the JSON decode recipe. The two tables differ only where they must: a
    /// construction runs the default's thunk, so *any* default makes the field optional; a decode can
    /// only bake a literal, so a non-literal default is recorded as
    /// [`noeta_ext_abi::FieldDefault::Dynamic`] (see [`Checker::field_default_recipe`]).
    pub(crate) fn record_optional_fields(&mut self, type_name: &str, fields: &[FieldDecl]) {
        let optional: HashSet<String> = fields
            .iter()
            .filter(|f| f.default.is_some())
            .map(|f| f.name.clone())
            .collect();
        if !optional.is_empty() {
            self.symbols
                .attribute_optional_fields
                .insert(type_name.to_string(), optional);
        }
        let decode_defaults: HashMap<String, noeta_ext_abi::FieldDefault> = fields
            .iter()
            .map(|f| (f.name.clone(), Checker::field_default_recipe(f)))
            .filter(|(_, d)| *d != noeta_ext_abi::FieldDefault::Required)
            .collect();
        if !decode_defaults.is_empty() {
            self.symbols
                .field_defaults
                .insert(type_name.to_string(), decode_defaults);
        }
    }

    /// Whether field `field` of attribute `attr_name` is optional (has a default), so a `#[...]`
    /// construction may omit it.
    pub(crate) fn is_optional_attribute_field(&self, attr_name: &str, field: &str) -> bool {
        self.symbols
            .attribute_optional_fields
            .get(attr_name)
            .is_some_and(|set| set.contains(field))
    }

    /// Register a synthesized/fallback method's signature and instance classification for
    /// `type_name`, unless the type already has one by that name — the registration UT5 default
    /// fallback and derive bridging share (the body itself is materialized by the backends'
    /// hoist; the checker needs the signature so member calls resolve and type).
    fn register_synth_method(&mut self, type_name: &str, m: &noeta_ast::FnDecl) {
        let key = (type_name.to_string(), m.name.to_string());
        if self.symbols.methods.contains_key(&key) {
            return;
        }
        // Hoisted from a trait, whose methods declare no type parameters of their own (E0058) —
        // so there is nothing here for a scope to resolve.
        let scope = param_scope(&m.type_params, &self.imports.extern_types);
        let params: Vec<Type> = m
            .params
            .iter()
            .map(|p| param_type(p, &self.imports.extern_types, &scope))
            .collect();
        let ret = async_return(
            m.ret
                .as_ref()
                .map(|t| from_ref_q(t, &self.imports.extern_types, &scope))
                .unwrap_or(Type::Unknown),
            m.is_async,
        );
        // Every method reaching here comes from a trait: a hoisted UT5 default, or a `@derive`
        // plan's bridge/forward. So a self-less one is `Either` like any other trait method — an
        // omitted default is reachable as `T.m()` (the documented UT5 spelling) *and* on a value,
        // exactly as the same body written out in the `impl` block would be.
        self.symbols.method_receiver.insert(
            key.clone(),
            Receiver::trait_method(m.body.iter().any(|s| s.mentions("self"))),
        );
        self.symbols.methods.insert(
            key,
            FnSig {
                params,
                param_names: m.params.iter().map(|p| p.name.clone()).collect(),
                ret,
                required: required_params(&m.params),
                generic: None,
            },
        );
    }

    /// Record an impl's associated-type bindings (slice 1a): fold its `type Name = Concrete;` entries
    /// over the trait's defaulted associated types into `trait_assoc[(type, trait)]`. A non-user trait
    /// (or one declaring no associated types and receiving no bindings) records nothing.
    fn record_assoc_bindings(
        &mut self,
        type_name: &str,
        trait_name: &str,
        bindings: &[(String, TypeRef)],
    ) {
        let Some(decl) = self.symbols.user_traits.get(trait_name).cloned() else {
            return;
        };
        if decl.assoc_types.is_empty() && bindings.is_empty() {
            return;
        }
        let mut map: HashMap<String, Type> = HashMap::new();
        // Defaults first, so an explicit binding below overrides.
        for a in &decl.assoc_types {
            if let Some(default) = &a.default {
                map.insert(
                    a.name.clone(),
                    from_ref_q(default, &self.imports.extern_types, &ParamScope::new()),
                );
            }
        }
        for (name, ty) in bindings {
            map.insert(
                name.clone(),
                from_ref_q(ty, &self.imports.extern_types, &ParamScope::new()),
            );
        }
        self.symbols
            .trait_assoc
            .insert((type_name.to_string(), trait_name.to_string()), map);
    }

    /// Concrete-receiver projection bake (slice 1a): re-register each in-body impl block's methods with
    /// every `Self::Name` in their signatures replaced by the impl's binding for `Name`, so a call on a
    /// concrete receiver types against the implementor's associated type. Overwrites the flattened
    /// (unresolved) registration from the main method walk. A block with no bindings is skipped (there
    /// is nothing to resolve).
    fn bake_impl_assoc(&mut self, type_name: &str, impls: &[ImplBlock], type_params: &[TypeParam]) {
        for b in impls {
            if b.assoc_bindings.is_empty() {
                continue;
            }
            let map: HashMap<&str, &TypeRef> = b
                .assoc_bindings
                .iter()
                .map(|(n, t)| (n.as_str(), t))
                .collect();
            let provided = trait_supplies_instance_interface(b.trait_name.as_str());
            for m in &b.methods {
                let resolved = subst_self_assoc_in_fn(m, &map);
                self.collect_method_sig_classified(type_name, &resolved, type_params, provided);
            }
        }
    }

    /// Record that user type `name` satisfies each of `traits` (its `@derive`/`impl` names). Only
    /// real built-in trait names matter for bound enforcement; unknown ones are reported elsewhere
    /// and harmlessly recorded here.
    /// Record which built-in traits `name` acquired **via `@derive`** — the conditional subset
    /// (see [`Self::derived_traits`]). Called beside [`Self::record_trait_impls`] at the three
    /// declaration sites.
    pub(crate) fn record_derived(&mut self, name: &str, derives: &[DeriveSpec]) {
        let entry = self
            .symbols
            .derived_traits
            .entry(name.to_string())
            .or_default();
        for t in derives
            .iter()
            .filter_map(|d| BuiltinTrait::from_name(d.name.as_str()))
        {
            entry.insert(t);
        }
        // A `via:` derive (built-in or user trait) also records its delegation field, so the
        // instantiation-site conditional checks judge the via field rather than every field.
        for d in derives {
            if let Some((via, _)) = &d.via {
                self.symbols
                    .via_derives
                    .entry(name.to_string())
                    .or_default()
                    .push((d.name.to_string(), via.clone()));
            }
        }
    }

    /// Populate [`Symbols::native_trait_default_sites`] (ExtBundle→ExtTrait convergence, slice 2): for
    /// every `(type, trait)` where the trait is a **native** trait carrying a default-body dispatch,
    /// record each defaulted method the type does not itself provide — a native inherent method
    /// (resolved through `method_return`, covering every native kind) or an `impl` override body
    /// (gathered from the AST here) both count as provided and are excluded, so source (1) wins by
    /// construction. A `.noe` trait's local name is never in `imports.extern_types`, so it never enters
    /// this table — its defaults hoist (source 3) untouched.
    fn seed_native_trait_defaults(&mut self, program: &Program) {
        // The method names each type PROVIDES a real body for, from the AST — a type's own methods, its
        // in-body `impl` blocks, and standalone `impl`s targeting it. (A native type has no AST here;
        // its inherent methods are caught by the `method_return` probe below.)
        let mut ast_provided: HashSet<(String, String)> = HashSet::new();
        let mut note = |ty: &str, methods: &[FnDecl]| {
            for m in methods {
                ast_provided.insert((ty.to_string(), m.name.to_string()));
            }
        };
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(d) => {
                    note(d.name.as_str(), &d.methods);
                    for b in &d.impls {
                        note(d.name.as_str(), &b.methods);
                    }
                }
                Stmt::Class(d) => {
                    note(d.name.as_str(), &d.methods);
                    for b in &d.impls {
                        note(d.name.as_str(), &b.methods);
                    }
                }
                Stmt::Enum(d) => {
                    note(d.name.as_str(), &d.methods);
                    for b in &d.impls {
                        note(d.name.as_str(), &b.methods);
                    }
                }
                Stmt::Impl(d) => note(d.target.as_str(), &d.methods),
                _ => {}
            }
        }
        // Resolve the routes under the immutable registry/import borrows, then write them in.
        // Sorted for the same reason the UT5 fallback below is: a `(type, method)` reachable through
        // *two* native traits used to be routed to whichever the hash walk reached last — and the
        // route is the body that runs, so two traits whose defaults disagree (the shipped
        // `vec.Kernels`/`vec.SatKernels` pair differs by wrapping vs saturating arithmetic) would
        // compute different answers in different processes. Rival routes are a diagnostic now
        // (E0027, reported in pass 2), so this order settles nothing but reproducibility.
        let impls = self.symbols.user_trait_impls.clone();
        let mut sorted_impls: Vec<(String, Vec<String>)> = impls
            .iter()
            .map(|(ty, traits)| {
                let mut names: Vec<String> = traits.keys().cloned().collect();
                names.sort();
                (ty.clone(), names)
            })
            .collect();
        sorted_impls.sort();
        let mut routes: Vec<((String, String), (String, String))> = Vec::new();
        let mut routed_by: HashMap<(String, String), String> = HashMap::new();
        let mut conflicts: Vec<crate::TraitDefaultConflict> = Vec::new();
        for (type_name, traits) in &sorted_impls {
            for local in traits {
                // Native traits only (a `use`-imported extern-type alias); `.noe` traits resolve nothing.
                let Some(qualified) = self.imports.extern_types.get(local).cloned() else {
                    continue;
                };
                let Some(tr) = self.reg().find_trait_qualified(&qualified) else {
                    continue;
                };
                if tr.dispatch.is_none() {
                    continue;
                }
                for m in tr.methods.iter().filter(|m| m.has_default) {
                    let method = m.sig.name;
                    let provided = ast_provided.contains(&(type_name.clone(), method.to_string()))
                        || crate::stdlib::method_return(
                            self.reg(),
                            &Type::Named(type_name.clone(), Vec::new()),
                            method,
                        )
                        .is_some();
                    if provided {
                        continue;
                    }
                    let key = (type_name.clone(), method.to_string());
                    if let Some(first) = routed_by.get(&key) {
                        conflicts.push(crate::TraitDefaultConflict {
                            type_name: type_name.clone(),
                            method: method.to_string(),
                            traits: (first.clone(), local.clone()),
                        });
                        continue;
                    }
                    routed_by.insert(key.clone(), local.clone());
                    routes.push((key, (qualified.clone(), local.clone())));
                }
            }
        }
        for (key, route) in routes {
            self.symbols.native_trait_default_sites.insert(key, route);
        }
        self.symbols.trait_default_conflicts.extend(conflicts);
    }

    /// Record a type's declared `From` conversions (error-ergonomics): each in-body
    /// `impl From<Source>` block registers its resolved source type under the target, so a `?`
    /// site can look up `(source → target)` regardless of statement order. Arity/validity of the
    /// block is checked in pass 2 (`check_trait_impl`); a malformed block records nothing.
    pub(crate) fn record_from_impls(&mut self, target: &str, impls: &[noeta_ast::ImplBlock]) {
        for block in impls {
            if block.trait_name == BuiltinTrait::From.name() && block.trait_args.len() == 1 {
                let source = from_ref_q(
                    &block.trait_args[0],
                    &self.imports.extern_types,
                    &ParamScope::new(),
                );
                self.symbols
                    .from_impls
                    .entry(target.to_string())
                    .or_default()
                    .push(source);
            }
        }
    }

    pub(crate) fn record_trait_impls<'a>(
        &mut self,
        name: &str,
        traits: impl Iterator<Item = &'a str>,
    ) {
        let entry = self
            .symbols
            .trait_impls
            .entry(name.to_string())
            .or_default();
        // Map each name to its trait at the boundary; a non-built-in name (a typo, or an
        // `@attribute` record name) is dead data here — it could never satisfy a real bound —
        // so it is dropped rather than stored. Name validity is diagnosed on the `impl`/`@derive`
        // path (E0014), not here.
        for t in traits.filter_map(BuiltinTrait::from_name) {
            entry.insert(t);
        }
    }

    /// Register a struct's `@attribute` opt-in (P2.5). `kinds` is `None` for an ordinary struct and
    /// `Some(list)` when the struct is marked `@attribute`: the struct joins [`Self::attributes`]
    /// (usable in `#[...]` position), and any placement kinds (`@attribute(Method, …)`) are validated
    /// — each must be a fixed [`TargetKind`] (unknown → `E0030` at its span) — and recorded so each
    /// use site can be checked. A bare `@attribute` (empty list) is an attribute with no placement
    /// restriction.
    pub(crate) fn record_attribute(&mut self, name: &str, kinds: Option<&[(String, Span)]>) {
        let Some(kinds) = kinds else { return };
        self.symbols.attributes.insert(name.to_string());
        let mut recognized = Vec::new();
        for (kind_name, span) in kinds {
            match TargetKind::from_name(kind_name) {
                Some(kind) => recognized.push(kind),
                None => {
                    self.error(
                        DiagnosticCode::InvalidAttributeTarget,
                        *span,
                        format!("`{kind_name}` is not a valid attribute target kind"),
                    )
                    .help(format!(
                        "the target kinds are {}",
                        noeta_ast::reflect::ATTRIBUTE_TARGET_KINDS.join(", ")
                    ));
                }
            }
        }
        if !recognized.is_empty() {
            self.symbols.attachable.insert(name.to_string(), recognized);
        }
    }
}

/// Recursively hoist every **nested** `fn` declaration's name — a `Stmt::Fn` at any depth below
/// the top level, including inside closure and match-arm bodies. `top_level` is true only for the
/// program's direct statements (whose `fn`s are ordinary top-level functions, already in
/// [`Symbols::functions`]).
fn collect_nested_fn_names(stmt: &Stmt, top_level: bool, out: &mut HashSet<String>) {
    let body = |stmts: &[Stmt], out: &mut HashSet<String>| {
        for s in stmts {
            collect_nested_fn_names(s, false, out);
        }
    };
    let fn_decl = |decl: &FnDecl, is_nested: bool, out: &mut HashSet<String>| {
        if is_nested {
            out.insert(decl.name.to_string());
        }
        for s in &decl.body {
            collect_nested_fn_names(s, false, out);
        }
        for p in &decl.params {
            if let Some(d) = &p.default {
                collect_nested_fns_in_expr(d, out);
            }
        }
    };
    match stmt {
        Stmt::Fn(decl) => fn_decl(decl, !top_level, out),
        Stmt::Struct(d) => {
            d.methods.iter().for_each(|m| fn_decl(m, false, out));
            d.impls
                .iter()
                .flat_map(|b| &b.methods)
                .for_each(|m| fn_decl(m, false, out));
        }
        Stmt::Class(d) => {
            d.methods.iter().for_each(|m| fn_decl(m, false, out));
            d.impls
                .iter()
                .flat_map(|b| &b.methods)
                .for_each(|m| fn_decl(m, false, out));
            if let Some(dtor) = &d.destructor {
                body(dtor, out);
            }
        }
        Stmt::Enum(d) => {
            d.methods.iter().for_each(|m| fn_decl(m, false, out));
            d.impls
                .iter()
                .flat_map(|b| &b.methods)
                .for_each(|m| fn_decl(m, false, out));
        }
        Stmt::Impl(d) => d.methods.iter().for_each(|m| fn_decl(m, false, out)),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            collect_nested_fns_in_expr(cond, out);
            body(then_body, out);
            if let Some(b) = else_body {
                body(b, out);
            }
        }
        Stmt::For {
            iterable, body: b, ..
        } => {
            collect_nested_fns_in_expr(iterable, out);
            body(b, out);
        }
        Stmt::While { cond, body: b, .. } => {
            collect_nested_fns_in_expr(cond, out);
            body(b, out);
        }
        Stmt::Concurrent { body: b, .. } | Stmt::TierBlock { items: b, .. } => body(b, out),
        Stmt::Binding { value, .. }
        | Stmt::Destructure { value, .. }
        | Stmt::Echo { value, .. }
        | Stmt::Yield { value, .. }
        | Stmt::Expr { expr: value, .. } => collect_nested_fns_in_expr(value, out),
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                collect_nested_fns_in_expr(v, out);
            }
        }
        Stmt::Trait(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. } => {}
    }
}

/// Chase statement bodies hiding inside expressions (closure blocks, match-arm blocks) for nested
/// `fn`s. Containers recurse; leaves carry nothing.
fn collect_nested_fns_in_expr(e: &Expr, out: &mut HashSet<String>) {
    use noeta_ast::{ClosureBody, StrPart};
    let block = |stmts: &[Stmt], out: &mut HashSet<String>| {
        for s in stmts {
            collect_nested_fn_names(s, false, out);
        }
    };
    let closure_body = |body: &ClosureBody, out: &mut HashSet<String>| match body {
        ClosureBody::Expr(inner) => collect_nested_fns_in_expr(inner, out),
        ClosureBody::Block(stmts) => block(stmts, out),
    };
    match e {
        Expr::Closure { body, .. } => closure_body(body, out),
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_nested_fns_in_expr(scrutinee, out);
            for arm in arms {
                closure_body(&arm.body, out);
            }
        }
        Expr::Object(lit) => {
            for f in &lit.fields {
                collect_nested_fns_in_expr(&f.value, out);
            }
            if let Some(s) = &lit.spread {
                collect_nested_fns_in_expr(s, out);
            }
        }
        Expr::Unary { operand: inner, .. }
        | Expr::InstantiatedType { recv: inner, .. }
        | Expr::Member {
            receiver: inner, ..
        }
        | Expr::TupleIndex {
            receiver: inner, ..
        }
        | Expr::Try { expr: inner, .. }
        | Expr::Await { expr: inner, .. }
        | Expr::Spawn { future: inner, .. }
        | Expr::As { expr: inner, .. }
        | Expr::TypeTest { expr: inner, .. }
        | Expr::Channel {
            capacity: inner, ..
        } => collect_nested_fns_in_expr(inner, out),
        // One arm for the whole reflection surface. A turbofish operand is a type — no expression,
        // so no nested fn — which `for_each_expr` already knows; three of the thirteen used to sit
        // in the leaf group below, where a dynamic operand's nested fn would have been missed.
        Expr::Reflect { operand, .. } => {
            operand.for_each_expr(&mut |e| collect_nested_fns_in_expr(e, out));
        }
        Expr::Binary { lhs: a, rhs: b, .. }
        | Expr::Pipeline {
            left: a, right: b, ..
        }
        | Expr::Range {
            start: a, end: b, ..
        }
        | Expr::Index {
            receiver: a,
            index: b,
            ..
        }
        | Expr::Coalesce {
            value: a,
            fallback: b,
            ..
        }
        | Expr::FieldSet {
            receiver: a,
            value: b,
            ..
        } => {
            collect_nested_fns_in_expr(a, out);
            collect_nested_fns_in_expr(b, out);
        }
        Expr::Call { callee, args, .. } => {
            collect_nested_fns_in_expr(callee, out);
            noeta_ast::CallArg::values(args).for_each(|a| collect_nested_fns_in_expr(a, out));
        }
        Expr::TypedModuleCall { recv, args, .. } | Expr::TypedMethodCall { recv, args, .. } => {
            collect_nested_fns_in_expr(recv, out);
            noeta_ast::CallArg::values(args).for_each(|a| collect_nested_fns_in_expr(a, out));
        }
        // A turbofish call (`f::<T>(args)`) carries only a name and arguments — walk the args.
        Expr::TypedCall { args, .. } => {
            noeta_ast::CallArg::values(args).for_each(|a| collect_nested_fns_in_expr(a, out));
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => items
            .iter()
            .for_each(|i| collect_nested_fns_in_expr(i, out)),
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                collect_nested_fns_in_expr(k, out);
                collect_nested_fns_in_expr(v, out);
            }
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(inner) = part {
                    collect_nested_fns_in_expr(inner, out);
                }
            }
        }
        Expr::TierExpr { holes, .. } => holes
            .iter()
            .for_each(|h| collect_nested_fns_in_expr(h, out)),
        Expr::Ident { .. }
        | Expr::NativeFnRef { .. }
        | Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::IntN { .. }
        | Expr::Bool { .. } => {}
    }
}

/// A type's `@derive` arguments as bundle-binding candidates `(target, bundle-path, span)` — the
/// derive-form counterpart of a standalone `impl <module>.<Bundle> for T {}`. Only those whose name
/// actually resolves to a registered bundle are kept (by the caller); the rest are ordinary trait
/// derives handled elsewhere.
fn bundle_derive_candidates<'a>(
    name: &'a str,
    derives: &'a [DeriveSpec],
) -> Vec<(&'a str, &'a str, Span)> {
    derives
        .iter()
        .map(|d| (name, d.name.as_str(), d.span))
        .collect()
}

/// The checker's answers for the shared derive cascade: user traits from the symbol table (which
/// pass 1 filled from every `trait` in the linked program), native recipes from this check's
/// extension registry.
struct CheckerDeriveContext<'a> {
    user_traits: &'a HashMap<String, noeta_ast::TraitDecl>,
    registry: &'static noeta_ext_abi::registry::Registry,
}

impl noeta_ast::derive::DeriveContext for CheckerDeriveContext<'_> {
    fn user_trait(&self, name: &str) -> Option<noeta_ast::TraitDecl> {
        self.user_traits.get(name).cloned()
    }

    fn native_recipe(&self, name: &str) -> Option<Vec<(String, usize, String)>> {
        let ext = self.registry.find_ext_derive(name)?;
        Some(
            ext.methods
                .iter()
                .map(|m| (m.name.to_string(), m.arity, m.handler.to_string()))
                .collect(),
        )
    }
}

/// Rewrite every `Self::Name` projection inside a [`TypeRef`] to its bound concrete type (slice 1a),
/// recursing through composite types so `List<Self::Item>` / `?Self::Item` are covered. A projection
/// whose name has no binding is left as-is (it later degrades to `Type::Unknown` via `Type::from_ref`).
fn subst_self_assoc(ty: &TypeRef, bindings: &HashMap<&str, &TypeRef>) -> TypeRef {
    match ty {
        TypeRef::AssocProjection { name, .. } => bindings
            .get(name.as_str())
            .map(|t| (*t).clone())
            .unwrap_or_else(|| ty.clone()),
        TypeRef::Named { name, args, span } => TypeRef::Named {
            name: name.clone(),
            args: args.iter().map(|a| subst_self_assoc(a, bindings)).collect(),
            span: *span,
        },
        TypeRef::Optional { inner, span } => TypeRef::Optional {
            inner: Box::new(subst_self_assoc(inner, bindings)),
            span: *span,
        },
        TypeRef::Union { members, span } => TypeRef::Union {
            members: members
                .iter()
                .map(|m| subst_self_assoc(m, bindings))
                .collect(),
            span: *span,
        },
        TypeRef::Tuple { elements, span } => TypeRef::Tuple {
            elements: elements
                .iter()
                .map(|e| subst_self_assoc(e, bindings))
                .collect(),
            span: *span,
        },
        TypeRef::Fn { params, ret, span } => TypeRef::Fn {
            params: params
                .iter()
                .map(|p| subst_self_assoc(p, bindings))
                .collect(),
            ret: Box::new(subst_self_assoc(ret, bindings)),
            span: *span,
        },
        TypeRef::DynTrait { .. } => ty.clone(),
    }
}

/// A method signature with every `Self::Name` in its parameter and return annotations resolved to the
/// impl's binding (slice 1a). The body is untouched — projection is a typing concern, not a runtime one.
fn subst_self_assoc_in_fn(m: &FnDecl, bindings: &HashMap<&str, &TypeRef>) -> FnDecl {
    let mut out = m.clone();
    for p in &mut out.params {
        if let Some(ty) = &p.ty {
            p.ty = Some(subst_self_assoc(ty, bindings));
        }
    }
    if let Some(ret) = &out.ret {
        out.ret = Some(subst_self_assoc(ret, bindings));
    }
    out
}

/// Whether an `impl <trait_name>` block's methods belong to the trait's **instance interface** —
/// the question that decides whether a self-less one is [`Receiver::Either`] (reachable both ways)
/// or [`Receiver::Static`] (on the type only).
///
/// True for every user, native, and built-in trait *except* one whose contract declares its method
/// **`static`** ([`BuiltinTrait::declares_static`] — `From::from` builds a value rather than acting
/// on one, and the checker already refuses an implementation whose body mentions `self`). Deciding
/// it from the closed built-in set rather than from `symbols.user_traits` keeps it independent of
/// source order: an `impl` written above its `trait` must classify like one written below it, and
/// during this walk the trait table is only half-populated.
///
/// A **user** trait's `static` declaration deliberately does NOT feed this, so a user static method
/// stays `Receiver::Either` while `From`'s is `Receiver::Static`. Two reasons, and the asymmetry is
/// a KNOWN one rather than an oversight:
///
///   * The arc is non-breaking by construction. An unmarked self-less trait method is `Either`
///     today, so marking one `static` adds a promise (`T.m(…)` under a bound, and no `self` in any
///     implementation) without withdrawing the `x.m(…)` spelling that already worked.
///   * Reading `symbols.user_traits` here would reintroduce exactly the source-order dependence
///     this function is written to avoid: this runs in the FIRST collect walk, the same one that
///     registers `Stmt::Trait`, so a type declared above its trait would classify differently from
///     one declared below it.
///
/// Closing the gap — making a declared-`static` user method type-only, as `From`'s is — needs a
/// reclassification pass after `user_traits` is complete, not a lookup here. It is deliberately not
/// done: it would be the one part of this arc that changes what an existing spelling means.
fn trait_supplies_instance_interface(trait_name: &str) -> bool {
    BuiltinTrait::from_name(trait_name).is_none_or(|t| !t.declares_static())
}

/// Every method of every in-body `impl Trait { … }` block, paired with whether its trait supplies an
/// instance interface ([`trait_supplies_instance_interface`]) — the one place the struct, class, and
/// enum walks agree on how to classify a block's methods.
fn impl_block_methods(impls: &[ImplBlock]) -> impl Iterator<Item = (&FnDecl, bool)> {
    impls.iter().flat_map(|b| {
        let provided = trait_supplies_instance_interface(b.trait_name.as_str());
        b.methods.iter().map(move |m| (m, provided))
    })
}
