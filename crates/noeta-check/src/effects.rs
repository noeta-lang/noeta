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
            && let Some(sig) = self.symbols.functions.get(name.as_str())
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
    pub(crate) fn type_arg_subst(&self, name: &str, args: &[Type]) -> Subst {
        self.symbols
            .generic_types
            .get(name)
            .map(|params| params.iter().map(|p| p.id).zip(args.iter().cloned()).collect())
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

    /// Track A.3a/A.6: an `.await` inside an `async fn` is compiled into a poll-state of the state
    /// machine. It is legal in every **value position** — statement position (a binding /
    /// expression-statement / `return` / `echo` / destructure value, optionally under one `?`), any
    /// unconditionally-evaluated sub-expression (hoisted to a preceding statement-position await by the
    /// IR lowering, A.6), and — since A.6b and its residual — any **conditionally-evaluated** value
    /// position: the right operand of `&&`/`||`, the fallback of `??`, and a `match`/`if…then…else` arm
    /// body, each rewritten into control flow by the state-machine desugar so the guarded await runs
    /// exactly when the surrounding expression would evaluate it. What stays rejected (E0040) is an
    /// `.await` in a **condition or loop head** — an `if`/`while` condition or a `for` iterable, which
    /// the desugar does not hoist — and awaiting into a `yield`. Recurses into control-flow bodies; a
    /// closure resets async coloring, so its `.await`s are rejected by the ordinary coloring rule.
    pub(crate) fn check_await_positions(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
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
                // Awaiting into a `yield` is not supported (a fn is either async or a generator).
                Stmt::Yield { value, .. } => self.reject_nested_await(value),
                _ => {}
            }
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
