//! **Call typing**: call synthesis and its callee dispatch (user fns, imports, prelude,
//! module/native calls), enum construction inference, user-method invocation, deferred
//! closure-argument finalization, and arity/argument checking. All `Checker` methods moved
//! verbatim out of the crate root.

use crate::*;
use noeta_ast::CallArg;

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
        arg_exprs: &[CallArg],
        env: &mut Env,
    ) {
        for (i, expr) in CallArg::values(arg_exprs).enumerate() {
            if !self.is_deferred_arg(expr, env) {
                continue;
            }
            let Some(slot) = args.get_mut(i) else {
                continue;
            };
            if !matches!(slot, Type::Unknown) {
                continue;
            }
            // Absorb the expected parameter type where it can guide the literal — a `Fn` for a
            // closure (or a deferred polymorphic-function reference, F1), a `List`/`Map` for a
            // container literal; anything else (a mismatched param, or an unknown one) synthesizes
            // standalone, preserving the pre-deferral behavior (the mismatch is then caught by
            // `check_args`' assignability check).
            *slot = match (expr, params.get(i)) {
                (Expr::Closure { .. } | Expr::Ident { .. }, Some(expected @ Type::Fn { .. })) => {
                    self.check(expr, expected, env)
                }
                (
                    Expr::List { .. } | Expr::Map { .. },
                    Some(expected @ (Type::List(_) | Type::Map(..))),
                ) => self.check(expr, expected, env),
                // A target-typed `.{ … }` absorbs **whatever** the parameter type is — unlike the
                // arms above it does not pre-filter on the expected type's shape, because the
                // literal has no standalone meaning to fall back to. `check` owns the decision:
                // a concrete named record type is adopted, anything else is E0023 reported there.
                // Falling through to `synth` instead would report "no expected type", which would
                // be a lie at a call site that has one.
                (Expr::Object(lit), Some(expected)) if lit.type_name.is_none() => {
                    self.check(expr, expected, env)
                }
                _ => self.synth(expr, env),
            };
        }
    }

    /// Whether a call argument should be **deferred** until the callee's parameter types are
    /// known: the literal forms ([`is_deferred_literal_arg`] — closures and container literals),
    /// plus (F1, poly-values) a bare identifier naming an unshadowed **polymorphic named function**
    /// — a generic user fn, or a prelude constructor (`Ok`/`Err`/`some`) — whose precise type only
    /// an expected `Fn` type can instantiate. Everything else synthesizes eagerly as before.
    pub(crate) fn is_deferred_arg(&self, expr: &Expr, env: &Env) -> bool {
        if is_deferred_literal_arg(expr) {
            return true;
        }
        matches!(expr, Expr::Ident { name, .. }
            if lookup(env, name).is_none()
                && (matches!(name.as_str(), "Ok" | "Err" | "some")
                    || self
                        .symbols
                        .functions
                        .get(name)
                        .is_some_and(|sig| sig.generic.is_some())))
    }

    /// The precise monomorphic [`Type::Fn`] of a **polymorphic named function used in value
    /// position** against an expected function type (F1, poly-values): a generic user fn — or a
    /// prelude constructor (`Ok`/`Err`/`some`, and `panic`) — instantiates its type parameters
    /// from the expectation via the same structural binding a call site uses
    /// ([`bind_type_params`]), enforces its declared bounds, and yields the substituted signature
    /// (`xs.map(double_generic)` sees `fn(int) -> int`, `results.map(Ok)` sees
    /// `fn(int) -> Result<int, E>`). Instantiation happens **at the use site from the expected
    /// type only** — no type scheme is stored in the lattice, and a bare (expectation-free)
    /// reference keeps today's erased value (a generic fn's `dyn`-erased signature; a constructor
    /// stays a deferred hole), the honest gradual fallback the `dyn` top exists for.
    ///
    /// `None` when `name` is not such a function, or a local shadows it: the caller falls back to
    /// ordinary synthesis. The caller subsumes the result against `expected`, so a genuinely
    /// incompatible instantiation (`ints.map(first_elem_generic)` where the parameter shapes
    /// cannot line up) still reports.
    pub(crate) fn instantiate_fn_value(
        &mut self,
        name: &str,
        expected: &Type,
        span: Span,
        env: &Env,
    ) -> Option<Type> {
        let Type::Fn {
            params: exp_params,
            ret: exp_ret,
        } = expected
        else {
            return None;
        };
        if lookup(env, name).is_some() {
            return None;
        }
        // The prelude constructors, typed as the generic constructors they are:
        // `Ok<T, E>(v: T): Result<T, E>` (also the nullary `Ok(): Result<void, E>`),
        // `Err<T, E>(e: E): Result<T, E>`, `some<T>(v: T): Option<T>`. The synthetic `$T`/`$E`
        // parameter names cannot collide with a user type (no source name contains `$`), and any
        // residue erases to `dyn` below. `panic` has no parameters to instantiate: its value type
        // is `fn(dyn) -> ?` — the language has no bottom/`never` type, so the return stays an
        // inference hole (divergent in practice, compatible with any expected return).
        let t = || Type::Named("$T".to_string(), Vec::new());
        let e = || Type::Named("$E".to_string(), Vec::new());
        /// The instantiable shape of a polymorphic value: its (bounded) type parameters and
        /// un-erased params/return — the same trio [`GenericInfo`] carries.
        type CtorShape = (Vec<(String, Vec<BoundReq>)>, Vec<Type>, Type);
        let (params, raw_params, raw_ret): CtorShape = match name {
            "panic" => {
                return Some(Type::Fn {
                    params: vec![Type::Dyn],
                    ret: Box::new(Type::Unknown),
                });
            }
            "some" => (Vec::new(), vec![t()], Type::Option(Box::new(t()))),
            "Ok" if exp_params.is_empty() => (
                Vec::new(),
                Vec::new(),
                Type::Result(Box::new(Type::Unit), Box::new(e())),
            ),
            "Ok" => (
                Vec::new(),
                vec![t()],
                Type::Result(Box::new(t()), Box::new(e())),
            ),
            "Err" => (
                Vec::new(),
                vec![e()],
                Type::Result(Box::new(t()), Box::new(e())),
            ),
            _ => {
                // A generic user function: its un-erased `GenericInfo` drives the instantiation
                // exactly as a call site's does, bounds included.
                let sig = self.symbols.functions.get(name)?;
                let generic = sig.generic.clone()?;
                let required = sig.required;
                // The value adopts the expectation's arity when the declaration's defaults
                // allow it (a trailing-defaulted parameter may be dropped from the value's
                // face); otherwise keep the full parameter list and let subsumption report
                // the arity mismatch.
                let n = if (required..=generic.raw_params.len()).contains(&exp_params.len()) {
                    exp_params.len()
                } else {
                    generic.raw_params.len()
                };
                let mut raw_params = generic.raw_params;
                raw_params.truncate(n);
                (generic.params, raw_params, generic.raw_ret)
            }
        };
        let is_prelude_ctor = params.is_empty();
        let tps: HashSet<String> = if is_prelude_ctor {
            ["$T".to_string(), "$E".to_string()].into_iter().collect()
        } else {
            params.iter().map(|(n, _)| n.clone()).collect()
        };
        // Bind parameters first (positionally, contravariance is irrelevant for binding), then let
        // the expected return pin anything the parameters left open (`f: () -> Order = make`).
        let mut subst: HashMap<String, Type> = HashMap::new();
        for (raw, exp) in raw_params.iter().zip(exp_params) {
            bind_type_params(raw, exp, &tps, &mut subst);
        }
        bind_type_params(&raw_ret, exp_ret, &tps, &mut subst);
        self.enforce_type_param_bounds(name, &params, &subst, &tps, span);
        // A constructor slot the expectation left open stays an **inference hole** — matching the
        // call form (`Err(e)` synthesizes `Result<?, E>`), so `xs.map(Err)` flows into a declared
        // `List<Result<int, Low>>` boundary exactly as the per-element calls would. A user generic
        // fn's residue erases to `dyn` (below), matching its call sites.
        if is_prelude_ctor {
            for p in ["$T", "$E"] {
                subst.entry(p.to_string()).or_insert(Type::Unknown);
            }
        }
        // A FORWARDING generic fn as a value (poly-deferrals D2c): the expectation pinned the
        // instantiation, so the hidden type-argument slots can be resolved HERE and bound into
        // the value — lowering wraps the reference in a closure that supplies them (a partial
        // application over the slots). An instantiation the expectation leaves open (or a
        // pass-through with no matching enclosing slot) returns `None`: the caller falls back to
        // synthesis, whose `Ident` arm reports the value boundary once, exactly as before.
        if self.symbols.forwarding.contains_key(name) {
            let hidden = self.resolve_value_hidden_slots(name, &subst, &tps, span)?;
            self.sites.hidden_arg_sites.insert(span, hidden);
            self.sites
                .fn_value_sites
                .insert(span, (name.to_string(), raw_params.len() as u32));
        }
        Some(Type::Fn {
            params: raw_params
                .iter()
                .map(|p| subst_or_dyn(p, &subst, &tps))
                .collect(),
            ret: Box::new(subst_or_dyn(&raw_ret, &subst, &tps)),
        })
    }

    /// Resolve a forwarding fn's hidden slots for a VALUE-position instantiation (D2c): every
    /// slot template must resolve — concretely (interned into the type-argument table, recipe
    /// checked) or as a pass-through of the enclosing fn's matching slot. `None` when any slot
    /// stays open, poisoned, or unbuildable; the caller then falls back to the bare-binding
    /// boundary (one E0058), so a half-bound value can never exist.
    fn resolve_value_hidden_slots(
        &mut self,
        name: &str,
        subst: &HashMap<String, Type>,
        tps: &HashSet<String>,
        span: Span,
    ) -> Option<Vec<noeta_ext_abi::HiddenArg>> {
        if self.symbols.forwarding_poisoned.contains(name) {
            return None;
        }
        let fwd = self.symbols.forwarding.get(name).cloned()?;
        let mut hidden = Vec::with_capacity(fwd.len());
        for slot in &fwd {
            if params_mentioned(&slot.template, tps).iter().any(|p| {
                subst
                    .get(p)
                    .is_none_or(|t| t.defers_to_runtime() || t.contains_unknown())
            }) {
                return None;
            }
            let sigma = apply_subst(&slot.template, subst);
            if self.mentions_in_scope_param(&sigma) {
                let j = self
                    .coloring
                    .current_forwarding
                    .iter()
                    .position(|t| t == &sigma)?;
                hidden.push(noeta_ext_abi::HiddenArg::Forward(j as u32));
                continue;
            }
            let recipe = self.type_to_recipe(&sigma);
            if slot.needs_recipe && recipe.is_none() {
                // Report the precise unbuildable-type error (mirroring the call site) and keep
                // resolving: the program is already rejected, and falling back to synthesis here
                // would only stack the generic value-boundary E0058 on top.
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "`{sigma}` cannot be built by the call-site-typed `::<{}>` position of \
                         `{name}`",
                        slot.template
                    ),
                );
            }
            let info = noeta_ext_abi::TypeArgInfo {
                name: sigma.to_string(),
                recipe,
            };
            let idx = match self.sites.type_arg_table.iter().position(|e| *e == info) {
                Some(i) => i,
                None => {
                    self.sites.type_arg_table.push(info);
                    self.sites.type_arg_table.len() - 1
                }
            };
            hidden.push(noeta_ext_abi::HiddenArg::Table(idx as u32));
        }
        Some(hidden)
    }

    /// If `expr` is a plain call of an unshadowed **generic user function** (`load(text)` — an
    /// `Ident` callee with a collected `GenericInfo`), the trio a return-position seeding arm
    /// needs: the callee's name, its span, and its un-erased generic signature. The shared guard
    /// of the check-mode `Call`/`Try`/`??` seeding arms (F2c + poly-deferrals D1), so the three
    /// positions cannot drift on what counts as seedable.
    pub(crate) fn seedable_generic_call(
        &self,
        expr: &Expr,
        env: &Env,
    ) -> Option<(String, Span, GenericInfo)> {
        let Expr::Call { callee, .. } = expr else {
            return None;
        };
        let Expr::Ident { name, span } = callee.as_ref() else {
            return None;
        };
        if lookup(env, name).is_some() {
            return None;
        }
        let generic = self.symbols.functions.get(name)?.generic.clone()?;
        Some((name.clone(), *span, generic))
    }

    /// The seeded-call worker behind the check-mode seeding arms: defer the deferrable arguments,
    /// run the shared seeded generic-call machinery (hidden forwarding slots keyed on
    /// `call_span`), and finish with the deferred-argument safety net `synth_call` applies — so a
    /// seeded position types its arguments exactly like a synthesized call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn check_seeded_generic_call(
        &mut self,
        name: &str,
        generic: &GenericInfo,
        required: usize,
        args: &[CallArg],
        callee_span: Span,
        call_span: Span,
        seed: HashMap<String, Type>,
        env: &mut Env,
    ) -> Type {
        let mut arg_types: Vec<Type> = args
            .iter()
            .map(|a| {
                if self.is_deferred_arg(&a.value, env) {
                    Type::Unknown
                } else {
                    self.synth(&a.value, env)
                }
            })
            .collect();
        let ret = self.check_generic_call_seeded(
            name,
            generic,
            required,
            &mut arg_types,
            args,
            callee_span,
            seed,
            Some(call_span),
            env,
        );
        // The deferred-argument safety net, mirroring `synth_call`.
        for (i, arg) in CallArg::values(args).enumerate() {
            if self.is_deferred_arg(arg, env) && matches!(arg_types.get(i), Some(Type::Unknown)) {
                self.synth(arg, env);
            }
        }
        ret
    }

    /// Seed a coalesce's generic-call VALUE from the success-arm expectation (poly-deferrals D1):
    /// `load(text) ?? default` binds the callee's declared `Result`/`Option` payload against
    /// `success_expected` and runs the seeded call, returning the still-WRAPPED type (the caller
    /// unwraps the payload). The caller has already established `value` is seedable.
    pub(crate) fn check_coalesce_seeded(
        &mut self,
        value: &Expr,
        success_expected: &Type,
        env: &mut Env,
    ) -> Type {
        let (name, callee_span, generic) = self
            .seedable_generic_call(value, env)
            .expect("caller-guarded: value is a seedable generic call");
        let Expr::Call {
            args,
            span: call_span,
            ..
        } = value
        else {
            unreachable!("seedable_generic_call matches plain calls only")
        };
        let required = self.symbols.functions[&name].required;
        let tps: HashSet<String> = generic.params.iter().map(|(n, _)| n.clone()).collect();
        let mut seed: HashMap<String, Type> = HashMap::new();
        match &generic.raw_ret {
            Type::Result(ok, _) => bind_type_params(ok, success_expected, &tps, &mut seed),
            Type::Option(some) => bind_type_params(some, success_expected, &tps, &mut seed),
            _ => {}
        }
        self.check_seeded_generic_call(
            &name,
            &generic,
            required,
            args,
            callee_span,
            *call_span,
            seed,
            env,
        )
    }

    /// Unwrap the operand type of a `?` (`Expr::Try`) — the one shared judgment for synthesis and
    /// the check-mode seeding arm (poly-deferrals D1): a `Result` yields its `Ok` payload and runs
    /// the error-position `From`-conversion rule (E0057 / `try_conversion_sites`); an `Option`
    /// yields its payload; a `dyn`/hole defers; anything else is E0012.
    pub(crate) fn try_unwrap(&mut self, inner: &Type, span: Span) -> Type {
        match inner {
            Type::Result(ok, err) => {
                // The error-position rule (error-ergonomics): a mismatched `Err` type either
                // converts through the target's `impl From<Source>` (site recorded for lowering)
                // or is E0057.
                let err = (**err).clone();
                self.check_try_error(&err, span);
                (**ok).clone()
            }
            Type::Option(some) => (**some).clone(),
            // A hole carries no info; `dyn` defers to runtime — both accept `?` without a
            // diagnostic, yielding the same deferred type.
            t if t.defers_to_runtime() => t.clone(),
            other => {
                self.error(
                    DiagnosticCode::InvalidTry,
                    span,
                    format!("`?` expects a `Result` or `Option`, found `{other}`"),
                )
                .help("`?` only propagates `Result`/`Option`; this value is neither");
                Type::Unknown
            }
        }
    }

    /// Type an **explicitly instantiated user-generic call** `f::<T, ...>(args)` (poly-values F2).
    /// The named type arguments bind to the function's declared type parameters IN ORDER and are
    /// seeded as winning bindings into the shared generic-call machinery
    /// ([`Self::check_generic_call_seeded`]): argument inference can only fill parameters the
    /// turbofish left implicit (there are none today — arity is exact), and an argument that
    /// disagrees with an explicit binding fails the ordinary assignability check against the
    /// substituted parameter (E0007). Misapplied turbofish — an unknown callee, a non-generic
    /// function, a local binding, or a type-argument arity mismatch — is E0058.
    ///
    /// A type argument may itself be an in-scope type parameter (`load::<T>(p)` inside another
    /// generic fn): it binds as the opaque `Named` parameter, checks structurally, and erases at
    /// runtime like every generic call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn synth_typed_call(
        &mut self,
        name: &str,
        name_span: Span,
        type_args: &[TypeRef],
        args: &mut [Type],
        arg_exprs: &[CallArg],
        span: Span,
        env: &mut Env,
    ) -> Type {
        for t in type_args {
            self.check_type_ref(t);
        }
        let resolved: Vec<Type> = type_args
            .iter()
            .map(|t| from_ref_q(t, &self.imports.extern_types))
            .collect();
        // A local binding cannot be instantiated, and shadows the free function in call position —
        // reject rather than silently routing past the shadow.
        if lookup(env, name).is_some() {
            self.finalize_closure_args(&[], args, arg_exprs, env);
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                name_span,
                format!("`{name}` is a local binding, not a generic function"),
            )
            .help("explicit type arguments apply only to a declared generic `fn`");
            return Type::Unknown;
        }
        let Some(sig) = self.symbols.functions.get(name).cloned() else {
            self.finalize_closure_args(&[], args, arg_exprs, env);
            if !self.config.session_mode && !self.is_known_name(name, env) {
                self.error(
                    DiagnosticCode::UnknownName,
                    name_span,
                    format!("cannot find `{name}` in this scope"),
                );
            } else {
                self.error(
                    DiagnosticCode::InvalidTypeArguments,
                    name_span,
                    format!("`{name}` is not a generic function"),
                )
                .help("explicit type arguments apply only to a declared generic `fn`");
            }
            return Type::Unknown;
        };
        let Some(generic) = sig.generic.clone() else {
            self.finalize_closure_args(&sig.params, args, arg_exprs, env);
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                name_span,
                format!("`{name}` takes no type parameters"),
            )
            .help("drop the `::<...>` — only a generic `fn` is instantiated explicitly");
            return sig.ret.clone();
        };
        if resolved.len() != generic.params.len() {
            self.finalize_closure_args(&[], args, arg_exprs, env);
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                span,
                format!(
                    "`{name}` expects {} type argument(s), found {}",
                    generic.params.len(),
                    resolved.len()
                ),
            );
            let tps: HashSet<String> = generic.params.iter().map(|(n, _)| n.clone()).collect();
            return erase_type_params(generic.raw_ret.clone(), &tps);
        }
        let seed: HashMap<String, Type> = generic
            .params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(resolved)
            .collect();
        self.check_generic_call_seeded(
            name,
            &generic,
            sig.required,
            args,
            arg_exprs,
            span,
            seed,
            Some(span),
            env,
        )
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
            // A hoisted top-level global (top-level code may reference one declared later) — but
            // NOT inside a sealed named-fn body, where top-level value bindings are out of scope
            // unless `use (…)`-captured (captures are in the sealed env, caught by `lookup`).
            || (!self.coloring.in_sealed_body
                && self.symbols.global_binding_names.contains(name))
            // A nested `fn`'s name is an item of its enclosing body — visible to recursion and
            // siblings even inside sealed bodies (a declaration, not captured value state).
            // Program-wide-hoisted, so out-of-scope references defer to the runtime error.
            || self.symbols.nested_fn_names.contains(name)
    }

    pub(crate) fn synth_call(
        &mut self,
        callee: &Expr,
        args: &[Type],
        arg_exprs: &[CallArg],
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
        for (i, expr) in noeta_ast::CallArg::values(arg_exprs).enumerate() {
            if self.is_deferred_arg(expr, env) && matches!(args.get(i), Some(Type::Unknown)) {
                self.synth(expr, env);
            }
        }
        ret
    }

    pub(crate) fn synth_call_inner(
        &mut self,
        callee: &Expr,
        args: &mut [Type],
        arg_exprs: &[CallArg],
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
                    let bound = self.bind_native_args(
                        module, func, arg_exprs, &params, required, args, span, call_span,
                    );
                    let arg_exprs = bound.as_deref().unwrap_or(arg_exprs);
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
                // A local binding of a user OBJECT type invoked as a value (`obj(args)`) — the
                // `Callable` protocol: typed as `obj.call(args)` when the type provides a `call`
                // method; a known user type without one is statically not callable (E0007).
                if let Some(recv @ Type::Named(..)) = lookup(env, name) {
                    let recv = recv.clone();
                    if let Some(ret) = self.synth_callable_object(&recv, args, arg_exprs, span, env)
                    {
                        return ret;
                    }
                }
                if let Some(sig) = self.symbols.functions.get(name) {
                    let required = sig.required;
                    // Named arguments bind to the parameters they name, so normalize the written
                    // list into parameter order ONCE, here — everything downstream (generic
                    // instantiation, closure finalization, arity and assignability) then sees a
                    // plain positional call and needs no notion of labels at all.
                    let (permuted, supplied_params);
                    let arg_exprs = if noeta_ast::CallArg::any_named(arg_exprs) {
                        let (names, types) = (sig.param_names.clone(), sig.params.clone());
                        match self.order_arguments(
                            arg_exprs, &names, &types, required, name, args, span, call_span,
                        ) {
                            Some((a, p)) => {
                                permuted = a;
                                supplied_params = Some(p);
                                &permuted[..]
                            }
                            None => return self.symbols.functions[name].ret.clone(),
                        }
                    } else {
                        supplied_params = None;
                        arg_exprs
                    };
                    let sig = &self.symbols.functions[name];
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
                            Some(call_span),
                            env,
                        );
                    }
                    let ret = sig.ret.clone();
                    // With labels the supplied parameters are already compacted parallel to the
                    // arguments, and every required one is present — so each supplied value is
                    // checked against ITS parameter, not the one that happens to sit at its index.
                    let (params, required) = match supplied_params {
                        Some(p) => {
                            let n = p.len();
                            (p, n)
                        }
                        None => (sig.params.clone(), required),
                    };
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
                        let bound = self.bind_native_args(
                            &module, &func, arg_exprs, &params, required, args, span, call_span,
                        );
                        let arg_exprs = bound.as_deref().unwrap_or(arg_exprs);
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
                    // In a SEALED named-fn body, a callee that names a real top-level binding
                    // (e.g. a closure bound at top level) gets the capture hint.
                    let sealed_global_miss = self.coloring.in_sealed_body
                        && self.symbols.global_binding_names.contains(name);
                    let diag = self.error(
                        DiagnosticCode::UnknownName,
                        span,
                        format!("cannot find `{name}` in this scope"),
                    );
                    if sealed_global_miss {
                        diag.help(format!(
                            "`{name}` is a top-level binding, which a named function does not \
                             see implicitly — add `use ({name})` to the signature, or pass it \
                             as a parameter"
                        ));
                    }
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
                    && let Some(key) = self.enum_type_key(tn)
                    && self.is_enum_variant(&key, name)
                {
                    // Payload types bind the enum's generics, so a closure payload must be real.
                    self.finalize_closure_args(&[], args, arg_exprs, env);
                    return self.enum_construction_type(&key, name, args, call_span);
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
                    // 2-string-arg call whose result is `Result<dyn, JsonError>` (it decodes a JSON
                    // body into the type named by a *runtime* string, recoverably — the same
                    // path-carrying error story as `json.try_parse::<T>`). It is not a registered
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
                        let json_error = Type::Named(
                            stdlib::qualified_extern(self.reg(), "JsonError"),
                            Vec::new(),
                        );
                        return Type::Result(Box::new(Type::Dyn), Box::new(json_error));
                    }
                    if let Some(params) = stdlib::module_params(self.reg(), &qm, name, args) {
                        let required =
                            stdlib::module_required(self.reg(), &qm, name).unwrap_or(params.len());
                        let bound = self.bind_native_args(
                            &qm, name, arg_exprs, &params, required, args, span, call_span,
                        );
                        let arg_exprs = bound.as_deref().unwrap_or(arg_exprs);
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
                    return self.call_user_method(
                        name,
                        &sig,
                        args,
                        arg_exprs,
                        span,
                        &[],
                        Some(call_span),
                        env,
                    );
                }
                // `receiver.method(args)` — a built-in method, a user method, or (on a `dyn`/hole
                // receiver) a runtime-dispatched call that stays deferred.
                let recv = self.synth(receiver, env);
                // A trait default-body method (ExtBundle→ExtTrait convergence, slice 2): a native
                // trait's *defaulted* method the receiver's concrete type does not provide — the TRAIT
                // answers (source 2). Checked FIRST, before the `symbols.methods` resolution below,
                // because a native-default method has no real signature there (it is deliberately not
                // UT5-registered — a synth signature would misclassify a no-`self` body as an associated
                // fn). `native_trait_default_sites` already excludes overrides and inherent methods, so
                // a hit here is unambiguous; the route is baked at the call span.
                if let Some(ret) =
                    self.trait_default_method_call(&recv, name, args, span, call_span)
                {
                    return ret;
                }
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
                    return self.call_user_method(
                        name,
                        &sig,
                        args,
                        arg_exprs,
                        span,
                        recv_args,
                        Some(call_span),
                        env,
                    );
                }
                // `obj.f(args)` where `f` is a FIELD of the receiver's type — the
                // **field-access-then-call desugar**: no method `f` exists (a real method wins in
                // call position, checked above; in value position the field already wins, so the
                // two positions agree with `g = obj.f; g(x)` as the escape hatch when both exist).
                // A `Fn`-typed field is checked exactly like a call through a `Fn`-typed local
                // (same arity/argument checking, same `required = 0` because a function value does
                // not record defaults), and the call span is recorded so lowering emits field-get
                // + indirect call instead of method dispatch. A `dyn`/hole field stays deferred —
                // lowered as a field call, its misuse caught by the runtime's "not callable"
                // (E0007). A field of any other concrete type is statically not callable (E0007) —
                // the method table was already consulted, so nothing can resolve this at runtime.
                if let Type::Named(n, recv_args) = &recv
                    && let Some(fty) = self.record_field_type(n, name, recv_args)
                {
                    if !self.field_visible(n, name) {
                        self.report_private_field(n, name, FieldAccess::Read, span);
                    }
                    self.sites.field_call_sites.insert(call_span);
                    match fty {
                        Type::Fn { params, ret } => {
                            self.finalize_closure_args(&params, args, arg_exprs, env);
                            let erased_import = params.is_empty() && matches!(*ret, Type::Dyn);
                            if !erased_import {
                                self.check_args(&params, 0, args, arg_exprs, span, name);
                            }
                            return *ret;
                        }
                        t if t.defers_to_runtime() => {
                            self.finalize_closure_args(&[], args, arg_exprs, env);
                            return t;
                        }
                        t => {
                            self.finalize_closure_args(&[], args, arg_exprs, env);
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!(
                                    "field `{name}` of `{n}` has type `{t}` and is not callable"
                                ),
                            );
                            return Type::Unknown;
                        }
                    }
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
                if let Some(ret) =
                    self.bundle_method_call(&recv, name, args, arg_exprs, span, call_span)
                {
                    return ret;
                }
                let ret = self.method_call_return(&recv, name);
                // A method call on a CLOSED builtin type with no such built-in method is an error,
                // mirroring the non-indexable check (`42[0]`). `dyn`/holes defer (their result is
                // the deferred type, not `Unknown`), and a user `Named` type may resolve the call
                // through a trait at runtime — so both are left lenient; only the closed types are
                // flagged. Without this the runtime is left to raise E0005 ("no method `x` on
                // string") on a program the checker called clean — the editor shows nothing and
                // Run fails, which is exactly how the playground's `client.get(url)` (a `Result`,
                // missing its `?`) reached production looking correct.
                if matches!(ret, Type::Unknown) && closed_to_new_methods(&recv) {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("type `{recv}` has no method `{name}`"),
                    );
                }
                ret
            }
            _ => {
                let ty = self.synth(callee, env);
                // A computed callee whose static type is a FUNCTION type (`h["x"](1)`,
                // `mk()(1)`, `fns[0](1)`) is checked exactly as a `Fn`-typed local or field is:
                // the callee's type carries its parameter and return types whether it arrived
                // through a name or through an index/call, so where it came from cannot be what
                // decides whether the call is checked. Binding first (`f = h["x"]; f("nope")`)
                // already produced the argument E0007 via the `Ident` arm above — the two
                // spellings now agree, and the call's type is the function's declared return
                // rather than `Unknown` (which used to let the result flow into any annotation).
                // `required` is `0` for the same reason as those arms: a function *value* does not
                // record which parameters carry defaults, so only the upper arity bound is
                // enforced. Ordering matches the `Ident` arm (`Fn` before the object protocol);
                // the two are disjoint anyway — `synth_callable_object` answers only for
                // `Type::Named` — so no `Callable` receiver changes route.
                if let Type::Fn { params, ret } = ty {
                    let ret = *ret;
                    self.finalize_closure_args(&params, args, arg_exprs, env);
                    // The same erased selective-import placeholder the other `Fn` arms exempt:
                    // `fn() -> dyn` carries no real signature, so checking against it would
                    // reject every argument of a perfectly good call.
                    let erased_import = params.is_empty() && matches!(ret, Type::Dyn);
                    if !erased_import {
                        let label = callee_label(callee);
                        self.check_args(&params, 0, args, arg_exprs, span, &label);
                    }
                    return ret;
                }
                // Any other callee expression whose static type is a user OBJECT type — the
                // `Callable` protocol for computed callees (`make()(args)`, `pipeline[0](x)`).
                if let Some(ret) = self.synth_callable_object(&ty, args, arg_exprs, span, env) {
                    return ret;
                }
                // A `dyn`/hole callee stays deferred, its arguments unchecked — the dynamic
                // escape hatch, caught by the runtime's "not callable"/arity checks. Only the
                // statically-known `Fn` case above tightened.
                Type::Unknown
            }
        }
    }

    /// Type an OBJECT invoked as a value — the **`Callable` protocol** (`obj(args)` means
    /// `obj.call(args)`): when `recv` names a user type providing a `call` method, the call types
    /// against that method's signature (generics instantiate from the receiver's type arguments,
    /// exactly like an explicit method call). A known user type *without* a `call` method is
    /// statically not callable — E0007 with the protocol as the help — since the method set of a
    /// user type is closed. `None` when `recv` is not a resolvable user type (an extern type, a
    /// type parameter, `dyn`): those stay lenient/deferred exactly as before.
    pub(crate) fn synth_callable_object(
        &mut self,
        recv: &Type,
        args: &mut [Type],
        arg_exprs: &[CallArg],
        span: Span,
        env: &mut Env,
    ) -> Option<Type> {
        let Type::Named(n, recv_args) = recv else {
            return None;
        };
        if let Some(sig) = self
            .symbols
            .methods
            .get(&(n.clone(), "call".to_string()))
            .cloned()
        {
            return Some(
                self.call_user_method("call", &sig, args, arg_exprs, span, recv_args, None, env),
            );
        }
        // An **extern** type participates in the protocol exactly as a user type does (http arc
        // H10): a registered `call` method makes its values invocable. Extern types already join
        // every other protocol (`Display`, `Error`, `Equatable`); being uncallable was an
        // inconsistency, not a decision. This is what lets a native extension hand user code a
        // callable value — a middleware's `next` — without a parallel callback mechanism.
        // Resolved as a unit: a registered `call` whose parameters do not resolve would otherwise
        // be checked against an EMPTY signature, reporting "call expects 0 arguments" for a
        // perfectly good call instead of surfacing the inconsistency. Falling through keeps the
        // honest diagnostic below.
        if let Some(params) = stdlib::method_params(self.reg(), recv, "call") {
            let required =
                stdlib::method_required(self.reg(), recv, "call").unwrap_or(params.len());
            let result = stdlib::method_return(self.reg(), recv, "call").unwrap_or(Type::Dyn);
            self.finalize_closure_args(&params, args, arg_exprs, env);
            self.check_args(&params, required, args, arg_exprs, span, "call");
            return Some(result);
        }
        if self.symbols.types.contains(n) || self.symbols.enums.contains_key(n) {
            self.finalize_closure_args(&[], args, arg_exprs, env);
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("type `{n}` is not callable"),
            )
            .help(format!(
                "implement `Callable` with a `call` method to make a `{n}` invocable as `value(...)`"
            ));
            return Some(Type::Unknown);
        }
        None
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
        arg_exprs: &[CallArg],
        span: Span,
        recv_args: &[Type],
        call_span: Option<Span>,
        env: &mut Env,
    ) -> Type {
        // Named arguments on a METHOD, normalized into parameter order here for the same reason as
        // on a free function: everything downstream — generic seeding, closure finalization, arity
        // and assignability — then sees a plain positional call. Lowering keys the permutation off
        // the call span, so a call with no span to key (a synthesized or forwarded one) keeps its
        // written order and its labels are refused rather than silently ignored.
        let permuted;
        let arg_exprs = if noeta_ast::CallArg::any_named(arg_exprs) {
            let Some(call_span) = call_span else {
                self.error(
                    DiagnosticCode::InvalidArgument,
                    span,
                    format!("`{name}` cannot take named arguments at this call site"),
                );
                return sig.ret.clone();
            };
            let (names, types) = (sig.param_names.clone(), sig.params.clone());
            match self.order_arguments(
                arg_exprs,
                &names,
                &types,
                sig.required,
                name,
                args,
                span,
                call_span,
            ) {
                Some((a, _)) => {
                    permuted = a;
                    &permuted[..]
                }
                None => return sig.ret.clone(),
            }
        } else {
            arg_exprs
        };
        if let Some(generic) = &sig.generic {
            // Return-position seeding for a generic METHOD (D3): when this exact call sits in a
            // checked position, check-mode armed the expectation under the call's span — bind the
            // declared return against it, first-wins AFTER the receiver's own arguments (the
            // receiver stays authoritative for the class's parameters; the expectation fills what
            // it leaves open — typically the method's own).
            let mut seed: HashMap<String, Type> = generic
                .params
                .iter()
                .map(|(n, _)| n.clone())
                .zip(recv_args.iter().cloned())
                .filter(|(_, t)| !t.defers_to_runtime())
                .collect();
            if let Some((pending_span, expected)) = self.pending_member_ret.clone()
                && call_span == Some(pending_span)
            {
                let tps: HashSet<String> = generic.params.iter().map(|(n, _)| n.clone()).collect();
                bind_type_params(&generic.raw_ret, &expected, &tps, &mut seed);
            }
            return self.check_generic_call_seeded(
                name,
                generic,
                sig.required,
                args,
                arg_exprs,
                span,
                seed,
                // Methods never forward (the pinned D3 boundary), so no hidden site.
                None,
                env,
            );
        }
        let params = sig.params.clone();
        self.finalize_closure_args(&params, args, arg_exprs, env);
        self.check_args(&params, sig.required, args, arg_exprs, span, name);
        sig.ret.clone()
    }

    /// Type an **explicitly instantiated METHOD call** `recv.m::<U, ...>(args)` (generic
    /// methods, D3). The receiver is a value (instance method) or a bare type name (associated
    /// function); the named type arguments bind to the method's OWN type parameters in order —
    /// the class's parameters come from the receiver's type arguments, never the turbofish — and
    /// both substitutions compose through the one seeded generic-call machinery (the receiver's
    /// bindings first, the turbofish's next, arguments filling only what those leave open).
    /// Misapplied turbofish — an unknown method, a non-generic one, or a type-argument arity
    /// mismatch against the method's own parameters — is E0058.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn synth_typed_method_call(
        &mut self,
        recv: &Expr,
        name: &str,
        name_span: Span,
        type_args: &[TypeRef],
        args: &mut [Type],
        arg_exprs: &[CallArg],
        span: Span,
        env: &mut Env,
    ) -> Type {
        for t in type_args {
            self.check_type_ref(t);
        }
        let resolved: Vec<Type> = type_args
            .iter()
            .map(|t| from_ref_q(t, &self.imports.extern_types))
            .collect();
        // Resolve the receiver: a bare unshadowed TYPE name is the associated form (no receiver
        // instantiation); anything else synthesizes and must be a user type with methods.
        let (type_name, recv_args, associated) = match recv {
            Expr::Ident { name: tn, .. }
                if lookup(env, tn).is_none() && self.symbols.types.contains(tn) =>
            {
                (tn.clone(), Vec::new(), true)
            }
            _ => match self.synth(recv, env) {
                Type::Named(n, targs) => (n, targs, false),
                t if t.defers_to_runtime() => {
                    // A `dyn`/hole receiver cannot resolve a method signature to instantiate —
                    // the explicit turbofish demands a statically-known method (unlike a plain
                    // deferred method call, which stays lenient).
                    self.error(
                        DiagnosticCode::InvalidTypeArguments,
                        name_span,
                        format!(
                            "cannot instantiate `{name}` explicitly on a dynamically-typed \
                             receiver"
                        ),
                    )
                    .help("explicit type arguments need a statically-known method");
                    return Type::Unknown;
                }
                t => {
                    self.error(
                        DiagnosticCode::InvalidTypeArguments,
                        name_span,
                        format!("type `{t}` has no generic method `{name}`"),
                    )
                    .help("explicit type arguments apply only to a user type's generic method");
                    return Type::Unknown;
                }
            },
        };
        // A **call-site-typed extern method** (http arc H8) — `resp.json::<User>()`. Checked
        // before the user-generic path because the two spellings are identical: presence in the
        // receiver type's `typed_methods` table is the whole distinction. The associated form is
        // excluded (a typed method is an instance method; there is no receiver to dispatch on),
        // and so is the multi-argument spelling (the turbofish names ONE result type).
        if !associated
            && resolved.len() == 1
            && self.reg().find_typed_method(&type_name, name).is_some()
        {
            return self.synth_typed_extern_method(
                &type_name,
                &recv_args,
                name,
                name_span,
                &resolved[0],
                args,
                arg_exprs,
                span,
            );
        }
        let Some(sig) = self
            .symbols
            .methods
            .get(&(type_name.clone(), name.to_string()))
            .cloned()
        else {
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                name_span,
                format!("`{type_name}` has no method `{name}`"),
            );
            return Type::Unknown;
        };
        // The associated/instance discipline (E0047) holds for the turbofish form exactly as for
        // plain calls.
        let is_instance = self
            .symbols
            .method_instance
            .get(&(type_name.clone(), name.to_string()))
            .copied()
            .unwrap_or(true);
        if associated && is_instance {
            self.error(
                DiagnosticCode::InvalidReceiver,
                name_span,
                format!("`{name}` is an instance method of `{type_name}`"),
            )
            .help(format!(
                "call it on a value (`x.{name}::<...>(...)`), or pass `{type_name}.{name}` as a \
                 handle"
            ));
            return sig.ret.clone();
        }
        if !associated && !is_instance {
            self.error(
                DiagnosticCode::InvalidReceiver,
                name_span,
                format!("`{name}` is an associated function of `{type_name}`"),
            )
            .help(format!(
                "call it on the type: `{type_name}.{name}::<...>(...)`"
            ));
            return sig.ret.clone();
        }
        let Some(generic) = sig.generic.clone() else {
            self.finalize_closure_args(&sig.params, args, arg_exprs, env);
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                name_span,
                format!("`{name}` takes no type parameters"),
            )
            .help("drop the `::<...>` — only a generic method is instantiated explicitly");
            return sig.ret.clone();
        };
        let own = &generic.params[generic.class_params..];
        if resolved.len() != own.len() {
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                span,
                format!(
                    "`{name}` expects {} type argument(s), found {}",
                    own.len(),
                    resolved.len()
                ),
            );
            let tps: HashSet<String> = generic.params.iter().map(|(n, _)| n.clone()).collect();
            return erase_type_params(generic.raw_ret.clone(), &tps);
        }
        // The composed seed: the receiver's type arguments bind the class's parameters
        // (positionally, exactly as a plain instance call's), the turbofish binds the method's
        // own — both first-wins, so arguments can only fill what they leave open.
        let mut seed: HashMap<String, Type> = generic
            .params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(recv_args.iter().cloned())
            .filter(|(_, t)| !t.defers_to_runtime())
            .collect();
        for ((n, _), t) in own.iter().zip(resolved) {
            seed.entry(n.clone()).or_insert(t);
        }
        self.check_generic_call_seeded(
            name,
            &generic,
            sig.required,
            args,
            arg_exprs,
            span,
            seed,
            None,
            env,
        )
    }

    /// Type a **call-site-typed extern method** (http arc H8) — the `resp.json::<User>()` twin of
    /// the `json.parse::<User>(...)` module path, and structurally its mirror: resolve the
    /// turbofish into a [`noeta_ext_abi::TypeRecipe`], record it at the call span for lowering,
    /// then type the call from the method's declared signature and wrapper.
    ///
    /// Recording the recipe is what makes lowering emit a native `Rvalue::TypedMethodCall` instead
    /// of erasing the turbofish to a plain method call — the two paths are distinguished by this
    /// map alone.
    #[allow(clippy::too_many_arguments)]
    fn synth_typed_extern_method(
        &mut self,
        type_name: &str,
        recv_args: &[Type],
        name: &str,
        name_span: Span,
        t: &Type,
        arg_types: &[Type],
        arg_exprs: &[CallArg],
        span: Span,
    ) -> Type {
        // A type with no build recipe (an enum, a class, an unconstrained generic) cannot be
        // constructed at the call site. Same rule, same wording shape, as the module path.
        let has_recipe = match self.type_to_recipe(t) {
            Some(recipe) => {
                self.sites.typed_method_call_sites.insert(span, recipe);
                true
            }
            None => false,
        };
        match stdlib::typed_type_method(
            self.reg(),
            type_name,
            recv_args,
            name,
            arg_types,
            t.clone(),
        ) {
            Some((params, required, result)) => {
                self.check_args(&params, required, arg_types, arg_exprs, span, name);
                if !has_recipe {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`{t}` cannot be built by `{name}::<T>`"),
                    );
                }
                result
            }
            // Unreachable: the caller only routes here when `find_typed_method` already matched.
            None => {
                self.error(
                    DiagnosticCode::UnknownName,
                    name_span,
                    format!("`{name}::<T>(...)` is not a call-site-typed native method"),
                );
                t.clone()
            }
        }
    }

    /// Arity- and type-check a method call's arguments against the resolved parameter signature
    /// (a built-in method, a user method, or a trait object's declared contract); a deferred
    /// receiver or an unknown method is not checked.
    pub(crate) fn check_method_args(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        arg_exprs: &[CallArg],
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
        } else if let Type::DynTrait(tr) = recv
            && let Some((params, required, _)) = self.dyn_trait_method(tr, name)
        {
            // A `dyn Trait` call is typed by the trait's contract on the way out (`method_call_return`)
            // — so its arguments are checked against the same contract on the way in, exactly as the
            // bound receiver's are. Without this a trait object was the one receiver whose arguments
            // nothing checked: `g.greet(42)` against `fn greet(who: string)` passed `noeta check`.
            self.check_args(&params, required, args, arg_exprs, span, name);
        }
    }

    /// Check a call's argument count and types against the callable's parameter types, reporting
    /// at `span`. Lenient where either side defers to runtime (`dyn`/hole) and on numeric widening
    /// (`int` where `float` is expected), so polymorphic and numeric calls are not false positives.
    ///
    /// **A label that reaches here never bound.** Every callee that *can* honour one resolves its
    /// binding first, in [`Checker::order_arguments`], which permutes the list into parameter order
    /// and clears the labels on the way through. So a surviving `name:` means the callee had no
    /// parameter names to bind it against — a native or built-in function, whose parameters are
    /// positional — and the only honest answers are to bind it or to refuse it. It used to be
    /// neither: `math.pow(base: 2.0, exp: 3.0)` ran with both labels discarded, and
    /// `"abc".replace(zzz: "a", "b")` ran with a label naming nothing at all.
    pub(crate) fn check_args(
        &mut self,
        params: &[Type],
        required: usize,
        args: &[Type],
        arg_exprs: &[CallArg],
        span: Span,
        callee: &str,
    ) {
        self.reject_unbound_labels(arg_exprs, callee);
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
            if let Some(arg) = arg_exprs.get(i)
                && self.try_adapt_literal(&arg.value, param).is_some()
            {
                continue;
            }
            if !self.arg_assignable(arg, param) {
                let d = self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{param}`"),
                );
                // `number` is the one parameter type whose name does not list its members, so say
                // what it admits — once, here, rather than in the message every call site prints.
                if param.is_arith_numeric_union() {
                    d.help(
                        "`number` is any numeric scalar: `int`, `float`, `f32`, `f64`, and the \
                         fixed widths `i8`…`u64`",
                    );
                }
            }
        }
    }
}

/// Whether `ty` is a **closed** builtin type — one whose method table is complete at check time, so
/// "the lookup found nothing" is proof the method does not exist rather than merely that this pass
/// could not see it.
///
/// Closedness is the language's, not a convenience: `impl Trait for string` (and for every other
/// type here) is rejected with E0013 — "not a record, class, or enum declared in this module" — so
/// no user code can add a method to one of these after the fact. Native extensions *can*, but they
/// register into the same instance registry [`Checker::method_call_return`] already consulted, so
/// theirs are found too.
///
/// Deliberately excluded, and why:
/// - [`Type::Named`] — a declared or imported type, whose method may still arrive through a trait
///   impl or a runtime dispatch this pass cannot see. Lenient by design.
/// - [`Type::Dyn`], [`Type::Unknown`] and the other gradual holes — they never reach here, because
///   `method_call_return` answers them with the deferred type rather than `Unknown`.
/// - [`Type::DynTrait`] — resolved against the trait's declared methods, but a trait object's
///   dispatch is the runtime's call to make.
pub(crate) fn closed_to_new_methods(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Int
            | Type::IntN { .. }
            | Type::Float
            | Type::F32
            | Type::F64
            | Type::Bool
            | Type::Unit
            | Type::String
            | Type::Bytes
            | Type::List(_)
            | Type::Map(_, _)
            | Type::Set(_)
            | Type::Option(_)
            | Type::Result(_, _)
    )
}

/// A compact human label for a **computed callee** — the expression standing in for a name in an
/// arity diagnostic. A call through a name reports that name (`` `f` expects … ``); a computed
/// callee has none, so the shape it was computed from is reported instead (`h[…]`, `mk(…)`,
/// `obj.f`). Purely cosmetic: it never affects whether a call is accepted, only how the arity
/// message reads. Anything unrecognized falls back to a neutral description rather than an
/// invented name.
fn callee_label(callee: &Expr) -> String {
    match callee {
        Expr::Ident { name, .. } => name.clone(),
        Expr::Index { receiver, .. } => format!("{}[…]", callee_label(receiver)),
        Expr::Call { callee, .. } => format!("{}(…)", callee_label(callee)),
        Expr::Member { receiver, name, .. } => format!("{}.{name}", callee_label(receiver)),
        _ => "this function value".to_string(),
    }
}
