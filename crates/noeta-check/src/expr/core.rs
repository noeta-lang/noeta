//! THE BIDIRECTIONAL CORE, kept together on purpose: the mutually recursive
//! [`Checker::check`]/[`Checker::synth`] judgments and their dispatch bodies, plus subsumption
//! and literal adaptation. `check ↔ synth` mutual recursion IS the algorithm — splitting the
//! dispatch across files would trade one long file for hidden coupling, so the arm groups that
//! left this file are only the delegated siblings (`ops`/`calls`/`member`/`patterns`).

use crate::*;

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
        let ty = self.check_inner(expr, expected, env);
        if self.config.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.symbols.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
    }

    pub(crate) fn check_inner(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        match expr {
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
                self.check(&args[0], inner, env);
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
                        self.check(arg, ok, env);
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
                self.check(&args[0], err, env);
                expected.clone()
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
                            .map(|t| from_ref_q(t, &self.imports.extern_types))
                            .or_else(|| expected_params.get(i).cloned())
                            .unwrap_or(Type::Unknown)
                    })
                    .collect();
                for (p, pty) in params.iter().zip(&bound) {
                    self.check_reserved_name(&p.name, p.name_span);
                    // Closure params land in the just-pushed frame — any env hit is a shadow
                    // (E0058), enclosing capture and same-list duplicate alike.
                    self.check_shadow(&p.name, p.name_span, env, crate::ShadowScopes::All);
                    bind(env, &p.name, pty.clone());
                }
                // An explicit return annotation is the body's expected type and the closure's return
                // type; it must also satisfy the context's expected return. Without one the expected
                // return drives the body — UNLESS that expectation is `dyn`, the builtin "any
                // result" shape (`map` expects `(T) -> dyn`): checking against `dyn` would erase the
                // body's real type and starve the call-site refinements (`xs.map(f) → List<R>`), so
                // the body is inferred instead; `dyn` accepts whatever comes out.
                let declared = ann
                    .as_ref()
                    .map(|t| from_ref_q(t, &self.imports.extern_types));
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
            // A bare numeric literal adapts into a fixed-width context — `x: u8 = 200`, `y: i8 = -5`,
            // `z: f32 = 1.5`, `w: f64 = 1.5` (P-NUM-SYM). Shared with call-argument checking via
            // `try_adapt_literal`; a non-adapting pair falls through to synthesize-and-check.
            _ => {
                if let Some(adapted) = self.try_adapt_literal(expr, expected) {
                    return adapted;
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
        if self.config.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.symbols.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
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
            Expr::Ident { name, span } => match lookup(env, name)
                .cloned()
                // A bare user-function reference is a first-class value of its **full** signature
                // type — parameters included, so passing it where a `Fn(A) -> B` is declared
                // (`map_bounded(items, n, dbl)`, `xs.map(inc)`) checks like the equivalent
                // closure. A generic function's erased params are `dyn`, which defers per
                // position. (Was params-erased until higher-order-abi H2 made module signatures
                // carry declared `Fn` params, which an erased handle could never satisfy.)
                .or_else(|| {
                    self.symbols.functions.get(name).map(|sig| Type::Fn {
                        params: sig.params.clone(),
                        ret: Box::new(sig.ret.clone()),
                    })
                })
                // A selectively-imported module function referenced as a value (`let f = sqrt`).
                .or_else(|| {
                    self.imports
                        .imported_fns
                        .contains_key(name)
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
                    } else if !self.config.session_mode && !self.is_known_name(name, env) {
                        // A bare reference to a name that resolves to nothing — a genuinely
                        // undefined value (F1), the same static `E0005` as an unknown callee. A
                        // session defers (a later entry may define it). In a SEALED named-fn
                        // body a miss that names a real top-level binding gets the capture hint.
                        let sealed_global_miss = self.coloring.in_sealed_body
                            && self.symbols.global_binding_names.contains(name);
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
                        if is_deferred_literal_arg(a) {
                            Type::Unknown
                        } else {
                            self.synth(a, env)
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
                env.push(HashMap::new());
                for p in params {
                    self.check_reserved_name(&p.name, p.name_span);
                    // Same rule as the check-mode arm: any env hit is a shadow (E0058).
                    self.check_shadow(&p.name, p.name_span, env, crate::ShadowScopes::All);
                    bind(env, &p.name, param_type(p, &self.imports.extern_types));
                }
                // With an explicit return annotation, check the body against it (and adopt it as the
                // closure's return type); otherwise infer it from the body (the arrow expression's
                // type, or a block's joined `return`s).
                let declared = ann
                    .as_ref()
                    .map(|t| from_ref_q(t, &self.imports.extern_types));
                let ret = self.closure_body_type(body, declared.as_ref(), env);
                env.pop();
                Type::Fn {
                    params: params
                        .iter()
                        .map(|p| param_type(p, &self.imports.extern_types))
                        .collect(),
                    ret: Box::new(ret),
                }
            }
            Expr::Pipeline { left, right, .. } => {
                // `left |> right` threads `left` as the first argument of `right`.
                let piped = self.synth(left, env);
                self.synth_piped(right, piped, env)
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
                    .get(&lit.type_name)
                    .cloned()
                    .unwrap_or_default();
                let decls = self
                    .symbols
                    .records
                    .get(&lit.type_name)
                    .cloned()
                    .unwrap_or_default();
                let pset: HashSet<String> = params.iter().cloned().collect();
                let mut subst: HashMap<String, Type> = HashMap::new();
                for f in &lit.fields {
                    let vty = self.synth(&f.value, env);
                    // A literal that sets a private field is only valid inside the declaring type's
                    // own methods (slice 2d) — a `class` with private fields is built externally
                    // through an associated `fn`/constructor, not a bare literal.
                    if !self.field_visible(&lit.type_name, &f.name) {
                        self.report_private_field(
                            &lit.type_name,
                            &f.name,
                            FieldAccess::Set,
                            f.name_span,
                        );
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
                        if !self.arg_assignable(&vty, &expected) {
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
                let args = if subst.is_empty() {
                    Vec::new()
                } else {
                    params
                        .iter()
                        .map(|p| subst.get(p).cloned().unwrap_or(Type::Dyn))
                        .collect()
                };
                let ty = Type::Named(lit.type_name.clone(), args);
                self.note_construction(&ty, lit.span);
                ty
            }
            Expr::Try { expr, span } => {
                let inner = self.synth(expr, env);
                match &inner {
                    Type::Result(ok, err) => {
                        // The error-position rule (error-ergonomics): a mismatched `Err` type
                        // either converts through the target's `impl From<Source>` (site recorded
                        // for lowering) or is E0057.
                        let err = (**err).clone();
                        self.check_try_error(&err, *span);
                        (**ok).clone()
                    }
                    Type::Option(some) => (**some).clone(),
                    // A hole carries no info; `dyn` defers to runtime — both accept `?` without a
                    // diagnostic, yielding the same deferred type.
                    t if t.defers_to_runtime() => t.clone(),
                    other => {
                        self.error(
                            DiagnosticCode::InvalidTry,
                            *span,
                            format!("`?` expects a `Result` or `Option`, found `{other}`"),
                        )
                        .help("`?` only propagates `Result`/`Option`; this value is neither");
                        Type::Unknown
                    }
                }
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
                value, fallback, ..
            } => {
                let v = self.synth(value, env);
                self.synth(fallback, env);
                match v {
                    Type::Result(ok, _) => *ok,
                    Type::Option(some) => *some,
                    _ => Type::Unknown,
                }
            }
            Expr::As { expr, ty, span } => {
                let src = self.synth(expr, env);
                self.check_type_ref(ty);
                let target = from_ref_q(ty, &self.imports.extern_types);
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
            Expr::TypeTest { expr, ty, .. } => {
                // A type *test* is always well-formed on any source — even a concrete one (it is
                // simply a constant `true`/`false`), unlike `.as<T>()` whose narrowing of a known
                // concrete value is an `E0028`. We only validate the target type names something.
                self.synth(expr, env);
                self.check_type_ref(ty);
                Type::Bool
            }
            Expr::AttributesOf { ty, span } => {
                self.check_type_ref(ty);
                let target = from_ref_q(ty, &self.imports.extern_types);
                // The type argument must itself be an attribute — a struct marked `@attribute` (the
                // same capability gate as a `#[T(...)]` use). Otherwise the manifest holds no `T` to
                // materialize.
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
            Expr::RolesOf { ty, span } => {
                // The compiler-built role index, surfaced as `List<RoleBinding>`. The optional
                // turbofish scopes the query to one role enum, which — like `attributes_of`'s
                // `@attribute` gate — must be a `@semantic` enum (only those contribute roles).
                if let Some(ty) = ty {
                    self.check_type_ref(ty);
                    let target = from_ref_q(ty, &self.imports.extern_types);
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
                let elem = from_ref_q(ty, &self.imports.extern_types);
                // The element type must be a packable `@packed` struct — the blob is a flat packed
                // buffer. Recording the layout in `packed_list_sites` (the channel list literals use)
                // hands the backend the schema to rebuild the list. Generic over any declared packable
                // type (no hardcoded list — extension-friendly).
                match self.packed_layout(&elem) {
                    Some(layout) => {
                        self.sites.packed_list_sites.insert(*span, layout);
                    }
                    None => {
                        self.error(
                            DiagnosticCode::InvalidPackedType,
                            *span,
                            format!(
                                "`from_bytes::<{elem}>` requires a packable `@packed` struct element type"
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
                let t = from_ref_q(elem, &self.imports.extern_types);
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
                    Expr::Ident { name, .. } => name.clone(),
                    _ => String::new(),
                };
                let module = self
                    .imports
                    .modules
                    .get(&binding)
                    .cloned()
                    .unwrap_or_else(|| binding.clone());
                // Arguments are synthesized (checked as expressions) regardless of which function.
                let arg_types: Vec<Type> = args.iter().map(|a| self.synth(a, env)).collect();
                self.check_type_ref(ty);
                let t = from_ref_q(ty, &self.imports.extern_types);
                // Record the build recipe for the turbofish `T`; a type with no recipe (an enum,
                // class, unconstrained generic, …) cannot be built at the call site — a clear error.
                // Deferred so the diagnostic sits after the function-resolution error, if any.
                let has_recipe = match self.type_to_recipe(&t) {
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
            Expr::Invoke {
                recv, name, args, ..
            } => {
                // The receiver is either a value (→ instance method) or a bare type name (→
                // associated function). A bare type name is not an ordinary value expression, so it
                // is licensed here rather than synthesized; any other receiver is synthesized
                // normally (it must be well-typed, but its type is unconstrained — dispatch is
                // dynamic). The name (a `string`) and args (a `List`) are runtime-checked, so they
                // are synthesized leniently. By-name invocation is fallible by construction:
                // unknown name / wrong arity are runtime `Err`, never static errors.
                let recv_is_type = matches!(
                    recv.as_ref(),
                    Expr::Ident { name, .. } if self.symbols.types.contains(name)
                );
                if !recv_is_type {
                    self.synth(recv, env);
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
}
