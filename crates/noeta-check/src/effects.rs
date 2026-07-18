//! **Effect coloring**: the isolate `Send` boundary (E0042), `.await` position rules (E0040 /
//! AsyncMisuse), and the yield/await body classifiers that color a function as
//! generator/async. Reads only the coloring state on [`Checker`]. All items moved verbatim out
//! of the crate root purely to shrink `lib.rs`.

use super::*;

impl Checker {
    /// Check an `isolate f(args)` boundary (isolates milestone, E0042): the call's result crosses back
    /// and its arguments cross into a fresh heap, so both must be `Send`. `isolate` also requires a
    /// **direct call** so it knows what to ship. `result` is the already-synthesized `Future<T>`.
    pub(crate) fn check_isolate_send(&mut self, future: &Expr, result: &Type, span: Span) {
        let Expr::Call { callee, .. } = future else {
            self.error(
                DiagnosticCode::NotSend,
                span,
                "`isolate` expects a direct call, e.g. `isolate work(x)`".to_string(),
            )
            .help(
                "the argument to `isolate` must be a function call so the arguments and \
                            the function to run can be shipped to the fresh isolate",
            );
            return;
        };
        // The result `T` (from `Future<T>`) crosses back to this isolate.
        if let Type::Named(n, targs) = result
            && n == stdlib::FUTURE
        {
            let t = targs.first().cloned().unwrap_or(Type::Unknown);
            if !self.is_send(&t, &mut Vec::new()) {
                self.error(
                    DiagnosticCode::NotSend,
                    span,
                    format!("an isolate's result type `{t}` is not `Send`"),
                )
                .help(
                    "only value types cross an isolate boundary; a `class` (reference type) has \
                         identity and cannot — return a `struct` instead",
                );
            }
        }
        // The arguments cross into the fresh isolate — check the called function's declared parameter
        // types (a direct-call callee), so a `class` argument is rejected without re-synthesizing args.
        if let Expr::Ident { name, .. } = callee.as_ref()
            && let Some(sig) = self.symbols.functions.get(name)
        {
            for param in sig.params.clone() {
                if !self.is_send(&param, &mut Vec::new()) {
                    self.error(
                        DiagnosticCode::NotSend,
                        span,
                        format!("an isolate argument of type `{param}` is not `Send`"),
                    )
                    .help(
                        "only value types cross an isolate boundary; a `class` (reference type) \
                             has identity and cannot — pass a `struct` instead",
                    );
                }
            }
        }
    }

    /// Whether a value of type `ty` may cross an isolate boundary (isolates milestone). Value types are
    /// `Send` (copied, or borrow-shared under the scope lifetime); reference `class`es and the stateful
    /// built-ins (`Future`/`Iterator`/`FileHandle`/closures) are `!Send`. Structural — a container /
    /// `struct` / `enum` is `Send` iff its elements / fields / payloads are — with a `visited` set so a
    /// recursive value type terminates. `dyn` is conservatively `!Send` (can't prove it isn't a class);
    /// an inference hole (`Unknown`) is permissive (it will resolve; blocking it would be spurious).
    /// The substitution mapping a declared type's generic parameters to the type arguments a use site
    /// supplied (`Box<int>` → `{T: int}`) — used to instantiate field/payload types before the `Send`
    /// check. Empty for a non-generic type or when no arguments are given.
    pub(crate) fn type_arg_subst(&self, name: &str, args: &[Type]) -> HashMap<String, Type> {
        self.symbols
            .generic_types
            .get(name)
            .map(|params| params.iter().cloned().zip(args.iter().cloned()).collect())
            .unwrap_or_default()
    }

