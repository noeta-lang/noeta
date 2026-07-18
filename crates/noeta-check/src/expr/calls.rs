//! **Call typing**: call synthesis and its callee dispatch (user fns, imports, prelude,
//! module/native calls), enum construction inference, user-method invocation, deferred
//! closure-argument finalization, and arity/argument checking. All `Checker` methods moved
//! verbatim out of the crate root.

use crate::*;

impl Checker {
    /// Finalize the deferred closure arguments of a call once the callee's parameter types are
    /// known (the dyn-closure gap): a `Fn`-typed parameter checks the closure against it — the
    /// absorption arm adopts the parameter types and, for a `-> dyn` expectation, infers the body's
    /// real return — anything else synthesizes standalone (the pre-deferral behavior). Idempotent:
    /// a closure some earlier branch already typed (never `Unknown`) is left alone.
    pub(crate) fn finalize_closure_args(
        &mut self,
        params: &[Type],
        args: &mut [Type],
        arg_exprs: &[Expr],
        env: &mut Env,
    ) {
        for (i, expr) in arg_exprs.iter().enumerate() {
            if !is_deferred_literal_arg(expr) {
                continue;
            }
            let Some(slot) = args.get_mut(i) else {
                continue;
            };
            if !matches!(slot, Type::Unknown) {
                continue;
            }
            // Absorb the expected parameter type where it can guide the literal — a `Fn` for a
            // closure, a `List`/`Map` for a container literal; anything else (a mismatched param, or
            // an unknown one) synthesizes standalone, preserving the pre-deferral behavior (the
            // mismatch is then caught by `check_args`' assignability check).
            *slot = match (expr, params.get(i)) {
                (Expr::Closure { .. }, Some(expected @ Type::Fn { .. })) => {
                    self.check(expr, expected, env)
                }
                (
                    Expr::List { .. } | Expr::Map { .. },
                    Some(expected @ (Type::List(_) | Type::Map(..))),
                ) => self.check(expr, expected, env),
                _ => self.synth(expr, env),
            };
        }
    }

    /// Whether `name` resolves to **something the checker knows** — a local binding, a top-level
    /// or selectively-imported function, a bound module, a user type or enum, or a reserved
    /// prelude name. The unknown-name gate (F1) uses its negation: a name that is none of these
    /// is genuinely undefined, a static `E0005` rather than a deferral to the runtime `E0005`.
    pub(crate) fn is_known_name(&self, name: &str, env: &Env) -> bool {
        lookup(env, name).is_some()
            || self.symbols.functions.contains_key(name)
            || self.imports.imported_fns.contains_key(name)
            || self.imports.modules.contains_key(name)
            || self.symbols.types.contains(name)
            || self.symbols.enums.contains_key(name)
            || RESERVED_PRELUDE.contains(&name)
            // Built-in namable types/enums (`Ordering`, `Type`, `Semantic`, iterator types, …)
            // are legitimate bare references — `Ordering.Less` names the prelude enum's variant.
            || PRELUDE_TYPES.contains(&name)
            // A hoisted top-level global (a fn body may reference one declared later).
            || self.symbols.global_binding_names.contains(name)
    }

    pub(crate) fn synth_call(
        &mut self,
        callee: &Expr,
        args: &[Type],
        arg_exprs: &[Expr],
        call_span: Span,
        env: &mut Env,
    ) -> Type {
        let mut args = args.to_vec();
        let ret = self.synth_call_inner(callee, &mut args, arg_exprs, call_span, env);
        // Safety net for the deferred closure arguments: any closure no resolution branch
        // finalized (an unknown callee, a deferred receiver, a variadic prelude call) is
        // synthesized standalone here, so its body is always checked (diagnostics, hover index)
        // exactly as before the deferral existed. A closure's type is never `Unknown` once typed,
        // so the placeholder is an unambiguous marker.
        for (i, expr) in arg_exprs.iter().enumerate() {
            if is_deferred_literal_arg(expr) && matches!(args.get(i), Some(Type::Unknown)) {
                self.synth(expr, env);
            }
        }
        ret
    }

