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

use crate::{ClosureBody, Expr, Program, Stmt};
use noeta_span::Span;

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
            Some((tier.name.clone(), f.name.clone()))
        })
        .collect()
}

/// Construct the handler call a [`Expr::TierExpr`] means: `handler([statics…], [fn() => hole,
/// …])`. `tier_span` becomes the callee's span (so "go to definition" on `@sql` lands on the
/// handler and a bad-call diagnostic points at the tier name); each closure carries its hole's
/// real span, so hole-type errors land inside the block.
pub fn tier_expr_call(
    handler: &str,
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
    Expr::Call {
        callee: Box::new(Expr::Ident {
            name: handler.to_string(),
            span: tier_span,
        }),
        args: vec![statics_list, holes_list],
        span,
    }
}