    pub(crate) fn is_send(&self, ty: &Type, visited: &mut Vec<String>) -> bool {
        match ty {
            Type::Int
            | Type::Float
            | Type::F32
            | Type::Bool
            | Type::String
            | Type::Bytes
            | Type::Unit
            | Type::Unknown => true,
            Type::List(e) | Type::Set(e) | Type::Option(e) => self.is_send(e, visited),
            Type::Map(k, v) | Type::Result(k, v) => {
                self.is_send(k, visited) && self.is_send(v, visited)
            }
            Type::Tuple(elems) | Type::Union(elems) => {
                elems.iter().all(|e| self.is_send(e, visited))
            }
            Type::Named(name, args) => match self.symbols.type_kinds.get(name) {
                Some(noeta_types::TypeKind::Class) => false,
                Some(noeta_types::TypeKind::Struct) => {
                    if visited.iter().any(|v| v == name) {
                        return true; // recursive struct — its fields are covered by the outer frame
                    }
                    visited.push(name.clone());
                    // Substitute the type arguments into the field types before checking, so a generic
                    // value type is `Send` iff its *instantiated* fields are (`Box<int>` → `Send`,
                    // `Box<Conn>` → `!Send`). Without this a generic struct's field `T` (`Named("T")`)
                    // classified `!Send` unconditionally, making every generic struct `!Send`.
                    let subst = self.type_arg_subst(name, args);
                    let fields_send = self.symbols.records.get(name).is_none_or(|fs| {
                        fs.iter()
                            .all(|(_, t)| self.is_send(&apply_subst(t, &subst), visited))
                    });
                    visited.pop();
                    fields_send
                }
                Some(noeta_types::TypeKind::Enum) => {
                    if visited.iter().any(|v| v == name) {
                        return true;
                    }
                    visited.push(name.clone());
                    // Substitute the type arguments into the payload types (as for a struct's fields).
                    let subst = self.type_arg_subst(name, args);
                    let payloads_send = self.symbols.enums.get(name).is_none_or(|vs| {
                        vs.iter().all(|v| {
                            v.fields
                                .iter()
                                .all(|t| self.is_send(&apply_subst(t, &subst), visited))
                        })
                    });
                    visited.pop();
                    payloads_send
                }
                // A built-in `Named` type: the payload-free prelude `Ordering` enum is `Send`. A
                // channel endpoint (`Sender<T>`/`Receiver<T>`, isolates I.1) is a scheduler-owned id,
                // `Send` iff its message type is — so a receiver of `Send` values can be shipped into
                // an isolate. Other stateful/reference-like built-ins (`Future`/`Iterator`/
                // `FileHandle`/…) are `!Send`.
                None if name == stdlib::SENDER || name == stdlib::RECEIVER => {
                    args.first().is_none_or(|t| self.is_send(t, visited))
                }
                None => name == "Ordering",
            },
            // Closures capture the heap; `dyn` can't be proven non-`class`; anything else is `!Send`.
            _ => false,
        }
    }

    /// Track A.3a: an `.await` inside an `async fn` is compiled into a poll-state of the state machine
    /// only when it is in **statement position** — the whole value of a binding / expression-statement /
    /// `return` / `echo`, optionally under one `?`. An `.await` buried in a sub-expression (a call
    /// argument, an operand, a condition, a `match` arm, …) is not yet supported (E0040): flag it rather
    /// than let it compile to a drive-to-completion, which would not yield to a sibling under
    /// concurrency (A.3b). Recurses into control-flow bodies; a closure resets async coloring, so its
    /// `.await`s are already rejected by the ordinary E0040 rule.
    pub(crate) fn check_await_positions(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::Binding { value, .. }
                | Stmt::Expr { expr: value, .. }
                | Stmt::Echo { value, .. } => self.check_value_await(value),
                Stmt::Return {
                    value: Some(value), ..
                } => self.check_value_await(value),
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                    ..
                } => {
                    self.reject_nested_await(cond);
                    self.check_await_positions(then_body);
                    if let Some(body) = else_body {
                        self.check_await_positions(body);
                    }
                }
                Stmt::While { cond, body, .. } => {
                    self.reject_nested_await(cond);
                    self.check_await_positions(body);
                }
                Stmt::For { iterable, body, .. } => {
                    self.reject_nested_await(iterable);
                    self.check_await_positions(body);
                }
                // A destructuring binding hosts mid-expression awaits the same way (A.6): the whole
                // value is hoisted, then the destructure runs on the ready result.
                Stmt::Destructure { value, .. } => self.check_value_await(value),
                // Awaiting into a `yield` is not supported (a fn is either async or a generator).
                Stmt::Yield { value, .. } => self.reject_nested_await(value),
                _ => {}
            }
        }
    }

    /// Check the value of a statement for a disallowed `.await` (Track A.6). A mid-expression `.await`
    /// in an **unconditionally-evaluated** position (a call argument, an operand, a list/map element,
    /// an index, a member receiver, …) is fine — the IR lowering hoists it to a preceding
    /// statement-position await, left-to-right. Only an `.await` in a **conditionally-evaluated**
    /// position — the right operand of `&&`/`||`, the fallback of `??`, or a `match` / `if…then…else`
    /// arm body — is still rejected (E0040), because hoisting it out would change short-circuit
    /// semantics (A.6b).
    pub(crate) fn check_value_await(&mut self, value: &Expr) {
        if let Some(span) = conditional_await_span(value) {
            self.error(
                DiagnosticCode::AsyncMisuse,
                span,
                "`.await` in a conditionally-evaluated position is not yet supported".to_string(),
            )
            .help(
                "an `.await` in the right side of `&&`/`||`/`??` or a `match`/`if…then…else` \
                     branch would change short-circuit evaluation — bind it to a variable first, \
                     e.g. `x = f().await`, then use `x`",
            );
        }
    }

    /// Flag `expr` (E0040) if it contains any `.await` at this callable level — used where no await is
    /// permitted at all (an `if`/`while` condition or a `for` iterable, which A.6 does not hoist).
    /// [`Expr::has_await`] already stops at closure boundaries.
    pub(crate) fn reject_nested_await(&mut self, expr: &Expr) {
        if expr.has_await() {
            self.error(
                DiagnosticCode::AsyncMisuse,
                expr.span(),
                "`.await` is not supported in a condition or loop head".to_string(),
            )
            .help("bind the awaited value to a variable first, e.g. `x = f().await`, then use `x`");
        }
    }
}

