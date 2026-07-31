//! **Match & pattern typing**: match-arm synthesis, exhaustiveness (E0011), `for`/`match`
//! pattern binding, and enum-variant payload resolution. All `Checker` methods moved verbatim
//! out of the crate root.

use crate::*;

/// The Option-`none` pattern spelling. Parsed as a [`Pattern::Binding`] like any other bare
/// identifier — which is exactly why it needs naming here.
const NONE_PATTERN: &str = "none";

/// The E0066 help for a bare `none` arm that did **not** resolve to the Option case: the scrutinee
/// is not an option, so the name is an ordinary binding and matches every value.
const NONE_ARM_HELP: &str = "`none` resolves to the Option case only against an `?T` scrutinee; \
                             this one is not an option, so `none` here is a plain binding — it \
                             matches every value and has to come last";

impl Checker {
    /// Type a `match` in *synthesis* position — no expectation reaches the arms, so each arm body
    /// synthesizes on its own. See [`Self::match_type`] for the full rule.
    pub(crate) fn synth_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        env: &mut Env,
        value_used: bool,
    ) -> Type {
        self.match_type(scrutinee, arms, span, env, value_used, None)
    }

    /// Type a `match`. `value_used` is `true` when the match stands in value position (its result
    /// is consumed — a binding RHS, an argument, an operand, a `return`), and `false` only when it
    /// is the whole of an expression statement (its value discarded). Block-bodied arms (aether F1)
    /// produce no value — blocks are statement sequences in Noeta — so in value position they are a
    /// hard error (E0055) rather than silently contributing `unit`; in statement position they are
    /// the intended side-effect form.
    ///
    /// `expected` carries the *bidirectional expectation* the whole `match` was checked against, and
    /// is threaded into every arm body so an arm is checked against exactly the type the expression
    /// as a whole is. That is what lets an absorbing literal — a mixed `Map<string, dyn>`, an empty
    /// `{}`/`[]`, a `.{ … }` — appear directly in an arm instead of having to be lifted into its own
    /// annotated binding or function. `if c then a else b` desugars to a `match` in the parser, so
    /// its branches ride the same path. `None` is a genuinely open position (a statement-position
    /// `match`, or one whose context supplies no type), where every arm synthesizes exactly as
    /// before.
    pub(crate) fn match_type(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        env: &mut Env,
        value_used: bool,
        expected: Option<&Type>,
    ) -> Type {
        let scrut = self.synth(scrutinee, env);
        self.check_exhaustive(&scrut, arms, span);
        // Flow-narrowing: an `is T` arm sees the scrutinee narrowed to `T`, but only when the
        // scrutinee is a bare identifier (there is then a name to re-type in the arm scope).
        let scrut_ident = match scrutinee {
            Expr::Ident { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let mut result = Type::Unknown;
        // The arm sequence's own soundness (E0066), threaded through the loop so it reports in
        // source order alongside each arm's body diagnostics. `catch_all` is the first unguarded
        // irrefutable arm seen so far — everything after it is dead.
        let mut catch_all: Option<(String, Span)> = None;
        for arm in arms {
            self.check_arm_reachability(arm, &scrut, &mut catch_all);
            env.push(HashMap::new());
            self.bind_pattern(&arm.pattern, &scrut, env);
            // An `is T` arm's target is a type annotation like any other — validate it, so an arm
            // naming a type that does not exist is the same E0013 the expression form
            // (`d is Nonexistent`) already reported. It used to be accepted in silence purely
            // because this path never called the validator.
            if let Pattern::IsType { ty, .. } = &arm.pattern {
                let before = self.diags.len();
                self.check_type_ref(ty);
                // A type-parameter target is refused here — unlike the expression form, which now
                // resolves it. See `reject_type_param_pattern` for why the pattern cannot.
                self.reject_type_param_pattern(ty);
                // …and, when the target does resolve, the same always-false rule the expression
                // form applies (E0065): an `is P` arm on an `Option<P>` scrutinee can never be
                // taken, so it is reported and — below — narrows nothing.
                if self.diags.len() == before
                    && let Some(idiom) = self.impossible_type_test(&scrut, ty)
                {
                    let target = self.annot(ty);
                    self.warn(
                        DiagnosticCode::ImpossibleTypeTest,
                        ty.span(),
                        format!(
                            "`{scrut}` is its own runtime type, not its payload's; \
                             this `is {target}` arm can never be taken"
                        ),
                    )
                    .help(idiom);
                }
            }
            // Flow-narrowing applies…except when the test can never hold: an unreachable arm
            // narrows nothing.
            if let (Some(name), Pattern::IsType { ty, .. }) = (scrut_ident, &arm.pattern)
                && self.impossible_type_test(&scrut, ty).is_none()
            {
                bind(env, name, self.annot(ty));
            }
            // The guard (`pattern if cond`) is checked in the arm scope — after the pattern
            // bindings and any `is`-narrowing above, so `Ok(age) if age >= 18` and
            // `c is Circle if c.r > 1.0` both resolve. It is a bool position (bidirectional
            // `check` against `bool`, E0007 on mismatch). It cannot `.await`: the state-machine
            // lowering cannot suspend between a pattern test and its guard (a hoisted await
            // would run eagerly, breaking the only-the-taken-arm-evaluates rule).
            if let Some(guard) = &arm.guard {
                self.check(guard, &Type::Bool, env);
                if guard.has_await() {
                    self.error(
                        DiagnosticCode::AsyncMisuse,
                        guard.span(),
                        "`.await` is not allowed in a `match` guard".to_string(),
                    )
                    .help(
                        "await the value before the `match` and test the result in the guard, \
                         or move the `.await` into the arm's body",
                    );
                }
            }
            let t = match &arm.body {
                // The arm body inherits the whole `match`'s expectation, so a form that can only be
                // typed *against* one (a heterogeneous map, an empty collection, `.{ … }`, a bare
                // numeric literal narrowing to a fixed width) works in an arm exactly as it does
                // after a `return`. Without an expectation this is plain synthesis.
                noeta_ast::ClosureBody::Expr(e) => match expected {
                    Some(exp) => self.check(e, exp, env),
                    None => self.synth(e, env),
                },
                // A statement-block arm (aether F1): check its statements in the arm scope; the
                // arm's value is `unit`. In value position that is a silent value loss — blocks
                // never produce values — so reject it (E0055).
                noeta_ast::ClosureBody::Block(stmts) => {
                    if value_used {
                        self.error(
                            DiagnosticCode::MatchArmNotValue,
                            arm.span,
                            "a block-bodied match arm produces no value, but this `match` is used \
                             as an expression; give the arm a value with `=> <expr>`, or use the \
                             `match` as a statement (block arms are for side effects)"
                                .to_string(),
                        );
                    }
                    for stmt in stmts {
                        self.check_stmt(stmt, env);
                    }
                    Type::Unit
                }
            };
            env.pop();
            if result.is_gradual() {
                result = t;
            }
        }
        result
    }

    /// One arm's **reachability** check (E0066), folded over the arm list: `catch_all` carries the
    /// first unguarded irrefutable arm seen so far (its rendered pattern text and span), so every
    /// later arm is provably dead.
    ///
    /// Irrefutability is a `_` wildcard or an unguarded bare-identifier [`Pattern::Binding`] that
    /// did **not** resolve to a payload-free variant of the scrutinee's own enum
    /// ([`Self::payload_free_variant`]) — a resolved one is a case test like any other variant
    /// pattern and leaves the arms after it live. Both irrefutable forms compile to *no test at all*
    /// in either backend (the binding form merely names the scrutinee), so nothing downstream can
    /// rescue a later arm. A guard makes an arm refutable (the checker cannot prove a guard ever
    /// true), and every other pattern form emits a test.
    fn check_arm_reachability(
        &mut self,
        arm: &MatchArm,
        scrut: &Type,
        catch_all: &mut Option<(String, Span)>,
    ) {
        if let Some((catch_text, catch_span)) = catch_all.clone() {
            let help = if catch_text == NONE_PATTERN {
                // A bare `none` that reached catch-all status is a `none` the scrutinee's type did
                // not resolve — the one case where the prelude spelling really is just a binding.
                NONE_ARM_HELP.to_string()
            } else {
                format!(
                    "delete this arm, or move the catch-all `{catch_text}` arm to last position"
                )
            };
            self.error(
                DiagnosticCode::UnreachableMatchArm,
                arm.pattern.span(),
                format!(
                    "this `match` arm can never run: the `{catch_text}` arm above already matches \
                     every value"
                ),
            )
            // Both spans are labelled explicitly: the renderer drops its implicit primary label as
            // soon as a diagnostic carries any of its own, and the arm that died is half the story.
            .label(catch_span, "this pattern already matches every value…")
            .label(arm.pattern.span(), "…so this arm is never reached")
            .help(help);
        }
        // Only an *unguarded* wildcard/binding closes the match — a guard leaves the case open.
        if catch_all.is_none() && arm.guard.is_none() {
            let text = match &arm.pattern {
                Pattern::Wildcard { .. } => Some("_".to_string()),
                Pattern::Binding { name, .. } if self.is_binding_pattern(scrut, name) => {
                    Some(name.clone())
                }
                _ => None,
            };
            if let Some(text) = text {
                *catch_all = Some((text, arm.pattern.span()));
            }
        }
    }

    /// Whether a bare identifier `name` really is a **binding** against this scrutinee — the
    /// negation of [`Self::payload_free_variant`], named so the three consumers that must agree
    /// (arm reachability, coverage, and [`Self::bind_pattern`]) read the same way.
    fn is_binding_pattern(&self, scrut: &Type, name: &str) -> bool {
        self.payload_free_variant(scrut, name).is_none()
    }

    /// **The one bare-identifier-pattern resolution.** The **payload-free** enum variant a bare
    /// identifier names, given the type of the value it is matched against — `(Some("Type"),
    /// "String")` for `String` against a `Type` that declares `String;`.
    ///
    /// A payload-carrying variant is call-shaped (`Type.List(inner)`, `some(x)`) and so is never
    /// ambiguous; a payload-free one spelled bare is indistinguishable *in the source* from a
    /// binding, and this is where the ambiguity is decided: the scrutinee's own enum wins, and
    /// everything else stays a binding. Resolution is therefore **scrutinee-directed** — a name that
    /// belongs to some *other* enum, or any name at all when the scrutinee's type is gradual /
    /// `dyn` / not concretely known, resolves to nothing.
    ///
    /// The built-in `Option` joins in on its own terms: `none` is the correct bare spelling of its
    /// payload-free case and has no written type name, so it resolves with no qualifier (`Ok`/`Err`
    /// and `some` all carry a payload and never reach here). That is what makes
    /// `match o { none => …, some(v) => … }` mean what it reads as in either order.
    pub(crate) fn payload_free_variant(
        &self,
        scrut: &Type,
        name: &str,
    ) -> Option<(Option<String>, String)> {
        match scrut {
            Type::Option(_) if name == NONE_PATTERN => Some((None, NONE_PATTERN.to_string())),
            Type::Named(type_name, _) => {
                let key = self.enum_type_key(type_name)?;
                self.symbols
                    .enums
                    .get(&key)?
                    .iter()
                    .any(|v| v.name == name && v.fields.is_empty())
                    // The qualifier is the name as *written* in the scrutinee's type, which is
                    // exactly what an author would spell in `Type.Variant` — so an import alias
                    // keeps flowing through the backends' alias resolution unchanged.
                    .then(|| (Some(type_name.clone()), name.to_string()))
            }
            _ => None,
        }
    }

    /// Promote a non-exhaustive `match` to a compile error (`E0011`), and record an *exhaustive*
    /// one in [`Checker::exhaustive_matches`] so the return-flow analysis (`E0048`) can count it.
    /// The judgement itself lives in [`Self::match_coverage`]; this is only its reporting half.
    pub(crate) fn check_exhaustive(&mut self, scrut: &Type, arms: &[MatchArm], span: Span) {
        let (cases, domain) = match self.match_coverage(scrut, arms) {
            // Control is guaranteed to enter some arm. Remember it by span: the E0048 walk runs
            // over the bare AST *after* the body is typed and has no scrutinee types of its own,
            // so this is the only place the answer exists.
            MatchCoverage::Total => {
                self.exhaustive_matches.insert(span);
                return;
            }
            // An open or unknown domain — no judgement either way; the runtime `MatchFail`
            // backstop stands, and E0048 must not count the `match`.
            MatchCoverage::Unknown => return,
            MatchCoverage::Missing { cases, domain } => (cases, domain),
        };
        // A guarded arm is the usual reason a case looks uncovered, so say so when one is present.
        let help = if arms.iter().any(|a| a.guard.is_some()) {
            format!(
                "{}; a guarded arm (`pattern if cond`) does not count — its case stays \
                 uncovered when the guard is false",
                domain.help()
            )
        } else {
            domain.help().to_string()
        };
        self.error(
            DiagnosticCode::NonExhaustiveMatch,
            span,
            format!("non-exhaustive `match`: missing {}", cases.join(", ")),
        )
        .help(help);
    }

    /// **The one exhaustiveness judgement.** Whether `arms` cover every value the scrutinee type
    /// `scrut` admits — answered once and consumed twice: `E0011` reports
    /// [`MatchCoverage::Missing`], and the `E0048` return-flow analysis counts a
    /// [`MatchCoverage::Total`] `match` whose arms all diverge as diverging itself.
    ///
    /// Only a concretely-known enum / `Result` / `Option` (or a union under `is` arms) has a domain
    /// the checker can enumerate; anything else (an `int`/`string`/`bool` scrutinee, or a gradual
    /// type) is [`MatchCoverage::Unknown`] rather than either verdict — keeping E0011 free of false
    /// positives and E0048 sound in the same stroke.
    ///
    /// A **guarded** arm (`pattern if cond`) contributes nothing to coverage: the checker cannot
    /// prove a guard ever true, so its case stays uncovered for when the guard is false. Only
    /// unguarded arms count below. (A guarded arm followed by an irrefutable `_` is still total —
    /// the `_` covers what the guard may decline.)
    pub(crate) fn match_coverage(&self, scrut: &Type, arms: &[MatchArm]) -> MatchCoverage {
        // A wildcard or bare binding arm catches everything — unless it is guarded, or the bare
        // identifier resolved to a payload-free variant of this very scrutinee's enum, in which
        // case it is a case test and covers only that one case (counted with the variant arms
        // below). The same judgement `check_arm_reachability` and `bind_pattern` make.
        if arms.iter().any(|a| {
            a.guard.is_none()
                && match &a.pattern {
                    Pattern::Wildcard { .. } => true,
                    Pattern::Binding { name, .. } => self.is_binding_pattern(scrut, name),
                    _ => false,
                }
        }) {
            return MatchCoverage::Total;
        }
        // A type-pattern match (`is T` arms): the domain is *types*, not variant names. A union is
        // a closed domain — exhaustive iff every member is covered by some `is` arm; `dyn` is the
        // open top — a finite set of `is` arms can never exhaust it, so it needs a `_`.
        let type_targets: Vec<Type> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::IsType { ty, .. } if a.guard.is_none() => {
                    Some(self.annot(ty))
                }
                _ => None,
            })
            .collect();
        // Any `is` arm (guarded or not) selects the type-domain analysis; only the unguarded
        // targets collected above count as coverage.
        let has_is_arms = arms
            .iter()
            .any(|a| matches!(&a.pattern, Pattern::IsType { .. }));
        if has_is_arms {
            let cases: Vec<String> = match scrut {
                Type::Union(members) => members
                    .iter()
                    .filter(|m| !type_targets.iter().any(|t| Type::subtype(m, t)))
                    .map(|m| m.to_string())
                    .collect(),
                Type::Dyn => vec!["a `dyn` value (open type domain)".into()],
                // A concrete or gradual scrutinee with `is` arms is not exhaustiveness-checked.
                _ => return MatchCoverage::Unknown,
            };
            return MatchCoverage::from_missing(cases, MatchDomain::Types);
        }
        let all: Vec<String> = match scrut {
            Type::Result(..) => vec!["Ok".into(), "Err".into()],
            Type::Option(..) => vec!["some".into(), "none".into()],
            Type::Named(n, _) => match self.symbols.enums.get(n) {
                Some(variants) => variants.iter().map(|v| v.name.clone()).collect(),
                None => return MatchCoverage::Unknown,
            },
            _ => return MatchCoverage::Unknown,
        };
        let covered: HashSet<&str> = arms
            .iter()
            .filter(|a| a.guard.is_none())
            .filter_map(|a| match &a.pattern {
                Pattern::Variant { variant, .. } => Some(variant.as_str()),
                // A bare payload-free variant covers its case exactly as the qualified spelling
                // does — so a `match` naming every case bare is exhaustive with no `_`.
                Pattern::Binding { name, .. } if !self.is_binding_pattern(scrut, name) => {
                    Some(name.as_str())
                }
                _ => None,
            })
            .collect();
        let cases: Vec<String> = all
            .into_iter()
            .filter(|v| !covered.contains(v.as_str()))
            .collect();
        MatchCoverage::from_missing(cases, MatchDomain::Variants)
    }
}

