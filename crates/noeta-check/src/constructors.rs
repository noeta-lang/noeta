//! **Fresh-constructor pre-pass** (generic constructor reflection): which associated functions of a
//! generic struct/class are *provable constructors* — every `return` hands back a freshly-built
//! literal of the enclosing type, and nothing else.
//!
//! The problem it exists to solve. A value's reflected type tag (`type_of` after a `dyn` launder)
//! is stamped at the object literal, from the type the checker resolved there. Inside
//!
//! ```noe
//! class Repo<T> { fn new(tbl: string): Repo<T> { return Repo { tbl: tbl }; } }
//! ```
//!
//! the literal's type is `Repo<T>` with `T` still a *parameter* — there is no instantiation to
//! record, so `Repo.new("todos")` produced an untagged (or `Repo<dyn>`) object no matter what it
//! was assigned to. The instantiation is known one frame up: `rt: Repo<Todo> = Repo.new("todos")`
//! resolves `T = Todo` at the CALL. So the call site stamps the tag.
//!
//! That is only sound if the object the call returns is **freshly allocated by this very call**. A
//! factory that hands back a cached or borrowed instance (`fn shared(): Repo<T> { return CACHE; }`)
//! would let two differently-instantiated call sites re-tag one object, and the second would
//! silently rewrite the first's answer — precisely the silent-wrong-answer failure the reflected
//! tag exists to avoid. So the property is *proved syntactically before bodies are checked*: a
//! function qualifies only when it has at least one `return` and every one of them returns an
//! object literal of the enclosing type with no spread base. Anything else — a returned local, a
//! delegating call, a `?`-propagation, a generator/async wrapper — simply does not qualify, and its
//! call sites keep today's untagged behavior rather than getting a guess.
//!
//! Scope: **associated** functions (no `self`) of a **generic** type, since only those construct an
//! instance whose arguments the call site can determine. Instance methods reach the same
//! instantiation through the receiver's own tag, which is already there.

use noeta_ast::{Expr, FnDecl, Program, Stmt};
use std::collections::HashSet;

/// The qualified `Type.method` keys of every provable fresh constructor in the program.
pub(crate) type FreshConstructors = HashSet<(String, String)>;

/// Compute the program's fresh-constructor set. Purely syntactic and independent of body checking,
/// so a call site records its tag whether it appears before or after the declaration.
pub(crate) fn compute_fresh_constructors(program: &Program) -> FreshConstructors {
    let mut out = FreshConstructors::new();
    for stmt in &program.stmts {
        let (name, type_params, methods) = match stmt {
            Stmt::Class(d) => (&d.name, &d.type_params, &d.methods),
            Stmt::Struct(d) => (&d.name, &d.type_params, &d.methods),
            _ => continue,
        };
        // Only a generic type has an instantiation to recover; a non-generic nominal already
        // reflects precisely from its shape name alone.
        if type_params.is_empty() {
            continue;
        }
        for method in methods {
            if is_fresh_constructor(name.as_str(), method) {
                out.insert((name.to_string(), method.name.to_string()));
            }
        }
    }
    out
}

/// Whether `method` provably builds a fresh `type_name` on every return path.
fn is_fresh_constructor(type_name: &str, method: &FnDecl) -> bool {
    // An `async fn` returns a `Future`, and a generator an `Iterator` — the value the call site
    // receives is the wrapper, not the object, so there is nothing of the type to tag.
    if method.is_async {
        return false;
    }
    let mut returns = 0usize;
    if !all_returns_fresh(type_name, &method.body, &mut returns) {
        return false;
    }
    // A body with no `return` at all falls off the end (unit) — nothing is constructed.
    returns > 0
}

/// Walk `stmts` for `return`s, counting them and verifying each hands back a bare literal of
/// `type_name`. Nested `fn` declarations and closures are deliberately NOT walked: their `return`s
/// leave *their* frame, not this one. A `yield` anywhere makes the function a generator, whose call
/// yields an iterator rather than the object.
fn all_returns_fresh(type_name: &str, stmts: &[Stmt], returns: &mut usize) -> bool {
    for stmt in stmts {
        let ok = match stmt {
            Stmt::Return { value, .. } => {
                *returns += 1;
                matches!(value, Some(v) if is_fresh_literal(type_name, v))
            }
            Stmt::Yield { .. } => false,
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                all_returns_fresh(type_name, then_body, returns)
                    && else_body
                        .as_ref()
                        .is_none_or(|b| all_returns_fresh(type_name, b, returns))
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => {
                all_returns_fresh(type_name, body, returns)
            }
            Stmt::Concurrent { body, .. } | Stmt::TierBlock { items: body, .. } => {
                all_returns_fresh(type_name, body, returns)
            }
            // Everything else carries no `return` of ours: expressions and bindings cannot contain
            // one (a closure's `return` is its own), and declarations open a new frame.
            Stmt::Echo { .. }
            | Stmt::Binding { .. }
            | Stmt::Destructure { .. }
            | Stmt::Expr { .. }
            | Stmt::Fn(_)
            | Stmt::Struct(_)
            | Stmt::Class(_)
            | Stmt::Enum(_)
            | Stmt::Trait(_)
            | Stmt::Impl(_)
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Whether `expr` is a bare object literal of `type_name` — the one expression form that is
/// *certainly* a fresh allocation this call owns alone.
///
/// A **spread** base (`Repo { ...other, tbl: t }`) is excluded on purpose: the in-place-reuse pass
/// may compile a spread into a mutation of the base rather than a new allocation, so the result
/// need not be fresh. An elided `.{ … }` qualifies — its type comes from the declared return type,
/// which is the enclosing type by the caller's own check.
fn is_fresh_literal(type_name: &str, expr: &Expr) -> bool {
    let Expr::Object(lit) = expr else {
        return false;
    };
    lit.spread.is_none()
        && lit
            .type_name
            .as_ref()
            .is_none_or(|n| n.as_str() == type_name)
}
