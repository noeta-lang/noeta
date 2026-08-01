//! THE BIDIRECTIONAL CORE, kept together on purpose: the mutually recursive
//! [`Checker::check`]/[`Checker::synth`] judgments and their dispatch bodies, plus subsumption
//! and literal adaptation. `check ↔ synth` mutual recursion IS the algorithm — splitting the
//! dispatch across files would trade one long file for hidden coupling, so the arm groups that
//! left this file are only the delegated siblings (`ops`/`calls`/`member`/`patterns`).

use crate::*;
use noeta_ast::{CallArg, ObjectLit, TypeOperand};

impl Checker {
    // ----- bidirectional judgments -----

    /// *Checking* mode: check `expr` against the `expected` type, returning the expression's
    /// actual type. Forms that can absorb an expectation propagate it inward (a list against
    /// `List<T>` checks each element against `T`; a closure against a function type adopts the
    /// expected parameter/return types); every other form synthesizes and is then subsumed.
    ///
    /// Callers pass real expectations here — a declared return at `return`, a parameter type at a
    /// call argument, a declared element type into a list/map literal — so the propagation arms
    /// below adopt the concrete type and [`Self::subsume`] enforces `actual <: expected`. Only a
    /// genuinely open position (e.g. `echo`) passes `Unknown`, where `check` reduces to bare
    /// [`Self::synth`].
    /// Check `expr` against `expected` (bidirectional position). Thin wrapper over
    /// [`Self::check_inner`] that, on the IDE path, records the result into the `expr_types`
    /// index — check-position expressions (an absorbed closure, an annotation-driven literal)
    /// previously never recorded, so hover and inlay hints missed them.
    pub(crate) fn check(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        // A NAMED object literal in a position that HAS an expectation publishes it, so the
        // construction can adopt type arguments its field values leave unconstrained (`r:
        // Repo<Todo> = Repo { tbl: "todos" }` — no field mentions `T`, so inference alone yields
        // `Repo` with no arguments, and the instance would record nothing to reflect). Keyed by the
        // literal's own span so a nested literal cannot pick up an outer expectation.
        let saved_expected_object = match expr {
            Expr::Object(lit) if lit.type_name.is_some() => Some(
                self.coloring
                    .expected_object
                    .replace((lit.span, expected.clone())),
            ),
            _ => None,
        };
        let ty = self.check_inner(expr, expected, env);
        if let Some(saved) = saved_expected_object {
            self.coloring.expected_object = saved;
        }
        self.note_if_never(expr, &ty);
        if self.config.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.symbols.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
    }

    /// Record an expression that types as the **bottom** — a call to something that does not
    /// return. Unconditional (unlike the `expr_types` index beside it): it is one insert on the rare
    /// expression that diverges, and both consumers — E0048's must-diverge analysis and the tier
    /// runners' shared-setup filter — need it on the ordinary compile path, not only under the IDE
    /// flag. See [`crate::sites::SiteMaps::never_exprs`].
    fn note_if_never(&mut self, expr: &Expr, ty: &Type) {
        if *ty == Type::Never {
            self.sites.never_exprs.insert(expr.span());
        }
    }