/// Whether a statement sequence contains a `yield` (Track G), making its enclosing function a
/// **generator**. Descends into control-flow bodies (`if`/`for`/`while`) but **not** into nested
/// function declarations or closures — a `yield` there belongs to that inner callable, not this one.
pub(crate) fn body_has_yield(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

/// Whether `stmts` contain a `.await` at **this callable level** (Track A): inspecting each
/// statement's expressions with [`Expr::has_await`] (which stops at closures) and recursing through
/// control flow, but NOT into a nested `fn` declaration (its own callable) or a stripped tier block.
/// Decides whether a function body or the module top level is an async context.
pub(crate) fn block_has_await(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_await)
}

pub(crate) fn stmt_has_await(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Echo { value, .. }
        | Stmt::Binding { value, .. }
        | Stmt::Destructure { value, .. }
        | Stmt::Yield { value, .. }
        | Stmt::Expr { expr: value, .. } => value.has_await(),
        Stmt::Return { value, .. } => value.as_ref().is_some_and(Expr::has_await),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            cond.has_await()
                || block_has_await(then_body)
                || else_body.as_deref().is_some_and(block_has_await)
        }
        Stmt::For { iterable, body, .. } => iterable.has_await() || block_has_await(body),
        Stmt::While { cond, body, .. } => cond.has_await() || block_has_await(body),
        // A `concurrent { }` requires (and thus establishes) an async context at this level, so the
        // top level is async when it contains one — even with no `.await` directly in its body.
        Stmt::Concurrent { .. } => true,
        // A nested `fn` is its own callable; declarations, imports, and stripped tier blocks carry no
        // top-level-level `.await`.
        _ => false,
    }
}

pub(crate) fn stmt_has_yield(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Yield { .. } => true,
        Stmt::If {
            then_body,
            else_body,
            ..
        } => body_has_yield(then_body) || else_body.as_deref().is_some_and(body_has_yield),
        Stmt::For { body, .. } | Stmt::While { body, .. } => body_has_yield(body),
        _ => false,
    }
}

