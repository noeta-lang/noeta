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
use std::rc::Rc;

use lang_ir::{Arm, Block, ClassDef, Decl, Func, Program, Rvalue, Stmt};
use lang_span::Span;

use crate::liveness::{self, BlockLiveness, StmtLiveness, VarSet};

/// The destructor-relevance of bindings, as the checker computes it (mirrors
/// `lang_check::DestructorRelevance`; kept here so this crate needs no checker dependency — the
/// caller copies the two sets across). A binding **absent** from both sets is provably non-relevant;
/// a binding present *may* run a destructor. `None` passed to [`insert_drops`] means "no information"
/// → every drop is conservatively relevant.
#[derive(Debug, Clone, Default)]
pub struct Relevance {
    /// `name_span`s of non-parameter bindings whose value's type is destruct-reachable.
    pub locals: std::collections::HashSet<Span>,
    /// `(function span, parameter name)` of parameters whose type is destruct-reachable.
    pub params: std::collections::HashSet<(Span, String)>,
}

/// A function's droppable locals mapped to their destructor-relevance bit.
type DropSet = HashMap<String, bool>;

/// Program-wide context threaded through the rewrite: the globals (never dropped) and the
/// destructor-relevance oracle (`None` ⇒ conservatively relevant).
struct Cx<'a> {
    globals: &'a VarSet,
    relevance: Option<&'a Relevance>,
}

/// Insert source-variable drops throughout a program, returning the annotated IR. The top level is
/// a global scope (no drops); every function/method/closure body gets drops for its single-
/// assignment locals, each tagged with its destructor-relevance from `relevance`.
pub fn insert_drops(program: &Program, relevance: Option<&Relevance>) -> Program {
    let globals = top_level_names(&program.top);
    let cx = Cx {
        globals: &globals,
        relevance,
    };
    // The top-level block is a global scope: no `DropVar`s for its own bindings (an empty drop set),
    // but `rewrite_block` still recurses into its nested function bodies.
    let live = liveness::analyze(program).top;
    // The top level is the global scope: neither last-use nor scope-exit drops apply (globals are
    // teardown-reclaimed — the destructor-ordering spec keeps their timing), so both sets are empty.
    let top = rewrite_block(&program.top, &live, &DropSet::new(), &DropSet::new(), &cx);
    Program {
        top,
        temp_count: program.temp_count,
        span: program.span,
    }
}

// --- Rewrite ------------------------------------------------------------------------------------

