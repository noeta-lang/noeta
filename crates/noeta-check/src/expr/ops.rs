//! **Operator typing**: binary/unary synthesis with the width rules (Tier W) and numeric
//! adaptation, operator-trait satisfaction, the operator-error reporter, and pipeline (`|>`)
//! synthesis. All `Checker` methods moved verbatim out of the crate root.

use crate::*;

impl Checker {
    pub(crate) fn synth_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        env: &mut Env,
    ) -> Type {
        let lt = self.synth(lhs, env);
        let rt = self.synth(rhs, env);
        match op {
            // `~` concatenates two lists (their element types unified, `dyn` on a concrete clash)
            // or display-concatenates any other operands to a string.
            BinaryOp::Concat => {
                if let (Type::List(a), Type::List(b)) = (&lt, &rt) {
                    Type::List(Box::new(unify_element(a, b).unwrap_or(Type::Dyn)))
                } else {
                    Type::String
                }
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                // Fixed-width integers (Tier W): `+ - * / %` on two same-width `IntN` yield that
                // width — `+ - *` mask the result (W2, sign-agnostic), `/ %` use the width-carrying
                // sign-aware op (W3). Mixed-width or `IntN` mixed with `int`/`float` needs an explicit
                // conversion (no implicit widening) → E0044. Intercept before the generic numeric
                // path, whose widening lattice does not model `IntN`.
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    return self.synth_intn_arith(op, &lt, &rt, span);
                }
                // Strict fixed-width floats (P-NUM-SYM): `f32`/`f64` arithmetic is same-type-only,
                // exactly like `IntN` — no implicit widening with `int`/`float` or between each other.
                if matches!(lt, Type::F32 | Type::F64) || matches!(rt, Type::F32 | Type::F64) {
                    return self.synth_fixed_float_arith(op, &lt, &rt, span);
                }
                // Arithmetic is trait-backed: `+`→`Add`, … (`%` has no trait — numerics only). An
                // operand must satisfy that trait — a built-in numeric, a user type that `impl`s it,
                // or a type parameter bounded by it; a `dyn`/hole defers. Otherwise it is rejected,
                // statically catching what the runtime would (`cannot apply` / a missing bound).
                let trait_name = required_operator_trait(op);
                let acceptable = |this: &Self, t: &Type| match trait_name {
                    Some(n) => this.operand_satisfies_operator(t, n),
                    None => t.is_numeric() || t.defers_to_runtime(),
                };
                if !acceptable(self, &lt) || !acceptable(self, &rt) {
                    self.report_operator_error(op, &lt, &rt, trait_name, span);
                    Type::Unknown
                } else if let (Some(lr), Some(rr)) = (lt.numeric_rank(), rt.numeric_rank()) {
                    // Numeric widening lattice `int < f32 < float`: the result is the higher-ranked
                    // operand (`f32 + int → f32`, `f32 + float → float`), the production widening rule.
                    if lr >= rr { lt } else { rt }
                } else {
                    Type::Unknown
                }
            }
            // Ordering comparisons require `Comparable`: a built-in scalar, a user type that derives
            // or `impl`s it, or a type parameter bounded by it. A concrete type that does not is
            // `E0007` (the runtime's "cannot compare"); an unbounded type parameter is `E0025`.
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                // Fixed-width ordering (Tier W3) is sign-dependent (unsigned `u64` ordering differs
                // from signed past bit 63), so it consults the operand width the way W2's arithmetic
                // does — same-width `IntN` only; mixed → E0044. Intercept before the generic
                // `Comparable` path (which the width-carrying `WideInt` op then implements).
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    self.synth_intn_compare(op, &lt, &rt, span);
                    return Type::Bool;
                }
                if matches!(lt, Type::F32 | Type::F64) || matches!(rt, Type::F32 | Type::F64) {
                    self.synth_fixed_float_compare(op, &lt, &rt, span);
                    return Type::Bool;
                }
                if !self.operand_satisfies_operator(&lt, BuiltinTrait::Comparable)
                    || !self.operand_satisfies_operator(&rt, BuiltinTrait::Comparable)
                {
                    self.report_operator_error(op, &lt, &rt, Some(BuiltinTrait::Comparable), span);
                }
                Type::Bool
            }
            // `==`/`!=` are universal (structural equality fallback) and the logical operators take
            // bools; none impose a trait bound, so none is checked here.
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::And | BinaryOp::Or => Type::Bool,
            // `===`/`!==` ask reference identity (*same instance*), meaningful only for the
            // reference kind `class`. A definitely-value operand (scalar, collection, struct/enum,
            // tuple, fn) has no identity → E0034; a `dyn`/hole or class (or a union of them) defers.
            BinaryOp::Identity | BinaryOp::NotIdentity => {
                if !self.is_reference_comparable(&lt) || !self.is_reference_comparable(&rt) {
                    self.error(
                        DiagnosticCode::InvalidIdentityCompare,
                        span,
                        format!(
                            "`{}` compares reference identity, which only a `class` has; \
                             `{lt}` and `{rt}` are value types — compare them with `==`",
                            op.symbol(),
                        ),
                    );
                }
                Type::Bool
            }
            // Symmetric bitwise `& | ^` (P-BITS Tier B on `int`; W5 on fixed-width). Two same-width
            // `IntN` yield that width — the erased op is already correctly extended, so no mask.
            // Mixed-width or `IntN`+`int` → E0044. Otherwise both operands must be `int` → `int`
            // (a `dyn`/hole defers); anything else is E0043 (`bool` uses `&&`/`||`).
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    return self.synth_intn_bitwise(op, &lt, &rt, span);
                }
                let ok = |t: &Type| matches!(t, Type::Int) || t.defers_to_runtime();
                if !ok(&lt) || !ok(&rt) {
                    self.report_noninteger_bitwise(op, &lt, &rt, span);
                }
                Type::Int
            }
            // Shifts `<< >>` are asymmetric: the left operand is the value (it sets the result type),
            // the right is a count (any integer — its width is irrelevant). On a fixed-width value
            // (W5) `<<` masks the result into the width (sign-agnostic, like `+ - *`), and `>>` is
            // sign-dependent — **arithmetic** (sign-fill) on a signed width, **logical** (zero-fill)
            // on an unsigned one — so it lowers to the width-carrying `WideInt`.
            BinaryOp::Shl | BinaryOp::Shr => {
                let amount_ok =
                    |t: &Type| matches!(t, Type::Int | Type::IntN { .. }) || t.defers_to_runtime();
                if let Type::IntN { signed, bits } = lt {
                    if !amount_ok(&rt) {
                        self.error(
                            DiagnosticCode::NonIntegerBitwise,
                            span,
                            format!(
                                "`{}` shift amount must be an integer, found `{rt}`",
                                op.symbol()
                            ),
                        );
                    }
                    // Both `<<` (via `MaskWidth`) and `>>` (via `WideInt`) read the width from here;
                    // lowering routes by the operator.
                    self.sites.width_sites.insert(span, (signed, bits));
                    return Type::IntN { signed, bits };
                }
                let ok = |t: &Type| matches!(t, Type::Int) || t.defers_to_runtime();
                if !ok(&lt) || !amount_ok(&rt) {
                    self.report_noninteger_bitwise(op, &lt, &rt, span);
                }
                Type::Int
            }
        }
    }

    /// Whether `ty` may be a **reference (`class`) instance**, so `===`/`!==` is meaningful on it.
    /// True for a `dyn`/inference hole (may hold a class at runtime), the `Class` kind-type, a
    /// concrete `class` (or an as-yet-unresolved named type, deferring to its own diagnostic), and a
    /// union all of whose members qualify. False for every definitely-value type (scalars,
    /// collections, `struct`/`enum`, functions) — those drive E0034.
    pub(crate) fn is_reference_comparable(&self, ty: &Type) -> bool {
        match ty {
            Type::Unknown | Type::Dyn => true,
            Type::Kind(noeta_types::TypeKind::Class) => true,
            Type::Named(n, _) => matches!(
                self.symbols.type_kinds.get(n),
                Some(noeta_types::TypeKind::Class) | None
            ),
            Type::Union(members) => members.iter().all(|m| self.is_reference_comparable(m)),
            _ => false,
        }
    }

    /// Whether `operand` may be used with an operator requiring `trait_name`: a `dyn`/hole defers;
    /// an in-scope **type parameter** is licensed only by its declared bounds; any other type by the
    /// satisfaction model ([`Self::satisfies`] — built-in table + `@derive`/`impl` index).
    pub(crate) fn operand_satisfies_operator(&self, operand: &Type, t: BuiltinTrait) -> bool {
        if operand.defers_to_runtime() {
            return true;
        }
        if let Type::Named(n, _) = operand
            && let Some(bounds) = self.coloring.type_params.get(n)
        {
            return bounds.iter().any(|b| b == t.name());
        }
        self.satisfies(operand, t)
    }

    /// The name of an in-scope type parameter (`operand`) that lacks `trait_name` among its bounds,
    /// or `None` if `operand` is not such a parameter — used to pick the diagnostic flavor.
    pub(crate) fn unbounded_type_param(&self, operand: &Type, t: BuiltinTrait) -> Option<String> {
        match operand {
            Type::Named(n, _) => match self.coloring.type_params.get(n) {
                Some(bounds) if !bounds.iter().any(|b| b == t.name()) => Some(n.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Report a trait-backed operator applied to an unsupported operand: an unbounded type parameter
    /// is `E0025` (a missing bound, fixable at the declaration); any other concrete mismatch is
    /// `E0007` (the same "cannot apply" the runtime raised). Reported once for the operator.
    pub(crate) fn report_operator_error(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        trait_name: Option<BuiltinTrait>,
        span: Span,
    ) {
        if let Some(tn) = trait_name
            && let Some(n) = self
                .unbounded_type_param(lt, tn)
                .or_else(|| self.unbounded_type_param(rt, tn))
        {
            self.error(
                DiagnosticCode::TraitBoundNotSatisfied,
                span,
                format!(
                    "operator `{}` requires `{n}: {}`, but `{n}` is an unbounded type \
                         parameter",
                    op.symbol(),
                    tn.name()
                ),
            )
            .help(format!("add the bound, e.g. `<{n}: {}>`", tn.name()));
        } else {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("cannot apply `{}` to `{lt}` and `{rt}`", op.symbol()),
            );
        }
    }

    /// Synthesize a pipeline right-hand side `left |> right`, where `piped` is the type of `left`,
    /// threaded as `right`'s first argument. `right` may be a call (`add(10)` → `add(left, 10)`)
    /// or a bare callee (`inc` → `inc(left)`).
    pub(crate) fn synth_piped(&mut self, right: &Expr, piped: Type, env: &mut Env) -> Type {
        match right {
            Expr::Call { callee, args, .. } => {
                let mut arg_types = vec![piped];
                arg_types.extend(args.iter().map(|a| self.synth(a, env)));
                self.synth_call(callee, &arg_types, &[], right.span(), env)
            }
            Expr::Ident { .. } | Expr::Member { .. } => {
                self.synth_call(right, &[piped], &[], right.span(), env)
            }
            other => {
                self.synth(other, env);
                Type::Unknown
            }
        }
    }
}
