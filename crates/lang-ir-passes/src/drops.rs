//! Drop-insertion pass (Phase 3.2): an IR→IR transform that places [`Stmt::DropVar`] at the
//! last-use death points of function-local source variables, so their values are reclaimed
//! promptly rather than held to scope/teardown.
//!
//! # What is dropped (deliberately conservative)
//!
//! This first slice drops only **single-assignment function locals**: a parameter that the body
//! never reassigns, or a name bound exactly once in the function body, that is neither a top-level
//! global (those stay teardown-reclaimed — the destructor-ordering spec keeps globals' timing) nor
//! a loop/`match`-pattern binding. Restricting to single-assignment bindings sidesteps every
//! reassignment-vs-drop double-free question (a reassigned binding's displaced value is already
//! released by the reassignment itself, §5), and a drop is only emitted after a **straight-line**
//! statement, never at a control-flow or `return` boundary. Everything outside this set is simply
//! left to scope/teardown — sound by the "never too early" invariant (a missed drop costs only
//! promptness). Wider coverage (reassigned accumulators, loop-scoped bindings, move-out at
//! `return`/`?`) is layered on in later slices.
//!
//! # Behavior
//!
//! A [`Stmt::DropVar`] lowers to a plain reference release in both backends (Phase 3.3), firing no
//! destructor, so inserting these drops changes reclamation *timing* (peak residency) but not
//! observable output — the differential stays green, the leak oracle reaches zero sooner.

use std::collections::HashMap;

use lang_ir::{Arm, Block, ClassDef, Decl, Func, Program, Rvalue, Stmt};
use lang_span::Span;

use crate::liveness::{self, BlockLiveness, StmtLiveness, VarSet};

/// Insert source-variable drops throughout a program, returning the annotated IR. The top level is
/// a global scope (no drops); every function/method/closure body gets drops for its single-
/// assignment locals.
pub fn insert_drops(program: &Program) -> Program {
    let globals = top_level_names(&program.top);
    // The top-level block is a global scope: no `DropVar`s for its own bindings, but its nested
    // function bodies are rewritten. An empty droppable set + a no-op liveness achieves exactly
    // that (no name is ever droppable here), while `rewrite_block` still recurses into functions.
    let live = liveness::analyze(program).top;
    let top = rewrite_block(&program.top, &live, &VarSet::new(), &globals);
    Program {
        top,
        temp_count: program.temp_count,
        span: program.span,
    }
}

// --- Rewrite ------------------------------------------------------------------------------------

/// Rewrite a function body: compute its liveness and single-assignment locals, then walk it
/// inserting drops. Nested functions inside it are rewritten by recursion.
fn rewrite_func(func: &Func, globals: &VarSet) -> Func {
    let live = liveness_of_body(func);
    let droppable = single_assignment_locals(func, globals);
    let body = rewrite_block(&func.body, &live, &droppable, globals);
    Func {
        params: func.params.clone(),
        // Default thunks run in the definition scope; this slice leaves them undropped (conservative).
        defaults: func.defaults.clone(),
        body,
        temp_count: func.temp_count,
        span: func.span,
    }
}

fn liveness_of_body(func: &Func) -> BlockLiveness {
    // Mirror `liveness::analyze_func`: the body is its own scope with nothing live after it.
    liveness::analyze(&Program {
        top: func.body.clone(),
        temp_count: func.temp_count,
        span: func.span,
    })
    .top
}

/// Rewrite a block in parallel with its liveness, inserting `DropVar`s after straight-line deaths
/// of droppable locals (reverse-construction LIFO when several die together) and recursing into
/// nested control-flow blocks and function bodies.
fn rewrite_block(
    block: &Block,
    live: &BlockLiveness,
    droppable: &VarSet,
    globals: &VarSet,
) -> Block {
    let mut out: Vec<Stmt> = Vec::with_capacity(block.stmts.len());
    for (stmt, sl) in block.stmts.iter().zip(&live.stmts) {
        out.push(rewrite_stmt(stmt, sl, droppable, globals));
        if is_straight_line(stmt) {
            for name in lifo_deaths(&sl.dies_here, droppable) {
                out.push(Stmt::DropVar {
                    name,
                    span: stmt_span(stmt),
                });
            }
        }
    }
    // Collapse adjacent duplicate `DropVar`s for the same name (a drop the input IR already carried
    // here meets the one we just inserted). This makes the pass idempotent — re-running on annotated
    // IR is a no-op — and is harmless since a repeated drop would be a no-op at runtime anyway.
    out.dedup_by(|a, b| {
        matches!((&*a, &*b), (Stmt::DropVar { name: x, .. }, Stmt::DropVar { name: y, .. }) if x == y)
    });
    Block {
        stmts: out,
        tail: block.tail.clone(),
    }
}

