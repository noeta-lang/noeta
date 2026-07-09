//! Last-use / liveness analysis over the Core IR (Phase 3.1).
//!
//! A **backward** dataflow over the structured IR. For a program point, the *live* set is the set
//! of source-variable names that may still be read on some path starting after that point. Walking
//! statements in reverse, a use makes a variable live, a (re)binding kills the prior liveness of
//! its name, and structured control flow joins the live sets of its arms / iterates a loop to a
//! fixpoint. From the live sets we read each variable's **death** points: a use whose variable is
//! not live afterward is that variable's last use on that path.
//!
//! Only **source variables** ([`noeta_ir::Atom::Var`]) are tracked. ANF temporaries are single-use
//! (lowering gives each `let` exactly one consumer), so their last use is structural, not a
//! dataflow result, and the backends handle them directly.
//!
//! The analysis is **uniform** — it computes liveness everywhere, including the top level and
//! inside loops. The *policy* of which deaths become drops (function locals yes; top-level globals
//! stay teardown-reclaimed; a returned/propagated value moves out rather than dies) belongs to the
//! drop-insertion pass that consumes this result, not here.
//!
//! # Soundness direction
//!
//! Uses are **over-approximated** where precision would cost code: a closure / nested `fn` is taken
//! to use *every* variable named anywhere in its body and defaults (a superset of its true free
//! variables). Over-approximating uses keeps variables live at least as long as they truly are, so
//! no death is ever reported too early — the "never too early" invariant holds by construction.

use std::collections::BTreeSet;

use noeta_ir::{Atom, Block, Decl, ForPattern, Func, Pattern, Program, Rvalue, Stmt};

/// A set of source-variable names. `BTreeSet` so the analysis output is deterministic (tests and
/// the future drop-insertion pass both want a stable order).
pub type VarSet = BTreeSet<String>;

/// Liveness of a whole program: the top-level block plus the per-function-body results reached
/// through declarations and closures (stored inline at their statement sites, see
/// [`StmtLiveness::sub`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramLiveness {
    pub top: BlockLiveness,
}

/// Liveness annotations for one [`Block`], mirroring its statement structure 1:1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLiveness {
    /// The variables live on entry to the block (before its first statement) — its `live_in`.
    pub live_in: VarSet,
    /// The variables live on **exit** from the block — the `live_out` it was analyzed against. For a
    /// loop body this is the converged fixpoint set, so it includes names that flow around the
    /// back-edge (live in a later iteration). The drop pass uses it to keep its scope-exit drops off
    /// any value the block does not actually abandon.
    pub live_out: VarSet,
    /// One entry per statement in `block.stmts`, in order.
    pub stmts: Vec<StmtLiveness>,
}

/// Liveness annotations for one [`Stmt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StmtLiveness {
    /// Source variables whose **last use on this path** is this statement — i.e. read here (in the
    /// statement's own operands, not in a nested block) and not live afterward. These are the
    /// candidate straight-line drop points the drop-insertion pass will use.
    pub dies_here: VarSet,
    /// Liveness of this statement's nested blocks, in a fixed per-statement order:
    /// * `If`        → `[then, else?]` (else present only when the `If` has one);
    /// * `While`     → `[cond, body]`;
    /// * `For`       → `[body]`;
    /// * `Match`     → one per arm, in arm order;
    /// * `Logical`   → `[right]`;
    /// * `Coalesce`  → `[fallback]`;
    /// * `Decl(Fn)`  → `[body]` (the function body, its own scope);
    /// * a `Let`/`Eval` whose rvalue is a `Closure` → `[body]`;
    /// * every other statement → empty.
    pub sub: Vec<BlockLiveness>,
}

/// Every source-variable name appearing anywhere in a block (operands, bindings, nested closures,
/// patterns) — a sound over-approximation of the names it reads, used by the drop pass to
/// over-approximate closure captures.
pub fn referenced_vars_in_block(block: &Block) -> VarSet {
    let mut out = VarSet::new();
    collect_block_vars(block, &mut out);
    out
}

/// Compute liveness for a whole program.
pub fn analyze(program: &Program) -> ProgramLiveness {
    // Top-level source bindings are globals; nothing is live after the program's last statement
    // (global teardown is modeled by the consumer's policy, not as a use here).
    let top = analyze_block(&program.top, &VarSet::new());
    ProgramLiveness { top }
}