    pub(crate) fn check_inner(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        match expr {
            // A target-typed `.{ … }` absorbs the expected type's **name**: the one thing the source
            // elided. This is the only absorbing arm that changes an expression's *nominal identity*
            // rather than refining its element types, so it is deliberately narrow — the expectation
            // must be a concrete named record type. Everything else is E0023 rather than a guess:
            //   * a union (`Foo | Bar`) names no single type to adopt;
            //   * `Unknown`/`Dyn` is an open position with nothing to read (arms 8/9/10 guard the
            //     same way);
            //   * `?Foo` is **not** peeled. No literal form in this language implicitly lifts `T`
            //     into `?T` — `a: ?int = 5` is E0007 and `a: ?List<int> = [1,2,3]` likewise — so
            //     `.{ … }` stays consistent with `[…]` and the spelling is `some(.{ … })`.
            // A type name that is not a record (an enum, a class-only name, an extern type) also
            // falls through: `symbols.records` is exactly the set a field-initializer list can build.
            Expr::Object(lit) if lit.type_name.is_none() => {
                let name = match expected {
                    Type::Named(n, _) if self.symbols.records.contains_key(n) => n.clone(),
                    _ => {
                        // `Unknown`/`Dyn` is an *open* position, not a wrong one — it reached `check`
                        // only because some caller passes the top type through. Saying "the expected
                        // type `?` is not a struct" would be misleading, so it gets the same wording
                        // as the synthesis path: there is simply nothing here to infer from.
                        if matches!(expected, Type::Unknown | Type::Dyn) {
                            self.error(
                                DiagnosticCode::CannotInfer,
                                lit.type_name_span,
                                "cannot infer the type of `.{ … }` here: this position has no \
                                 expected type"
                                    .to_string(),
                            )
                            .help(
                                "name the type at the literal (`x = TypeName { … }`) or annotate \
                                 the position it flows into (`x: TypeName = .{ … }`)",
                            );
                        } else {
                            self.error(
                                DiagnosticCode::CannotInfer,
                                lit.type_name_span,
                                format!(
                                    "cannot infer the type of `.{{ … }}` here: the expected type \
                                     `{expected}` is not a single named struct type"
                                ),
                            )
                            .help(
                                "name the type at the literal (`TypeName { … }`); for an optional \
                                 expectation wrap it explicitly (`some(.{ … })`)",
                            );
                        }
                        for f in &lit.fields {
                            self.synth(&f.value, env);
                        }
                        if let Some(spread) = &lit.spread {
                            self.synth(spread, env);
                        }
                        return Type::Unknown;
                    }
                };
                let ty = self.synth_object_named(lit, &name, env);
                // The resolved name is the whole output of this arm, and it cannot be written back
                // into the AST (checking holds it by shared reference), so it travels to lowering
                // through a span-keyed side table — the same shape the other checker→IR hints use.
                // It gets its own map rather than riding `construction_sites`: that map is the
                // *reflection* hint and deliberately drops non-generic nominals
                // (`is_nongeneric_nominal`), so a plain `.{ … }` for a non-generic struct would
                // never be recorded there.
                self.sites.inferred_object_types.insert(lit.span, name);
                // Annotation-driven, exactly like the list/map arms: record the *expected* type as
                // the construction's reflected type so a generic instantiation the fields left
                // unconstrained still tags precisely.
                self.note_construction(expected, lit.span);
                ty
            }
            // A list literal absorbs an expected `List<T>`: check each element against `T`.
            Expr::List { items, span } if matches!(expected, Type::List(_)) => {
                let Type::List(elem) = expected else {
                    unreachable!()
                };
                for item in items {
                    self.check(item, elem, env);
                }
                self.note_packed_list(elem, *span);
                // Annotation-driven: record the *expected* element type (so `List<dyn> = [1,2,3]`
                // tags `List(Dyn)`, not the inferred `List(int)`).
                let ty = Type::List(elem.clone());
                self.note_construction(&ty, *span);
                ty
            }
            // An empty map literal absorbs an expected `Map<K, V>` (the map analogue of the list
            // arm); a non-empty map synthesizes its own element types and is then subsumed.
            Expr::Map { entries, span }
                if entries.is_empty() && matches!(expected, Type::Map(..)) =>
            {
                // Annotation-driven: record the *expected* map type (R1) so `Map<string, dyn> = {}`
                // tags `Map(String, Dyn)`, the map analogue of the list arm above.
                self.note_construction(expected, *span);
                expected.clone()
            }
            // A non-empty map literal absorbs an expected `Map<K, V>`: check each key against `K`
            // and each value against `V`, so heterogeneous values that are each a member of `V` (a
            // union, or `dyn`) are accepted instead of being cross-unified into a single element
            // type (`{"route": "/x", "status": 200}` against `Map<string, string|int|float|bool>`).
            // The map analogue of the list arm; the empty case is the preceding arm.
            Expr::Map { entries, span } if matches!(expected, Type::Map(..)) => {
                let Type::Map(kty, vty) = expected else {
                    unreachable!()
                };
                for (k, v) in entries {
                    self.check(k, kty, env);
                    self.check(v, vty, env);
                }
                let ty = Type::Map(kty.clone(), vty.clone());
                self.note_construction(&ty, *span);
                ty
            }
            // `none` absorbs an expected `Option<T>` (`?T`): it carries no payload, so it simply
            // adopts the expectation instead of leaking an inference hole.
            Expr::Ident { name, .. } if name == "none" && matches!(expected, Type::Option(_)) => {
                expected.clone()
            }
            // The polymorphic constructors absorb their expected algebraic type and check their
            // payload against the corresponding slot — so `some("x")` against `Option<int>` or
            // `Ok("x")` against `Result<int, _>` is now caught instead of deferring to a hole.
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "some")
                    && args.len() == 1
                    && matches!(expected, Type::Option(_)) =>
            {
                let Type::Option(inner) = expected else {
                    unreachable!()
                };
                self.check(&args[0].value, inner, env);
                expected.clone()
            }
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Ok")
                    && args.len() <= 1
                    && matches!(expected, Type::Result(..)) =>
            {
                let Type::Result(ok, _) = expected else {
                    unreachable!()
                };
                match args.first() {
                    Some(arg) => {
                        self.check(&arg.value, ok, env);
                    }
                    // `Ok()` carries a unit payload (`Result<void, E>`).
                    None => self.subsume(&Type::Unit, ok, expr.span()),
                }
                expected.clone()
            }
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Err")
                    && args.len() == 1
                    && matches!(expected, Type::Result(..)) =>
            {
                let Type::Result(_, err) = expected else {
                    unreachable!()
                };
                self.check(&args[0].value, err, env);
                expected.clone()
            }
            // A call of a generic user function absorbs the expected type through its RETURN
            // position (poly-values F2c): `r: Result<Order, JsonError> = load(text)` binds `T`
            // from the expectation via the same structural binding a call's arguments use, seeded
            // first-wins into the shared generic-call machinery — so the arguments can only fill
            // what the return leaves open. This is what lets a forwarding generic infer its
            // instantiation from an annotated binding without a turbofish.
            Expr::Call {
                callee: _,
                args,
                span,
            } if !matches!(expected, Type::Unknown | Type::Dyn)
                && self.seedable_generic_call(expr, env).is_some() =>
            {
                let (name, callee_span, generic) =
                    self.seedable_generic_call(expr, env).expect("guarded");
                let required = self.symbols.functions[&name].required;
                let tps: ParamSet = generic.params.iter().map(|(p, _)| p.id).collect();
                let mut seed: Subst = Subst::new();
                bind_type_params(&generic.raw_ret, expected, &tps, &mut seed);
                let actual = self.check_seeded_generic_call(
                    &name,
                    &generic,
                    required,
                    args,
                    callee_span,
                    *span,
                    seed,
                    env,
                );
                self.subsume(&actual, expected, expr.span());
                actual
            }
            // Return-position inference THROUGH `?` (poly-deferrals D1): the expected type of the
            // `?` EXPRESSION is the success-arm payload, and the callee's declared return names the
            // wrapper (`Result<T, E>` / `?T`) — so `o: Order = load(text)?` seeds `T = Order` by
            // binding the declaration's success arm against the expectation. The error arm still
            // resolves from the declaration (its `From`-conversion check at `?` runs unchanged via
            // the shared unwrap below), so E0057/`try_conversion_sites` behave exactly as in
            // synthesis position.
            Expr::Try { expr: inner, span }
                if !matches!(expected, Type::Unknown | Type::Dyn)
                    && self.seedable_generic_call(inner, env).is_some() =>
            {
                let (name, callee_span, generic) =
                    self.seedable_generic_call(inner, env).expect("guarded");
                let Expr::Call {
                    args,
                    span: call_span,
                    ..
                } = inner.as_ref()
                else {
                    unreachable!("seedable_generic_call matches plain calls only")
                };
                let required = self.symbols.functions[&name].required;
                let tps: ParamSet = generic.params.iter().map(|(p, _)| p.id).collect();
                let mut seed: Subst = Subst::new();
                // Bind the declared SUCCESS arm against the expectation; a declared return that is
                // not a `Result`/`Option` leaves the seed empty (the unwrap below then reports the
                // ordinary `?`-misuse, exactly as synthesis position would).
                match &generic.raw_ret {
                    Type::Result(ok, _) => bind_type_params(ok, expected, &tps, &mut seed),
                    Type::Option(some) => bind_type_params(some, expected, &tps, &mut seed),
                    _ => {}
                }
                let wrapped = self.check_seeded_generic_call(
                    &name,
                    &generic,
                    required,
                    args,
                    callee_span,
                    *call_span,
                    seed,
                    env,
                );
                let actual = self.try_unwrap(&wrapped, *span);
                self.subsume(&actual, expected, expr.span());
                actual
            }
            // The same success-arm seeding in `??` fallback position (poly-deferrals D1):
            // `o: Order = load(text) ?? default` — the expectation reaches the call through the
            // coalesce's payload, and the fallback must satisfy the same expectation.
            Expr::Coalesce {
                value, fallback, ..
            } if !matches!(expected, Type::Unknown | Type::Dyn)
                && self.seedable_generic_call(value, env).is_some() =>
            {
                let wrapped = self.check_coalesce_seeded(value, expected, env);
                self.check(fallback, expected, env);
                let actual = match wrapped {
                    Type::Result(ok, _) => *ok,
                    Type::Option(some) => *some,
                    _ => Type::Unknown,
                };
                self.subsume(&actual, expected, expr.span());
                actual
            }
            // A closure absorbs an expected function type: an explicit parameter annotation wins,
            // otherwise the parameter adopts the expected type; the body is checked against the
            // expected return.
            Expr::Closure {
                params,
                ret: ann,
                body,
                span: closure_span,
            } if matches!(expected, Type::Fn { .. }) => {
                let Type::Fn {
                    params: expected_params,
                    ret,
                } = expected
                else {
                    unreachable!()
                };
                // A closure default is evaluated in the captured (enclosing) scope, so validate it
                // against `env` before the parameter frame is pushed.
                self.validate_param_defaults(params, env);
                self.check_param_attrs(params);
                env.push(HashMap::new());
                // Each parameter's bound type: an explicit annotation wins, else the expectation.
                // KEPT for the closure's own type below — returning `param_type` here used to
                // forget the absorption, leaving the recorded closure `(dyn) -> R` even when the
                // parameters were known (the dyn-closure gap's second half).
                let bound: Vec<Type> = params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        p.ty.as_ref()
                            .map(|t| self.annot(t))
                            .or_else(|| expected_params.get(i).cloned())
                            .unwrap_or(Type::Unknown)
                    })
                    .collect();
                for (p, pty) in params.iter().zip(&bound) {
                    self.check_reserved_name(&p.name, p.name_span);
                    // Closure params land in the just-pushed frame — any env hit is a shadow
                    // (E0059), enclosing capture and same-list duplicate alike.
                    self.check_shadow(&p.name, p.name_span, env, crate::ShadowScopes::All);
                    bind(env, &p.name, pty.clone());
                }
                // An explicit return annotation is the body's expected type and the closure's return
                // type; it must also satisfy the context's expected return. Without one the expected
                // return drives the body — UNLESS that expectation is `dyn`, the builtin "any
                // result" shape (`map` expects `(T) -> dyn`): checking against `dyn` would erase the
                // body's real type and starve the call-site refinements (`xs.map(f) → List<R>`), so
                // the body is inferred instead; `dyn` accepts whatever comes out.
                let declared = ann.as_ref().map(|t| self.annot(t));
                let body_expected = declared
                    .clone()
                    .or_else(|| (!matches!(**ret, Type::Dyn)).then(|| (**ret).clone()));
                let body_ty = self.closure_body_type(body, body_expected.as_ref(), env);
                env.pop();
                if let Some(declared) = &declared {
                    self.subsume(declared, ret, *closure_span);
                }
                Type::Fn {
                    params: bound,
                    ret: Box::new(declared.unwrap_or(body_ty)),
                }
            }
            // A `match` in a checked position pushes the expectation THROUGH into every arm, so an
            // arm is checked against exactly the type the whole expression is. Without this the
            // expectation stopped at the `match` and each arm synthesized blind, so a form that can
            // only be typed against an expectation — a mixed `{"type": "array", "n": 1}` against
            // `Map<string, dyn>`, an empty `{}`/`[]`, a `.{ … }` — worked after `return` but not
            // inside a `match` arm, forcing the author to lift each arm into its own function purely
            // so the literal had a return type to read.
            //
            // `if c then a else b` is parsed as a desugared `match`, so both of its branches ride
            // this same arm.
            //
            // Guarded on a *real* expectation: `Unknown`/`Dyn` is an open position with nothing to
            // push (the sibling absorbing arms guard identically), and a statement-position `match`
            // never reaches `check` at all — it routes through `synth_match` with `value_used`
            // false — so neither behavior changes.
            Expr::Match {
                scrutinee,
                arms,
                span,
            } if !matches!(expected, Type::Unknown | Type::Dyn) => {
                // No outer `subsume` here, deliberately: every arm was just checked against this
                // same expectation, so a mismatching arm has already reported at ITS span. Re-testing
                // the joined result would only report the identical mismatch a second time, on the
                // whole `match` — the exact double-diagnostic the arm-level span is meant to replace.
                self.match_type(scrutinee, arms, *span, env, true, Some(expected))
            }
            // A bare numeric literal adapts into a fixed-width context — `x: u8 = 200`, `y: i8 = -5`,
            // `z: f32 = 1.5`, `w: f64 = 1.5` (P-NUM-SYM). Shared with call-argument checking via
            // `try_adapt_literal`; a non-adapting pair falls through to synthesize-and-check.
            _ => {
                if let Some(adapted) = self.try_adapt_literal(expr, expected) {
                    return adapted;
                }
                // A polymorphic named function in value position instantiates against the expected
                // `Fn` type (F1, poly-values): `f: (int) -> int = double_generic` and
                // `results.map(Ok)` see the precise monomorphic signature instead of the erased
                // one. Subsumption still runs, so an incompatible instantiation reports.
                if let Expr::Ident { name, span } = expr
                    && let Some(fn_ty) =
                        self.instantiate_fn_value(name.as_str(), expected, *span, env)
                {
                    self.subsume(&fn_ty, expected, *span);
                    return fn_ty;
                }
                // A METHOD call in a checked position absorbs the expectation through its RETURN
                // (generic methods, D3 — the method twin of the F2c free-fn arm): arm the pending
                // expectation, keyed by THIS call's span, for `call_user_method` to seed a generic
                // method's instantiation with; consumed only on an exact span match, and cleared
                // unconditionally after synthesis so it can never leak into a sibling call.
                if let Expr::Call { callee, span, .. } = expr
                    && matches!(callee.as_ref(), Expr::Member { .. })
                    && !matches!(expected, Type::Unknown | Type::Dyn)
                {
                    self.pending_member_ret = Some((*span, expected.clone()));
                    let actual = self.synth(expr, env);
                    self.pending_member_ret = None;
                    self.subsume(&actual, expected, expr.span());
                    return actual;
                }
                let actual = self.synth(expr, env);
                self.subsume(&actual, expected, expr.span());
                actual
            }
        }
    }

    /// If `expr` is a bare numeric literal that adapts into the fixed-width `expected` type — an
    /// integer literal (optionally negated) into an in-range [`Type::IntN`], or a float literal into
    /// [`Type::F32`]/[`Type::F64`] — perform the adaptation and return the adapted type. Range-checks
    /// an `IntN` (E0044 out of range) and records the `f32` narrowing site so lowering emits a
    /// `Const::F32`. Returns `None` for any non-adapting pair. Shared by binding checks (`mut x: T =
    /// …`) and call-argument checks (`f(…)`) so a bare `5`/`1.5` flows into an `i64`/`f32`/`f64`
    /// identically in both positions. (A *suffixed* literal like `200u8`/`1.5f32` is its own
    /// `Expr::IntN`/`Expr::F32`, already the fixed-width type — it never reaches here.)
    pub(crate) fn try_adapt_literal(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        match expected {
            Type::IntN { signed, bits } => {
                let is_int_literal = matches!(expr, Expr::Int { .. })
                    || matches!(
                        expr,
                        Expr::Unary {
                            op: UnaryOp::Neg,
                            ..
                        }
                    );
                if !is_int_literal {
                    return None;
                }
                let value = int_literal_value(expr)?;
                let (lo, hi) = Self::int_width_range(*signed, *bits);
                if value < lo || value > hi {
                    self.error(
                        DiagnosticCode::FixedWidthOutOfRange,
                        expr.span(),
                        format!(
                            "literal `{value}` is out of range for `{expected}` (valid range {lo}..={hi})"
                        ),
                    );
                }
                Some(expected.clone())
            }
            // `f64` is bit-identical to `float`, so no narrowing is needed — only the static type.
            Type::F64 if matches!(expr, Expr::Float { .. }) => Some(Type::F64),
            // `f32` is a distinct 32-bit representation; record the site so lowering narrows it.
            Type::F32 if matches!(expr, Expr::Float { .. }) => {
                if let Expr::Float { span, .. } = expr {
                    self.sites.f32_literal_sites.insert(*span);
                }
                Some(Type::F32)
            }
            _ => None,
        }
    }

    /// Subsumption: require `actual <: expected`. A violation is a type mismatch (`E0007`, the
    /// same code the arithmetic/runtime mismatch path uses). An inference hole on either side
    /// makes [`Type::subtype`] hold, so a not-yet-inferred interior type never produces a false
    /// positive — the deliberate residual tolerance (holes are removed at typed boundaries, not
    /// here).
    /// Whether `name` is a declared (or prelude) type of `kind` — the registry-dependent half of the
    /// abstract kind-type membership rule the pure lattice cannot decide.
    pub(crate) fn is_of_kind(&self, name: &str, kind: noeta_types::TypeKind) -> bool {
        self.symbols.type_kinds.get(name) == Some(&kind)
    }

    /// Kind-aware assignability: `actual <: expected`, extending [`Type::subtype`] with the one rule
    /// it cannot decide on its own — a concrete `Named(n)` widens into an abstract `Kind(k)` when
    /// `n` is a declared type of kind `k`. Recurses through the covariant containers and unions so
    /// the rule composes (`List<WebRole> <: List<Enum>`); every non-kind case delegates to the pure
    /// lattice. This is the single funnel for assignment, argument, return, and field checks.
    pub(crate) fn assignable(&self, actual: &Type, expected: &Type) -> bool {
        // A **trait object** `dyn Trait` (L1 user traits, UT4) — a registry-dependent membership rule
        // like `Kind`, decided here rather than in the pure lattice. An implementor widens into it; a
        // `dyn`/hole defers; a `dyn Trait` widens into bare `dyn` (or the same trait object). This is
        // the direct/element-wise coercion the common cases (a `dyn Trait` parameter, an annotated
        // `List<dyn Trait>` literal checked element-by-element) go through.
        if let Type::DynTrait(tr) = expected {
            return match actual {
                Type::DynTrait(a) => a == tr,
                Type::Named(n, _) => self.type_impls_trait(n, tr),
                other => other.defers_to_runtime(),
            };
        }
        if let Type::DynTrait(_) = actual {
            return matches!(expected, Type::Dyn)
                || actual == expected
                || expected.defers_to_runtime();
        }
        // The pure subtype lattice, plus the one registry-dependent rule it defers: whether a
        // `Named(n)` is a member of an abstract `Kind(k)`. Threading it through [`Type::subtype_with`]
        // reaches every nested covariant position without re-implementing the variance walk here.
        Type::subtype_with(actual, expected, &|n, k| self.is_of_kind(n, k))
    }

    /// Whether the named type `n` implements the user trait `tr` (L1, UT4) — a recorded in-body or
    /// standalone `impl`. The membership rule behind `dyn Trait` coercion.
    fn type_impls_trait(&self, n: &str, tr: &str) -> bool {
        self.symbols
            .user_trait_impls
            .get(n)
            .is_some_and(|s| s.contains_key(tr))
    }

    /// Whether an argument of type `arg` may be passed where `param` is expected — the kind-aware
    /// counterpart of the free [`arg_compatible`]. A `dyn`/hole on either side defers to the runtime;
    /// otherwise the argument must be assignable to the parameter under the strict subtype lattice.
    /// There is **no** numeric-widening leniency: an `int` is not accepted where a `float` is expected
    /// (write `f(2.0)`, not `f(2)`), matching every other typed boundary — a binding, a return, a list
    /// element — where `int → float` is already rejected, and so an inlay-hinted parameter type is a
    /// promise the caller must meet.
    pub(crate) fn arg_assignable(&self, arg: &Type, param: &Type) -> bool {
        self.assignable(arg, param) || arg.defers_to_runtime() || param.defers_to_runtime()
    }

    /// Whether `expr` is a **literal form that would absorb** `expected` — whether one of
    /// [`Self::check_inner`]'s absorbing arms fires for this exact pair.
    ///
    /// Checking mode is only worth entering when the expectation actually reaches the literal. For
    /// a *reassignment* (`Stmt::Binding`'s un-annotated arm) that distinction is what keeps the
    /// tailored `E0007` — the one that names the binding and offers the union — as the single
    /// report on a mismatch: an expectation the value cannot absorb would be enforced anonymously
    /// by [`Self::subsume`] first, and then reported a second time by the reassignment's own check.
    ///
    /// Deliberately literal-only. A call, a `?`, and a `??` also have absorbing arms above, but
    /// each ends in its own `subsume`, so routing a reassignment through them buys precision the
    /// reassignment check already provides, at the cost of that double report. A literal arm
    /// returns the expectation unchanged and reports only about its *elements*, which is strictly
    /// more precise than one message about the whole value.
    ///
    /// Kept adjacent to the arms it mirrors: a new absorbing literal arm needs a line here.
    pub(crate) fn absorbs_expectation(&self, expr: &Expr, expected: &Type) -> bool {
        match expr {
            Expr::List { .. } => matches!(expected, Type::List(_)),
            Expr::Map { .. } => matches!(expected, Type::Map(..)),
            Expr::Ident { name, .. } => name == "none" && matches!(expected, Type::Option(_)),
            Expr::Closure { .. } => matches!(expected, Type::Fn { .. }),
            // A target-typed `.{ … }` absorbs the expected type's *name*, and only a concrete
            // named record type supplies one.
            Expr::Object(lit) => {
                lit.type_name.is_none()
                    && matches!(expected, Type::Named(n, _) if self.symbols.records.contains_key(n))
            }
            Expr::Call { callee, args, .. } => match callee.as_ref() {
                Expr::Ident { name, .. } if name == "some" => {
                    args.len() == 1 && matches!(expected, Type::Option(_))
                }
                // `Ok()` is the unit-payload form, so zero or one argument.
                Expr::Ident { name, .. } if name == "Ok" => {
                    args.len() <= 1 && matches!(expected, Type::Result(..))
                }
                Expr::Ident { name, .. } if name == "Err" => {
                    args.len() == 1 && matches!(expected, Type::Result(..))
                }
                _ => false,
            },
            _ => false,
        }
    }

    pub(crate) fn subsume(&mut self, actual: &Type, expected: &Type, span: Span) {
        if !self.assignable(actual, expected) {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("expected `{expected}`, found `{actual}`"),
            );
        }
    }

    // ----- synthesis -----

    /// Synthesize an expression's type. Thin wrapper over [`Self::synth_inner`] that, on the IDE
    /// path ([`Self::record_expr_types`]), records the result into the `expr_types` index for hover.
    /// Every expression — and every subexpression, since the checker recurses through here — flows
    /// through this one choke point, so the index covers the whole tree with a single insertion site.
    pub(crate) fn synth(&mut self, expr: &Expr, env: &mut Env) -> Type {
        let ty = self.synth_inner(expr, env);
        self.note_if_never(expr, &ty);
        if self.config.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.symbols.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
    }

    /// Reject a name-keyed reflection turbofish whose type is an **erased type parameter**, and
    /// report whether it did.
    ///
    /// These queries are keyed on a type NAME, and a type parameter has no name at run time: generics
    /// are erased, so one compiled body serves every instantiation and `T` is only ever the literal
    /// three characters `T`. Nothing is registered under that key, so `field_specs_of::<T>()` inside
    /// `fn count_of<T>()` matched nothing and returned the EMPTY schema — indistinguishable from a
    /// real type that happens to have no fields, and with no diagnostic at all. `construct::<T>(…)`
    /// answered `Err` the same way. That silent wrong answer is the whole reason this is an error.
    ///
    /// Reported as `E0058` — the code for a turbofish instantiation that cannot apply — because that
    /// is exactly what this is: the type argument is well-formed and in scope, it simply cannot serve
    /// this application. The alternative is always available and is what the help points at: reflect
    /// where the type is concrete and pass the result in.
    ///
    /// Only the **head** is rejected, matching [`TypeRef::head_name`] — what the query actually keys
    /// on. `field_specs_of::<List<T>>()` heads at `List`, a real type with no field schema, and keeps
    /// its honest empty answer.
    ///
    /// The turbofish arm stays a **compile-time** key for every type that HAS one: it resolves like
    /// an annotation, follows a `namespace`/`use … as`/rename, and folds to a constant. A bare type
    /// parameter has no such key — but where one of the two per-instantiation channels carries the
    /// instantiation's name into the body, the surface resolves it at run time instead
    /// ([`Self::check_type_operand`], [`Self::record_type_param`]), and never reaches here. So this
    /// function now reports exactly the residue: a parameter **no channel** delivers. There is no
    /// "compose it yourself out of `type_name`" advice left to give, because `type_name::<T>()`
    /// would fail in the very same places for the very same reason — the fix is always to get the
    /// instantiation to this body, which is what each branch below says how to do.
    fn reject_erased_type_param(&mut self, ty: &TypeRef, surface: &str) -> bool {
        let TypeRef::Named { args, span, .. } = ty else {
            return false;
        };
        if !args.is_empty() {
            return false;
        }
        let Type::Param(param) = self.annot(ty) else {
            return false;
        };
        let name = &param.name;
        // A parameter of the ENCLOSING generic type that this site cannot reach. The instantiation
        // DOES exist at run time — it is on the receiver's reflected type tag — so the blanket
        // "generics are erased" would send the author looking for a fix that is not the one. Two
        // distinct reasons, each with its own fix, so each says which it is:
        //
        //   * this member has no receiver to read the tag from (an associated function, or a
        //     method whose body never touches `self` — which is what makes it associated);
        //   * the name is shadowed by the METHOD's own type parameter, which is a different `T`
        //     entirely and has no per-call channel of its own.
        //
        // `self_type_params` is non-empty exactly inside an instance method of a generic type, and
        // holds a blank in the slot of any parameter the method's own `<…>` shadows — so "in scope
        // on the type, but not reachable here" splits cleanly on it.
        //
        // A self-less member whose class parameter DOES reach it — through the hidden type-argument
        // slot, which is that member's only channel — is excluded here: the callers resolve it
        // through [`Self::record_type_param`] before ever asking this function, so it does not
        // reach the diagnostic at all. The guard stays because the two are different questions —
        // `forwardable_params` is the *capability*, `current_forwarding` the realized layout — and
        // where they disagree the blanket branch's advice is the honest one, while "this member has
        // no receiver" would be true and useless.
        if let Some(owner) = self.coloring.current_type.clone()
            && !self.coloring.forwardable_params.contains(&param)
            && self
                .symbols
                .generic_types
                .get(&owner)
                .is_some_and(|ps| ps.iter().any(|p| p.name == *name))
        {
            let shadowed = !self.coloring.self_type_params.is_empty();
            let msg = if shadowed {
                format!(
                    "`{surface}` cannot reflect over `{name}` here: this method declares its own \
                     `{name}`, which shadows `{owner}`'s and is erased"
                )
            } else {
                format!(
                    "`{surface}` cannot reflect over `{name}` here: this member of `{owner}` has \
                     no receiver, and `{name}` is carried by the instance"
                )
            };
            let help = if shadowed {
                format!(
                    "rename the method's parameter if you meant `{owner}`'s `{name}` (an \
                     instance's own type arguments are recorded at construction and reachable \
                     from `self`); a method's own type parameter has no such channel"
                )
            } else {
                format!(
                    "an instance of a generic type records its type arguments at construction, so \
                     `{surface}::<{name}>()` resolves in a method that takes `self` — read a field \
                     of `self` (or take the value as a parameter and reflect at the call site)"
                )
            };
            self.error(DiagnosticCode::InvalidTypeArguments, *span, msg)
                .help(help);
            return true;
        }
        // Neither channel reaches this body — not the receiver's reflected tag, not the hidden
        // type-argument slot — so there is no runtime name to route to and no composition out of
        // `type_name` that would do better: `type_name::<T>()` here is this same error. The only
        // fix is to reflect where the instantiation IS known and hand the answer in. (This used to
        // point at `{surface}(type_name::<T>())` where the slot was open; the turbofish arm now
        // takes that route itself, so reaching here means the route is shut.)
        let help = format!(
            "reflect where the type is concrete and pass the result in — give this function a \
             parameter for it and let the caller supply `{surface}::<TheRealType>`"
        );
        self.error(
            DiagnosticCode::InvalidTypeArguments,
            *span,
            format!(
                "`{surface}` cannot reflect over the type parameter `{name}` — generics are \
                 erased, so `{name}` names no type at run time"
            ),
        )
        .help(help);
        true
    }

    /// Record the run-time channel that carries a **type parameter's instantiation NAME** into the
    /// body being checked at `span`, and report whether one reaches it at all.
    ///
    /// Generics are erased — one compiled body serves every instantiation — so a parameter is only
    /// *nameable* at run time where some channel delivers the instantiation per call. The language
    /// has exactly two, and this is the single place that decides between them, so every
    /// name-keyed surface over a parameter (`type_name::<T>()`, `v.as<T>()`, `v is T`) answers with
    /// the same `T`:
    ///
    ///   * a parameter of the enclosing generic **TYPE**, inside one of its instance methods — it
    ///     rides the receiver's reflected type tag, stamped at the construction site
    ///     ([`SiteMaps::self_type_arg_sites`](crate::sites::SiteMaps));
    ///   * a parameter of the enclosing generic **fn or method** — it rides the hidden
    ///     type-argument slot that also carries `json.try_parse::<T>`'s decode recipe, of which
    ///     this reads only the NAME ([`SiteMaps::forwarded_slot_sites`](crate::sites::SiteMaps)).
    ///
    /// The receiver is consulted **first**: inside an instance method of a generic type the class's
    /// parameters deliberately take no hidden slot (two channels for one fact would let a call
    /// through a receiverless entry point supply nothing where the tag was right there), so the tag
    /// is the only one populated there and the slot list holds the *method's own* parameters.
    ///
    /// `false` means neither reaches this body — the caller reports it, because what to *say* about
    /// it depends on the surface.
    fn record_type_param(&mut self, param: &ParamRef, span: Span) -> bool {
        if let Some(i) = self
            .coloring
            .self_type_params
            .iter()
            .position(|p| p.as_ref() == Some(param))
        {
            let owner = self.coloring.current_type.clone().unwrap_or_default();
            self.sites
                .self_type_arg_sites
                .insert(span, (owner, i as u32));
            return true;
        }
        if let Some(idx) = self
            .coloring
            .current_forwarding
            .iter()
            .position(|t| matches!(t, Type::Param(p) if p == param))
        {
            self.sites.forwarded_slot_sites.insert(span, idx as u32);
            return true;
        }
        false
    }

    /// The first type parameter a **narrow's** target mentions in a position the runtime match
    /// actually *tests*, other than a bare head — the composite shapes a head-constructor match
    /// cannot express.
    ///
    /// A narrow tests the head constructor and, for a parametrized target, its reflected type
    /// arguments (R3) — recursing through a union, whose members are each tested. Every other
    /// position is **erased** by the match and so cannot be wrong about a parameter: an
    /// `?T`/`(T, int)`/`fn(T)` target checks only "is an `Option`" / "is a tuple" / "is callable",
    /// exactly as it does for a concrete `?int`, and a `Self::Name` projection stays the permissive
    /// top. Those are left alone; only a *tested* position is reported.
    fn narrow_tested_param(&self, ty: &TypeRef) -> Option<String> {
        match ty {
            TypeRef::Union { members, .. } => members.iter().find_map(|m| {
                match m {
                    // A union member's head is itself tested, so a bare parameter there counts —
                    // unlike the whole target's head, which the caller resolves through a channel.
                    TypeRef::Named { name, args, .. }
                        if args.is_empty()
                            && self.coloring.type_params.contains_key(name.as_str()) =>
                    {
                        Some(name.to_string())
                    }
                    other => self.narrow_tested_param(other),
                }
            }),
            TypeRef::Named { name, args, .. } => {
                if !args.is_empty() && self.coloring.type_params.contains_key(name.as_str()) {
                    return Some(name.to_string());
                }
                args.iter().find_map(|a| match a {
                    TypeRef::Named { name, args, .. }
                        if args.is_empty()
                            && self.coloring.type_params.contains_key(name.as_str()) =>
                    {
                        Some(name.to_string())
                    }
                    other => self.narrow_tested_param(other),
                })
            }
            // Erased by the head-constructor match — see the doc above.
            _ => None,
        }
    }

    /// Check a **narrowing target** (`x.as<T>()`, `x is T`) that names a type parameter, wiring the
    /// site to the channel that carries the instantiation's name — or reporting `E0058` where none
    /// can.
    ///
    /// A narrow is a head-constructor match on the target's runtime **name**
    /// ([`noeta_ast::Expr::As`]), which is exactly what `type_name::<T>()` answers with, so a
    /// parameter target needs no more than that surface already has and rides the very same two
    /// channels ([`Self::record_type_param_name`]). Left unwired it matched the literal letter `T`,
    /// which nothing is ever registered under, and `.as<T>()` answered `none` — and `x is T`
    /// `false` — for every value, with no diagnostic. That silent wrong answer is why the
    /// unresolvable cases below are errors rather than a permissive miss.
    ///
    /// Reported as `E0058` — a type argument that is well-formed and in scope but cannot serve this
    /// application — the same code [`Self::reject_erased_type_param`] and the forwarding call sites
    /// raise for the same situation.
    /// `span` is the **whole narrowing expression's** span — the key lowering looks the site up
    /// under, since that is the node it emits the name atom beside. The diagnostics point at the
    /// target type instead, which is the part the author would change.
    fn check_narrow_target(&mut self, ty: &TypeRef, span: Span, surface: &str) {
        // The target's own head is the parameter: the resolvable shape, and the only one.
        if let TypeRef::Named { args, .. } = ty
            && args.is_empty()
            && let Type::Param(param) = self.annot(ty)
        {
            if self.record_type_param(&param, span) {
                return;
            }
            let name = &param.name;
            let span = &ty.span();
            let owner = self.coloring.current_type.clone();
            let help = match &owner {
                Some(owner) => format!(
                    "an instance of a generic type records its type arguments at construction, so \
                     `{surface}` over `{owner}`'s `{name}` resolves in a method that takes `self` \
                     — read a field of `self`, or declare `{name}` as this member's own type \
                     parameter so the call site supplies it"
                ),
                None => format!(
                    "declare `{name}` as this function's own type parameter and spell the \
                     turbofish at the call site — a nested `fn`'s own parameter has no per-call \
                     channel, and neither does a parameter this body only inherited by name"
                ),
            };
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                *span,
                format!(
                    "`{surface}` cannot narrow to the type parameter `{name}` here: a narrow \
                     matches the instantiation's runtime name, and no channel carries `{name}`'s \
                     into this body"
                ),
            )
            .help(help);
            return;
        }
        // A parameter the match would TEST but cannot name: one narrow reads one name, so a
        // composite has nowhere to put the rest. Refused rather than answered — the arguments are
        // compared against the value's reflected tag, where a bare `T` matches nothing at all.
        if let Some(param) = self.narrow_tested_param(ty) {
            let target = self.annot(ty);
            self.error(
                DiagnosticCode::InvalidTypeArguments,
                ty.span(),
                format!(
                    "`{surface}` cannot narrow to `{target}`: the type parameter `{param}` sits in \
                     a position the runtime match tests, and a narrow resolves one name — the \
                     target's head"
                ),
            )
            .help(format!(
                "narrow to the head first and check the payload yourself — `{surface}` over the \
                 bare `{param}` resolves per instantiation, an argument position does not"
            ));
        }
    }

    /// Refuse a **`match` arm's** `is T` whose target names a type parameter (`Pattern::IsType`).
    ///
    /// The pattern form shares the runtime matcher with the expression form but not its operand
    /// plumbing, and cannot: a pattern is tested in place, and the IR reuses the *AST*
    /// [`noeta_ast::Pattern`] verbatim, so there is no operand position an instantiation's name could
    /// be computed into — the expression form carries exactly one such position (`Rvalue::As`'s
    /// `dynamic`) and that is what makes it resolvable. Rather than invent a third way to deliver a
    /// type argument, the arm says so and points at the form that works.
    ///
    /// Left alone it matched the letter `T`, so the arm was simply never taken — the same silent
    /// wrong answer `.as<T>()` gave, and reported for the same reason.
    pub(crate) fn reject_type_param_pattern(&mut self, ty: &TypeRef) {
        let param = match ty {
            TypeRef::Named { name, args, .. }
                if args.is_empty() && self.coloring.type_params.contains_key(name.as_str()) =>
            {
                name.to_string()
            }
            other => match self.narrow_tested_param(other) {
                Some(p) => p,
                None => return,
            },
        };
        let target = self.annot(ty);
        self.error(
            DiagnosticCode::InvalidTypeArguments,
            ty.span(),
            format!(
                "an `is {target}` arm cannot match the type parameter `{param}`: a pattern is \
                 tested in place, so there is nowhere to resolve `{param}`'s instantiation into"
            ),
        )
        .help(format!(
            "use the expression form, which does resolve it: \
             `match v.as<{param}>() {{ some(x) => …, none => … }}`"
        ));
    }

    /// Check a name-keyed reflection surface's type operand (`field_specs_of`, `construct`).
    ///
    /// The **turbofish** arm carries a real `TypeRef`, so it is resolved like any other type
    /// annotation — a name that resolves to nothing is an E0013, not a silent empty schema. That is
    /// only possible because the type stays a `TypeRef` through the linker: the checker runs on the
    /// already-qualified program, so `field_specs_of::<Todo>()` under `namespace app.storage` looks
    /// up `app.storage.Todo` and a native extern spelled by its qualified path
    /// (`field_specs_of::<std.test.Skip>()`) resolves without a `use`, exactly as
    /// `attributes_of::<T>()` already did. Leniency about the *answer* is untouched: a type that
    /// resolves but has no field schema (an enum, a `dyn` trait) still yields the empty list.
    ///
    /// The **dynamic** arm is an ordinary expression that must be a `string`. Nothing is resolved
    /// there — its value is only known at runtime, which is the whole point of the surface.
    ///
    /// A turbofish naming a **bare type parameter of an enclosing generic** is neither: the type is
    /// erased, so there is no compile-time key, but the instantiation's NAME does reach the body
    /// through one of the two per-instantiation channels ([`Self::record_type_param`]) — which is
    /// all these registries key on. Recording the site routes the surface through its own dynamic
    /// arm at run time, making `field_specs_of::<T>()` mean exactly
    /// `field_specs_of(type_name::<T>())` without the author writing the composition. Only a *bare*
    /// parameter, matching [`TypeRef::head_name`]: `field_specs_of::<List<T>>()` heads at `List`
    /// and stays the folded constant it always was. A parameter no channel reaches is still
    /// [`Self::reject_erased_type_param`]'s E0058, with its tailored message.
    fn check_type_operand(
        &mut self,
        operand: &TypeOperand,
        env: &mut Env,
        span: noeta_span::Span,
        surface: &str,
        help: &str,
    ) {
        let e = match operand {
            TypeOperand::Static(ty) => {
                // The same two channels, consulted in the same order, as `type_name::<T>()` — one
                // helper, so every name-keyed surface answers with the same `T`.
                if let TypeRef::Named { args, .. } = ty
                    && args.is_empty()
                    && let Type::Param(p) = self.annot(ty)
                    && self.record_type_param(&p, span)
                {
                    return;
                }
                if !self.reject_erased_type_param(ty, surface) {
                    self.check_type_ref(ty);
                }
                return;
            }
            TypeOperand::Dynamic(e) => e,
        };
        let name_ty = self.synth(e, env);
        if !matches!(name_ty, Type::String) && !name_ty.defers_to_runtime() {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("`{surface}` expects a `string` type name, found `{name_ty}`"),
            )
            .help(help.to_string());
        }
    }

    pub(crate) fn synth_inner(&mut self, expr: &Expr, env: &mut Env) -> Type {
        match expr {
            // A resolved native-fn reference as a *value* — a loose `Fn` type, like a
            // selectively-imported module function referenced bare (the precise per-call signature
            // is applied in the `Call` callee arm). The desugar only ever uses it as a callee.
            Expr::NativeFnRef { .. } => Type::Fn {
                params: Vec::new(),
                ret: Box::new(Type::Dyn),
            },
            Expr::Str { .. } => Type::String,
            Expr::Int { .. } => Type::Int,
            Expr::Float { .. } => Type::Float,
            Expr::F32 { .. } => Type::F32,
            Expr::F64 { .. } => Type::F64,
            Expr::IntN {
                magnitude,
                signed,
                bits,
                span,
            } => self.check_intn_literal(*magnitude, *signed, *bits, false, *span),
            Expr::Bool { .. } => Type::Bool,
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let StrPart::Hole(e) = part {
                        self.synth(e, env);
                    }
                }
                Type::String
            }
            // An expression-tier block types as the handler call it desugars to (`Try`/`Await`
            // architecture: the node is kept, the checker types it, IR lowering rewrites it
            // through the same [`noeta_ast::desugar`] constructor). Checking the constructed
            // call is the whole typing rule: each hole closure checks against the handler's
            // `List<() -> U>` — so a hole-type error lands on the hole's real span — and the
            // block's type is the handler's declared return. A block whose tier is not
            // `expr:`-declared (`x = @doc { … }`) is E0052; its holes still synth for IDE
            // coverage inside the body.
            Expr::TierExpr {
                tier,
                tier_span,
                statics,
                holes,
                span,
            } => {
                let handler = self.symbols.tier_registry.expr_tier_handler(tier);
                match handler {
                    Some(handler) => {
                        let call = noeta_ast::desugar::tier_expr_call(
                            &handler, *tier_span, statics, holes, *span,
                        );
                        self.synth(&call, env)
                    }
                    None => {
                        for hole in holes {
                            self.synth(hole, env);
                        }
                        self.error(
                            DiagnosticCode::InvalidTierExpression,
                            *tier_span,
                            format!(
                                "`@{tier}` is not an expression tier — its blocks are not values"
                            ),
                        )
                        .help(
                            "only a tier declared `@tier(name, …, expr: Type)` yields a value \
                             from `@name { … }`; a text tier's blocks are runner input, not \
                             expressions",
                        );
                        Type::Unknown
                    }
                }
            }
            // The one lookup site that needs an *owned* type (synthesis returns `Type` by value),
            // so it clones here rather than in `lookup` (audit-3 Finding 12).
            Expr::Ident { name, span }
                if lookup(env, name.as_str()).is_none()
                    && self.symbols.forwarding.contains_key(name.as_str())
                    && self.symbols.functions.contains_key(name.as_str()) =>
            {
                // A FORWARDING generic fn referenced bare — with NO expectation to pin its
                // instantiation — is not a value (poly-values F2b): its hidden type-argument slot
                // would be silently missing. With a pinning expected function type the reference
                // IS a value (D2c — `instantiate_fn_value` binds the slots into a wrapper); this
                // arm is only reached when synthesis finds nothing to pin it.
                self.error(
                    DiagnosticCode::InvalidTypeArguments,
                    *span,
                    format!(
                        "cannot use `{name}` as a value here: its type parameter determines a \
                         call-site-typed result, and nothing pins the instantiation"
                    ),
                )
                .help(format!(
                    "bind it against an expected function type (`f: (string) -> ... = {name}`), \
                     call it directly (`{name}::<T>(...)`), or wrap it in a closure"
                ));
                Type::Unknown
            }
            Expr::Ident { name, span } => match lookup(env, name.as_str())
                .cloned()
                // A bare user-function reference is a first-class value of its **full** signature
                // type — parameters included, so passing it where a `Fn(A) -> B` is declared
                // (`map_bounded(items, n, dbl)`, `xs.map(inc)`) checks like the equivalent
                // closure. A generic function's erased params are `dyn`, which defers per
                // position. (Was params-erased until higher-order-abi H2 made module signatures
                // carry declared `Fn` params, which an erased handle could never satisfy.)
                .or_else(|| {
                    self.symbols
                        .functions
                        .get(name.as_str())
                        .map(|sig| Type::Fn {
                            params: sig.params.clone(),
                            ret: Box::new(sig.ret.clone()),
                        })
                })
                // A selectively-imported module function referenced as a value (`let f = sqrt`).
                .or_else(|| {
                    self.imports
                        .imported_fns
                        .contains_key(name.as_str())
                        .then(|| Type::Fn {
                            params: Vec::new(),
                            ret: Box::new(Type::Dyn),
                        })
                }) {
                Some(t) => t,
                None => {
                    // A bare name inside a type's own body that names one of its FIELDS is a
                    // targeted static error (prelude-redesign EX.1): member access is explicit, so
                    // the field is only reachable as `self.name`. Any other unknown ident stays
                    // tolerated here (deferred to the runtime E0005, as before).
                    if let Some(ct) = self.coloring.current_type.clone()
                        && self
                            .symbols
                            .records
                            .get(&ct)
                            .is_some_and(|fs| fs.iter().any(|(f, _)| f == name))
                    {
                        self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("cannot find `{name}` in this scope"),
                        )
                        .help(format!(
                            "member access is explicit — the field is `self.{name}`"
                        ));
                    } else if !self.config.session_mode && !self.is_known_name(name.as_str(), env) {
                        // A bare reference to a name that resolves to nothing — a genuinely
                        // undefined value (F1), the same static `E0005` as an unknown callee. A
                        // session defers (a later entry may define it). In a SEALED named-fn
                        // body a miss that names a real top-level binding gets the capture hint.
                        let sealed_global_miss = self.coloring.in_sealed_body
                            && self.symbols.global_binding_names.contains(name.as_str());
                        let diag = self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!("cannot find `{name}` in this scope"),
                        );
                        if sealed_global_miss {
                            diag.help(format!(
                                "`{name}` is a top-level binding, which a named function does \
                                 not see implicitly — add `use ({name})` to the signature, or \
                                 pass it as a parameter"
                            ));
                        }
                    }
                    Type::Unknown
                }
            },
            Expr::Unary { op, operand, span } => {
                // A negated fixed-width literal (`-128i8`, `-1i32`): check against the *signed*
                // negative range here, so the inner literal's positive-range check does not fire a
                // false positive on the boundary value `128i8` that only `-128i8` may reach.
                if let (
                    UnaryOp::Neg,
                    Expr::IntN {
                        magnitude,
                        signed,
                        bits,
                        span: lit_span,
                    },
                ) = (op, operand.as_ref())
                {
                    return self.check_intn_literal(*magnitude, *signed, *bits, true, *lit_span);
                }
                let t = self.synth(operand, env);
                // A list spread `...xs` (the marker the L2 desugar wraps spread operands in) must
                // spread a list — otherwise the desugared `~` would silently fall through to
                // display-concatenation. It always types list-shaped so the surrounding literal
                // stays a list: a list passes through; a `dyn`/hole spread contributes `dyn`
                // elements; a concrete non-list is an error (and still resolves to `List<dyn>`,
                // suppressing a second diagnostic from the desugared concat).
                if matches!(op, UnaryOp::Spread) {
                    return match &t {
                        Type::List(_) => t,
                        _ if t.defers_to_runtime() => Type::List(Box::new(Type::Dyn)),
                        _ => {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot spread `{t}` — `...` expects a list"),
                            );
                            Type::List(Box::new(Type::Dyn))
                        }
                    };
                }
                // Unary `-` on a fixed-width integer (Tier W): the result is the same width, masked so
                // `-i8::MIN` wraps back to `i8::MIN`; negating an *unsigned* width has no meaning →
                // E0044. (A negated fixed-width *literal* is handled by the intercept above.)
                if let (UnaryOp::Neg, Type::IntN { signed, bits }) = (op, &t) {
                    if *signed {
                        self.sites.width_sites.insert(*span, (*signed, *bits));
                    } else {
                        self.error(
                            DiagnosticCode::FixedWidthOutOfRange,
                            *span,
                            format!("cannot negate `u{bits}`: unary `-` requires a signed type"),
                        );
                    }
                    return t;
                }
                // Other unary type errors have no corpus case and the operand is often gradual;
                // infer for nested checks but do not promote (kept conservative).
                t
            }
            Expr::Binary { op, lhs, rhs, span } => self.synth_binary(*op, lhs, rhs, *span, env),
            Expr::Call {
                callee, args, span, ..
            } => {
                // Bidirectional literal arguments: a closure's parameter types — and a container
                // literal's expected element/value type — come from the CALLEE's resolved signature,
                // so both are deferred (placeholder `Unknown`) and typed by `synth_call` once the
                // signature is known (a `{"route": "/x", "status": 200}` map literal then absorbs a
                // `Map<string, string|int|float|bool>` parameter, checking each value against the
                // union instead of cross-unifying them). Everything else synthesizes as before.
                let arg_types: Vec<Type> = args
                    .iter()
                    .map(|a| {
                        if self.is_deferred_arg(&a.value, env) {
                            Type::Unknown
                        } else {
                            self.synth(&a.value, env)
                        }
                    })
                    .collect();
                self.synth_call(callee, &arg_types, args, *span, env)
            }
            Expr::Closure {
                params,
                ret: ann,
                body,
                ..
            } => {
                self.validate_param_defaults(params, env);
                // A closure's parameters take the same annotation grammar as a named callable's, so
                // an attribute written on one must face the same gates: it has to BE an attribute
                // struct (E0029), permitted at `Param` (E0030), and constructible from its literal
                // arguments. It is inert beyond that — the attribute manifest keys on a callable's
                // name and a closure has none, so nothing can ever query it back — but "inert" and
                // "unvalidated" are different things, and only the first is acceptable.
                self.check_param_attrs(params);
                env.push(HashMap::new());
                for p in params {
                    self.check_reserved_name(&p.name, p.name_span);
                    // Same rule as the check-mode arm: any env hit is a shadow (E0059).
                    self.check_shadow(&p.name, p.name_span, env, crate::ShadowScopes::All);
                    bind(env, &p.name, self.annot_param(p));
                }
                // With an explicit return annotation, check the body against it (and adopt it as the
                // closure's return type); otherwise infer it from the body (the arrow expression's
                // type, or a block's joined `return`s).
                let declared = ann.as_ref().map(|t| self.annot(t));
                let ret = self.closure_body_type(body, declared.as_ref(), env);
                env.pop();
                Type::Fn {
                    params: params.iter().map(|p| self.annot_param(p)).collect(),
                    ret: Box::new(ret),
                }
            }
            Expr::Pipeline { left, right, .. } => {
                // `left |> right` threads `left` as the first argument of `right`.
                let piped = self.synth(left, env);
                self.synth_piped(left, right, piped, env)
            }
            Expr::List { items, span } => {
                // Synthesize a single element type by unifying the items. Concretely incompatible
                // elements (e.g. `[1, "two"]`) are a static error here in *synthesis* position;
                // a mixed list is written explicitly as `List<dyn>` (in which case the checker
                // arrives through `check`, element-by-element against `dyn`, not here).
                let mut elem = Type::Unknown;
                let mut heterogeneous = false;
                for item in items {
                    let t = self.synth(item, env);
                    match unify_element(&elem, &t) {
                        Some(u) => elem = u,
                        None => heterogeneous = true,
                    }
                }
                if heterogeneous {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "list elements have differing types",
                    )
                    .help("make the elements one type, or annotate a `List<dyn>` for a mixed list");
                    elem = Type::Dyn; // recover as a mixed list
                }
                self.note_packed_list(&elem, *span);
                let ty = Type::List(Box::new(elem));
                self.note_construction(&ty, *span);
                ty
            }
            // A tuple literal `(a, b, …)` synthesizes a `Type::Tuple` of its elements' types,
            // positionally — heterogeneity is the point (no unification, unlike a list).
            Expr::Tuple { items, .. } => {
                Type::Tuple(items.iter().map(|item| self.synth(item, env)).collect())
            }
            // Tuple projection `receiver.N`: the Nth element type of a tuple receiver. An out-of-range
            // index is `E0007`; a `.N` on a non-tuple concrete type is rejected; a `dyn`/hole defers.
            Expr::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let recv = self.synth(receiver, env);
                match &recv {
                    Type::Tuple(elements) => match elements.get(*index as usize) {
                        Some(t) => t.clone(),
                        None => {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "tuple index `{index}` is out of range for `{recv}` ({} element(s))",
                                    elements.len()
                                ),
                            );
                            Type::Unknown
                        }
                    },
                    _ if recv.defers_to_runtime() => Type::Unknown,
                    _ => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("cannot apply tuple index `.{index}` to non-tuple `{recv}`"),
                        );
                        Type::Unknown
                    }
                }
            }
            Expr::Range {
                start, end, span, ..
            } => {
                // A range builds a `List<int>`; both bounds must be `int` (a `dyn`/hole defers).
                let st = self.synth(start, env);
                let en = self.synth(end, env);
                let bad = |t: &Type| !matches!(t, Type::Int) && !t.defers_to_runtime();
                if bad(&st) || bad(&en) {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("range bounds must be `int`, found `{st}` and `{en}`"),
                    );
                }
                Type::List(Box::new(Type::Int))
            }
            Expr::Map { entries, span } => {
                // Synthesize key/value types by unifying the entries (mirroring the list path).
                // Runtime map keys are always strings, so keys unify trivially in practice; values
                // that concretely disagree (`{"a": 1, "b": "two"}`) are a static error, recovering
                // as a `Map<_, dyn>`. An empty `{}` leaves both unspecified (an inference hole).
                let mut key_ty = Type::Unknown;
                let mut val_ty = Type::Unknown;
                let mut heterogeneous = false;
                for (k, v) in entries {
                    let kt = self.synth(k, env);
                    let vt = self.synth(v, env);
                    key_ty = unify_element(&key_ty, &kt).unwrap_or(Type::Dyn);
                    match unify_element(&val_ty, &vt) {
                        Some(u) => val_ty = u,
                        None => heterogeneous = true,
                    }
                }
                if heterogeneous {
                    self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "map values have differing types",
                        )
                        .help(
                            "make the values one type, or annotate a `Map<string, dyn>` for a mixed map",
                        );
                    val_ty = Type::Dyn; // recover as a mixed map
                }
                // A literal keyed by a type without a runtime key form is rejected statically
                // (extern-types X4 / P-PKEY S3), matching the `Map<K, _>` formation gate.
                if let Type::Named(key_name, _) = &key_ty
                    && self.named_key_capable(key_name, false) == Some(false)
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{key_ty}` cannot key a map: it is not a key-capable type"),
                    )
                    .help(
                        "key-capable types are strings, key-capable extern types (e.g. `Uuid`), \
                         and `@packed` structs of int/bool fields",
                    );
                }
                let ty = Type::Map(Box::new(key_ty), Box::new(val_ty));
                self.note_construction(&ty, *span);
                ty
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => self.synth_member(receiver, name, *name_span, *span, env),
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                // Index into the receiver: a list element, a map value, a string char, or `dyn`.
                let recv = self.synth(receiver, env);
                self.synth(index, env);
                // Note a list-typed index so a `list[i].field` member access can fuse (P-PACK 2.5+).
                // Recorded here — where the receiver's type is already in hand — so `synth_member`
                // need not re-synthesize the inner receiver.
                if matches!(recv, Type::List(_)) {
                    self.coloring.index_on_list.insert(*span);
                }
                match stdlib::index_return(&recv) {
                    Some(t) => t,
                    None => {
                        // A concrete primitive cannot be indexed (`42[0]`). A `Named` type may
                        // implement `Index`, and a hole/`dyn` defers — neither errors here.
                        if matches!(recv, Type::Int | Type::Float | Type::Bool | Type::Unit) {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot index into `{recv}`"),
                            );
                        }
                        Type::Unknown
                    }
                }
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
                // Reached through `synth`/`check` — the match is a sub-expression, so its value is
                // used (a statement-position match routes through `Stmt::Expr` with `value_used`
                // false instead).
            } => self.synth_match(scrutinee, arms, *span, env, true),
            Expr::Object(lit) => {
                // A target-typed `.{ … }` reaching *synthesis* is one in a position with no expected
                // type — nothing to adopt a name from. (The check path below intercepts every
                // position that does have one.)
                let Some(type_name) = lit.type_name.clone() else {
                    self.error(
                        DiagnosticCode::CannotInfer,
                        lit.type_name_span,
                        "cannot infer the type of `.{ … }` here: this position has no expected type"
                            .to_string(),
                    )
                    .help(
                        "name the type at the literal (`x = TypeName { … }`) or annotate the \
                         position it flows into (`x: TypeName = .{ … }`)",
                    );
                    // Still walk the field values so their own errors surface.
                    for f in &lit.fields {
                        self.synth(&f.value, env);
                    }
                    if let Some(spread) = &lit.spread {
                        self.synth(spread, env);
                    }
                    return Type::Unknown;
                };
                self.synth_object_named(lit, type_name.as_str(), env)
            }
            Expr::Try { expr, span } => {
                let inner = self.synth(expr, env);
                self.try_unwrap(&inner, *span)
            }
            Expr::Await { expr, span } => {
                let inner = self.synth(expr, env);
                // Coloring (Track A): `.await` is legal only inside an async context (an `async fn`
                // body or the implicitly-async top level). A `.await` in a sync `fn` — or in a closure
                // passed to a builtin, where `current_async` was reset at the boundary — is E0040.
                if !self.coloring.current_async {
                    self.error(
                        DiagnosticCode::AsyncMisuse,
                        *span,
                        "`.await` is only allowed inside an `async fn` (or the async top level)"
                            .to_string(),
                    )
                    .help(
                        "mark the enclosing function `async fn`; `.await` cannot be used in a \
                             synchronous function or in a closure passed to a builtin",
                    );
                }
                // `Future<T>.await` yields `T`; a hole/`dyn` defers to runtime; anything else is a
                // `.await` on a non-future.
                match &inner {
                    Type::Named(n, args) if n == stdlib::FUTURE => {
                        args.first().cloned().unwrap_or(Type::Unknown)
                    }
                    t if t.defers_to_runtime() => t.clone(),
                    other => {
                        self.error(
                            DiagnosticCode::AsyncMisuse,
                            *span,
                            format!("`.await` expects a `Future`, found `{other}`"),
                        )
                        .help("`.await` unwraps a `Future<T>` produced by an `async fn`");
                        Type::Unknown
                    }
                }
            }
            Expr::Spawn {
                future,
                isolate,
                span,
            } => {
                let kw = if *isolate { "isolate" } else { "spawn" };
                let inner = self.synth(future, env);
                // Structured concurrency (Track A.3b): `spawn`/`isolate` are legal only inside a
                // `concurrent { }` scope. An orphan one (no enclosing scope — incl. one in a closure,
                // where the depth was reset) is E0041 by construction, so a spawned unit can never
                // outlive a scope.
                if self.coloring.concurrent_depth == 0 {
                    self.error(
                        DiagnosticCode::OrphanSpawn,
                        *span,
                        format!("`{kw}` is only allowed inside a `concurrent {{ }}` scope"),
                    )
                    .help(format!(
                        "wrap the `{kw}` in a `concurrent {{ }}` block; a task must have an owning \
                             scope that joins it"
                    ));
                }
                // `spawn e`/`isolate f(args)` take a `Future<T>` (an `async fn` call) and yield a handle
                // that is itself a `Future<T>` — so `spawn f().await` produces the result. A non-future
                // operand is E0041 (a hole/`dyn` defers to runtime).
                let result = match &inner {
                    Type::Named(n, _) if n == stdlib::FUTURE => inner.clone(),
                    t if t.defers_to_runtime() => {
                        Type::Named(stdlib::FUTURE.to_string(), vec![t.clone()])
                    }
                    other => {
                        self.error(
                            DiagnosticCode::OrphanSpawn,
                            *span,
                            format!("`{kw}` expects a `Future`, found `{other}`"),
                        )
                        .help(format!("`{kw}` an `async fn` call, e.g. `{kw} fetch(url)`"));
                        Type::Named(stdlib::FUTURE.to_string(), vec![Type::Unknown])
                    }
                };
                // `isolate` runs in a fresh heap, so its arguments and result must be `Send` (E0042) —
                // the check the object-model arc parked here. `spawn` (same heap) has no such limit.
                if *isolate {
                    self.check_isolate_send(future, &result, *span);
                }
                result
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => {
                // A generic-call value seeds its instantiation from the FALLBACK's type
                // (poly-deferrals D1): `o = load(text) ?? default` — with no annotation, the only
                // expectation in sight is the fallback, so it is synthesized first and its type
                // bound against the callee's declared success arm. A deferring fallback leaves the
                // seed empty (bind_type_params never binds from `dyn`/holes), reducing to the
                // pre-existing behavior.
                if self.seedable_generic_call(value, env).is_some() {
                    let fb = self.synth(fallback, env);
                    let wrapped = self.check_coalesce_seeded(value, &fb, env);
                    return match wrapped {
                        Type::Result(ok, _) => *ok,
                        Type::Option(some) => *some,
                        _ => Type::Unknown,
                    };
                }
                let v = self.synth(value, env);
                let fb = self.synth(fallback, env);
                match v {
                    Type::Result(ok, _) => *ok,
                    Type::Option(some) => *some,
                    // `??` unwraps a fallible value, so a left side that is already a single
                    // concrete type has nothing to fall back FROM — the fallback is dead and both
                    // backends abort on it at run time. Reject it here rather than leaking
                    // `Unknown` (which unified with anything and let the program check clean).
                    // A `dyn`/hole left side still defers, exactly as before.
                    _ => {
                        if !v.defers_to_runtime() {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`??` expects a `Result` or `Option` on the left, but this \
                                         value is `{v}`"
                                ),
                            )
                            .help(
                                "`??` supplies a value for a `none`/`Err`; a value that is \
                                     always present does not need it — drop the `?? …`, or use \
                                     `map.get_or(key, default)` for a lookup that may miss",
                            );
                        }
                        // Recover as the fallback's type: the program meant "a value of that
                        // shape", so downstream typing stays useful instead of cascading.
                        fb
                    }
                }
            }
            Expr::As { expr, ty, span } => {
                let src = self.synth(expr, env);
                self.check_type_ref(ty);
                // A **type parameter** target is answerable: the narrow keys on the instantiation's
                // runtime name, and the same two channels `type_name::<T>()` rides deliver it.
                // Unresolvable shapes are E0058 here rather than a silent `none` at run time.
                self.check_narrow_target(ty, *span, ".as<…>()");
                let target = self.annot(ty);
                // Narrowing is the explicit way *out* of an open type: the dynamic top `dyn`, an
                // un-inferred hole (which defers), a **union** (a *closed* `dyn`), or an abstract
                // **kind-type** (`Enum`/`Struct`/`Class` — narrow to a concrete member). A value
                // whose static type is already a single concrete type has nothing dynamic to narrow
                // — that is an `E0028`.
                if !src.defers_to_runtime() && !matches!(src, Type::Union(_) | Type::Kind(_)) {
                    self.error(
                        DiagnosticCode::InvalidNarrow,
                        *span,
                        format!(
                            "`.as<{target}>()` can only narrow a `dyn` or union value, but \
                                 this value is already `{src}`"
                        ),
                    )
                    .help(
                        "narrowing converts an open type (`dyn` or a union) to a checked `?T`; \
                             a value with a single known concrete type does not need it",
                    );
                }
                Type::Option(Box::new(target))
            }
            Expr::TypeTest { expr, ty, span } => {
                // A type *test* is always well-formed on any source — even a concrete one (it is
                // simply a constant `true`/`false`), unlike `.as<T>()` whose narrowing of a known
                // concrete value is an `E0028`. We only validate the target type names something.
                //
                // A `dyn Trait` target is a PRECISE membership test at runtime (the shared
                // reflection `trait_impls` table). Future work: when the scrutinee's static type is
                // a single concrete nominal with a known impl (`user_trait_impls` hit), the test is
                // provably constant-true and could fold or warn — today the runtime test simply
                // runs (and agrees), so no warning machinery is spent on it.
                let scrut = self.synth(expr, env);
                let before = self.diags.len();
                self.check_type_ref(ty);
                // Same channels, same refusals as `.as<T>()` — the two surfaces share the runtime
                // matcher, so they must share what a parameter target means.
                self.check_narrow_target(ty, *span, "is");
                // A test against a *reified container's* payload (`x is P` where `x: ?P`) is
                // statically always false — the value's tag is `some`/`none`, never `P` (E0065,
                // warning). Reported here; the narrowing sites (`if`, and a `match`'s `is T` arm)
                // consult the same predicate and decline to narrow, so the dead branch stops
                // type-checking as the payload.
                //
                // Skipped when the target itself did not resolve (`x is none` — E0013, already
                // reported just above with the constructor-vs-type help): a second diagnostic on
                // the same span about a type that does not exist is noise, not information.
                if self.diags.len() == before
                    && let Some(idiom) = self.impossible_type_test(&scrut, ty)
                {
                    let target = self.annot(ty);
                    self.warn(
                        DiagnosticCode::ImpossibleTypeTest,
                        ty.span(),
                        format!(
                            "`{scrut}` is its own runtime type, not its payload's; \
                             `x is {target}` is always false"
                        ),
                    )
                    .help(idiom);
                }
                // A bare-scalar test against an *erased* fixed width (`iN`/`f64`) is the one family
                // the **runtime** cannot answer: no scalar value carries a width tag (every integer
                // width is the same NaN-boxed word, and an `f64` *is* a `float` bit for bit), so the
                // shared matcher reaches no head for it and the test comes back `false` whatever the
                // value. The *checker* frequently can answer it, and where it can, `false` is simply
                // the wrong answer — so the answer is decided here and folded at lowering
                // (`Sites::folded_type_tests`), and only a scrutinee that leaves the width genuinely
                // unrecoverable warns (E0063).
                //
                // `f32` is exempt (reified — a real narrowing head, Part A) and a container target
                // (`List<i32>`) is exempt (packed element widths live in the buffer's schema) —
                // both filtered by matching only a *bare* (`args.is_empty()`) erased-width name.
                if let TypeRef::Named {
                    name,
                    args,
                    span: target_span,
                } = ty
                    && args.is_empty()
                    && let Some(base) = erased_scalar_width_base(name.as_str())
                {
                    match settled_width_answer(&scrut, &self.annot(ty)) {
                        Some(answer) => {
                            self.sites.folded_type_tests.insert(*span, answer);
                        }
                        None => {
                            self.warn(
                                DiagnosticCode::ErasedWidthNarrow,
                                *target_span,
                                format!(
                                    "`{name}` shares one runtime representation with `{base}`, and \
                                     this value's static type does not fix the width — so \
                                     `x is {name}` cannot be answered"
                                ),
                            )
                            .help(format!(
                                "test the base type instead (`x is {base}`); a scrutinee whose \
                                 static type names the width is answered statically"
                            ));
                        }
                    }
                }
                Type::Bool
            }
            // `type_name::<T>()` — a type's qualified runtime identity as a `string`. The type is
            // resolved like any annotation (an unresolvable `T` is E0013). A type *parameter* is
            // answerable exactly when the instantiation reaches the body through one of the two
            // channels the language already has, and E0058 when neither does:
            //
            //   * a parameter of the enclosing generic TYPE, in an instance method — it rides the
            //     receiver's reflected type tag (`self_type_arg_sites`, below);
            //   * a parameter of the enclosing top-level generic FN — it rides the hidden
            //     type-argument slot that already carries `json.try_parse::<T>`'s decode recipe,
            //     and this surface needs only the slot's NAME, no recipe at all.
            Expr::TypeName { ty, span } => {
                // A bare parameter of an enclosing generic is not erased after all: one of the two
                // per-instantiation channels carries its name (the receiver's reflected type tag
                // inside a generic type's instance method — generic constructor reflection, Gap B —
                // or the enclosing fn's hidden type-argument slot, poly-values F2b). Recorded as a
                // site rather than folded to a constant: one compiled body serves every
                // instantiation, so there is no constant to fold to.
                //
                // Checked only for a *bare* parameter — the head is what this surface answers with,
                // and `type_name::<List<T>>()` heads at `List` whatever `T` is, so it stays the
                // folded constant. The narrow surfaces (`.as<T>()`, `x is T`) read the same two
                // channels through the same helper, which is what makes them agree about `T`.
                if let TypeRef::Named { args, .. } = ty
                    && args.is_empty()
                    && let Type::Param(p) = self.annot(ty)
                    && self.record_type_param(&p, *span)
                {
                    return Type::String;
                }
                if !self.reject_erased_type_param(ty, "type_name") {
                    self.check_type_ref(ty);
                }
                Type::String
            }
            Expr::AttributesOf { ty, span } => {
                self.check_type_ref(ty);
                let target = self.annot(ty);
                // The type argument must itself be an attribute — a struct marked `@attribute` (the
                // same capability gate as a `#[T(...)]` use). Otherwise the manifest holds no `T` to
                // materialize.
                // A FORWARDED type parameter (poly-values F2b): the attribute type arrives
                // per-instantiation through the enclosing fn's hidden slot; the manifest query
                // resolves the concrete NAME at runtime. Whether that instantiation is an
                // attribute is a per-name manifest fact (an entry-less name yields the empty
                // list, exactly like the runtime path).
                if let Type::Param(_) = &target {
                    match self
                        .coloring
                        .current_forwarding
                        .iter()
                        .position(|t| t == &target)
                    {
                        Some(idx) => {
                            self.sites.forwarded_slot_sites.insert(*span, idx as u32);
                        }
                        None => {
                            self.error(
                                DiagnosticCode::InvalidTypeArguments,
                                *span,
                                format!(
                                    "cannot forward `{target}` here: call-site-typed forwarding \
                                     carries a generic `fn`'s or method's OWN type parameters, \
                                     and `{target}` is not one of this body's"
                                ),
                            )
                            .help(
                                "an enclosing generic TYPE's parameter reaches a method through \
                                 the receiver, which records the instantiation's name but no \
                                 build recipe — take the type as the method's own parameter \
                                 instead",
                            );
                        }
                    }
                    return Type::List(Box::new(Type::Named(
                        "Attributed".to_string(),
                        vec![target],
                    )));
                }
                let is_attribute = matches!(&target, Type::Named(n, _)
                    if self.symbols.attributes.contains(n));
                if !is_attribute {
                    self.error(
                        DiagnosticCode::NotAnAttribute,
                        *span,
                        format!(
                            "`attributes_of` requires an attribute type, but `{target}` is not one"
                        ),
                    )
                    .help("name a record marked `@attribute`");
                    return Type::List(Box::new(Type::Dyn));
                }
                Type::List(Box::new(Type::Named(
                    "Attributed".to_string(),
                    vec![target],
                )))
            }
            Expr::TypeOf { value, span } => {
                // Synthesize the operand's static type; the result of `type_of` is always the
                // prelude `Type` enum. When the operand is concretely typed, record the precise
                // `TypeRepr` so the backends bake a full-fidelity `Type` constant (A); otherwise the
                // site stays absent and falls back to the runtime head-constructor path (B).
                let operand = self.synth(value, env);
                if let Some(repr) = type_to_repr_top(&operand, &self.symbols.type_kinds) {
                    self.sites.type_of_sites.insert(*span, repr);
                }
                Type::Named("Type".to_string(), Vec::new())
            }
            Expr::FieldsOf { value, .. } => {
                // The value-level counterpart of `type_of` (derive layer 3): a struct/class
                // instance's fields as `List<FieldEntry>`; any other value is the empty list.
                self.synth(value, env);
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::FIELD_ENTRY.to_string(),
                    Vec::new(),
                )))
            }
            Expr::TraitsOf { value, .. } => {
                // The trait-membership query: the qualified trait names the value's nominal type
                // has a registered `impl` for, as a sorted `List<string>` (the same shared table
                // the precise `is dyn Trait` narrowing tests). A non-nominal value is the empty
                // list, mirroring `fields_of`.
                self.synth(value, env);
                Type::List(Box::new(Type::String))
            }
            Expr::RolesOf { ty, span } => {
                // The compiler-built role index, surfaced as `List<RoleBinding>`. The optional
                // turbofish scopes the query to one role enum, which — like `attributes_of`'s
                // `@attribute` gate — must be a `@semantic` enum (only those contribute roles).
                if let Some(ty) = ty {
                    self.check_type_ref(ty);
                    let target = self.annot(ty);
                    let is_semantic = matches!(&target, Type::Named(n, _)
                        if self.symbols.semantic_enums.contains(n));
                    if !is_semantic {
                        self.error(
                            DiagnosticCode::InvalidRole,
                            *span,
                            format!(
                                "`roles_of` requires a `@semantic` enum, but `{target}` is not one"
                            ),
                        )
                        .help("mark the enum `@semantic` to query its roles");
                    }
                }
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::ROLE_BINDING.to_string(),
                    Vec::new(),
                )))
            }
            Expr::ParamsOf { target, span } => {
                // The compiler-built parameter index, surfaced as `List<ParamInfo>`. The `target`
                // operand is a runtime `string` naming a fn or method (a bare name or `Type.method`).
                let target_ty = self.synth(target, env);
                if !matches!(target_ty, Type::String) && !target_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`params_of` expects a `string` target, found `{target_ty}`"),
                    )
                    .help("pass a fn name or `Type.method` string");
                }
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::PARAM_INFO.to_string(),
                    Vec::new(),
                )))
            }
            Expr::ReturnsOf { target, span } => {
                // The other half of the compiler-built signature index, surfaced as `?Type`. Same
                // runtime `string` target as `params_of` (a bare fn name or `Type.method`), and the
                // same leniency about *what* it names — an unknown callable is a runtime `none`, not
                // a static error, because the target is generally computed (a framework walks
                // `roles_of()` and asks about each controller method it finds).
                //
                // The result is an OPTION where `params_of` answers an empty list, and the asymmetry
                // is deliberate: an empty parameter list is a legitimate answer, so `params_of` can
                // fold "unknown target" into it, but every callable has a return type — `void`
                // included — so there is no return value that could stand for "no such callable".
                // Folding them would make a typo indistinguishable from a `void` method.
                let target_ty = self.synth(target, env);
                if !matches!(target_ty, Type::String) && !target_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`returns_of` expects a `string` target, found `{target_ty}`"),
                    )
                    .help("pass a fn name or `Type.method` string");
                }
                Type::Option(Box::new(Type::Named(
                    noeta_ast::reflect::TYPE_ENUM.to_string(),
                    Vec::new(),
                )))
            }
            Expr::FieldSpecsOf { name, span } => {
                // The type-level field schema, surfaced as `List<FieldSpec>`. The turbofish surface
                // names the type statically (so an unresolvable `T` is an E0013); the dynamic surface
                // takes a runtime `string` naming a declared struct/class type, and stays lenient
                // like `params_of` — an unknown name there is a runtime empty list, not an error.
                self.check_type_operand(
                    name,
                    env,
                    *span,
                    "field_specs_of",
                    "pass a struct or class type name, or use the turbofish `field_specs_of::<T>()`",
                );
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::FIELD_SPEC.to_string(),
                    Vec::new(),
                )))
            }
            Expr::VariantsOf { name, span } => {
                // The type-level variant schema, surfaced as `List<VariantSpec>`. The enum twin of
                // `field_specs_of`, checked through the SAME `check_type_operand` so the turbofish
                // resolves a name (and reports an erased type parameter as E0058) identically, and
                // the dynamic surface stays lenient — a name that is not an enum is a runtime empty
                // list, not a static error.
                self.check_type_operand(
                    name,
                    env,
                    *span,
                    "variants_of",
                    "pass an enum type name, or use the turbofish `variants_of::<T>()`",
                );
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::VARIANT_SPEC.to_string(),
                    Vec::new(),
                )))
            }
            Expr::Construct { name, fields, span } => {
                // The dynamic struct constructor: build a value of the type `name` from `fields`, a
                // runtime `List<dyn>` of field values in declaration order. Fallible by construction
                // (unknown type / arity / type-mismatch / missing required field are runtime `Err`),
                // so both operands are synthesized leniently and the result is `Result<dyn, string>`.
                self.check_type_operand(
                    name,
                    env,
                    *span,
                    "construct",
                    "pass a struct or class type name, or use the turbofish `construct::<T>(fields)`",
                );
                self.synth(fields, env);
                Type::Result(Box::new(Type::Dyn), Box::new(Type::String))
            }
            Expr::FromBytes { ty, blob, span } => {
                // The operand must be a `bytes` buffer (gradual holes tolerated).
                let blob_ty = self.synth(blob, env);
                if !matches!(blob_ty, Type::Bytes) && !blob_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        blob.span(),
                        format!("`from_bytes` expects a `bytes` value, found `{blob_ty}`"),
                    );
                }
                self.check_type_ref(ty);
                let elem = self.annot(ty);
                // The element type must be a packable `@packed` struct — the blob is a flat packed
                // buffer. Recording the layout in `packed_list_sites` (the channel list literals use)
                // hands the backend the schema to rebuild the list. Generic over any declared packable
                // type (no hardcoded list — extension-friendly).
                match self.packed_list_layout(&elem) {
                    Some(layout) => {
                        self.sites.packed_list_sites.insert(*span, layout);
                        // Validation arc: if the packed element type implements `Validate`, mark the
                        // site so both backends run `validate()` on each decoded element (the abort
                        // door — consistent with `from_bytes`'s shape-error behavior, and closing the
                        // hole a `@validated` packed type would otherwise have here).
                        if self.satisfies(&elem, noeta_types::BuiltinTrait::Validate) {
                            self.sites.from_bytes_validated.insert(*span);
                        }
                    }
                    None => {
                        self.error(
                            DiagnosticCode::InvalidPackedType,
                            *span,
                            format!(
                                "`from_bytes::<{elem}>` requires a packable element type — a `@packed` struct or a sub-8-byte fixed-width numeric (`i32`/`u8`/`f32`, …)"
                            ),
                        );
                    }
                }
                Type::List(Box::new(elem))
            }
            Expr::Channel {
                elem,
                capacity,
                span: _,
            } => {
                // The capacity is a buffer size — an `int` (gradual holes tolerated).
                let cap_ty = self.synth(capacity, env);
                if !matches!(cap_ty, Type::Int) && !cap_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        capacity.span(),
                        format!("`channel` expects an `int` capacity, found `{cap_ty}`"),
                    );
                }
                self.check_type_ref(elem);
                let t = self.annot(elem);
                // The split-endpoint pair: a `Sender<T>` and a `Receiver<T>` over the message type.
                Type::Tuple(vec![
                    Type::Named(stdlib::SENDER.to_string(), vec![t.clone()]),
                    Type::Named(stdlib::RECEIVER.to_string(), vec![t]),
                ])
            }
            Expr::TypedModuleCall {
                recv,
                func,
                func_span,
                ty,
                args,
                span,
            } => {
                // The receiver's local binding (`json` from `use std.json`) resolves to the module's
                // qualified identity through the imports; falling back to the raw binding lets an
                // unimported/typo'd receiver resolve to nothing and report cleanly below.
                let binding = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.to_string(),
                    _ => String::new(),
                };
                // `recv.func::<T>(args)` where the receiver is NOT an imported native module is a
                // generic METHOD call (D3): the atom that spells `json.parse::<T>(s)` also captures
                // a single-type-argument member turbofish on a bare identifier, so `box.pick::<U>(x)`
                // (a value's instance method) and `Box.make::<U>(x)` (a user type's associated
                // function) arrive here too. Route them to the one method-call typing path — the
                // multi-type-argument spelling parses as `Expr::TypedMethodCall` and lands there
                // directly. Record the span so lowering desugars it to a plain method call rather
                // than a native `Rvalue::TypedModuleCall`. A bare identifier that is neither a
                // module, a local binding, nor a user type (a typo'd module) falls through to the
                // native-call path below and reports there, unchanged.
                if let Expr::Ident { name, .. } = recv.as_ref()
                    && !self.imports.modules.contains_key(name.as_str())
                    && (lookup(env, name.as_str()).is_some()
                        || self.symbols.types.contains(name.as_str()))
                {
                    self.check_type_ref(ty);
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
                    self.sites.member_method_call_sites.insert(*span);
                    let ret = self.synth_typed_method_call(
                        recv,
                        func,
                        *func_span,
                        std::slice::from_ref(ty),
                        &mut arg_types,
                        args,
                        *span,
                        env,
                    );
                    // The deferred-argument safety net, as the `TypedMethodCall` arm does.
                    for (i, expr) in CallArg::values(args).enumerate() {
                        if self.is_deferred_arg(expr, env)
                            && matches!(arg_types.get(i), Some(Type::Unknown))
                        {
                            self.synth(expr, env);
                        }
                    }
                    return ret;
                }
                let module = self
                    .imports
                    .modules
                    .get(&binding)
                    .cloned()
                    .unwrap_or_else(|| binding.clone());
                // Arguments are synthesized (checked as expressions) regardless of which function.
                let arg_types: Vec<Type> =
                    CallArg::values(args).map(|a| self.synth(a, env)).collect();
                self.check_type_ref(ty);
                let t = self.annot(ty);
                // A turbofish MENTIONING an in-scope type parameter (poly-values F2b; composites
                // D2a): the recipe is per-instantiation, delivered through the enclosing
                // forwarding fn's hidden slot for this exact template — the bare `T` or the whole
                // composite (`List<T>`), both computed identically by the pre-pass — so record
                // the dynamic site instead of a baked recipe. Only a top-level generic fn (or a
                // nested fn inside one, D2b) has hidden slots to read; method contexts are
                // rejected — the honest boundary, not silently wrong.
                let forwarded = if self.mentions_in_scope_param(&t) {
                    match self
                        .coloring
                        .current_forwarding
                        .iter()
                        .position(|s| s == &t)
                    {
                        Some(idx) => {
                            self.sites.forwarded_slot_sites.insert(*span, idx as u32);
                        }
                        None => {
                            self.error(
                                DiagnosticCode::InvalidTypeArguments,
                                *span,
                                format!(
                                    "cannot forward `{t}` here: call-site-typed forwarding \
                                     carries a generic `fn`'s or method's OWN type parameters, \
                                     and `{t}` is not one of this body's"
                                ),
                            )
                            .help(
                                "an enclosing generic TYPE's parameter reaches a method through \
                                 the receiver, which records the instantiation's name but no \
                                 build recipe — take the type as the method's own parameter \
                                 instead",
                            );
                        }
                    }
                    true
                } else {
                    false
                };
                // Record the build recipe for the turbofish `T`; a type with no recipe (an enum,
                // class, unconstrained generic, …) cannot be built at the call site — a clear error.
                // Deferred so the diagnostic sits after the function-resolution error, if any.
                let has_recipe = forwarded
                    || match self.type_to_recipe(&t) {
                        Some(recipe) => {
                            self.sites.typed_module_call_sites.insert(*span, recipe);
                            true
                        }
                        None => false,
                    };
                // Resolve `module.func::<T>` through the registry's call-site-typed table. A
                // turbofish on a non-call-site-typed or unknown function keeps a clear error; a
                // resolved function validates arity/argument types from its declared signature (the
                // ordinary `ExtFn` argument machinery) and types the result per its declared wrapper
                // (`T`, `Option<T>`, or `Result<T, E>`). `json.parse`/`try_parse` are registered
                // this way — no name is special-cased here.
                match stdlib::typed_module_call(self.reg(), &module, func, &arg_types, t.clone()) {
                    Some((params, required, result)) => {
                        self.check_args(&params, required, &arg_types, args, *span, func);
                        if !has_recipe {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("`{t}` cannot be built by `{binding}.{func}::<T>`"),
                            );
                        }
                        result
                    }
                    None => {
                        self.error(
                            DiagnosticCode::UnknownName,
                            *func_span,
                            format!(
                                "`{binding}.{func}::<T>(...)` is not a call-site-typed native function"
                            ),
                        );
                        t
                    }
                }
            }
            // `f::<T, ...>(args)` — an explicitly instantiated user-generic call (poly-values F2).
            // Arguments defer exactly as a plain call's do (a closure/polymorphic-fn argument
            // finalizes against the SUBSTITUTED parameter type inside the seeded generic check).
            Expr::TypedCall {
                name,
                name_span,
                type_args,
                args,
                span,
            } => {
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
                let ret = self.synth_typed_call(
                    name.as_str(),
                    *name_span,
                    type_args,
                    &mut arg_types,
                    args,
                    *span,
                    env,
                );
                // The deferred-argument safety net, mirroring `synth_call`: any deferred argument
                // no branch finalized is synthesized standalone so its body is always checked.
                for (i, expr) in CallArg::values(args).enumerate() {
                    if self.is_deferred_arg(expr, env)
                        && matches!(arg_types.get(i), Some(Type::Unknown))
                    {
                        self.synth(expr, env);
                    }
                }
                ret
            }
            // `recv.m::<U, ...>(args)` — an explicitly instantiated METHOD call (generic methods,
            // D3). Arguments defer exactly as the free-fn turbofish's do.
            Expr::TypedMethodCall {
                recv,
                name,
                name_span,
                type_args,
                args,
                span,
            } => {
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
                let ret = self.synth_typed_method_call(
                    recv,
                    name,
                    *name_span,
                    type_args,
                    &mut arg_types,
                    args,
                    *span,
                    env,
                );
                // The deferred-argument safety net, as above.
                for (i, expr) in CallArg::values(args).enumerate() {
                    if self.is_deferred_arg(expr, env)
                        && matches!(arg_types.get(i), Some(Type::Unknown))
                    {
                        self.synth(expr, env);
                    }
                }
                ret
            }
            // `Repo::<Todo>` reaching SYNTHESIS means it did not land where it is meaningful. The
            // one place that reads it is the `Type.assoc(args)` static-call arm, which peels it off
            // the receiver before ever synthesizing it (see [`Expr::peel_instantiation`]); anything
            // else — `Repo::<Todo>.new` as a bare handle, `Repo::<Todo>.tbl`, an instance method on
            // a value — has no instantiation to consume and would otherwise type as whatever the
            // underlying reference types as, silently discarding the type arguments.
            Expr::InstantiatedType {
                recv,
                type_args,
                span,
            } => {
                for t in type_args {
                    self.check_type_ref(t);
                }
                let head = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.to_string(),
                    other => format!("{:?}", other.span()),
                };
                self.error(
                    DiagnosticCode::InvalidTypeArguments,
                    *span,
                    "a call-site type argument list must be followed by an associated call"
                        .to_string(),
                )
                .help(format!(
                    "write `{head}::<...>.method(args)`. The instantiation is consumed by the \
                     call; a bare `{head}::<...>` is not a value, and neither an instance method \
                     nor a field reads it (a value carries its own instantiation)"
                ));
                self.synth(recv, env)
            }
            Expr::Invoke {
                recv, name, args, ..
            } => {
                // With a receiver, it is either a value (→ instance method) or a bare type name (→
                // associated function). A bare type name is not an ordinary value expression, so it
                // is licensed here rather than synthesized; any other receiver is synthesized
                // normally (it must be well-typed, but its type is unconstrained — dispatch is
                // dynamic). The name (a `string`) and args (a `List`) are runtime-checked, so they
                // are synthesized leniently. By-name invocation is fallible by construction:
                // unknown name / wrong arity are runtime `Err`, never static errors.
                //
                // Without a receiver (`invoke(name, args)`), the name is a runtime string naming a
                // top-level function. Nothing is licensed and nothing is resolved statically: the
                // name need not be a literal, so there is no declaration to point at, and treating
                // an unresolvable one as a static error would contradict the primitive's contract
                // that *every* resolution failure is a runtime `Err`. Both forms therefore
                // synthesize to the same lenient `Result<dyn, dyn>`.
                if let Some(recv) = recv {
                    let recv_is_type = matches!(
                        recv.as_ref(),
                        Expr::Ident { name, .. } if self.symbols.types.contains(name.as_str())
                    );
                    if !recv_is_type {
                        self.synth(recv, env);
                    }
                }
                self.synth(name, env);
                self.synth(args, env);
                Type::Result(Box::new(Type::Dyn), Box::new(Type::Dyn))
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                ..
            } => self.synth_field_set(receiver, field, *field_span, value, env),
        }
    }

    /// Check/synthesize an object literal whose nominal type is already **known** — either spelled at
    /// the literal (`Name { … }`) or adopted from the expected type by the target-typed `.{ … }`
    /// form. Extracted so both entry points share one body: the two forms differ only in where the
    /// name comes from, and every rule below (`@validated`, private fields, generic-argument
    /// inference, per-field `E0007`) must apply identically to each.
    pub(crate) fn synth_object_named(
        &mut self,
        lit: &ObjectLit,
        type_name: &str,
        env: &mut Env,
    ) -> Type {
        // Resolve the source-written name to its **canonical record key** (native-extensibility S2):
        // a native class is seeded under its *qualified* identity (`geo.Point`), so a source literal
        // `Point { … }` must first map its short name through the `use`-import alias — exactly as
        // native-enum construction resolves `Hue.Red`. A user type of the same short name is in
        // `records` directly (a direct hit wins), and an ordinary user construction is unaffected.
        let canonical: String = if self.symbols.records.contains_key(type_name) {
            type_name.to_string()
        } else {
            self.imports
                .extern_types
                .get(type_name)
                .cloned()
                .unwrap_or_else(|| type_name.to_string())
        };
        let type_name: &str = &canonical;
        // `@validated` (validation arc): a `@validated` type may only be built from OUTSIDE
        // its own `impl`/methods through a validating constructor. A bare literal or a
        // record-update spread outside the type would bypass the invariant, so it is E0060.
        // Construction inside the type's own methods (`current_type`) stays legal; the recipe
        // doors never reach here (they materialize directly), so they remain exempt and
        // auto-validate — that is the whole point.
        if self.symbols.validated_types.contains(type_name)
            && self.coloring.current_type.as_deref() != Some(type_name)
        {
            let kind = if lit.spread.is_some() {
                "a record-update"
            } else {
                "literal construction"
            };
            self.error(
                DiagnosticCode::ValidatedConstruction,
                lit.type_name_span,
                format!(
                    "`{}` is `@validated`: {kind} outside its own `impl` is not allowed",
                    type_name
                ),
            )
            .help(format!(
                "build `{0}` through one of its constructor functions (which runs \
                 `validate()` and returns `Result<{0}, E>`)",
                type_name
            ));
        }
        if let Some(spread) = &lit.spread {
            self.synth(spread, env);
        }
        // Infer the type's arguments from the field values: match each field's declared
        // type (which may be a type parameter) against the value's type, then read the
        // parameters off in declaration order. `Box { value: 1 }` → `Box<int>`. With no
        // generic parameters the result is the bare name; if nothing constrained any
        // parameter the arguments stay empty (a wildcard, compatible with any instantiation).
        let params = self
            .symbols
            .generic_types
            .get(type_name)
            .cloned()
            .unwrap_or_default();
        let decls = self
            .symbols
            .records
            .get(type_name)
            .cloned()
            .unwrap_or_default();
        let pset: ParamSet = params.iter().map(|p| p.id).collect();
        let mut subst: Subst = Subst::new();
        for f in &lit.fields {
            // A polymorphic named function assigned to a **concretely `Fn`-typed field**
            // instantiates against the field's declared type (F1, poly-values) — the field
            // analogue of the parameter/binding absorption — so `Ops { op: double_generic }`
            // checks precisely. Any other value synthesizes exactly as before.
            let declared_field = decls.iter().find(|(n, _)| n == &f.name);
            let field_fn_expectation = declared_field
                .and_then(|(_, declared)| {
                    (matches!(declared, Type::Fn { .. })
                        && !mentions_param(declared, &pset)
                        && self.is_deferred_arg(&f.value, env)
                        && matches!(f.value, Expr::Ident { .. }))
                    .then(|| declared.clone())
                })
                // A nested target-typed `.{ … }` takes the **field's declared type** as its
                // expectation, so `Wrapper { inner: .{ … }, … }` names `inner`'s type. The type's
                // own parameters are erased first, exactly as the assignability check below does:
                // they are inferred *from* this value, so they cannot also constrain it.
                .or_else(|| match (&f.value, declared_field) {
                    (Expr::Object(l), Some((_, declared))) if l.type_name.is_none() => {
                        Some(erase_type_params(declared.clone(), &pset))
                    }
                    _ => None,
                });
            // A generic type's **fresh-constructor call** in a field initializer absorbs the
            // field's declared type, so `Outer { inner: Inner.new("todos") }` against `inner:
            // Inner<Todo>` pins `T = Todo` exactly as the annotated binding `i: Inner<Todo> =
            // Inner.new("todos")` and the argument position do. A field initializer is a *checked*
            // position — the declared type is right there — and synthesizing it bottom-up left the
            // construction site with no instantiation to record. The type's own parameters are
            // erased first (they are inferred *from* this value), and only a fully-concrete
            // expectation is pushed: an open one makes no claim.
            //
            // One exception to the erasure, and it is what lets a generic type construct another
            // generic type out of its own parameter: a parameter the ENCLOSING member forwards on a
            // hidden slot is not inferred from this value — it is *delivered* to it — so erasing it
            // would throw away the only fact the position has. `repo: Repository<T>` inside
            // `LiveRepository<T>.new` keeps its `T`, and `dynamic_ctor_slot` (consulted by
            // `absorbs_constructor_expectation` below) is what decides whether that `T` really
            // arrives; a `T` with no slot still erases and records nothing.
            let inferred_params: ParamSet = pset
                .iter()
                .filter(|id| {
                    !self
                        .coloring
                        .forwardable_params
                        .iter()
                        .any(|p| p.id == **id)
                })
                .copied()
                .collect();
            let absorbed_declared = if field_fn_expectation.is_none()
                && let Some((_, declared)) = declared_field
                && let e = erase_type_params(declared.clone(), &inferred_params)
                && self.absorbs_constructor_expectation(&f.value, &e, env)
            {
                Some(e)
            } else {
                None
            };
            let vty = match field_fn_expectation.as_ref().or(absorbed_declared.as_ref()) {
                Some(expected) => self.check(&f.value, expected, env),
                None => self.synth(&f.value, env),
            };
            // A literal that sets a private field is only valid inside the declaring type's
            // own methods (slice 2d) — a `class` with private fields is built externally
            // through an associated `fn`/constructor, not a bare literal.
            if !self.field_visible(type_name, &f.name) {
                self.report_private_field(type_name, &f.name, FieldAccess::Set, f.name_span);
            }
            if let Some((_, declared)) = decls.iter().find(|(n, _)| n == &f.name) {
                if !pset.is_empty() {
                    bind_type_params(declared, &vty, &pset, &mut subst);
                }
                // The field value must be assignable to the declared field type (`E0007`),
                // mirroring the field-default check. The type's own parameters are erased to
                // `dyn` (they are inferred from this very value above), so a generic field
                // accepts any value while a concrete field type is enforced.
                let expected = erase_type_params(declared.clone(), &pset);
                // …unless the value was just CHECKED against that very type above, whose `subsume`
                // has already reported any mismatch at this same span. Re-testing it here would
                // print the identical error twice.
                if absorbed_declared.as_ref() != Some(&expected)
                    && !self.arg_assignable(&vty, &expected)
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        f.value.span(),
                        format!(
                            "field `{}` expects type `{expected}`, found `{vty}`",
                            f.name
                        ),
                    );
                }
            }
        }
        // Fill any parameter the field values left unconstrained from the CHECKED position's
        // expectation. Purely additive — an argument the fields DID pin stays as inferred, so the
        // established "fields determine the instantiation" rule is untouched; this only decides
        // what an otherwise-unconstrained parameter is, where the alternative is nothing at all.
        // Only a fully concrete expectation contributes: a `dyn`/open argument makes no claim.
        if !params.is_empty()
            && let Some((expected_span, Type::Named(n, expected_args))) =
                self.coloring.expected_object.clone()
            && expected_span == lit.span
            && n == type_name
        {
            for (i, p) in params.iter().enumerate() {
                if !subst.contains_key(&p.id)
                    && let Some(t) = expected_args.get(i)
                    && self.fully_concrete(t)
                {
                    subst.insert(p.id, t.clone());
                }
            }
        }
        let args = if subst.is_empty() {
            Vec::new()
        } else {
            params
                .iter()
                .map(|p| subst.get(&p.id).cloned().unwrap_or(Type::Dyn))
                .collect()
        };
        let ty = Type::Named(type_name.to_string(), args);
        self.note_construction(&ty, lit.span);
        ty
    }
}

