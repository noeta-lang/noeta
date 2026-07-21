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
    pub(crate) fn collect_imports(&mut self, program: &Program) {
        use noeta_ext_abi::registry::UseKind;
        for stmt in &program.stmts {
            let Stmt::Use { path, names, .. } = stmt else {
                continue;
            };
            for name in names {
                let local = name.local().to_string();
                // One shared classifier decides what every `use` target binds — so the checker, the
                // compiler, and the eval reference never diverge on whether a name is a module, a
                // namespace group, a member function, a type, or an error (the check/run divergence
                // this closes). `UnknownUnderRoot` stays lenient in this slice (except the existing
                // member-function-miss diagnostic); slice 2 tightens it to a hard E0019.
                match self.reg().classify_use(path, &name.name) {
                    UseKind::Module(qualified) => {
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
    fn collect_method_sig(
        &mut self,
        type_name: &str,
        m: &FnDecl,
        type_tps: &HashSet<String>,
        type_generics: &[(String, Vec<BoundReq>)],
    ) {
        self.symbols.method_instance.insert(
            (type_name.to_string(), m.name.clone()),
            m.body.iter().any(|s| s.mentions("self")),
        );
        let own_generics: Vec<(String, Vec<BoundReq>)> = m
            .type_params
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    bound_reqs(&p.bounds, &self.imports.extern_types),
                )
            })
            .collect();
        let mut tps = type_tps.clone();
        tps.extend(m.type_params.iter().map(|p| p.name.clone()));
        let raw_params: Vec<Type> = m
            .params
            .iter()
            .map(|p| param_type(p, &self.imports.extern_types))
            .collect();
        let raw_ret = async_return(
            m.ret
                .as_ref()
                .map(|t| from_ref_q(t, &self.imports.extern_types))
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
                params: type_generics.iter().cloned().chain(own_generics).collect(),
                class_params: type_generics.len(),
                raw_params,
                raw_ret,
            });
        self.symbols.methods.insert(
            (type_name.to_string(), m.name.clone()),
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
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(r) => {
                    let fields = r
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                field_type(&f.ty, &self.imports.extern_types),
                            )
                        })
                        .collect();
                    self.symbols.records.insert(r.name.clone(), fields);
                    if let Some(directive) = &r.decorators.packed {
                        self.symbols.packed_structs.insert(r.name.clone());
                        if directive.layout == noeta_ast::PackedLayout::Column {
                            self.symbols.column_structs.insert(r.name.clone());
                        }
                    }
                    if r.decorators.validated.is_some() {
                        self.symbols.validated_types.insert(r.name.clone());
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
                        self.symbols.mut_fields.insert(r.name.clone(), muts);
                    }
                    self.symbols.types.insert(r.name.clone());
                    self.symbols
                        .type_kinds
                        .insert(r.name.clone(), noeta_types::TypeKind::Struct);
                    self.record_optional_fields(&r.name, &r.fields);
                    // A struct satisfies a trait it `@derive`s or in-body `impl`s — the same
                    // chain a class/enum records. (The impls half was missing here: a struct's
                    // `impl Comparable` never registered, so bounds falsely rejected it.)
                    self.record_trait_impls(
                        &r.name,
                        r.decorators
                            .derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(r.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.record_derived(&r.name, &r.decorators.derives);
                    self.record_from_impls(&r.name, &r.impls);
                    self.record_attribute(&r.name, r.decorators.attribute.as_deref());
                    self.symbols.generic_types.insert(
                        r.name.clone(),
                        r.type_params.iter().map(|p| p.name.clone()).collect(),
                    );
                    // The same parameters WITH bounds, for checking a standalone `impl`'s bodies.
                    self.symbols
                        .type_params
                        .insert(r.name.clone(), r.type_params.clone());
                    // Record each struct method's signature + instance classification, exactly as
                    // for a class (this closed a long-standing gap: struct associated calls —
                    // `B.new(1)` — previously typed as a hole because struct methods were never
                    // registered; prelude-redesign EX.2 needs the classification for all kinds).
                    let tps: HashSet<String> =
                        r.type_params.iter().map(|p| p.name.clone()).collect();
                    let struct_generics: Vec<(String, Vec<BoundReq>)> = r
                        .type_params
                        .iter()
                        .map(|p| {
                            (
                                p.name.clone(),
                                bound_reqs(&p.bounds, &self.imports.extern_types),
                            )
                        })
                        .collect();
                    let methods: Vec<&FnDecl> = r
                        .methods
                        .iter()
                        .chain(r.impls.iter().flat_map(|b| b.methods.iter()))
                        .collect();
                    for m in methods {
                        self.collect_method_sig(&r.name, m, &tps, &struct_generics);
                    }
                }
                Stmt::Class(c) => {
                    let fields = c
                        .fields
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                field_type(&f.ty, &self.imports.extern_types),
                            )
                        })
                        .collect();
                    self.symbols.records.insert(c.name.clone(), fields);
                    if c.decorators.validated.is_some() {
                        self.symbols.validated_types.insert(c.name.clone());
                    }
                    let muts: HashSet<String> = c
                        .fields
                        .iter()
                        .filter(|f| f.mut_field)
                        .map(|f| f.name.clone())
                        .collect();
                    if !muts.is_empty() {
                        self.symbols.mut_fields.insert(c.name.clone(), muts);
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
                        self.symbols.private_fields.insert(c.name.clone(), private);
                    }
                    self.symbols.types.insert(c.name.clone());
                    self.symbols
                        .type_kinds
                        .insert(c.name.clone(), noeta_types::TypeKind::Class);
                    // A class with a `destruct { ... }` block seeds destruct-reachability (Phase 3.2b).
                    if c.destructor.is_some() {
                        self.symbols.destructor_classes.insert(c.name.clone());
                    }
                    // A class satisfies a trait it `@derive`s or `impl`s; record both for bound
                    // enforcement (the `impl`/`derive` *names* are validated elsewhere).
                    self.record_trait_impls(
                        &c.name,
                        c.decorators
                            .derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(c.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.record_derived(&c.name, &c.decorators.derives);
                    self.record_from_impls(&c.name, &c.impls);
                    // Record each method's signature (class methods and impl-block methods alike),
                    // so `obj.method(...)` resolves to a concrete type and its arguments are
                    // checked. The class's generic parameters are erased to `dyn` (erased at
                    // runtime, they accept any argument).
                    let tps: HashSet<String> =
                        c.type_params.iter().map(|p| p.name.clone()).collect();
                    // A generic class's type parameters + bounds, shared by every method's
                    // `GenericInfo` so a call instantiates the class's `T` from the arguments and
                    // enforces its bounds (S4.3b) — the class-level mirror of a generic function.
                    let class_generics: Vec<(String, Vec<BoundReq>)> = c
                        .type_params
                        .iter()
                        .map(|p| {
                            (
                                p.name.clone(),
                                bound_reqs(&p.bounds, &self.imports.extern_types),
                            )
                        })
                        .collect();
                    self.symbols.generic_types.insert(
                        c.name.clone(),
                        c.type_params.iter().map(|p| p.name.clone()).collect(),
                    );
                    // The same parameters WITH bounds, for checking a standalone `impl`'s bodies.
                    self.symbols
                        .type_params
                        .insert(c.name.clone(), c.type_params.clone());
                    let methods: Vec<&FnDecl> = c
                        .methods
                        .iter()
                        .chain(c.impls.iter().flat_map(|b| b.methods.iter()))
                        .collect();
                    for m in methods {
                        self.collect_method_sig(&c.name, m, &tps, &class_generics);
                    }
                }
                Stmt::Enum(e) => {
                    let variants = e
                        .variants
                        .iter()
                        .map(|v| VariantInfo {
                            name: v.name.clone(),
                            // A variant's **accurate** payload types (via `variant_field_type`, R2b),
                            // exactly as a struct's field types live in `self.symbols.records`: one source of
                            // truth for enum-construction type-argument inference **and** the `Send`
                            // classifier **and** destructor-relevance. (Previously `field_type(&p.ty)`,
                            // which is `Unknown` for a positional payload whose type parses into the
                            // `Param`'s *name* — an `Unknown` that silently classified an enum wrapping
                            // a `class` as `Send`, unlike the equivalent struct.)
                            fields: v
                                .fields
                                .iter()
                                .map(|v| variant_field_type(v, &self.imports.extern_types))
                                .collect(),
                        })
                        .collect();
                    self.symbols.enums.insert(e.name.clone(), variants);
                    self.symbols.types.insert(e.name.clone());
                    self.symbols
                        .type_kinds
                        .insert(e.name.clone(), noeta_types::TypeKind::Enum);
                    // `@semantic` makes the enum role-eligible (its fieldless variants may be named
                    // by `@role(Enum.Variant)`); recorded for the post-collect role-validation pass.
                    if e.decorators.semantic.is_some() {
                        self.symbols.semantic_enums.insert(e.name.clone());
                    }
                    // An enum satisfies a trait it `@derive`s or `impl`s (its in-body blocks are
                    // uniform with a class's — object-model slice 3); record both so an operator
                    // trait (`impl Add`, `impl Comparable`, …) is accepted on an enum operand.
                    self.record_trait_impls(
                        &e.name,
                        e.decorators
                            .derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(e.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.record_derived(&e.name, &e.decorators.derives);
                    self.record_from_impls(&e.name, &e.impls);
                    self.symbols.generic_types.insert(
                        e.name.clone(),
                        e.type_params.iter().map(|p| p.name.clone()).collect(),
                    );
                    // The same parameters WITH bounds, for checking a standalone `impl`'s bodies.
                    self.symbols
                        .type_params
                        .insert(e.name.clone(), e.type_params.clone());
                    // Record each enum method's signature (inherent + impl-block, the unified body —
                    // object-model slice 3) under `(Enum, method)`, exactly like a class's, so an
                    // instance call `status.label()` and an associated call `Status.parse(s)` resolve
                    // to a concrete type. The enum's generic parameters are erased to `dyn`.
                    let tps: HashSet<String> =
                        e.type_params.iter().map(|p| p.name.clone()).collect();
                    let enum_generics: Vec<(String, Vec<BoundReq>)> = e
                        .type_params
                        .iter()
                        .map(|p| {
                            (
                                p.name.clone(),
                                bound_reqs(&p.bounds, &self.imports.extern_types),
                            )
                        })
                        .collect();
                    for m in &e.methods {
                        self.collect_method_sig(&e.name, m, &tps, &enum_generics);
                    }
                }
                Stmt::Fn(f) => {
                    // The registered signature is **erased** (generic parameters → `dyn`): the
                    // arity check and the non-generic fast path use it. A generic function also
                    // carries un-erased `GenericInfo` so a call site can instantiate it precisely
                    // and enforce its bounds (S4.2); a non-generic function carries `None`.
                    let tps: HashSet<String> =
                        f.type_params.iter().map(|p| p.name.clone()).collect();
                    let raw_params: Vec<Type> = f
                        .params
                        .iter()
                        .map(|p| param_type(p, &self.imports.extern_types))
                        .collect();
                    // An `async fn f(): T` call produces `Future<T>` (Track A); wrap before erasure so
                    // the erased signature and the generic instantiation both carry the future.
                    let raw_ret = async_return(
                        f.ret
                            .as_ref()
                            .map(|t| from_ref_q(t, &self.imports.extern_types))
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
                            .map(|p| {
                                (
                                    p.name.clone(),
                                    bound_reqs(&p.bounds, &self.imports.extern_types),
                                )
                            })
                            .collect(),
                        class_params: 0,
                        raw_params,
                        raw_ret,
                    });
                    self.symbols.functions.insert(
                        f.name.clone(),
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
                        &decl.target,
                        std::iter::once(decl.trait_name.as_str()),
                    );
                    self.symbols
                        .standalone_impls
                        .entry(decl.target.clone())
                        .or_default()
                        .push((decl.trait_name.clone(), decl.trait_span));
                }
                // A user-defined trait (L1) is registered up front so forward references (an `impl`
                // or `<T: Trait>` bound textually above the `trait`) resolve. A duplicate declaration
                // keeps the first; pass 2 (`check_trait_decl`) reports the collision.
                Stmt::Trait(t) => {
                    self.symbols
                        .user_traits
                        .entry(t.name.clone())
                        .or_insert_with(|| t.clone());
                }
                _ => {}
            }
        }
        // Record which user traits each type implements (L1, UT2), from standalone `impl`s,
        // in-body `impl`s, and `@derive(UserTrait)` (a fully-defaulted trait adopted wholesale —
        // `check_derives` enforces the fully-defaulted part). Done after the main walk so every
        // `trait` is registered regardless of source order. The basis for UT3 bound satisfaction
        // and UT4 `dyn Trait` coercion.
        for stmt in &program.stmts {
            let (type_name, impls, derives): (&str, &[noeta_ast::ImplBlock], &[DeriveSpec]) =
                match stmt {
                    Stmt::Impl(decl) if self.symbols.user_traits.contains_key(&decl.trait_name) => {
                        let args: Vec<Type> = decl
                            .trait_args
                            .iter()
                            .map(|t| from_ref_q(t, &self.imports.extern_types))
                            .collect();
                        self.symbols
                            .user_trait_impls
                            .entry(decl.target.clone())
                            .or_default()
                            .entry(decl.trait_name.clone())
                            .or_insert(args);
                        continue;
                    }
                    Stmt::Struct(d) => (&d.name, &d.impls, &d.decorators.derives),
                    Stmt::Class(d) => (&d.name, &d.impls, &d.decorators.derives),
                    Stmt::Enum(d) => (&d.name, &d.impls, &d.decorators.derives),
                    _ => continue,
                };
            for (trait_name, trait_args) in impls
                .iter()
                .map(|b| (&b.trait_name, b.trait_args.as_slice()))
                .chain(derives.iter().map(|d| (&d.name, d.args.as_slice())))
            {
                if self.symbols.user_traits.contains_key(trait_name) {
                    let args: Vec<Type> = trait_args
                        .iter()
                        .map(|t| from_ref_q(t, &self.imports.extern_types))
                        .collect();
                    self.symbols
                        .user_trait_impls
                        .entry(type_name.to_string())
                        .or_default()
                        .entry(trait_name.clone())
                        .or_insert(args);
                }
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
                Stmt::Struct(d) => (&d.name, &d.fields, &d.methods, &d.decorators.derives),
                Stmt::Class(d) => (&d.name, &d.fields, &d.methods, &d.decorators.derives),
                Stmt::Enum(d) => (&d.name, &[], &d.methods, &d.decorators.derives),
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
        // GENERIC-trait impls (in-body and standalone) register their INSTANTIATED omitted
        // defaults' signatures — `impl Cache<string>` registers `fn get(k: string): …` — so the
        // member calls type concretely. The non-generic case is covered by the name-set loop
        // below; an arity mismatch registers nothing (`check_trait_impl` reports it).
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
                        register(&d.name, &b.trait_name, &b.trait_args, &b.methods);
                    }
                }
                Stmt::Class(d) => {
                    for b in &d.impls {
                        register(&d.name, &b.trait_name, &b.trait_args, &b.methods);
                    }
                }
                Stmt::Enum(d) => {
                    for b in &d.impls {
                        register(&d.name, &b.trait_name, &b.trait_args, &b.methods);
                    }
                }
                Stmt::Impl(d) => {
                    register(&d.target, &d.trait_name, &d.trait_args, &d.methods);
                }
                _ => {}
            }
        }
        // Default-method fallback (UT5): a trait method the implementor omits falls back to the
        // trait's default body (the backends hoist it via `hoist_standalone_impl_methods`), so its
        // SIGNATURE registers here — member calls on the implementing type resolve and type it. A
        // method the type provides itself wins (already registered above); a generic trait's
        // defaults are excluded (per-implementor substitution — deferred with generic-trait
        // derivation).
        for (type_name, trait_names) in self.symbols.user_trait_impls.clone() {
            for trait_name in trait_names.into_keys() {
                let Some(decl) = self.symbols.user_traits.get(&trait_name).cloned() else {
                    continue;
                };
                if !decl.type_params.is_empty() {
                    continue;
                }
                for tm in decl.methods.iter().filter(|tm| tm.has_default) {
                    self.register_synth_method(&type_name, &tm.sig);
                }
            }
        }
        // Method-bundle bindings (kernel-methods K1) resolve after the whole collect walk, so a
        // binding is visible to method typing regardless of where the `impl` sits relative to
        // the `use` that binds its module. Resolution failures stay silent here — pass 2's
        // `check_bundle_impl` reports them at the impl site.
        for stmt in &program.stmts {
            if let Stmt::Impl(decl) = stmt
                && let Some((module, bundle)) = self.resolve_bundle_ref(&decl.trait_name)
            {
                let bindings = self
                    .symbols
                    .bundle_impls
                    .entry(decl.target.clone())
                    .or_default();
                // A duplicate binding of the same bundle is a coherence error (reported there);
                // don't double-record it, or method typing would see each method twice.
                if !bindings
                    .iter()
                    .any(|b| b.module == module && b.bundle.name == bundle.name)
                {
                    bindings.push(BoundBundle {
                        module,
                        bundle,
                        span: decl.trait_span,
                    });
                }
            }
        }
    }

    /// Resolve a dotted trait path (`vec.Kernels`) to its registered bundle: everything before
    /// the last dot is a bound module name (`use std.{vec}`), the last segment the bundle.
    /// `None` when the module binding or the bundle doesn't exist — the impl-site check reports.
    pub(crate) fn resolve_bundle_ref(
        &self,
        trait_name: &str,
    ) -> Option<(String, &'static noeta_ext_abi::ExtBundle)> {
        let (module_ref, bundle_name) = trait_name.rsplit_once('.')?;
        let qualified = self.imports.modules.get(module_ref)?;
        let bundle = self.reg().find_bundle(qualified, bundle_name)?;
        Some((qualified.clone(), bundle))
    }

    /// Record which of a type's `fields` carry a default (`name: T = …`) — and so are **optional** in
    /// an attribute construction (object-model slice 6i). Used by the construction gate to omit a
    /// defaulted field without an E0009.
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
        let key = (type_name.to_string(), m.name.clone());
        if self.symbols.methods.contains_key(&key) {
            return;
        }
        let params: Vec<Type> = m
            .params
            .iter()
            .map(|p| param_type(p, &self.imports.extern_types))
            .collect();
        let ret = async_return(
            m.ret
                .as_ref()
                .map(|t| from_ref_q(t, &self.imports.extern_types))
                .unwrap_or(Type::Unknown),
            m.is_async,
        );
        self.symbols
            .method_instance
            .insert(key.clone(), m.body.iter().any(|s| s.mentions("self")));
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
            .filter_map(|d| BuiltinTrait::from_name(&d.name))
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
                    .push((d.name.clone(), via.clone()));
            }
        }
    }

    /// Record a type's declared `From` conversions (error-ergonomics): each in-body
    /// `impl From<Source>` block registers its resolved source type under the target, so a `?`
    /// site can look up `(source → target)` regardless of statement order. Arity/validity of the
    /// block is checked in pass 2 (`check_trait_impl`); a malformed block records nothing.
    pub(crate) fn record_from_impls(&mut self, target: &str, impls: &[noeta_ast::ImplBlock]) {
        for block in impls {
            if block.trait_name == BuiltinTrait::From.name() && block.trait_args.len() == 1 {
                let source = from_ref_q(&block.trait_args[0], &self.imports.extern_types);
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
            out.insert(decl.name.clone());
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
        | Expr::Member {
            receiver: inner, ..
        }
        | Expr::TupleIndex {
            receiver: inner, ..
        }
        | Expr::Try { expr: inner, .. }
        | Expr::Await { expr: inner, .. }
        | Expr::Spawn { future: inner, .. }
        | Expr::TypeOf { value: inner, .. }
        | Expr::FieldsOf { value: inner, .. }
        | Expr::ParamsOf { target: inner, .. }
        | Expr::As { expr: inner, .. }
        | Expr::TypeTest { expr: inner, .. }
        | Expr::FromBytes { blob: inner, .. }
        | Expr::Channel {
            capacity: inner, ..
        } => collect_nested_fns_in_expr(inner, out),
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
        Expr::Invoke {
            recv, name, args, ..
        } => {
            collect_nested_fns_in_expr(recv, out);
            collect_nested_fns_in_expr(name, out);
            collect_nested_fns_in_expr(args, out);
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
        | Expr::AttributesOf { .. }
        | Expr::RolesOf { .. }
        | Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::IntN { .. }
        | Expr::Bool { .. } => {}
    }
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
