//! **Match & pattern typing**: match-arm synthesis, exhaustiveness (E0011), `for`/`match`
//! pattern binding, and enum-variant payload resolution. All `Checker` methods moved verbatim
//! out of the crate root.

use crate::*;

/// The Option-`none` pattern spelling. Parsed as a [`Pattern::Binding`] like any other bare
/// identifier — which is exactly why it needs naming here.
const NONE_PATTERN: &str = "none";

/// The E0066 help for the one prelude pattern that is irrefutable *and* reads as a case test.
const NONE_ARM_HELP: &str = "`none` in pattern position is a bare binding — it matches every \
                             value, so it has to come last; put the `some(…)` arm first";

impl Checker {
    /// Type a `match`. `value_used` is `true` when the match stands in value position (its result
    /// is consumed — a binding RHS, an argument, an operand, a `return`), and `false` only when it
    /// is the whole of an expression statement (its value discarded). Block-bodied arms (aether F1)
    /// produce no value — blocks are statement sequences in Noeta — so in value position they are a
    /// hard error (E0059) rather than silently contributing `unit`; in statement position they are
    /// the intended side-effect form.
    pub(crate) fn synth_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        env: &mut Env,
        value_used: bool,
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
        // The arm sequence's own soundness (E0066/E0067), threaded through the loop so it reports in
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
                // …and, when the target does resolve, the same always-false rule the expression
                // form applies (E0065): an `is P` arm on an `Option<P>` scrutinee can never be
                // taken, so it is reported and — below — narrows nothing.
                if self.diags.len() == before
                    && let Some(idiom) = self.impossible_type_test(&scrut, ty)
                {
                    let target = from_ref_q(ty, &self.imports.extern_types);
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
                bind(env, name, from_ref_q(ty, &self.imports.extern_types));
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
                noeta_ast::ClosureBody::Expr(e) => self.synth(e, env),
                // A statement-block arm (aether F1): check its statements in the arm scope; the
                // arm's value is `unit`. In value position that is a silent value loss — blocks
                // never produce values — so reject it (E0059).
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

    /// One arm's **reachability** (E0066) and **variant-shadowing** (E0067) check, folded over the
    /// arm list: `catch_all` carries the first unguarded irrefutable arm seen so far (its rendered
    /// pattern text and span), so every later arm is provably dead.
    ///
    /// Irrefutability here is deliberately syntactic — a `_` wildcard or a bare-identifier
    /// [`Pattern::Binding`], unguarded. Both compile to *no test at all* in either backend (the
    /// binding form merely names the scrutinee), so nothing downstream can rescue a later arm. A
    /// guard makes an arm refutable (the checker cannot prove a guard ever true), and every other
    /// pattern form emits a test.
    ///
    /// The two diagnostics are exclusive, most-specific-first: an already-dead arm is E0066 (with
    /// the qualified spelling folded into its help when the dead pattern is *also* a shadowed
    /// variant), and only a still-live arm is E0067. So each faulty arm produces exactly one
    /// diagnostic naming the one thing to fix.
    fn check_arm_reachability(
        &mut self,
        arm: &MatchArm,
        scrut: &Type,
        catch_all: &mut Option<(String, Span)>,
    ) {
        // The variant a bare-identifier pattern silently shadows, if any — the fact that turns
        // "unreachable arm" from a puzzle into an instruction.
        let shadowed = match &arm.pattern {
            Pattern::Binding { name, .. } => self
                .shadowed_payload_free_variant(scrut, name)
                .map(|qualified| (name.clone(), qualified)),
            _ => None,
        };
        if let Some((catch_text, catch_span)) = catch_all.clone() {
            let help = match &shadowed {
                Some((name, qualified)) => format!(
                    "`{name}` here is a binding, not the variant `{qualified}` — write it \
                     qualified as `{qualified}` so it matches only that case"
                ),
                // A bare `none` reads as the Option-none pattern but is a binding like any other,
                // so it swallows the arms after it. Worth saying outright: the fix is ordering.
                None if catch_text == NONE_PATTERN => NONE_ARM_HELP.to_string(),
                None => format!(
                    "delete this arm, or move the catch-all `{catch_text}` arm to last position"
                ),
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
        } else if let Some((name, qualified)) = &shadowed {
            self.error(
                DiagnosticCode::VariantShadowedByBinding,
                arm.pattern.span(),
                format!(
                    "`{name}` here binds the whole value instead of matching the variant \
                     `{qualified}`: a bare identifier pattern is always a binding, so this arm runs \
                     for every case"
                ),
            )
            .help(format!(
                "write the variant qualified as `{qualified}`; a payload-carrying variant is \
                 call-shaped (`Variant(x)`) and needs no qualification, a payload-free one does"
            ));
        }
        // Only an *unguarded* wildcard/binding closes the match — a guard leaves the case open.
        if catch_all.is_none() && arm.guard.is_none() {
            let text = match &arm.pattern {
                Pattern::Wildcard { .. } => Some("_".to_string()),
                Pattern::Binding { name, .. } => Some(name.clone()),
                _ => None,
            };
            if let Some(text) = text {
                *catch_all = Some((text, arm.pattern.span()));
            }
        }
    }

    /// The qualified spelling of the **payload-free** enum variant a bare-identifier pattern
    /// shadows, given the scrutinee's type — `Some("Type.String")` for `String` matched against a
    /// `Type` that declares `String;`.
    ///
    /// Only a *declared* enum qualifies ([`Type::Named`]): a payload-free variant is the one form
    /// whose pattern spelling is indistinguishable from a binding, and it is the one form the
    /// language requires to be written qualified. `Option`/`Result` are deliberately excluded —
    /// `none` is the *correct* bare spelling of its case, so naming it here would be advice to write
    /// something that does not exist.
    fn shadowed_payload_free_variant(&self, scrut: &Type, name: &str) -> Option<String> {
        let Type::Named(type_name, _) = scrut else {
            return None;
        };
        let key = self.enum_type_key(type_name)?;
        self.symbols
            .enums
            .get(&key)?
            .iter()
            .any(|v| v.name == name && v.fields.is_empty())
            .then(|| format!("{type_name}.{name}"))
    }

    /// Promote a non-exhaustive `match` to a compile error (`E0011`), but only when the
    /// scrutinee's type is a concretely-known enum / `Result` / `Option`. Anything else (an
    /// `int`/`string`/`bool` scrutinee, or a gradual type) has an open or unknown domain and is
    /// left to the runtime backstop — keeping the check free of false positives.
    ///
    /// A **guarded** arm (`pattern if cond`) contributes nothing to coverage: the checker cannot
    /// prove a guard ever true, so its case stays uncovered for when the guard is false. Only
    /// unguarded arms count below.
    pub(crate) fn check_exhaustive(&mut self, scrut: &Type, arms: &[MatchArm], span: Span) {
        // A wildcard or bare binding arm catches everything — unless it is guarded.
        if arms.iter().any(|a| {
            a.guard.is_none()
                && matches!(
                    a.pattern,
                    Pattern::Wildcard { .. } | Pattern::Binding { .. }
                )
        }) {
            return;
        }
        let guarded = arms.iter().any(|a| a.guard.is_some());
        let guard_help = |help: &str| {
            if guarded {
                format!(
                    "{help}; a guarded arm (`pattern if cond`) does not count — its case stays \
                     uncovered when the guard is false"
                )
            } else {
                help.to_string()
            }
        };
        // A type-pattern match (`is T` arms): the domain is *types*, not variant names. A union is
        // a closed domain — exhaustive iff every member is covered by some `is` arm; `dyn` is the
        // open top — a finite set of `is` arms can never exhaust it, so it needs a `_`.
        let type_targets: Vec<Type> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::IsType { ty, .. } if a.guard.is_none() => {
                    Some(from_ref_q(ty, &self.imports.extern_types))
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
            let missing: Vec<String> = match scrut {
                Type::Union(members) => members
                    .iter()
                    .filter(|m| !type_targets.iter().any(|t| Type::subtype(m, t)))
                    .map(|m| m.to_string())
                    .collect(),
                Type::Dyn => vec!["a `dyn` value (open type domain)".into()],
                // A concrete or gradual scrutinee with `is` arms is not exhaustiveness-checked.
                _ => return,
            };
            if !missing.is_empty() {
                self.error(
                    DiagnosticCode::NonExhaustiveMatch,
                    span,
                    format!("non-exhaustive `match`: missing {}", missing.join(", ")),
                )
                .help(guard_help(
                    "add an `is T` arm for each missing type, or a `_` catch-all",
                ));
            }
            return;
        }
        let all: Vec<String> = match scrut {
            Type::Result(..) => vec!["Ok".into(), "Err".into()],
            Type::Option(..) => vec!["some".into(), "none".into()],
            Type::Named(n, _) => match self.symbols.enums.get(n) {
                Some(variants) => variants.iter().map(|v| v.name.clone()).collect(),
                None => return,
            },
            _ => return,
        };
        let covered: HashSet<&str> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::Variant { variant, .. } if a.guard.is_none() => Some(variant.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = all
            .into_iter()
            .filter(|v| !covered.contains(v.as_str()))
            .collect();
        if !missing.is_empty() {
            self.error(
                DiagnosticCode::NonExhaustiveMatch,
                span,
                format!("non-exhaustive `match`: missing {}", missing.join(", ")),
            )
            .help(guard_help(
                "add an arm for each missing case, or a `_` catch-all",
            ));
        }
    }

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
                // A bare `none` in pattern position reads as the Option-none case rather than a
                // fresh binding — exempt it from the reserved-name rule so
                // `match o { some(v) => …, none => … }` stays legal. It is still an irrefutable
                // binding underneath, which is why an arm *after* it is E0066.
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
}