/// Analyze a block given the set of variables live **after** it (`live_out`). Returns the block's
/// liveness (its `live_in` and per-statement annotations).
fn analyze_block(block: &Block, live_out: &VarSet) -> BlockLiveness {
    // The optional tail atom is evaluated last, after every statement.
    let mut live = live_out.clone();
    if let Some(tail) = &block.tail {
        use_atom(tail, &mut live);
    }

    let mut stmts: Vec<Option<StmtLiveness>> = (0..block.stmts.len()).map(|_| None).collect();
    for (i, stmt) in block.stmts.iter().enumerate().rev() {
        let sl = analyze_stmt(stmt, &mut live);
        stmts[i] = Some(sl);
    }
    BlockLiveness {
        live_in: live,
        live_out: live_out.clone(),
        stmts: stmts.into_iter().map(|s| s.unwrap()).collect(),
    }
}

/// Analyze one statement. `live` enters as the statement's `live_out` and is updated **in place**
/// to its `live_in`. Returns the statement's liveness annotation.
fn analyze_stmt(stmt: &Stmt, live: &mut VarSet) -> StmtLiveness {
    match stmt {
        // Straight-line statements: gather operand uses, kill the (re)bound name, record deaths.
        Stmt::Let { rvalue, .. } | Stmt::Eval { rvalue, .. } => {
            let sub = closure_subs(rvalue);
            let uses = rvalue_uses(rvalue);
            let dies = deaths(&uses, live);
            extend(live, uses);
            StmtLiveness {
                dies_here: dies,
                sub,
            }
        }
        Stmt::Bind { name, value, .. } => {
            let uses = atom_uses(value);
            let dies = deaths(&uses, live);
            // The name is (re)bound: its prior liveness is killed, then the value's uses are live.
            live.remove(name);
            extend(live, uses);
            StmtLiveness {
                dies_here: dies,
                sub: Vec::new(),
            }
        }
        Stmt::Echo { value, .. } => {
            let uses = atom_uses(value);
            let dies = deaths(&uses, live);
            extend(live, uses);
            StmtLiveness {
                dies_here: dies,
                sub: Vec::new(),
            }
        }
        Stmt::Return { value, .. } => {
            // The returned value is *used* (read) here; whether it is moved out rather than dropped
            // is the drop-insertion pass's call. After a return nothing in this frame is live.
            let uses = value.as_ref().map(atom_uses).unwrap_or_default();
            let dies = deaths(&uses, live);
            *live = uses;
            StmtLiveness {
                dies_here: dies,
                sub: Vec::new(),
            }
        }
        // `break`/`continue` use no variables; the loop's structural analysis routes liveness. A
        // `Drop`/`DropVar` is a release, not a use (and the analysis runs before drops are inserted).
        Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::ScopeBegin { .. }
        | Stmt::ScopeEnd { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. } => StmtLiveness {
            dies_here: VarSet::new(),
            sub: Vec::new(),
        },
        Stmt::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            // Both arms see the same `live_out`; the join of their `live_in`s plus the condition's
            // uses is the `If`'s `live_in`.
            let then_l = analyze_block(then_block, live);
            let mut joined = then_l.live_in.clone();
            let mut sub = vec![then_l];
            if let Some(else_block) = else_block {
                let else_l = analyze_block(else_block, live);
                joined.extend(else_l.live_in.iter().cloned());
                sub.push(else_l);
            } else {
                // No else arm: the value flows around the `if`, so it stays live.
                joined.extend(live.iter().cloned());
            }
            let cond_uses = atom_uses(cond);
            // A condition variable that survives into neither arm nor past the `if` dies at the test.
            let dies = deaths(&cond_uses, &joined);
            *live = joined;
            extend(live, cond_uses);
            StmtLiveness {
                dies_here: dies,
                sub,
            }
        }
        Stmt::While { cond, body, .. } => {
            let live_out = live.clone();
            // Fixpoint: `top` is the set live entering the condition test each iteration. The body's
            // live_out is `top` (control returns to the test); the condition's live_out is "enter the
            // body or exit the loop". Monotone increasing from `live_out`, so it converges.
            let mut top = live_out.clone();
            let (cond_l, body_l) = loop {
                let body_l = analyze_block(body, &top);
                let mut cond_out = body_l.live_in.clone();
                cond_out.extend(live_out.iter().cloned());
                let cond_l = analyze_block(cond, &cond_out);
                if cond_l.live_in == top {
                    break (cond_l, body_l);
                }
                top = cond_l.live_in.clone();
            };
            *live = cond_l.live_in.clone();
            StmtLiveness {
                dies_here: VarSet::new(),
                sub: vec![cond_l, body_l],
            }
        }
        Stmt::For {
            pattern,
            iterable,
            body,
            ..
        } => {
            let live_out = live.clone();
            let bound = for_pattern_names(pattern);
            // Fixpoint over `top` = live entering each iteration's body (after the loop variable is
            // freshly bound). The loop variable does not escape, so it is removed from the body's
            // live_in when threading around the back-edge.
            let mut top = live_out.clone();
            let body_l = loop {
                let body_l = analyze_block(body, &top);
                let mut next = body_l.live_in.clone();
                for name in &bound {
                    next.remove(name);
                }
                next.extend(live_out.iter().cloned());
                if next == top {
                    break body_l;
                }
                top = next;
            };
            // The iterable is evaluated once, before the loop.
            let iter_uses = atom_uses(iterable);
            *live = top;
            let dies = deaths(&iter_uses, live);
            extend(live, iter_uses);
            StmtLiveness {
                dies_here: dies,
                sub: vec![body_l],
            }
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            let live_out = live.clone();
            let mut joined = VarSet::new();
            let mut sub = Vec::with_capacity(arms.len());
            for arm in arms {
                let arm_l = analyze_block(&arm.body, &live_out);
                // Names the arm pattern binds are local to the arm and do not escape it.
                let bound = pattern_names(&arm.pattern);
                for name in arm_l.live_in.iter() {
                    if !bound.contains(name) {
                        joined.insert(name.clone());
                    }
                }
                sub.push(arm_l);
            }
            let scrut_uses = atom_uses(scrutinee);
            let dies = deaths(&scrut_uses, &joined);
            *live = joined;
            extend(live, scrut_uses);
            StmtLiveness {
                dies_here: dies,
                sub,
            }
        }
        Stmt::Logical { left, right, .. } => {
            // The right operand is evaluated only when the left does not short-circuit, so it sees
            // the same `live_out` as the whole expression.
            let right_l = analyze_block(right, live);
            let mut entry = right_l.live_in.clone();
            entry.extend(live.iter().cloned());
            let left_uses = atom_uses(left);
            let dies = deaths(&left_uses, &entry);
            *live = entry;
            extend(live, left_uses);
            StmtLiveness {
                dies_here: dies,
                sub: vec![right_l],
            }
        }
        Stmt::Coalesce {
            value, fallback, ..
        } => {
            let fb_l = analyze_block(fallback, live);
            let mut entry = fb_l.live_in.clone();
            entry.extend(live.iter().cloned());
            let value_uses = atom_uses(value);
            let dies = deaths(&value_uses, &entry);
            *live = entry;
            extend(live, value_uses);
            StmtLiveness {
                dies_here: dies,
                sub: vec![fb_l],
            }
        }
        Stmt::Decl(Decl::Fn { name, func, .. }) => {
            // A nested `fn` declaration binds `name` and captures (uses) its free variables. The
            // function body is its own scope, analyzed independently.
            let body_l = analyze_func(func);
            let uses = func_referenced_vars(func);
            let dies = deaths(&uses, live);
            live.remove(name);
            extend(live, uses);
            StmtLiveness {
                dies_here: dies,
                sub: vec![body_l],
            }
        }
        // Type declarations and `use` introduce no source-variable liveness here (a `use`-bound
        // native module is a global, reclaimed at teardown like any other).
        Stmt::Decl(_) => StmtLiveness {
            dies_here: VarSet::new(),
            sub: Vec::new(),
        },
    }
}