/// Which domain a `match`'s arms are being checked against — the shape of the "what is missing"
/// advice, kept as an enum rather than two loose help strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchDomain {
    /// `is T` type-pattern arms over a union (or the open `dyn` top).
    Types,
    /// Enum-variant arms, including `Result`'s `Ok`/`Err` and `Option`'s `some`/`none`.
    Variants,
}

impl MatchDomain {
    fn help(self) -> &'static str {
        match self {
            MatchDomain::Types => "add an `is T` arm for each missing type, or a `_` catch-all",
            MatchDomain::Variants => "add an arm for each missing case, or a `_` catch-all",
        }
    }
}

/// The checker's verdict on a `match`'s coverage — see [`Checker::match_coverage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MatchCoverage {
    /// Every value the scrutinee admits reaches some *unguarded* arm: the `match` cannot fail, so
    /// control is guaranteed to enter an arm.
    Total,
    /// These cases are provably uncovered (`E0011`).
    Missing {
        cases: Vec<String>,
        domain: MatchDomain,
    },
    /// The scrutinee's domain is open or not concretely known, so neither verdict is provable.
    Unknown,
}

impl MatchCoverage {
    /// [`MatchCoverage::Total`] for an empty missing-case list, [`MatchCoverage::Missing`]
    /// otherwise — the one place "nothing missing means exhaustive" is written down.
    fn from_missing(cases: Vec<String>, domain: MatchDomain) -> MatchCoverage {
        if cases.is_empty() {
            MatchCoverage::Total
        } else {
            MatchCoverage::Missing { cases, domain }
        }
    }
}