/// Rewrite one statement: recurse into its nested control-flow blocks (using the matching
/// sub-liveness) and into any function body it carries. Straight-line statements with no
/// sub-structure are returned with their nested functions (if any) rewritten.
fn rewrite_stmt(stmt: &Stmt, sl: &StmtLiveness, droppable: &VarSet, globals: &VarSet) -> Stmt {
    match stmt {
        Stmt::Let { dst, rvalue, span } => Stmt::Let {
            dst: *dst,
            rvalue: rewrite_rvalue(rvalue, globals),
            span: *span,
        },
        Stmt::Eval { rvalue, span } => Stmt::Eval {
            rvalue: rewrite_rvalue(rvalue, globals),
            span: *span,
        },
        Stmt::If {
            cond,
            then_block,
            else_block,
            span,
        } => {
            let then_block = rewrite_block(then_block, &sl.sub[0], droppable, globals);
            let else_block = else_block
                .as_ref()
                .map(|b| rewrite_block(b, &sl.sub[1], droppable, globals));
            Stmt::If {
                cond: cond.clone(),
                then_block,
                else_block,
                span: *span,
            }
        }
        Stmt::While { cond, body, span } => Stmt::While {
            cond: rewrite_block(cond, &sl.sub[0], droppable, globals),
            body: rewrite_block(body, &sl.sub[1], droppable, globals),
            span: *span,
        },
        Stmt::For {
            pattern,
            iterable,
            body,
            span,
        } => Stmt::For {
            pattern: pattern.clone(),
            iterable: iterable.clone(),
            body: rewrite_block(body, &sl.sub[0], droppable, globals),
            span: *span,
        },
        Stmt::Match {
            scrutinee,
            arms,
            dst,
            span,
        } => Stmt::Match {
            scrutinee: scrutinee.clone(),
            arms: arms
                .iter()
                .zip(&sl.sub)
                .map(|(arm, arm_live)| Arm {
                    pattern: arm.pattern.clone(),
                    body: rewrite_block(&arm.body, arm_live, droppable, globals),
                    span: arm.span,
                })
                .collect(),
            dst: *dst,
            span: *span,
        },
        Stmt::Logical {
            dst,
            op,
            left,
            right,
            span,
        } => Stmt::Logical {
            dst: *dst,
            op: *op,
            left: left.clone(),
            right: rewrite_block(right, &sl.sub[0], droppable, globals),
            span: *span,
        },
        Stmt::Coalesce {
            dst,
            value,
            fallback,
            span,
        } => Stmt::Coalesce {
            dst: *dst,
            value: value.clone(),
            fallback: rewrite_block(fallback, &sl.sub[0], droppable, globals),
            span: *span,
        },
        Stmt::Decl(decl) => Stmt::Decl(rewrite_decl(decl, globals)),
        // No nested blocks or functions: returned verbatim.
        Stmt::Bind { .. }
        | Stmt::Echo { .. }
        | Stmt::Return { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. } => stmt.clone(),
    }
}

fn rewrite_decl(decl: &Decl, globals: &VarSet) -> Decl {
    match decl {
        Decl::Fn { name, func, span } => Decl::Fn {
            name: name.clone(),
            func: std::rc::Rc::new(rewrite_func(func, globals)),
            span: *span,
        },
        Decl::Class(class) => Decl::Class(ClassDef {
            decl: class.decl.clone(),
            methods: class
                .methods
                .iter()
                .map(|(n, f)| (n.clone(), std::rc::Rc::new(rewrite_func(f, globals))))
                .collect(),
            destructor: class
                .destructor
                .as_ref()
                .map(|f| std::rc::Rc::new(rewrite_func(f, globals))),
            span: class.span,
        }),
        Decl::Enum(_) | Decl::Record(_) | Decl::Use { .. } => decl.clone(),
    }
}