/// Analyze a function body as its own scope: parameters are bound at entry, nothing is live after
/// the body (its frame is torn down on return).
fn analyze_func(func: &Func) -> BlockLiveness {
    analyze_block(&func.body, &VarSet::new())
}

/// The closure/`fn` bodies nested directly in an rvalue (for [`StmtLiveness::sub`]). Only
/// [`Rvalue::Closure`] carries a function body.
fn closure_subs(rvalue: &Rvalue) -> Vec<BlockLiveness> {
    match rvalue {
        Rvalue::Closure { func, .. } => vec![analyze_func(func)],
        _ => Vec::new(),
    }
}

/// The subset of `uses` that are **not** live after the statement — i.e. each variable whose last
/// use (on this path) is here.
fn deaths(uses: &VarSet, live_out: &VarSet) -> VarSet {
    uses.difference(live_out).cloned().collect()
}

fn extend(set: &mut VarSet, other: VarSet) {
    set.extend(other);
}

// --- Use collection ----------------------------------------------------------------------------

/// Record a source-variable reference in `live`.
fn use_atom(atom: &Atom, live: &mut VarSet) {
    if let Atom::Var { name, .. } = atom {
        live.insert(name.clone());
    }
}

/// The source variables an atom references.
fn atom_uses(atom: &Atom) -> VarSet {
    let mut s = VarSet::new();
    use_atom(atom, &mut s);
    s
}