impl Checker {
    // ----- pattern binding -----

    pub(crate) fn bind_for_pattern(&mut self, pattern: &ForPattern, iter_ty: &Type, env: &mut Env) {
        // The element type a `for` loop binds: a list/set's element, a map's **value** (iteration
        // yields values, like the runtime), or an `Iterator<T>`'s element (Track I.2). Anything else
        // (a `dyn`/gradual source) binds a hole.
        let elem = match iter_ty {
            Type::List(t) | Type::Set(t) => (**t).clone(),
            Type::Map(_, v) => (**v).clone(),
            Type::Named(n, args) if n == stdlib::ITERATOR => {
                args.first().cloned().unwrap_or(Type::Unknown)
            }
            _ => Type::Unknown,
        };
        match pattern {
            ForPattern::Single { name, name_span } => {
                self.check_reserved_name(name, *name_span);
                // The loop variable lands in the loop's just-pushed frame — any env hit is a
                // shadow (E0059).
                self.check_shadow(name, *name_span, env, crate::ShadowScopes::All);
                bind(env, name, elem)
            }
            // `for (a, b, …) in …` destructures each iterated **tuple** element positionally
            // (object-model slice 4b — `.enumerate()` yields `(int, T)` tuples). Each name binds to
            // its element type when the element is a known tuple, else `dyn`.
            ForPattern::Tuple { names, .. } => {
                for (i, (name, name_span)) in names.iter().enumerate() {
                    self.check_reserved_name(name, *name_span);
                    self.check_shadow(name, *name_span, env, crate::ShadowScopes::All);
                    let t = match &elem {
                        Type::Tuple(els) => els.get(i).cloned().unwrap_or(Type::Unknown),
                        _ => Type::Unknown,
                    };
                    bind(env, name, t);
                }
            }
        }
    }