    pub(crate) fn synth_call_inner(
        &mut self,
        callee: &Expr,
        args: &mut [Type],
        arg_exprs: &[Expr],
        call_span: Span,
        env: &mut Env,
    ) -> Type {
        let span = callee.span();
        match callee {
            // A **resolved native module-function** callee (expr-tiers arc): the expression-tier
            // desugar builds this for a native handler, so `handler(statics, holes)` types exactly
            // like the bare `use std.math.sqrt` call below — same params/return tables — no matter
            // that no import bound it. This is what lets a native and a Noeta handler share one
            // `Call` typing path.
            Expr::NativeFnRef { module, func, .. } => {
                if let Some(params) = stdlib::module_params(self.reg(), module, func, args) {
                    let required =
                        stdlib::module_required(self.reg(), module, func).unwrap_or(params.len());
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    self.check_args(&params, required, args, arg_exprs, span, func);
                }
                self.check_module_bounds(module, func, args, span);
                stdlib::module_return(self.reg(), module, func, args).unwrap_or(Type::Unknown)
            }
            // A plain `name(args)` call: a user function, else a prelude free function.
            Expr::Ident { name, .. } => {
                // A local binding that names — or shadows — a callable value takes precedence over
                // a same-named free function, so the call position agrees with value position
                // (which already prefers the local). With a concrete `Fn` type — a bare `fn`
                // reference bound to a name (`d = double; d(3)`), or a `(A) -> R` parameter that
                // shadows a free `fn` — the call is arity/type-checked against those params rather
                // than silently deferred or misrouted to the free function's signature. A
                // `dyn`/`Unknown` local (an untyped closure value) stays deferred, its arguments
                // unchecked as before.
                if let Some(Type::Fn { params, ret }) = lookup(env, name) {
                    let params = params.clone();
                    let ret = (**ret).clone();
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    // A selectively-imported module function bound to a name (`f = sqrt`) erases to
                    // `fn() -> dyn` — its real, often-variadic signature can't be a fixed param
                    // list — so that placeholder carries no checkable arity and its call stays
                    // deferred (as it was before this branch existed). Every other function value
                    // is checked: too many arguments, or a supplied argument of the wrong type, is
                    // caught. Arity's *lower* bound is not enforced — a function value does not
                    // record which of its parameters carry defaults, so `required` is `0`.
                    let erased_import = params.is_empty() && matches!(ret, Type::Dyn);
                    if !erased_import {
                        self.check_args(&params, 0, args, arg_exprs, span, name);
                    }
                    return ret;
                }
                if let Some(sig) = self.symbols.functions.get(name) {
                    let required = sig.required;
                    // A generic function is instantiated per call: bind its type parameters from the
                    // argument types, check arguments against the substituted parameters, enforce
                    // the bounds (E0025), and return the substituted result type.
                    if let Some(generic) = sig.generic.clone() {
                        return self.check_generic_call(
                            name,
                            &generic,
                            required,
                            args,
                            arg_exprs,
                            span,
                            &[],
                            env,
                        );
                    }
                    let params = sig.params.clone();
                    let ret = sig.ret.clone();
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    self.check_args(&params, required, args, arg_exprs, span, name);
                    return ret;
                }
                // A selectively-imported module function (`use std.math.sqrt`) called bare — typed
                // exactly like the qualified `math.sqrt(args)` (same params/return tables). A local
                // binding of the same name shadows it (checked first, in the arms above via `env`).
                if let Some((module, func)) = self.imports.imported_fns.get(name).cloned()
                    && lookup(env, name).is_none()
                {
                    if let Some(params) = stdlib::module_params(self.reg(), &module, &func, args) {
                        let required = stdlib::module_required(self.reg(), &module, &func)
                            .unwrap_or(params.len());
                        self.finalize_closure_args(&params, args, arg_exprs, env);
                        self.check_args(&params, required, args, arg_exprs, span, &func);
                    }
                    self.check_module_bounds(&module, &func, args, span);
                    return stdlib::module_return(self.reg(), &module, &func, args)
                        .unwrap_or(Type::Unknown);
                }
                // Prelude functions are polymorphic/variadic — their result is typed, but their
                // arguments are not arity-checked here. (The packed-result note the free `map`
                // recorded here moved to the list-method `map` arm in `synth_call`'s Member case —
                // the free form left the prelude, P1.2.) Closure arguments synthesize standalone
                // first, so a payload-typed result (`some(fn…)`) sees the real closure type.
                self.finalize_closure_args(&[], args, arg_exprs, env);
                if let Some(t) = stdlib::prelude_return(name, args) {
                    return t;
                }
                // Not a user fn, import, or prelude free function. A local (a closure value) or a
                // module/type name called here stays deferred to the runtime (a local closure's
                // args are not statically checked, unchanged); a name that resolves to *nothing*
                // is a genuinely undefined callee — a static `E0005` (F1), so a typo is caught at
                // check time instead of failing at runtime. A session defers (a later entry may
                // define it).
                if !self.config.session_mode && !self.is_known_name(name, env) {
                    self.error(
                        DiagnosticCode::UnknownName,
                        span,
                        format!("cannot find `{name}` in this scope"),
                    );
                }
                Type::Unknown
            }
            Expr::Member { receiver, name, .. } => {
                // `Enum.try_from(s)` → `?Enum` / `Enum.from(s)` → `Enum` — the built-in string→case
                // conversions (PHP `tryFrom`/`from`), reserved on every enum type. Checked before the
                // variant constructor so the names cannot be captured by a same-named variant.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && (name == "try_from" || name == "from")
                    && lookup(env, tn).is_none()
                    && self.symbols.enums.contains_key(tn)
                {
                    self.check_args(&[Type::String], 1, args, arg_exprs, span, name);
                    let ty = Type::Named(tn.clone(), Vec::new());
                    return if name == "from" {
                        ty
                    } else {
                        Type::Option(Box::new(ty))
                    };
                }
                // `Type.Variant(args)` — an algebraic enum constructor applied to its data. Infer the
                // enum's type arguments from the payload (R2b), so `Tree.Leaf(5)` is `Tree<int>`.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && self.is_enum_variant(tn, name)
                {
                    // Payload types bind the enum's generics, so a closure payload must be real.
                    self.finalize_closure_args(&[], args, arg_exprs, env);
                    return self.enum_construction_type(tn, name, args, call_span);
                }
                // `module.func(args)` — a native module call. The module identity comes from either a
                // bare module binding (`client.get`, `client` from `use std.http.client`) or a
                // namespace-group member chain (`http.client.get`, `http` from `use std.http`); both
                // key the same stdlib return-type tables, and the chain form records its span so
                // lowering materializes the leaf module value (`std.http.client`).
                let module_id = match receiver.as_ref() {
                    Expr::Ident { name: m, .. } => self.imports.modules.get(m).cloned(),
                    _ => None,
                }
                .or_else(|| self.resolve_namespace_module(receiver, env));
                if let Some(qm) = module_id {
                    // The router-facing runtime decode `json.decode_typed(name, text)` (L2.2 DI): a
                    // 2-string-arg call whose result is `Result<dyn, string>` (it decodes a JSON body
                    // into the type named by a *runtime* string, recoverably). It is not a registered
                    // native signature — `Result` has no `SigType` — so it is typed here directly, its
                    // call span recorded so lowering emits the dedicated `Rvalue::DecodeTyped`.
                    if name == "decode_typed"
                        && self.reg().find_module(&qm).map(|m| m.name) == Some("json")
                    {
                        self.finalize_closure_args(
                            &[Type::String, Type::String],
                            args,
                            arg_exprs,
                            env,
                        );
                        self.check_args(
                            &[Type::String, Type::String],
                            2,
                            args,
                            arg_exprs,
                            span,
                            name,
                        );
                        self.sites.decode_typed_sites.insert(call_span);
                        return Type::Result(Box::new(Type::Dyn), Box::new(Type::String));
                    }
                    if let Some(params) = stdlib::module_params(self.reg(), &qm, name, args) {
                        let required =
                            stdlib::module_required(self.reg(), &qm, name).unwrap_or(params.len());
                        self.finalize_closure_args(&params, args, arg_exprs, env);
                        self.check_args(&params, required, args, arg_exprs, span, name);
                    }
                    self.check_module_bounds(&qm, name, args, span);
                    return stdlib::module_return(self.reg(), &qm, name, args)
                        .unwrap_or(Type::Unknown);
                }
                // The receiver is a namespace group (`http` from `use std.http`) — a submodule chain
                // (`http.client.get`) already resolved above, so any member reaching here is either
                // an unknown member (`http.nope` — a hard error, a group is fully enumerable) or a
                // deferred non-module child (a sub-namespace/type used in call position). Either way
                // the group handle is not a value, so this must not fall through to the generic
                // method path (which would synthesize `http` as an unknown name).
                if let Some(prefix) = self.resolve_namespace_prefix(receiver, env) {
                    use noeta_ext_abi::registry::NsChild;
                    self.finalize_closure_args(&[], args, arg_exprs, env);
                    if matches!(
                        self.reg().resolve_namespace_child(&prefix, name),
                        NsChild::None
                    ) {
                        self.namespace_member_error(&prefix, name, span);
                    }
                    return Type::Unknown;
                }
                // `Type.assoc(args)` — an associated function / static call on a known user type
                // (`Box.new(1)`). Resolve to the type's method signature so the result is precisely
                // typed (a constructor result is `Box`, not a hole) and a generic class enforces its
                // bounds at construction. Guard on the receiver naming a type that is not shadowed
                // by a local variable.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && lookup(env, tn).is_none()
                    && self.symbols.types.contains(tn)
                    && let Some(sig) = self
                        .symbols
                        .methods
                        .get(&(tn.clone(), name.to_string()))
                        .cloned()
                {
                    // An INSTANCE method (its body references `self`) cannot be called
                    // associated-style — there is no receiver to become `self` (E0047,
                    // prelude-redesign EX.2). The classification is derived from the body.
                    if self
                        .symbols
                        .method_instance
                        .get(&(tn.clone(), name.to_string()))
                        .copied()
                        .unwrap_or(false)
                    {
                        self.error(
                            DiagnosticCode::InvalidReceiver,
                            span,
                            format!("`{name}` is an instance method of `{tn}`"),
                        )
                        .help(format!(
                            "call it on a value (`x.{name}(...)`), or pass `{tn}.{name}` \
                             as a handle"
                        ));
                        return sig.ret.clone();
                    }
                    // A static call: the type arguments are not known from a bare type name, so the
                    // method's own arguments instantiate any parameters (`Box.new(1)` infers `int`).
                    return self.call_user_method(name, &sig, args, arg_exprs, span, &[], env);
                }
                // `receiver.method(args)` — a built-in method, a user method, or (on a `dyn`/hole
                // receiver) a runtime-dispatched call that stays deferred.
                let recv = self.synth(receiver, env);
                // A user-declared instance method resolves through the same path as a static call
                // (generic methods instantiate + enforce bounds); the receiver's type arguments seed
                // the instantiation so the result is precise. A built-in method or a deferred
                // receiver falls through below.
                if let Type::Named(n, recv_args) = &recv
                    && let Some(sig) = self
                        .symbols
                        .methods
                        .get(&(n.clone(), name.to_string()))
                        .cloned()
                {
                    // An ASSOCIATED function (never touches `self`) is not callable on a value —
                    // the receiver would be silently discarded (E0047, prelude-redesign EX.2).
                    if !self
                        .symbols
                        .method_instance
                        .get(&(n.clone(), name.to_string()))
                        .copied()
                        .unwrap_or(true)
                    {
                        self.error(
                            DiagnosticCode::InvalidReceiver,
                            span,
                            format!("`{name}` is an associated function of `{n}`"),
                        )
                        .help(format!("call it on the type: `{n}.{name}(...)`"));
                        return sig.ret.clone();
                    }
                    return self
                        .call_user_method(name, &sig, args, arg_exprs, span, recv_args, env);
                }
                // A method call on an in-scope TYPE PARAMETER resolves through its user-trait
                // bounds, typed at the bound's instantiation (`<T: Keyed<int>>` → `x.key(): int`,
                // `x.same(other: int)`); a method no bound declares falls through and stays
                // lenient as before (dispatch may still resolve at runtime).
                if let Type::Named(n, _) = &recv
                    && let Some((params, required, ret)) = self.type_param_trait_method(n, name)
                {
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    self.check_args(&params, required, args, arg_exprs, span, name);
                    return ret;
                }
                // THE dyn-closure gap's primary site: a builtin method's parameter types carry
                // the receiver's element type (`List<int>.map` expects `(int) -> dyn`), so the
                // deferred closure argument finalizes against them here — its parameters adopt the
                // element type, its body infers a real return, and the `map` refinements below see
                // a precise `Fn` instead of a context-free one.
                let builtin_params =
                    stdlib::method_params(self.reg(), &recv, name).unwrap_or_default();
                self.finalize_closure_args(&builtin_params, args, arg_exprs, env);
                self.check_method_args(&recv, name, args, arg_exprs, span);
                // A bit intrinsic on a fixed-width receiver (Tier W5) must act within the width, not
                // the erased i64 (`(1u8).leading_zeros() == 7`), so mark the **call** span (the one
                // lowering's `Method` carries) — lowering then emits the width-carrying
                // `WidthIntMethod`. Conversions (`IntMethod::Convert`, the `to_*` names) are already
                // width-typed by name and stay ordinary methods. Signedness is irrelevant here.
                if let Type::IntN { bits, .. } = recv
                    && let Some(m) = noeta_ext_abi::IntMethod::from_name(name)
                    && !matches!(m, noeta_ext_abi::IntMethod::Convert { .. })
                {
                    self.sites.width_sites.insert(call_span, (false, bits));
                }
                // `it.zip(other)` → `Iterator<(A, B)>`: both element types are needed and only `recv`
                // reaches `method_return`, so the precise tuple is assembled here where the argument
                // type is in scope (A from the receiver, B from the argument iterator).
                if name == "zip"
                    && let Type::Named(rn, ra) = &recv
                    && rn == stdlib::ITERATOR
                {
                    let a = ra.first().cloned().unwrap_or(Type::Dyn);
                    let b = match args.first() {
                        Some(Type::Named(an, aa)) if an == stdlib::ITERATOR => {
                            aa.first().cloned().unwrap_or(Type::Dyn)
                        }
                        _ => Type::Dyn,
                    };
                    return Type::Named(
                        stdlib::ITERATOR.to_string(),
                        vec![Type::Tuple(vec![a, b])],
                    );
                }
                // `it.map(f)` → `Iterator<R>` where `R` is the closure's return type — known here from
                // the argument but not to `method_return` (which sees only the receiver). (Track I.1c.)
                if name == "map"
                    && let Type::Named(rn, _) = &recv
                    && rn == stdlib::ITERATOR
                {
                    let r = match args.first() {
                        Some(Type::Fn { ret, .. }) => (**ret).clone(),
                        _ => Type::Dyn,
                    };
                    return Type::Named(stdlib::ITERATOR.to_string(), vec![r]);
                }
                // `xs.map(f)` on a list → `List<R>`, `R` the closure's return type — the eager list
                // method form (prelude-redesign P1), refined here for the same reason as iterator
                // `map`. Matches the free `map(xs, f)` this replaces.
                if name == "map" && matches!(recv, Type::List(_)) {
                    let r = match args.first() {
                        Some(Type::Fn { ret, .. }) => (**ret).clone(),
                        _ => Type::Dyn,
                    };
                    // Record the packed-result note the free `map` gets (keyed by the call span), so a
                    // packed-struct element still lowers to a flat result.
                    self.note_map_packed(&r, call_span);
                    return Type::List(Box::new(r));
                }
                // A method-bundle method (kernel-methods K2): the receiver is a bound `@packed`
                // type (`Element`) or a `List<T>` of one (`Bulk`). Resolution is static: the
                // route is recorded at the call span for lowering to bake in — so dispatch is
                // call-site-resolved (an empty list receiver works) and a `dyn` receiver simply
                // never reaches here (the documented escape-hatch behavior).
                if let Some(ret) = self.bundle_method_call(&recv, name, args, span, call_span) {
                    return ret;
                }
                let ret = self.method_call_return(&recv, name);
                // A method call on a concrete primitive with no such built-in method is an error,
                // mirroring the non-indexable check (`42[0]`). `dyn`/holes defer (their result is
                // the deferred type, not `Unknown`), and a user `Named` type may resolve the call
                // through a trait at runtime — so both are left lenient; only the closed primitives
                // are flagged.
                if matches!(ret, Type::Unknown)
                    && matches!(
                        recv,
                        Type::Int | Type::IntN { .. } | Type::Float | Type::Bool | Type::Unit
                    )
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("type `{recv}` has no method `{name}`"),
                    );
                }
                ret
            }
            _ => {
                self.synth(callee, env);
                Type::Unknown
            }
        }
    }

    /// Check a call to a resolved user method or associated function (`Box.new(...)`, `obj.m(...)`).
    /// A generic one (a method of a generic class) instantiates and enforces its bounds through the
    /// shared [`Self::check_generic_call`]; a non-generic one checks arguments against its
    /// (erased) parameter types and returns its declared return type.
    /// The type of an enum-variant construction — `Tree.Leaf(5)` (payload) or `Color.Red` (nullary) —
    /// **inferring the enum's type arguments** (R2b): for a generic enum, unify the variant's declared
    /// payload types against the argument types (like a generic constructor call, reusing
    /// [`bind_type_params`]), filling any parameter the payload does not pin with `dyn`; for a
    /// non-generic enum, the empty argument list. Reuses the accurate [`VariantInfo::fields`] (the same
    /// source the `Send`/relevance analyses read). Records the construction site (`span`) so reflection
    /// can tag the value (R2b.2); the refined type also flows into the static `type_of` path.
    pub(crate) fn enum_construction_type(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Type],
        span: Span,
    ) -> Type {
        let params = self
            .symbols
            .generic_types
            .get(enum_name)
            .cloned()
            .unwrap_or_default();
        let type_args = if params.is_empty() {
            Vec::new()
        } else {
            let pset: HashSet<String> = params.iter().cloned().collect();
            let mut subst: HashMap<String, Type> = HashMap::new();
            if let Some(fields) = self
                .symbols
                .enums
                .get(enum_name)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| v.fields.clone())
            {
                for (decl, arg) in fields.iter().zip(args) {
                    bind_type_params(decl, arg, &pset, &mut subst);
                }
            }
            params
                .iter()
                .map(|p| subst.get(p).cloned().unwrap_or(Type::Dyn))
                .collect()
        };
        let ty = Type::Named(enum_name.to_string(), type_args);
        self.note_construction(&ty, span);
        ty
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call_user_method(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &mut [Type],
        arg_exprs: &[Expr],
        span: Span,
        recv_args: &[Type],
        env: &mut Env,
    ) -> Type {
        if let Some(generic) = &sig.generic {
            return self.check_generic_call(
                name,
                generic,
                sig.required,
                args,
                arg_exprs,
                span,
                recv_args,
                env,
            );
        }
        let params = sig.params.clone();
        self.finalize_closure_args(&params, args, arg_exprs, env);
        self.check_args(&params, sig.required, args, arg_exprs, span, name);
        sig.ret.clone()
    }

    /// Arity- and type-check a method call's arguments against the resolved parameter signature
    /// (a built-in method or a user method); a deferred receiver or an unknown method is not
    /// checked.
    pub(crate) fn check_method_args(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
    ) {
        if let Some(params) = stdlib::method_params(self.reg(), recv, name) {
            let required = stdlib::method_required(self.reg(), recv, name).unwrap_or(params.len());
            self.check_args(&params, required, args, arg_exprs, span, name);
        } else if let Type::Named(n, _) = recv
            && let Some(sig) = self.symbols.methods.get(&(n.clone(), name.to_string()))
        {
            let params = sig.params.clone();
            let required = sig.required;
            self.check_args(&params, required, args, arg_exprs, span, name);
        }
    }

    /// Check a call's argument count and types against the callable's parameter types, reporting
    /// at `span`. Lenient where either side defers to runtime (`dyn`/hole) and on numeric widening
    /// (`int` where `float` is expected), so polymorphic and numeric calls are not false positives.
    pub(crate) fn check_args(
        &mut self,
        params: &[Type],
        required: usize,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
        callee: &str,
    ) {
        if args.len() < required || args.len() > params.len() {
            let expected = if required == params.len() {
                format!("{}", params.len())
            } else {
                format!("between {required} and {}", params.len())
            };
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{callee}` expects {expected} argument(s), found {}",
                    args.len()
                ),
            );
            return;
        }
        // Only the supplied arguments are type-checked; the omitted trailing parameters are
        // filled by their defaults (already checked against their parameter types at the
        // declaration), so `zip` stopping at the shorter side is exactly right.
        for (i, (param, arg)) in params.iter().zip(args).enumerate() {
            // A bare numeric literal argument adapts into a fixed-width parameter (`f(200)` for a
            // `u8` param, `f(1.5)` for `f32`/`f64`) — exactly as it does at a binding of that type
            // (P-NUM-SYM). Try that first; a non-literal or non-adapting arg falls to `arg_assignable`
            // (which keeps the `int`/`float` widening leniency the strict fixed-width types lack).
            if let Some(expr) = arg_exprs.get(i)
                && self.try_adapt_literal(expr, param).is_some()
            {
                continue;
            }
            if !self.arg_assignable(arg, param) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{param}`"),
                );
            }
        }
    }
}