/// The runtime base type a scalar of an **erased** fixed width collapses to — `"int"` for any
/// `iN`/`uN`, `"float"` for `f64` — or `None` for a name that is *not* an erased scalar width.
/// Returns `None` for `f32` (reified at runtime, so `x is f32` is a real test) and for every
/// non-width name. Routed through the single [`noeta_ast::BuiltinTy`] decoder rather than a parallel
/// string match, so the erased-width vocabulary stays in one place. Drives the E0063 warning.
fn erased_scalar_width_base(name: &str) -> Option<&'static str> {
    match noeta_ast::BuiltinTy::from_name_any(name)? {
        noeta_ast::BuiltinTy::IntN { .. } => Some("int"),
        noeta_ast::BuiltinTy::F64 => Some("float"),
        _ => None,
    }
}

/// The answer to `<scrut> is <target>` when the scrutinee's **static type settles it**, for an
/// erased-width `target` (`iN`/`f64` — the caller has already established that). `None` means the
/// scrutinee does not settle it, which for these targets is the same as "nobody can": the width is
/// not on the value, so if it is not in the static type it is gone.
///
/// A width has *identity-only* subtyping — `i32` is not an `i64`, and `f64` deliberately does not
/// widen to or from `float` — so for a scrutinee that is a single concrete type, equality is the
/// whole answer. The unsettled cases are exactly the open ones:
///
/// - `dyn` and an inference hole: the launder that erased the width in the first place;
/// - a `dyn Trait`: open in the same way, over a smaller set;
/// - a bare type parameter: erased, and its instantiation is not in this body;
/// - a union (which is what `number` is): the checker knows a *set* of types, not which one — and
///   the value cannot be asked, so no member of the set can be ruled in or out;
/// - a kind-type (`Enum`/`Struct`/`Class`): abstract over declarations, not a single type.
///
/// [`Type::Never`] is *not* on that list: it is uninhabited, so the test is in unreachable code and
/// the constant `false` is as true as anything else there.
fn settled_width_answer(scrut: &Type, target: &Type) -> Option<bool> {
    match scrut {
        Type::Unknown
        | Type::Dyn
        | Type::DynTrait(_)
        | Type::Param(_)
        | Type::Union(_)
        | Type::Kind(_) => None,
        _ => Some(scrut == target),
    }
}