/// Rewrite a function body: compute its liveness, its droppable single-assignment locals and their
/// relevance, then walk it inserting drops. Nested functions inside it are rewritten by recursion.
fn rewrite_func(func: &Func, cx: &Cx) -> Func {
    let live = liveness_of_body(func);
    let droppable = drop_set(func, cx);
    // The broader **owned** set (Phase 4.2a): every owned local, including reassigned (multi-bind)
    // ones the single-assignment `droppable` set excludes. These drive the scope-exit drops that
    // reclaim a block-local's *final* value (a dead store, or the survivor of a reassignment) when
    // control falls off the block's end — the last-use drops above only catch values that die at a
    // straight-line use mid-block.
    let owned = owned_set(func, cx);
    let body = rewrite_block(&func.body, &live, &droppable, &owned, cx);
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
/// nested control-flow blocks and function bodies. When control **falls off the block's end**, a
/// trailing batch of **scope-exit** drops (Phase 4.2a) reclaims every owned local declared directly
/// in this block whose value still occupies its slot there — a dead store, or a reassignment's
/// surviving value — in reverse-construction order. Values that left the scope are excluded: ones
/// already dropped at a mid-block last use, and ones **moved out** through the block's `tail`.
fn rewrite_block(
    block: &Block,
    live: &BlockLiveness,
    droppable: &DropSet,
    owned: &DropSet,
    cx: &Cx,
) -> Block {
    let mut out: Vec<Stmt> = Vec::with_capacity(block.stmts.len());
    // Names directly bound in this block, in first-construction order, and the ones a last-use drop
    // already reclaimed here (a single-assignment local never rebinds, so "already dropped" is final
    // — its slot stays empty to the block's end).
    let mut construction_order: Vec<String> = Vec::new();
    let mut dropped_here: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (stmt, sl) in block.stmts.iter().zip(&live.stmts) {
        if let Stmt::Bind { name, .. } = stmt
            && !construction_order.contains(name)
        {
            construction_order.push(name.clone());
        }
        out.push(rewrite_stmt(stmt, sl, droppable, owned, cx));
        if is_straight_line(stmt) {
            for name in lifo_deaths(&sl.dies_here, droppable) {
                let relevant = droppable[&name];
                dropped_here.insert(name.clone());
                out.push(Stmt::DropVar {
                    name,
                    span: stmt_span(stmt),
                    relevant,
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
    // Scope-exit drops — only when control falls off the end (an early-exit terminator routes its own
    // drops in Phase 4.2b; appending here would be unreachable and could drop a moved-out value).
    if !ends_with_terminator(block) {
        let moved_out = tail_vars(block);
        for name in construction_order.into_iter().rev() {
            // Skip a value the block does not abandon: one already reclaimed at a mid-block last use,
            // one **moved out** through the tail, and — crucially — one still **live at the block's
            // exit** (it flows out to an enclosing scope, or back around a loop's back-edge into a
            // later iteration). Dropping a live value would null a slot still in use.
            if dropped_here.contains(&name)
                || moved_out.contains(&name)
                || live.live_out.contains(&name)
            {
                continue;
            }
            if let Some(&relevant) = owned.get(&name) {
                out.push(Stmt::DropVar {
                    name,
                    span: block_exit_span(block),
                    relevant,
                });
            }
        }
    }
    Block {
        stmts: out,
        tail: block.tail.clone(),
    }
}

/// Rewrite one statement: recurse into its nested control-flow blocks (using the matching
/// sub-liveness) and into any function body it carries. Straight-line statements with no
/// sub-structure are returned with their nested functions (if any) rewritten.
fn rewrite_stmt(
    stmt: &Stmt,
    sl: &StmtLiveness,
    droppable: &DropSet,
    owned: &DropSet,
    cx: &Cx,
) -> Stmt {
    match stmt {
        Stmt::Let { dst, rvalue, span } => Stmt::Let {
            dst: *dst,
            rvalue: rewrite_rvalue(rvalue, cx),
            span: *span,
        },
        Stmt::Eval { rvalue, span } => Stmt::Eval {
            rvalue: rewrite_rvalue(rvalue, cx),
            span: *span,
        },
        Stmt::If {
            cond,
            then_block,
            else_block,
            span,
        } => {
            let then_block = rewrite_block(then_block, &sl.sub[0], droppable, owned, cx);
            let else_block = else_block
                .as_ref()
                .map(|b| rewrite_block(b, &sl.sub[1], droppable, owned, cx));
            Stmt::If {
                cond: cond.clone(),
                then_block,
                else_block,
                span: *span,
            }
        }
        Stmt::While { cond, body, span } => Stmt::While {
            cond: rewrite_block(cond, &sl.sub[0], droppable, owned, cx),
            body: rewrite_block(body, &sl.sub[1], droppable, owned, cx),
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
            body: rewrite_block(body, &sl.sub[0], droppable, owned, cx),
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
                    body: rewrite_block(&arm.body, arm_live, droppable, owned, cx),
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
            right: rewrite_block(right, &sl.sub[0], droppable, owned, cx),
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
            fallback: rewrite_block(fallback, &sl.sub[0], droppable, owned, cx),
            span: *span,
        },
        Stmt::Decl(decl) => Stmt::Decl(rewrite_decl(decl, cx)),
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

fn rewrite_decl(decl: &Decl, cx: &Cx) -> Decl {
    match decl {
        Decl::Fn { name, func, span } => Decl::Fn {
            name: name.clone(),
            func: Rc::new(rewrite_func(func, cx)),
            span: *span,
        },
        Decl::Class(class) => Decl::Class(ClassDef {
            decl: class.decl.clone(),
            methods: class
                .methods
                .iter()
                .map(|(n, f)| (n.clone(), Rc::new(rewrite_func(f, cx))))
                .collect(),
            destructor: class
                .destructor
                .as_ref()
                .map(|f| Rc::new(rewrite_func(f, cx))),
            span: class.span,
        }),
        Decl::Enum(_) | Decl::Record(_) | Decl::Use { .. } => decl.clone(),
    }
}

/// Rewrite a [`Rvalue::Closure`]'s function body; all other rvalues are returned unchanged (they
/// carry no nested function body).
fn rewrite_rvalue(rvalue: &Rvalue, cx: &Cx) -> Rvalue {
    match rvalue {
        Rvalue::Closure { func, span } => Rvalue::Closure {
            func: Rc::new(rewrite_func(func, cx)),
            span: *span,
        },
        other => other.clone(),
    }
}

/// Build the droppable-locals → relevance map for one function: the single-assignment locals
/// (computed as before), each looked up in the relevance oracle. A parameter is keyed by
/// `(func.span, name)`; a local by its `Bind` `name_span`; with no oracle (or a name that cannot
/// be located) the drop is conservatively relevant.
fn drop_set(func: &Func, cx: &Cx) -> DropSet {
    relevance_map(func, cx, single_assignment_locals(func, cx.globals))
}

/// Build the **owned-locals → relevance** map for one function: *every* owned local (not just the
/// single-assignment ones), so a reassigned binding's surviving value is reclaimed at scope exit
/// (Phase 4.2a). Same exclusions as the droppable set otherwise — globals, captured names, and
/// loop/`match`-pattern bindings stay out.
fn owned_set(func: &Func, cx: &Cx) -> DropSet {
    relevance_map(func, cx, owned_locals(func, cx.globals))
}

/// Look each name up in the destructor-relevance oracle. A parameter is keyed by `(func.span, name)`;
/// a local by its `Bind` `name_span`; with no oracle (or a name that cannot be located) the drop is
/// conservatively relevant.
fn relevance_map(func: &Func, cx: &Cx, names: VarSet) -> DropSet {
    let mut bind_spans: HashMap<String, Span> = HashMap::new();
    collect_bind_spans(&func.body, &mut bind_spans);
    let params: std::collections::HashSet<&String> = func.params.iter().collect();
    names
        .into_iter()
        .map(|name| {
            let relevant = match cx.relevance {
                None => true,
                Some(r) => {
                    if params.contains(&name) {
                        r.params.contains(&(func.span, name.clone()))
                    } else if let Some(span) = bind_spans.get(&name) {
                        r.locals.contains(span)
                    } else {
                        true
                    }
                }
            };
            (name, relevant)
        })
        .collect()
}

/// Map each `Bind` name to its `name_span` within one function body (not descending into nested
/// function bodies). Droppable locals are single-assignment, so each has a unique span here.
fn collect_bind_spans(block: &Block, out: &mut HashMap<String, Span>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Bind {
                name, name_span, ..
            } => {
                out.insert(name.clone(), *name_span);
            }
            Stmt::If {
                then_block,
                else_block,
                ..
            } => {
                collect_bind_spans(then_block, out);
                if let Some(b) = else_block {
                    collect_bind_spans(b, out);
                }
            }
            Stmt::While { cond, body, .. } => {
                collect_bind_spans(cond, out);
                collect_bind_spans(body, out);
            }
            Stmt::For { body, .. } => collect_bind_spans(body, out),
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    collect_bind_spans(&arm.body, out);
                }
            }
            Stmt::Logical { right, .. } => collect_bind_spans(right, out),
            Stmt::Coalesce { fallback, .. } => collect_bind_spans(fallback, out),
            _ => {}
        }
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

/// Every **owned** function-local name: the same set as [`single_assignment_locals`] but without the
/// "bound exactly once" restriction, so reassigned (multi-bind) locals are included. A reassigned
/// local's *intermediate* values are destroyed at each assignment by the runtime (spec §5); this set
/// exists so its *surviving* value — and any never-read dead store — is reclaimed at scope exit
/// (Phase 4.2a). The exclusions are identical: globals, closure-captured names, and `for`/`match`
/// pattern bindings stay out.
fn owned_locals(func: &Func, globals: &VarSet) -> VarSet {
    let mut bind_counts: HashMap<String, u32> = HashMap::new();
    let mut excluded: VarSet = VarSet::new();
    count_binds_block(&func.body, &mut bind_counts, &mut excluded);
    collect_captured(&func.body, &mut excluded);

    let mut owned = VarSet::new();
    // A parameter is owned unless reassigned in the body, captured, or shadowing a global.
    for p in &func.params {
        if !bind_counts.contains_key(p) && !excluded.contains(p) && !globals.contains(p) {
            owned.insert(p.clone());
        }
    }
    // Any name bound in the body (any number of times), excluding pattern bindings, captures, globals,
    // and parameters (handled above).
    for name in bind_counts.keys() {
        if !excluded.contains(name) && !globals.contains(name) && !func.params.contains(name) {
            owned.insert(name.clone());
        }
    }
    owned
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
fn lifo_deaths(dies_here: &VarSet, droppable: &DropSet) -> Vec<String> {
    let mut names: Vec<String> = dies_here
        .iter()
        .filter(|n| droppable.contains_key(*n))
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

/// Whether a block's last statement transfers control out of the block (`return`/`break`/`continue`).
/// Scope-exit drops are suppressed for such a block — its abandoned-point drops are placed *before*
/// the terminator in Phase 4.2b, and anything appended after would be unreachable.
fn ends_with_terminator(block: &Block) -> bool {
    matches!(
        block.stmts.last(),
        Some(Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. })
    )
}

/// The source variable a block **moves out** through its tail atom, if any — its value is the block's
/// result, so a scope-exit drop must not reclaim it.
fn tail_vars(block: &Block) -> VarSet {
    let mut out = VarSet::new();
    if let Some(lang_ir::Atom::Var { name, .. }) = &block.tail {
        out.insert(name.clone());
    }
    out
}

/// A representative span for a scope-exit drop: the block's tail-var span if present, else its last
/// statement's span (drops are never themselves diagnostic sites — this is for potential tooling).
fn block_exit_span(block: &Block) -> Span {
    if let Some(lang_ir::Atom::Var { span, .. }) = &block.tail {
        return *span;
    }
    block.stmts.last().map(stmt_span).unwrap_or(Span::new(0, 0))
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