/// Rewrite a [`Rvalue::Closure`]'s function body; all other rvalues are returned unchanged (they
/// carry no nested function body).
fn rewrite_rvalue(rvalue: &Rvalue, globals: &VarSet) -> Rvalue {
    match rvalue {
        Rvalue::Closure { func, span } => Rvalue::Closure {
            func: std::rc::Rc::new(rewrite_func(func, globals)),
            span: *span,
        },
        other => other.clone(),
    }
}

// --- Droppable-set computation -----------------------------------------------------------------

/// The single-assignment function-local names eligible for a drop: a parameter the body never
/// reassigns, or a name bound exactly once in the body, excluding globals and loop/`match`-pattern
/// bindings. (See the module note for why single-assignment is the safe first target.)
fn single_assignment_locals(func: &Func, globals: &VarSet) -> VarSet {
    let mut bind_counts: HashMap<String, u32> = HashMap::new();
    let mut excluded: VarSet = VarSet::new();
    count_binds_block(&func.body, &mut bind_counts, &mut excluded);
    // A local captured by a nested closure must never be dropped here: the closure outlives this
    // death point and reads the value later (in eval through the shared scope; in the VM through a
    // shared cell). Over-approximate captures as every name referenced anywhere inside a nested
    // closure/`fn` body and exclude them — conservative (over-excludes → fewer drops → safe).
    collect_captured(&func.body, &mut excluded);

    let mut droppable = VarSet::new();
    // A parameter is bound once (at entry); it stays droppable only if the body never reassigns it.
    for p in &func.params {
        if !bind_counts.contains_key(p) && !excluded.contains(p) && !globals.contains(p) {
            droppable.insert(p.clone());
        }
    }
    // A name bound exactly once in the body (and not a parameter, not a pattern binding, not global).
    for (name, count) in &bind_counts {
        if *count == 1
            && !excluded.contains(name)
            && !globals.contains(name)
            && !func.params.contains(name)
        {
            droppable.insert(name.clone());
        }
    }
    droppable
}

/// Add to `out` every source-variable name referenced inside a nested closure / `fn` body within
/// `block` (a superset of the names those closures capture). Recurses through control-flow blocks
/// to find the closures, then collects *all* variable names in each closure body.
fn collect_captured(block: &Block, out: &mut VarSet) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let { rvalue, .. } | Stmt::Eval { rvalue, .. } => {
                if let Rvalue::Closure { func, .. } = rvalue {
                    collect_func_vars(func, out);
                }
            }
            Stmt::Decl(Decl::Fn { func, .. }) => collect_func_vars(func, out),
            Stmt::If {
                then_block,
                else_block,
                ..
            } => {
                collect_captured(then_block, out);
                if let Some(b) = else_block {
                    collect_captured(b, out);
                }
            }
            Stmt::While { cond, body, .. } => {
                collect_captured(cond, out);
                collect_captured(body, out);
            }
            Stmt::For { body, .. } => collect_captured(body, out),
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    collect_captured(&arm.body, out);
                }
            }
            Stmt::Logical { right, .. } => collect_captured(right, out),
            Stmt::Coalesce { fallback, .. } => collect_captured(fallback, out),
            _ => {}
        }
    }
}

/// Collect every source-variable name a nested function captures — over-approximated as every name
/// referenced anywhere in its body **and its parameter defaults** (a closure default is evaluated
/// in the captured scope, so it captures the variables it names, even ones the body never uses).
fn collect_func_vars(func: &Func, out: &mut VarSet) {
    for name in liveness::referenced_vars_in_block(&func.body) {
        out.insert(name);
    }
    for default in func.defaults.iter().flatten() {
        for name in liveness::referenced_vars_in_block(&default.body) {
            out.insert(name);
        }
    }
}

/// Count `Bind` sites per name within one function body (not descending into nested function
/// bodies, which have their own scopes). Names introduced by `for`/`match` patterns are recorded in
/// `excluded` (their per-iteration / per-arm scoping is out of this slice's scope).
fn count_binds_block(block: &Block, counts: &mut HashMap<String, u32>, excluded: &mut VarSet) {
    for stmt in &block.stmts {
        count_binds_stmt(stmt, counts, excluded);
    }
}