impl Checker {
    /// Whether `<scrut> is <ty>` is **statically impossible**, and if so the idiom that does what
    /// the author meant (E0065's help line).
    ///
    /// `Option` and `Result` are *reified* containers: each carries its own runtime head
    /// constructor (`some`/`none`, `Ok`/`Err`), never the payload's. So `x is P` on an
    /// `Option<P>` is always false — yet it reads exactly like "is it a `P`", which is why it gets
    /// written. The damage was never the constant test; it was that the checker went on to
    /// *narrow* `x` to `P` in the branch, so the dead code type-checked and only the runtime
    /// disagreed.
    ///
    /// `None` (i.e. "leave it alone") for every case where the tag genuinely could match:
    /// - an open target (`dyn`, `dyn Trait`, an inference hole) — the runtime decides;
    /// - a kind-type target — **both containers are enums at runtime**, so `x is Enum` is `true`
    ///   and flagging it would be simply wrong;
    /// - a bare type parameter — erased, and it may instantiate to the container itself;
    /// - the same container (`x is Option<…>` on an `Option`), which is the true test.
    pub(crate) fn impossible_type_test(&self, scrut: &Type, ty: &TypeRef) -> Option<String> {
        let target = self.annot(ty);
        if matches!(
            target,
            Type::Dyn | Type::Unknown | Type::DynTrait(_) | Type::Kind(_)
        ) {
            return None;
        }
        if matches!(target, Type::Param(_)) {
            return None;
        }
        match scrut {
            Type::Option(inner) if !matches!(target, Type::Option(_)) => Some(format!(
                "test presence with `x != none`, or reach the `{inner}` with \
                 `match x {{ some(v) => …, none => … }}`"
            )),
            Type::Result(..) if !matches!(target, Type::Result(..)) => Some(
                "match on it instead: `match x { Ok(v) => …, Err(e) => … }`, or unwrap it with `?`"
                    .to_string(),
            ),
            _ => None,
        }
    }
}