    pub(crate) fn bind_pattern(&mut self, pattern: &Pattern, ty: &Type, env: &mut Env) {
        match pattern {
            Pattern::Wildcard { .. }
            | Pattern::Int { .. }
            | Pattern::Str { .. }
            | Pattern::Bool { .. }
            // `is T` binds no name here — `synth_match` narrows the scrutinee identifier instead.
            | Pattern::IsType { .. } => {}
            Pattern::Binding { name, span } => {
                // **The resolution.** A bare identifier naming a payload-free variant of the value's
                // own enum IS that variant: it binds nothing, it is refutable, and lowering rewrites
                // this span into a `Pattern::Variant` so both backends see an ordinary case test.
                // Recorded here rather than in `match_type` because this is the recursive walk —
                // a nested pattern's scrutinee is the *field's* type (`Ok(none)`, a tuple element,
                // a variant payload), and each resolves against the type it is actually matched on.
                if let Some(resolved) = self.payload_free_variant(ty, name) {
                    self.sites.variant_pattern_sites.insert(*span, resolved);
                    return;
                }
                // A bare `none` the scrutinee did not resolve (a `dyn`/gradual value) still reads as
                // the Option-none case, so it stays exempt from the reserved-name rule. It is an
                // irrefutable binding underneath, which is why an arm *after* it is E0066.
                if name != NONE_PATTERN {
                    self.check_reserved_name(name, *span);
                    // A match-pattern binding lands in the arm's just-pushed frame — any env hit
                    // is a shadow (E0059).
                    self.check_shadow(name, *span, env, crate::ShadowScopes::All);
                }
                bind(env, name, ty.clone())
            }
            Pattern::Variant {
                variant, bindings, ..
            } => {
                let payloads = self.payload_types(ty, variant, bindings.len());
                for (sub, pty) in bindings.iter().zip(payloads) {
                    self.bind_pattern(sub, &pty, env);
                }
            }
            // A tuple pattern `(p, q, …)` binds each sub-pattern against the corresponding tuple
            // element type (object-model slice 4b); a non-tuple/gradual scrutinee binds `dyn`.
            Pattern::Tuple { elements, .. } => {
                for (i, sub) in elements.iter().enumerate() {
                    let pty = match ty {
                        Type::Tuple(els) => els.get(i).cloned().unwrap_or(Type::Unknown),
                        _ => Type::Unknown,
                    };
                    self.bind_pattern(sub, &pty, env);
                }
            }
        }
    }

