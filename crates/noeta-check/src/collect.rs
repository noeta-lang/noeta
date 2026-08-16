//! **Pass 0/1 — collection**: resolve every `use` import ([`Checker::collect_imports`]) and walk
//! the program registering every declaration into the symbol tables ([`Checker::collect`]),
//! plus the per-declaration recording helpers (optional fields, derives, trait impls, attribute
//! opt-ins). All `Checker` methods moved verbatim out of the crate root purely to shrink `lib.rs`.

use super::*;

/// One method a **trait's interface** supplied to a type: where its classification was recorded
/// ([`Symbols::method_receiver`] by name, [`Symbols::method_receiver_spans`] by declaration span)
/// and which trait supplied it.
///
/// Recorded at the one funnel every trait-supplied method passes through — an in-body
/// `impl Trait { … }` block, a standalone `impl Trait for T { … }`, a `@derive` bridge, a hoisted
/// default — and replayed by [`Checker::narrow_declared_static`] after the trait table is complete.
#[derive(Clone)]
pub(crate) struct TraitSuppliedMethod {
    /// The `(type, method)` key under which the classification was recorded.
    pub(crate) key: (String, String),
    /// The declaration's name span, the second key the same classification was recorded under.
    pub(crate) name_span: Span,
    /// The trait whose contract supplied this method — a user trait, a native trait seeded into
    /// `user_traits`, or a [`BuiltinTrait`].
    pub(crate) trait_name: String,
}

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
        // The spellings this program may write `#[Transient]` as — resolved once, from the shared
        // `noeta_ast` projection both backends resolve it with, so nothing downstream has to repeat
        // the import reasoning (or drift from it).
        self.transient_names
            .extend(noeta_ast::attribute_local_names(
                program,
                noeta_ast::reflect::JSON_ATTR_TRANSIENT,
            ));
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
        self.collect_method_sig_classified(type_name, m, type_params, None);
    }

    /// The **method-table key** a declaration occupies on its type: its declared name, except for
    /// one of several `From` conversions on a single type ([`Symbols::from_method_keys`]).
    ///
    /// Both walks that reach a conversion resolve it here — the type's flattened `methods` and the
    /// `impl` block itself hold the *same* `FnDecl`, so the span they share is what makes the two
    /// register one entry rather than two.
    fn method_key(&self, m: &FnDecl) -> String {
        self.symbols
            .from_method_keys
            .get(&m.name_span)
            .cloned()
            .unwrap_or_else(|| m.name.to_string())
    }

    /// Record a derived receiver discipline under **both** keys the two consumers ask by: the
    /// `(type, method)` name the checker resolves calls with, and the declaration's name span the
    /// IDE anchors its inlay hint to (see [`Symbols::method_receiver_spans`]). One writer, so the
    /// two indexes cannot disagree about a method — the classification is computed once, by the
    /// caller, and stored twice here rather than derived twice anywhere.
    ///
    /// A method whose `FnDecl` is *synthesized* (a hoisted trait default, a `@derive` bridge) writes
    /// the span it was cloned from, or a dummy one; either is harmless, because the span index is
    /// only ever read with a span taken from a parsed declaration.
    fn record_receiver(&mut self, key: (String, String), m: &FnDecl, receiver: Receiver) {
        self.symbols.method_receiver.insert(key, receiver);
        self.symbols
            .method_receiver_spans
            .insert(m.name_span, receiver);
    }

    /// Note that `trait_name`'s interface supplied `type_name.m` — the worklist entry
    /// [`Self::narrow_declared_static`] replays. Recorded beside every [`Self::record_receiver`]
    /// call that classifies a trait-supplied method, and nowhere else.
    fn note_trait_supplied(&mut self, type_name: &str, key: &str, m: &FnDecl, trait_name: &str) {
        self.symbols
            .trait_supplied_methods
            .push(TraitSuppliedMethod {
                key: (type_name.to_string(), key.to_string()),
                name_span: m.name_span,
                trait_name: trait_name.to_string(),
            });
    }

    /// [`Self::collect_method_sig`] with control over the receiver classification.
    ///
    /// `trait_provided` names the **trait whose interface** supplies this method — an `impl Trait`
    /// block's own method, in-body or standalone — and is `None` for an inherent one. It changes
    /// what a *self-less* body means: an inherent one is an associated function
    /// ([`Receiver::Static`], `T.m(…)` only), a trait's is reachable either way
    /// ([`Receiver::Either`]), because the trait's contract puts it in the instance interface —
    /// `dyn Trait` dispatches it on a value — while its body needs no receiver. A body that *does*
    /// read `self` is [`Receiver::Instance`] either way; the trait cannot conjure a receiver for it,
    /// and calling such a method as `T.m(…)` aborts at run time.
    ///
    /// The trait's name, rather than a bare "yes it does", is what lets
    /// [`Self::narrow_declared_static`] later ask the one further question this walk cannot answer
    /// yet: whether that trait declares *this* method **`static`**, which narrows the `Either` back
    /// to `Static`. See [`Symbols::trait_supplied_methods`] for why it has to be later.
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
        trait_provided: Option<&str>,
    ) {
        let uses_self = m.body.iter().any(|s| s.mentions("self"));
        let receiver = if trait_provided.is_some() {
            Receiver::trait_method(uses_self)
        } else {
            Receiver::inherent(uses_self)
        };
        // The key this method occupies in the type's table. It is the declared name for everything
        // except one of **several** `From` conversions on one type, which the parser flattened
        // under a shared `from` and which [`Symbols::from_method_keys`] tells apart by span. Read
        // once, here, so every index below — visibility, receiver discipline, the signature itself
        // — is written under the same key the call site resolves.
        let key = self.method_key(m);
        if let Some(trait_name) = trait_provided {
            self.note_trait_supplied(type_name, &key, m, trait_name);
        }
        // Method visibility, recorded at the ONE funnel every kind's methods pass through
        // (struct/class/enum inherent bodies, in-body `impl` blocks, standalone `impl`s) — so the
        // rule cannot be spelled once per kind and drift. A method is private unless declared
        // `pub`; a TRAIT-supplied one is public by construction (`trait_provided`), because the
        // trait's contract is what puts it on the outward surface.
        if trait_provided.is_none() && !m.is_public {
            self.symbols
                .private_methods
                .entry(type_name.to_string())
                .or_default()
                .insert(key.clone(), Some(m.name_span));
        } else {
            // A later registration WINS over an earlier one for the same key (an `impl` method
            // over an inherent of the same name), so a public one must clear a private entry
            // rather than leave it standing.
            if let Some(set) = self.symbols.private_methods.get_mut(type_name) {
                set.remove(key.as_str());
            }
        }
        self.record_receiver((type_name.to_string(), key.clone()), m, receiver);
        let xt = &self.imports.extern_types;
        // The type's parameters, then the method's own LAYERED OVER them: a method `<T>` inside a
        // class `<T>` shadows, so an annotation in this signature resolves to the METHOD's `T`.
        // Both remain in `generic.params` below — they are different parameters with different
        // identities, and each is seeded from its own channel (the receiver's type arguments for
        // the class's, the turbofish/arguments for the method's).
        // `Self` in a signature names the declaring type, and it is resolved HERE rather than at
        // the body-checking pass: a recorded signature is what every call site, `dyn` dispatch and
        // reflection reads, so a `Self` left standing in one would meet the concrete argument at
        // the call and mismatch it.
        let type_scope = type_body_scope(type_name, type_params, xt);
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
            (type_name.to_string(), key),
            FnSig {
                params,
                param_names: m.params.iter().map(|p| p.name.clone()).collect(),
                ret,
                required: required_params(&m.params),
                generic,
            },
        );
    }

    /// The scope a type's own `impl` arguments and annotations resolve in: its generic parameters,
    /// and `Self` bound to the type at its own instantiation.
    ///
    /// Read from the registered parameters rather than from the declaration in hand, so the
    /// standalone and in-body `impl` spellings resolve their arguments identically — the pair that
    /// otherwise records `Self` for one and the target for the other. A target this program does
    /// not declare has no parameters registered and resolves to itself, which is what the E0013
    /// reported elsewhere is about.
    fn target_scope(&self, target: &str) -> TypeScope {
        let params = self
            .symbols
            .type_params
            .get(target)
            .cloned()
            .unwrap_or_default();
        type_body_scope(target, &params, &self.imports.extern_types)
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
            // `Self` is the word for "the type this declaration is", so nothing may *be* a type by
            // that name: inside any type body the spelling already resolves to the enclosing type,
            // and a declared `Self` would mean one thing there and another at top level — one name
            // with two meanings, decided by where it is written.
            if local == SELF_TYPE {
                self.error(
                    DiagnosticCode::NameCollision,
                    span,
                    format!("`{SELF_TYPE}` cannot be declared as {what}"),
                )
                .help(
                    "`Self` names the enclosing type inside any `struct`/`class`/`enum`/`trait` \
                     body — pick another name for this declaration",
                );
            }
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
                    // is a `Type::Param` a later instantiation substitutes by identity — and
                    // against its `Self`, so a self-referential field (`next: ?Self`) records the
                    // declaring type rather than a nominal name nothing resolves.
                    let scope = type_body_scope(
                        r.name.as_str(),
                        &r.type_params,
                        &self.imports.extern_types,
                    );
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
                    for (m, from_trait) in impl_block_methods(&r.impls) {
                        self.collect_method_sig_classified(
                            r.name.as_str(),
                            m,
                            &r.type_params,
                            Some(from_trait),
                        );
                    }
                    self.bake_impl_assoc(r.name.as_str(), &r.impls, &r.type_params);
                }
                Stmt::Class(c) => {
                    // Field types resolve against the type's OWN parameters and its `Self`, exactly
                    // as a struct's do.
                    let scope = type_body_scope(
                        c.name.as_str(),
                        &c.type_params,
                        &self.imports.extern_types,
                    );
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
                    let private: HashMap<String, DeclSite> = c
                        .fields
                        .iter()
                        .filter(|f| !f.is_public)
                        .map(|f| (f.name.clone(), Some(f.name_span)))
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
                    // A class's field defaults and transient markers, exactly as a struct's — the
                    // same call, because they answer the same questions for both kinds. It ran only
                    // for structs while only a struct could decode, so a class's literal default
                    // never reached its recipe and an omitted defaulted field decoded as a missing
                    // one; that stopped being invisible the moment a class gained a decode.
                    self.record_optional_fields(c.name.as_str(), &c.fields);
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
                    for (m, from_trait) in impl_block_methods(&c.impls) {
                        self.collect_method_sig_classified(
                            c.name.as_str(),
                            m,
                            &c.type_params,
                            Some(from_trait),
                        );
                    }
                    self.bake_impl_assoc(c.name.as_str(), &c.impls, &c.type_params);
                }
                Stmt::Enum(e) => {
                    // As for a struct's fields: a payload naming the enum's `T` is a parameter, and
                    // one naming `Self` is the enum itself (`Cons(int, Self)`).
                    let scope = type_body_scope(
                        e.name.as_str(),
                        &e.type_params,
                        &self.imports.extern_types,
                    );
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
                    for (m, from_trait) in impl_block_methods(&e.impls) {
                        self.collect_method_sig_classified(
                            e.name.as_str(),
                            m,
                            &e.type_params,
                            Some(from_trait),
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
                        .push((
                            crate::traits::coherence_key(
                                decl.trait_name.as_str(),
                                &decl.trait_args,
                            ),
                            decl.trait_span,
                        ));
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
                        // The arguments are written in the TARGET's scope, so `impl Keyed<Self>
                        // for P` records `P` — the same thing the in-body spelling records, which
                        // is what stops a bound (`<T: Keyed<P>>`) from matching one and not the
                        // other.
                        let scope = self.target_scope(decl.target.as_str());
                        let args: Vec<Type> = decl
                            .trait_args
                            .iter()
                            .map(|t| from_ref_q(t, &self.imports.extern_types, &scope))
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
                    // In the type's own scope, exactly as a standalone impl's arguments are.
                    let scope = self.target_scope(type_name);
                    let args: Vec<Type> = trait_args
                        .iter()
                        .map(|t| from_ref_q(t, &self.imports.extern_types, &scope))
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
            // Each plan is paired with the trait it derives, because a synthesized method is a
            // TRAIT's method however it was spelled — the receiver rule asks the same question of
            // it as of a written-out `impl` block's.
            let plans: Vec<(String, Vec<noeta_ast::FnDecl>)> = derives
                .iter()
                .filter_map(|spec| {
                    noeta_ast::derive::plan_derive(&ctx, spec, type_name, fields, methods)
                        // A plan error is ignored here; `check_derives` reports it.
                        .and_then(|planned| planned.ok())
                        .map(|ms| (spec.name.to_string(), ms))
                })
                .collect();
            let type_name = type_name.to_string();
            // A native derive whose forward is `json.stringify(self)` — `@derive(Inspect)` — makes
            // the method a JSON door over the receiver. Recorded here, where the plan and the
            // registry's recipe are both in hand, and read at the *call* site: a synthesized body is
            // registered for its signature and never checked, so the hint cannot come from inside
            // it. A hand-written method of the same name wins (`register_synth_method` skips it),
            // so it is excluded here too.
            for (trait_name, _) in &plans {
                let Some(recipe) = self.reg().find_ext_derive(trait_name) else {
                    continue;
                };
                for m in recipe.methods {
                    if m.handler == noeta_ext_abi::JSON_STRINGIFY_HANDLER
                        && m.arity == 0
                        && !methods.iter().any(|own| own.name == m.name)
                    {
                        self.symbols
                            .json_forward_methods
                            .insert((type_name.clone(), m.name.to_string()));
                    }
                }
            }
            for (trait_name, ms) in &plans {
                for m in ms {
                    self.register_synth_method(&type_name, trait_name, m);
                }
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
            let from_trait = Some(d.trait_name.as_str());
            for m in &d.methods {
                if assoc.is_empty() {
                    self.collect_method_sig_classified(
                        d.target.as_str(),
                        m,
                        &type_params,
                        from_trait,
                    );
                } else {
                    let resolved = subst_self_assoc_in_fn(m, &assoc);
                    self.collect_method_sig_classified(
                        d.target.as_str(),
                        &resolved,
                        &type_params,
                        from_trait,
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
                        self.register_synth_method(type_name, trait_name, &tm.sig);
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
                    self.register_synth_method(&type_name, &trait_name, &tm.sig);
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
        // LAST — every trait is registered by now, which is the whole reason this is a pass and not
        // a lookup at the classification site.
        self.narrow_declared_static();
    }

    /// Narrow every **declared-`static`** trait method from [`Receiver::Either`] to
    /// [`Receiver::Static`], so `static` means one thing rather than two.
    ///
    /// A trait's `static fn m(…)` is a term of its contract: no implementation binds `self`. On a
    /// **concrete type** that makes `x.m(…)` exactly the shape E0047 exists for — `Thing.m(…)` and
    /// `x.m(…)` reach the same prototype, so the receiver really is evaluated and then discarded,
    /// which is the language's own stated reason for an inherent static function being type-only.
    /// The declaration says the receiver is not part of the method's meaning; the call spelling
    /// should not go on implying it is.
    ///
    /// **`dyn Trait` is untouched, and that is not an inconsistency.** This table is keyed by a
    /// *concrete type name*; a `dyn Trait` receiver is [`Type::DynTrait`] and resolves against the
    /// trait's declaration (`Checker::dyn_trait_method`), never through here. It has to: a trait
    /// object has no type name to call the method on, so the receiver is not a discarded value there
    /// — it is the only thing selecting which implementation runs. `Receiver::Either`'s own doc names
    /// the instance interface as how `dyn Trait` dispatches, and that path keeps working; the
    /// receiver is a dispatch token, never bound to `self`.
    ///
    /// **Scoped to a DECLARED `static`**, which is what makes it non-breaking rather than merely
    /// justified: the modifier is new syntax, so no program predating it has one. A merely
    /// self-less trait method — the far larger set — stays `Either` exactly as before, and nothing
    /// that checked yesterday stops checking. The built-in `From` flows through this same rule
    /// (`BuiltinTrait::declares_static`) rather than through a classification special case, so its
    /// long-standing type-only `from` is now an instance of the general law instead of the exception
    /// the general law was written around.
    ///
    /// Two guards, both about not overwriting someone else's answer:
    ///
    ///   * only an entry still reading `Either` is narrowed. `Either` is *only* ever written for a
    ///     self-less trait-supplied method, so seeing it is proof this worklist entry is still the
    ///     one standing — an inherent method that shadowed it (`Instance`/`Static`), or an
    ///     implementation whose body reads `self` in violation of the contract (`Instance`, E0015),
    ///     keeps the classification it earned.
    ///   * the span index is guarded independently. It keys *declarations*, so it holds entries the
    ///     name index no longer does, and the two must not fall out of agreement about the ones they
    ///     share.
    fn narrow_declared_static(&mut self) {
        let narrow: Vec<((String, String), Span)> = self
            .symbols
            .trait_supplied_methods
            .iter()
            .filter(|e| self.trait_declares_static(&e.trait_name, &e.key.1))
            .map(|e| (e.key.clone(), e.name_span))
            .collect();
        for (key, name_span) in narrow {
            if let Some(r) = self.symbols.method_receiver.get_mut(&key)
                && *r == Receiver::Either
            {
                *r = Receiver::Static;
            }
            if let Some(r) = self.symbols.method_receiver_spans.get_mut(&name_span)
                && *r == Receiver::Either
            {
                *r = Receiver::Static;
            }
        }
    }

    /// Whether `trait_name`'s contract declares `method` **`static`** — the one question the
    /// receiver rule asks of a trait, answered identically for all three kinds of trait.
    ///
    /// A user trait and a *native* trait share the answer by construction: a native trait is seeded
    /// into `symbols.user_traits` as a synthesized [`noeta_ast::TraitDecl`] whose
    /// [`noeta_ast::FnDecl::is_static`] carries its declared receiver-ness, so one lookup covers
    /// both. A [`BuiltinTrait`] has no `TraitDecl` at all and answers from the closed table
    /// ([`BuiltinTrait::declares_static`]), matched against the method it requires so a same-named
    /// method of a *different* protocol is never caught by it.
    /// The trait whose contract declared `type_name.method` **`static`** — its name, and the span
    /// of the `static fn m(…)` line that said so where there is one (a [`BuiltinTrait`] has no
    /// source to point at). `None` when the method is type-only for the ordinary reason: it is an
    /// inherent function whose own body binds no `self`.
    ///
    /// What E0047 names, so the reader is sent to the *declaration* that made the method type-only
    /// rather than left to wonder why an implementation with no `self` in it refuses a receiver.
    pub(crate) fn static_declaring_trait(
        &self,
        type_name: &str,
        method: &str,
    ) -> Option<(String, Option<Span>)> {
        let trait_name = self
            .symbols
            .trait_supplied_methods
            .iter()
            .find(|e| {
                e.key.0 == type_name
                    && e.key.1 == method
                    && self.trait_declares_static(&e.trait_name, method)
            })
            .map(|e| e.trait_name.clone())?;
        let at = self
            .symbols
            .user_traits
            .get(&trait_name)
            .and_then(|d| d.methods.iter().find(|tm| tm.sig.name.as_str() == method))
            .map(|tm| tm.sig.name_span);
        Some((trait_name, at))
    }

    fn trait_declares_static(&self, trait_name: &str, method: &str) -> bool {
        if let Some(decl) = self.symbols.user_traits.get(trait_name) {
            return decl
                .methods
                .iter()
                .any(|tm| tm.sig.name.as_str() == method && tm.sig.is_static);
        }
        BuiltinTrait::from_name(trait_name).is_some_and(|t| {
            t.declares_static() && t.required_method().is_some_and(|(name, _)| name == method)
        })
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
        // And which fields left the serialized shape (`#[std.json.Transient]`), resolved through the
        // program's own imports — the same set the backends resolve, so the checker's rules and the
        // shapes they build cannot disagree about which fields are transient.
        let transient: HashSet<String> = fields
            .iter()
            .filter(|f| noeta_ast::has_attribute(&f.attrs, &self.transient_names))
            .map(|f| f.name.clone())
            .collect();
        if !transient.is_empty() {
            self.symbols
                .transient_fields
                .insert(type_name.to_string(), transient);
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
    ///
    /// `trait_name` is the trait the synthesized method stands in for — every caller has one, since
    /// a synthesized method is by definition a trait's doing — and it goes on the same worklist an
    /// `impl` block's method does, so a hoisted default of a `static fn` narrows exactly like a
    /// written-out one.
    fn register_synth_method(&mut self, type_name: &str, trait_name: &str, m: &noeta_ast::FnDecl) {
        let key = (type_name.to_string(), m.name.to_string());
        if self.symbols.methods.contains_key(&key) {
            return;
        }
        // Hoisted from a trait, whose methods declare no type parameters of their own (E0058) —
        // so the layering below adds nothing. What the scope IS for is `Self`: a trait's
        // `fn me(): Self` hoisted onto an implementor returns that implementor, and recording the
        // literal word instead would make the signature uncallable at every one of them.
        let scope = extend_param_scope(
            &self.target_scope(type_name),
            &m.type_params,
            &self.imports.extern_types,
        );
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
        // exactly as the same body written out in the `impl` block would be, and narrowed back to
        // `Static` by the same pass if the trait declared it so.
        self.record_receiver(
            key.clone(),
            m,
            Receiver::trait_method(m.body.iter().any(|s| s.mentions("self"))),
        );
        self.note_trait_supplied(type_name, &key.1, m, trait_name);
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
        // Bound in the implementing type's scope, so `type Item = Self;` names that type.
        let scope = self.target_scope(type_name);
        // Defaults first, so an explicit binding below overrides.
        for a in &decl.assoc_types {
            if let Some(default) = &a.default {
                map.insert(
                    a.name.clone(),
                    from_ref_q(default, &self.imports.extern_types, &scope),
                );
            }
        }
        for (name, ty) in bindings {
            map.insert(
                name.clone(),
                from_ref_q(ty, &self.imports.extern_types, &scope),
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
            for m in &b.methods {
                let resolved = subst_self_assoc_in_fn(m, &map);
                self.collect_method_sig_classified(
                    type_name,
                    &resolved,
                    type_params,
                    Some(b.trait_name.as_str()),
                );
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
    /// `impl From<Source>` block registers its resolved source type — and the method-table key its
    /// `from` occupies — under the target, so a `?` site can look up `(source → target)` regardless
    /// of statement order and then dispatch through the right one. Arity/validity of the block is
    /// checked in pass 2 (`check_trait_impl`); a malformed block records nothing.
    pub(crate) fn record_from_impls(&mut self, target: &str, impls: &[noeta_ast::ImplBlock]) {
        let keys = noeta_ast::conversion::from_conversion_keys(impls);
        self.symbols.from_method_keys.extend(
            keys.iter()
                .map(|(span, key)| (*span, key.clone()))
                .collect::<Vec<_>>(),
        );
        for block in impls {
            if block.trait_name == BuiltinTrait::From.name() && block.trait_args.len() == 1 {
                // In the target's scope, like every other in-body `impl` argument.
                let source = from_ref_q(
                    &block.trait_args[0],
                    &self.imports.extern_types,
                    &self.target_scope(target),
                );
                let Some(m) = block
                    .methods
                    .iter()
                    .find(|m| Some(m.name.as_str()) == BuiltinTrait::From.required_method_name())
                else {
                    continue;
                };
                let method = keys
                    .get(&m.name_span)
                    .cloned()
                    .unwrap_or_else(|| m.name.to_string());
                self.symbols
                    .from_impls
                    .entry(target.to_string())
                    .or_default()
                    .push(FromConversion { source, method });
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

/// Every method of every in-body `impl Trait { … }` block, paired with the **trait** that supplies
/// it — the one place the struct, class, and enum walks agree on how to classify a block's methods.
fn impl_block_methods(impls: &[ImplBlock]) -> impl Iterator<Item = (&FnDecl, &str)> {
    impls.iter().flat_map(|b| {
        let trait_name = b.trait_name.as_str();
        b.methods.iter().map(move |m| (m, trait_name))
    })
}
