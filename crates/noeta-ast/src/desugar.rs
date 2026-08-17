//! The **expression-tier desugar** (expr-tiers arc): the shared construction that turns a
//! `@<name> { … }` block of a declared expression tier into a call of the tier's handler —
//!
//! ```text
//! @sql { select ${id} }   ⇒   handler(["select ", ""], [fn() => id])
//! ```
//!
//! [`Expr::TierExpr`] follows the same architecture as `Try`/`Await`/`as<T>`: it stays its own
//! node through parse (so `noeta fmt` re-emits it verbatim), the **checker types it** by checking
//! this constructed call (hole types flow bidirectionally against the handler's
//! `List<() -> U>`, the block types as the handler's return — ordinary call rules, real hole
//! spans), and **IR lowering rewrites it** through the same constructor, so both backends, the
//! REPL, and every embedding pipeline agree by construction. This module is the one place the
//! desugared shape exists; the checker and the lowerer both call it.
//!
//! Holes desugar to **zero-param closures**, so evaluation is the handler's choice (call each
//! thunk once for an eager DSL; wrap them in computeds for a reactive one), and a hole's lexical
//! captures ride the ordinary closure machinery.

use crate::{CallArg, ClosureBody, Expr, Name, Program, Stmt};
use noeta_span::Span;

/// The resolved handler an expression tier's block desugars to. A **program**-declared tier's
/// handler is a (qualified) Noeta function referenced by name; a **native** (extension) tier's is
/// a module function referenced directly. The desugar builds the matching callee for each, so both
/// flow through the identical `Call` typing and lowering — the callee is a function value either
/// way (the native one via [`Expr::NativeFnRef`], resolved without a user import).
#[derive(Debug, Clone, PartialEq)]
pub enum ExprTierHandler {
    /// A Noeta function by (link-qualified) name.
    Program(String),
    /// A native module function `(module, func)`.
    Native { module: String, func: String },
}

impl ExprTierHandler {
    /// Resolve an extension tier's `handler` string (`"std.json.render"`) into its module/func
    /// split, or a program tier's runner name — the shape [`tier_expr_call`] builds a callee from.
    pub fn from_native_path(path: &str) -> ExprTierHandler {
        match path.rsplit_once('.') {
            Some((module, func)) => ExprTierHandler::Native {
                module: module.to_string(),
                func: func.to_string(),
            },
            // A bare name (no module) is treated as a program fn — a misdeclared native handler is
            // a checker concern, not this constructor's.
            None => ExprTierHandler::Program(path.to_string()),
        }
    }
}

/// The program's declared **expression-tier handlers**: tier name → handler fn name, collected
/// from top-level `@tier(…, expr: T)` fns. On a linked program the handler names are qualified
/// identities (the linker qualified every top-level fn), so cross-package blocks resolve.
pub fn expr_tier_handlers(program: &Program) -> std::collections::HashMap<String, String> {
    program
        .stmts
        .iter()
        .filter_map(|stmt| {
            let Stmt::Fn(f) = stmt else { return None };
            let tier = f.tier.as_ref()?;
            tier.expr.as_ref()?;
            Some((tier.name.clone(), f.name.to_string()))
        })
        .collect()
}

/// Construct the handler call a [`Expr::TierExpr`] means: `handler([statics…], [fn() => hole,
/// …])`. `tier_span` becomes the callee's span (so "go to definition" on `@sql` lands on the
/// handler and a bad-call diagnostic points at the tier name); each closure carries its hole's
/// real span, so hole-type errors land inside the block.
pub fn tier_expr_call(
    handler: &ExprTierHandler,
    tier_span: Span,
    statics: &[String],
    holes: &[Expr],
    span: Span,
) -> Expr {
    let statics_list = Expr::List {
        items: statics
            .iter()
            .map(|s| Expr::Str {
                value: s.clone(),
                span,
            })
            .collect(),
        span,
    };
    let holes_list = Expr::List {
        items: holes
            .iter()
            .map(|h| Expr::Closure {
                params: Vec::new(),
                ret: None,
                body: ClosureBody::Expr(Box::new(h.clone())),
                span: h.span(),
            })
            .collect(),
        span,
    };
    // A program handler is an ordinary named-function callee; a native handler is a resolved
    // module-function reference — both are function *values*, so the `Call` is identical.
    let callee = match handler {
        ExprTierHandler::Program(name) => Expr::Ident {
            name: Name::canonical(name),
            span: tier_span,
        },
        ExprTierHandler::Native { module, func } => Expr::NativeFnRef {
            module: module.clone(),
            func: func.clone(),
            span: tier_span,
        },
    };
    Expr::Call {
        callee: Box::new(callee),
        args: vec![
            CallArg::positional(statics_list),
            CallArg::positional(holes_list),
        ],
        span,
    }
}

// --- REPL trailing-expression desugar (audit-3 finding 10) -------------------------------------
//
// Shared by BOTH session backends — `noeta-vm`'s `VmSession` and the `noeta-eval` oracle session —
// which previously carried verbatim copies of this constant + function that only the
// `session_parity` differential held together. The desugared shape exists here once, beside the
// expression-tier desugar above, for the same reason: both backends must agree by construction.

/// The reserved binding name a trailing bare REPL expression is rewritten into, so the lowering
/// path captures its value in a persistent slot both session backends can read back. Contains a
/// NUL so it can never collide with a user identifier and never shows up in displayed bindings.
pub const REPL_VALUE: &str = "\0repl-value";

/// The span of the expression a session **echoes** — the trailing bare expression
/// [`rewrite_trailing_expr`] captures — or `None` when the entry ends in anything else.
///
/// Shared by the session checker, which records the echo's render hint at this span, and by the
/// sessions, which read that hint back to render the value. Both ask this one function rather than
/// each matching on `stmts.last()`, so they cannot disagree about *which* expression is echoed —
/// and a disagreement would be silent, since a hint keyed at a span nobody looks up simply never
/// applies.
pub fn trailing_expr_span(program: &Program) -> Option<Span> {
    match program.stmts.last() {
        Some(Stmt::Expr { expr, .. }) => Some(expr.span()),
        _ => None,
    }
}

/// If `program`'s final statement is a bare expression, return a copy with that statement
/// rewritten to `mut <REPL_VALUE> = <expr>;` (so the lowering path captures its value) and `true`;
/// otherwise return the program unchanged and `false`. Only the trailing statement is touched —
/// earlier bare expressions stay discarded statements.
pub fn rewrite_trailing_expr(program: &Program) -> (Program, bool) {
    match program.stmts.last() {
        Some(Stmt::Expr { expr, span }) => {
            let mut stmts = program.stmts.clone();
            *stmts.last_mut().expect("non-empty: matched last") = Stmt::Binding {
                mut_decl: true,
                name: REPL_VALUE.to_string(),
                name_span: *span,
                ty: None,
                value: expr.clone(),
                span: *span,
            };
            (
                Program {
                    stmts,
                    span: program.span,
                },
                true,
            )
        }
        _ => (program.clone(), false),
    }
}