/// The span of a **conditionally-evaluated** sub-expression of `e` that contains an `.await`, or
/// `None` if every `.await` in `e` sits in an unconditionally-evaluated position (Track A.6). The IR
/// lowering can hoist unconditional awaits to statement position left-to-right, but an await guarded by
/// short-circuit evaluation — the right operand of `&&`/`||`, the fallback of `??`, or a
/// `match`/`if…then…else` arm body — cannot be hoisted without changing when it runs, so it is still
/// rejected (E0040). Recurses through the unconditional structure to find one nested deeper; a closure
/// is a separate callable (its awaits are handled by ordinary coloring, not here).
pub(crate) fn conditional_await_span(e: &Expr) -> Option<Span> {
    fn any(es: &[Expr]) -> Option<Span> {
        es.iter().find_map(conditional_await_span)
    }
    // The span of a guarded operand iff it hosts an await at this callable level.
    let guarded = |g: &Expr| g.has_await().then(|| g.span());
    match e {
        // Short-circuit `&&`/`||`: the guarded RHS may hold an await — the state-machine desugar
        // (Track A.6b) rewrites it into control flow so it runs only when the operator evaluates it.
        // Recurse into the RHS so an await still nested in *another* conditional position inside it
        // (a `??` fallback, a `match` arm) is caught; a plain (or nested-short-circuit) RHS await is fine.
        Expr::Binary {
            op: BinaryOp::And | BinaryOp::Or,
            lhs,
            rhs,
            ..
        } => conditional_await_span(lhs).or_else(|| conditional_await_span(rhs)),
        Expr::Coalesce {
            value, fallback, ..
        } => conditional_await_span(value).or_else(|| guarded(fallback)),
        Expr::Match {
            scrutinee, arms, ..
        } => conditional_await_span(scrutinee)
            .or_else(|| arms.iter().find_map(|a| a.body.has_await().then_some(a.span))),
        // A closure is a separate callable — its awaits are not this level's. An expression-tier
        // block's holes desugar to closures, so the same applies.
        Expr::Closure { .. } | Expr::TierExpr { .. } | Expr::NativeFnRef { .. } => None,
        // Unconditional compounds: recurse into every child (evaluation order does not matter here —
        // any conditional await anywhere disqualifies).
        Expr::Await { expr, .. }
        | Expr::Unary { operand: expr, .. }
        | Expr::TupleIndex { receiver: expr, .. }
        | Expr::Member { receiver: expr, .. }
        | Expr::Try { expr, .. }
        | Expr::Spawn { future: expr, .. }
        | Expr::As { expr, .. }
        | Expr::TypeTest { expr, .. }
        | Expr::TypeOf { value: expr, .. }
        | Expr::FieldsOf { value: expr, .. }
        | Expr::ParamsOf { target: expr, .. }
        | Expr::FromBytes { blob: expr, .. } => conditional_await_span(expr),
        Expr::Channel { capacity, .. } => conditional_await_span(capacity),
        Expr::Binary { lhs, rhs, .. }
        | Expr::Pipeline {
            left: lhs,
            right: rhs,
            ..
        }
        | Expr::Index {
            receiver: lhs,
            index: rhs,
            ..
        }
        | Expr::Range {
            start: lhs,
            end: rhs,
            ..
        }
        | Expr::FieldSet {
            receiver: lhs,
            value: rhs,
            ..
        } => conditional_await_span(lhs).or_else(|| conditional_await_span(rhs)),
        Expr::Call { callee, args, .. } => conditional_await_span(callee).or_else(|| any(args)),
        Expr::TypedModuleCall { recv, args, .. } => {
            conditional_await_span(recv).or_else(|| any(args))
        }
        Expr::Invoke {
            recv, name, args, ..
        } => conditional_await_span(recv)
            .or_else(|| conditional_await_span(name))
            .or_else(|| conditional_await_span(args)),
        Expr::List { items, .. } | Expr::Tuple { items, .. } => any(items),
        Expr::Map { entries, .. } => entries
            .iter()
            .find_map(|(k, v)| conditional_await_span(k).or_else(|| conditional_await_span(v))),
        Expr::Interp { parts, .. } => parts.iter().find_map(|part| match part {
            StrPart::Hole(e) => conditional_await_span(e),
            StrPart::Literal(_) => None,
        }),
        Expr::Object(lit) => lit
            .fields
            .iter()
            .find_map(|f| conditional_await_span(&f.value))
            .or_else(|| lit.spread.as_deref().and_then(conditional_await_span)),
        // Leaves — no sub-expressions.
        Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::IntN { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::Bool { .. }
        | Expr::Ident { .. }
        | Expr::AttributesOf { .. }
        | Expr::RolesOf { .. } => None,
    }
}