/// The source variables an rvalue's operands reference. A [`Rvalue::Closure`]'s captures are
/// counted as uses at the construction site (a superset — see the module-level soundness note).
fn rvalue_uses(rvalue: &Rvalue) -> VarSet {
    if let Rvalue::Closure { func, .. } = rvalue {
        return func_referenced_vars(func);
    }
    let mut s = VarSet::new();
    for_each_rvalue_atom(rvalue, &mut |a| use_atom(a, &mut s));
    s
}

/// Every source variable named anywhere in a function's body and parameter defaults — a sound
/// over-approximation of the names it captures from enclosing scopes.
fn func_referenced_vars(func: &Func) -> VarSet {
    let mut s = VarSet::new();
    collect_block_vars(&func.body, &mut s);
    for default in func.defaults.iter().flatten() {
        collect_block_vars(&default.body, &mut s);
    }
    s
}

fn collect_block_vars(block: &Block, out: &mut VarSet) {
    for stmt in &block.stmts {
        collect_stmt_vars(stmt, out);
    }
    if let Some(tail) = &block.tail {
        use_atom(tail, out);
    }
}

fn collect_stmt_vars(stmt: &Stmt, out: &mut VarSet) {
    match stmt {
        Stmt::Let { rvalue, .. } | Stmt::Eval { rvalue, .. } => collect_rvalue_vars(rvalue, out),
        Stmt::Bind { name, value, .. } => {
            out.insert(name.clone());
            use_atom(value, out);
        }
        Stmt::Echo { value, .. } => use_atom(value, out),
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                use_atom(value, out);
            }
        }
        Stmt::If {
            cond,
            then_block,
            else_block,
            ..
        } => {
            use_atom(cond, out);
            collect_block_vars(then_block, out);
            if let Some(else_block) = else_block {
                collect_block_vars(else_block, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_block_vars(cond, out);
            collect_block_vars(body, out);
        }
        Stmt::For {
            pattern,
            iterable,
            body,
            ..
        } => {
            for name in for_pattern_names(pattern) {
                out.insert(name);
            }
            use_atom(iterable, out);
            collect_block_vars(body, out);
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            use_atom(scrutinee, out);
            for arm in arms {
                for name in pattern_names(&arm.pattern) {
                    out.insert(name);
                }
                collect_block_vars(&arm.body, out);
            }
        }
        Stmt::Logical { left, right, .. } => {
            use_atom(left, out);
            collect_block_vars(right, out);
        }
        Stmt::Coalesce {
            value, fallback, ..
        } => {
            use_atom(value, out);
            collect_block_vars(fallback, out);
        }
        Stmt::Decl(Decl::Fn { name, func, .. }) => {
            out.insert(name.clone());
            collect_block_vars(&func.body, out);
            for default in func.defaults.iter().flatten() {
                collect_block_vars(&default.body, out);
            }
        }
        Stmt::Decl(_)
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::ScopeBegin { .. }
        | Stmt::ScopeEnd { .. }
        | Stmt::Drop(_)
        | Stmt::DropVar { .. } => {}
    }
}

fn collect_rvalue_vars(rvalue: &Rvalue, out: &mut VarSet) {
    if let Rvalue::Closure { func, .. } = rvalue {
        collect_block_vars(&func.body, out);
        for default in func.defaults.iter().flatten() {
            collect_block_vars(&default.body, out);
        }
        return;
    }
    for_each_rvalue_atom(rvalue, &mut |a| use_atom(a, out));
}