fn count_binds_stmt(stmt: &Stmt, counts: &mut HashMap<String, u32>, excluded: &mut VarSet) {
    match stmt {
        Stmt::Bind { name, .. } => {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        Stmt::If {
            then_block,
            else_block,
            ..
        } => {
            count_binds_block(then_block, counts, excluded);
            if let Some(else_block) = else_block {
                count_binds_block(else_block, counts, excluded);
            }
        }
        Stmt::While { cond, body, .. } => {
            count_binds_block(cond, counts, excluded);
            count_binds_block(body, counts, excluded);
        }
        Stmt::For { pattern, body, .. } => {
            for_pattern_names(pattern, excluded);
            count_binds_block(body, counts, excluded);
        }
        Stmt::Match { arms, .. } => {
            for arm in arms {
                pattern_names(&arm.pattern, excluded);
                count_binds_block(&arm.body, counts, excluded);
            }
        }
        Stmt::Logical { right, .. } => count_binds_block(right, counts, excluded),
        Stmt::Coalesce { fallback, .. } => count_binds_block(fallback, counts, excluded),
        // A nested `fn` binds its own name in this scope; its body is a separate scope (not counted).
        Stmt::Decl(Decl::Fn { name, .. }) => {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        Stmt::Let { .. }
        | Stmt::Eval { .. }
        | Stmt::Echo { .. }
        | Stmt::Return { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. }
        | Stmt::Decl(_) => {}
    }
}

// --- Helpers -----------------------------------------------------------------------------------

/// The droppable names dying at a statement, ordered **reverse-construction** (LIFO) for the §3
/// drop ordering. With a deterministic but coarse approximation: alphabetical descending, which is
/// stable and only matters when several destructor-bearing locals die at the very same point (rare
/// in this single-assignment slice). Precise construction-order LIFO arrives with broader coverage.
fn lifo_deaths(dies_here: &VarSet, droppable: &VarSet) -> Vec<String> {
    let mut names: Vec<String> = dies_here
        .iter()
        .filter(|n| droppable.contains(*n))
        .cloned()
        .collect();
    names.sort_unstable();
    names.reverse();
    names
}

/// Whether a statement is a straight-line statement after which a drop may be appended (i.e. not a
/// control-flow construct, `return`, or `break`/`continue`).
fn is_straight_line(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::Let { .. } | Stmt::Eval { .. } | Stmt::Bind { .. } | Stmt::Echo { .. }
    )
}

/// The top-level source-binding names — the program's globals, never dropped early.
fn top_level_names(top: &Block) -> VarSet {
    let mut names = VarSet::new();
    for stmt in &top.stmts {
        match stmt {
            Stmt::Bind { name, .. } => {
                names.insert(name.clone());
            }
            Stmt::Decl(Decl::Fn { name, .. }) => {
                names.insert(name.clone());
            }
            Stmt::Decl(Decl::Use { names: us, .. }) => {
                for u in us {
                    names.insert(u.name.clone());
                }
            }
            _ => {}
        }
    }
    names
}

/// A representative span for a `DropVar` inserted after `stmt` (used only for potential future
/// diagnostics; a drop is never itself a diagnostic site).
fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Let { span, .. }
        | Stmt::Eval { span, .. }
        | Stmt::Bind { span, .. }
        | Stmt::Echo { span, .. } => *span,
        _ => Span::new(0, 0),
    }
}

/// Names a `for` pattern binds.
fn for_pattern_names(pattern: &lang_ir::ForPattern, out: &mut VarSet) {
    match pattern {
        lang_ir::ForPattern::Single { name, .. } => {
            out.insert(name.clone());
        }
        lang_ir::ForPattern::Pair { first, second, .. } => {
            out.insert(first.clone());
            out.insert(second.clone());
        }
    }
}

/// Names a `match` arm pattern binds (recursing into variant sub-patterns).
fn pattern_names(pattern: &lang_ir::Pattern, out: &mut VarSet) {
    match pattern {
        lang_ir::Pattern::Binding { name, .. } => {
            out.insert(name.clone());
        }
        lang_ir::Pattern::Variant { bindings, .. } => {
            for sub in bindings {
                pattern_names(sub, out);
            }
        }
        lang_ir::Pattern::Wildcard { .. }
        | lang_ir::Pattern::Int { .. }
        | lang_ir::Pattern::Str { .. }
        | lang_ir::Pattern::Bool { .. }
        | lang_ir::Pattern::IsType { .. } => {}
    }
}