    /// The data-field types a variant pattern binds, given the scrutinee type. Falls back to
    /// `Unknown` per position when the type is gradual or the variant is unknown.
    pub(crate) fn payload_types(&self, ty: &Type, variant: &str, arity: usize) -> Vec<Type> {
        let known = match ty {
            Type::Result(ok, err) => match variant {
                "Ok" => vec![(**ok).clone()],
                "Err" => vec![(**err).clone()],
                _ => Vec::new(),
            },
            Type::Option(some) => match variant {
                "some" => vec![(**some).clone()],
                _ => Vec::new(),
            },
            // Substitute the enum's type arguments into the variant's declared payload types, so a
            // pattern on a generic enum binds the *instantiated* payload: `match t { Tree.Leaf(n) => … }`
            // where `t: Tree<int>` types `n` as `int`, not the abstract parameter `T`. Mirrors the
            // construction-side inference (R2b.1); the two are the same generic type-argument flow.
            Type::Named(n, args) => self
                .symbols
                .enums
                .get(n)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| {
                    let subst = self.type_arg_subst(n, args);
                    v.fields.iter().map(|t| apply_subst(t, &subst)).collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if known.len() == arity {
            known
        } else {
            vec![Type::Unknown; arity]
        }
    }

    pub(crate) fn is_enum_variant(&self, type_name: &str, variant: &str) -> bool {
        self.symbols
            .enums
            .get(type_name)
            .is_some_and(|vs| vs.iter().any(|v| v.name == variant))
    }

    /// How many positional payload values `variant` carries, or `None` when the enum has no such
    /// variant. `Some(0)` is the payload-free case — the distinction [`Self::is_enum_variant`]
    /// deliberately does not make, and the one that decides whether `Type.Variant` in **value**
    /// position is a value at all.
    pub(crate) fn enum_variant_fields(&self, type_name: &str, variant: &str) -> Option<usize> {
        self.symbols
            .enums
            .get(type_name)?
            .iter()
            .find(|v| v.name == variant)
            .map(|v| v.fields.len())
    }

    /// Resolve a source-written enum type name to the key it occupies in `symbols.enums` — the name
    /// itself for a user/prelude enum, or, for a **native** enum imported `use pkg.TheEnum`, the
    /// qualified identity its local short name aliases to. A native enum is seeded under its
    /// qualified name alone (S1's two-identity model), so source-level construction
    /// (`TheEnum.Variant`, native-extensibility S1b) follows the import alias — the same
    /// `extern_types` channel a native fn's *return* type resolves through — to find it and yield a
    /// construction result keyed by that qualified identity (so it unifies with a native signature).
    /// A direct hit wins, so a user enum of the same short name shadows the import. `None` when the
    /// name is not an enum in either form.
    pub(crate) fn enum_type_key(&self, type_name: &str) -> Option<String> {
        if self.symbols.enums.contains_key(type_name) {
            return Some(type_name.to_string());
        }
        self.imports
            .extern_types
            .get(type_name)
            .filter(|q| self.symbols.enums.contains_key(q.as_str()))
            .cloned()
    }

    /// The argument type of `Enum.from` / `Enum.try_from` — what a wire→case conversion accepts.
    ///
    /// A **backed** enum accepts its backing values, because a backing is the wire value its JSON
    /// Schema advertises and the value a real document carries. `string` stays accepted alongside,
    /// so a plain enum is unchanged and every program that already spelled the case name keeps
    /// working. Derived from the *folded backings* rather than from a separately tracked backing
    /// annotation, so the type the checker accepts and the values the backends match cannot drift.
    pub(crate) fn enum_probe_type(&self, type_name: &str) -> Type {
        let Some(variants) = self.symbols.enums.get(type_name) else {
            return Type::String;
        };
        let mut members: Vec<Type> = vec![Type::String];
        for v in variants {
            match &v.backing {
                Some(noeta_ast::AttrValue::Int(_)) => members.push(Type::Int),
                Some(noeta_ast::AttrValue::Float(_)) => members.push(Type::Float),
                Some(noeta_ast::AttrValue::Bool(_)) => members.push(Type::Bool),
                // A string backing adds nothing — `string` is already accepted.
                _ => {}
            }
        }
        Type::union(members)
    }
}