/// Visit each [`Atom`] operand of an rvalue (a [`Rvalue::Closure`]'s captured names are handled
/// separately — they are not reachable as operand atoms).
fn for_each_rvalue_atom(rvalue: &Rvalue, f: &mut impl FnMut(&Atom)) {
    match rvalue {
        Rvalue::Use(a) => f(a),
        Rvalue::Unary { operand, .. } => f(operand),
        Rvalue::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::WideInt { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Rvalue::WidthIntMethod { receiver, args, .. } => {
            f(receiver);
            args.iter().for_each(&mut *f);
        }
        Rvalue::Call { callee, args, .. } => {
            f(callee);
            args.iter().for_each(&mut *f);
        }
        Rvalue::Method { receiver, args, .. } | Rvalue::BundleMethod { receiver, args, .. } => {
            f(receiver);
            args.iter().for_each(&mut *f);
        }
        Rvalue::Field { receiver, .. } => f(receiver),
        // A method handle carries only static strings (no operand atoms) — nothing to visit.
        Rvalue::MethodHandle { .. } => {}
        // A bound handle captures its receiver operand.
        Rvalue::BoundHandle { recv, .. } => f(recv),
        Rvalue::SetField {
            receiver, value, ..
        } => {
            f(receiver);
            f(value);
        }
        Rvalue::Index {
            receiver, index, ..
        }
        | Rvalue::IndexField {
            receiver, index, ..
        } => {
            f(receiver);
            f(index);
        }
        Rvalue::List { items, .. } | Rvalue::Tuple { items, .. } => items.iter().for_each(&mut *f),
        Rvalue::PackedListNew { .. } => {}
        Rvalue::PackedListPush { list, value, .. } => {
            f(list);
            f(value);
        }
        Rvalue::TupleIndex { receiver, .. } => f(receiver),
        Rvalue::Map { entries, .. } => {
            for (k, v) in entries {
                f(k);
                f(v);
            }
        }
        Rvalue::Range { start, end, .. } => {
            f(start);
            f(end);
        }
        Rvalue::Object { fields, spread, .. } => {
            if let Some((a, _)) = spread {
                f(a);
            }
            for init in fields {
                f(&init.value);
            }
        }
        Rvalue::Interp { parts, .. } => {
            for part in parts {
                if let noeta_ir::InterpPart::Hole { atom, .. } = part {
                    f(atom);
                }
            }
        }
        Rvalue::Try { operand, .. }
        | Rvalue::As { operand, .. }
        | Rvalue::TypeTest { operand, .. }
        | Rvalue::TypeOf { operand, .. }
        | Rvalue::MaskWidth { operand, .. } => f(operand),
        // The generator desugar's `make_gen(step)` reads its step-closure operand (Track G.1b).
        Rvalue::MakeGen { step, .. } => f(step),
        Rvalue::MakeFuture { thunk, .. } => f(thunk),
        Rvalue::RunFuture { future, .. } => f(future),
        Rvalue::PollFuture { future, .. } => f(future),
        Rvalue::Pending { .. } => {}
        Rvalue::Spawn { future, .. } => f(future),
        Rvalue::SpawnIsolate { callee, args, .. } => {
            f(callee);
            args.iter().for_each(f);
        }
        // `channel::<T>(capacity)` reads its capacity operand (isolates I.1).
        Rvalue::MakeChannel { capacity, .. } => f(capacity),
        // `from_bytes::<T>(blob)` reads its byte operand (P-PACK 4.4).
        Rvalue::FromBytes { blob, .. } => f(blob),
        Rvalue::Invoke {
            recv, name, args, ..
        } => {
            f(recv);
            f(name);
            f(args);
        }
        // `module.func::<T>(args)` reads each argument atom (`json.parse::<T>(s)`).
        Rvalue::TypedModuleCall { args, .. } => args.iter().for_each(&mut *f),
        Rvalue::Closure { .. } | Rvalue::AttributesOf { .. } | Rvalue::RolesOf { .. } => {}
    }
}

// --- Pattern bindings --------------------------------------------------------------------------

fn for_pattern_names(pattern: &ForPattern) -> Vec<String> {
    match pattern {
        ForPattern::Single { name, .. } => vec![name.clone()],
        // Lowering desugars a tuple for-pattern to a `Single` hidden var, so this is unreachable in
        // the IR; kept for totality.
        ForPattern::Tuple { names, .. } => names.iter().map(|(n, _)| n.clone()).collect(),
    }
}

fn pattern_names(pattern: &Pattern) -> VarSet {
    let mut out = VarSet::new();
    collect_pattern_names(pattern, &mut out);
    out
}

fn collect_pattern_names(pattern: &Pattern, out: &mut VarSet) {
    match pattern {
        Pattern::Binding { name, .. } => {
            out.insert(name.clone());
        }
        Pattern::Variant { bindings, .. } => {
            for sub in bindings {
                collect_pattern_names(sub, out);
            }
        }
        Pattern::Tuple { elements, .. } => {
            for sub in elements {
                collect_pattern_names(sub, out);
            }
        }
        Pattern::Wildcard { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. }
        | Pattern::IsType { .. } => {}
    }
}
