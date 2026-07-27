//! Generator / async **state-machine desugar** — an AST → AST rewrite that runs *before*
//! ANF lowering.
//!
//! A `yield`/`.await` body is a suspendable computation the stackless runtime cannot express
//! directly, so [`desugar_state_machine`] flattens it into a control-flow graph of states and
//! renders that as an ordinary closure — a `while true` dispatch loop over a `$state` cell,
//! with each suspend point a state transition. The product is plain [`noeta_ast`] the normal
//! lowering paths then turn into IR; only the final `make_gen`/`make_future` wrapper is a
//! dedicated [`crate::Rvalue`]. This module shares nothing with the ANF [`super::Lowerer`]
//! except the surface AST it produces — it is a distinct pass, kept in its own file.

use std::collections::HashSet;

use noeta_ast::{
    BinaryOp, ClosureBody, Expr, ForPattern as AstForPattern, MatchArm, Param, Stmt as AstStmt,
    StrPart, UnaryOp,
};
use noeta_span::Span;

/// The synthetic dispatch discriminant cell of a desugared generator/async state machine. `$`-prefixed,
/// so it can never collide with a source name (the lexer forbids `$` in identifiers).
const STATE_VAR: &str = "$state";
/// The ignored resume parameter of the step closure (one argument; the poll driver passes unit).
const RESUME_PARAM: &str = "$resume";
/// The async desugar's single-poll primitive: `$poll(future)` → `some(v)` (ready) / `none` (pending).
/// The IR lowering (`Expr::Call` arm) turns this synthetic call into [`Rvalue::PollFuture`].
pub(super) const POLL_FN: &str = "$poll";
/// The async desugar's pending sentinel: a state-machine step returns `$pending` when it suspends at
/// an `.await`. The IR lowering (`Expr::Ident` arm) turns it into [`Rvalue::Pending`].
pub(super) const PENDING_IDENT: &str = "$pending";
/// The A.7 nested-`concurrent` desugar's scope primitives. A `concurrent { }` block inside an async fn
/// is split into state-machine states as `$scN = $scope_begin(); <body>; <join poll-state on
/// $scope_ready($scN)>; $scope_end()` — so the join is a real suspension point (the inner scope's tasks
/// interleave with the outer scope's siblings across polls) instead of an in-place drive-to-completion
/// loop. `$scope_begin()` opens the scope and yields its index; `$scope_ready(idx)` is the join's
/// per-poll readiness test; `$scope_end()` closes the (already-drained) scope. Lexer-forbidden `$` names,
/// so they never collide with source identifiers. Turned into [`Rvalue::ScopeBegin`]/[`Rvalue::ScopeReady`]
/// / [`Stmt::ScopeEnd`] by the IR lowering (`Expr::Call` arm).
pub(super) const SCOPE_BEGIN_FN: &str = "$scope_begin";
pub(super) const SCOPE_READY_FN: &str = "$scope_ready";
pub(super) const SCOPE_END_FN: &str = "$scope_end";

/// Which suspend primitive a state-machine desugar is built for — a generator's `yield` (pull) or an
/// async fn's `.await` (poll). Selects the terminator flavours and the completion protocol: a generator
/// step returns `some(elem)`/`none`(exhausted); an async step returns the raw completion value (so
/// `return e` and `?` work unchanged) or the `$pending` sentinel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SuspendMode {
    Gen,
    Async,
}

/// The AST product of a state-machine desugar ([`Lowerer::lower_generator`] / [`Lowerer::lower_async`]):
/// the hoisted-local prelude and the state-machine step closure. Kept as ordinary AST so the existing
/// lowering paths produce the IR — only the final `make_gen`/`make_future` wrapper is a dedicated rvalue.
pub(super) struct StateMachineDesugar {
    pub(super) prelude: Vec<AstStmt>,
    pub(super) step: Expr,
}

/// Build the [`StateMachineDesugar`] for a generator body (Track G.1b straight-line + G.2 control flow).
///
/// The body is flattened into a **control-flow graph of states** — a stackless state machine. Each
/// `yield` is a suspend point; `if`/`while` carrying a `yield` (and any `break`/`continue` reaching an
/// enclosing flattened loop) become state transitions. The step closure is a `while true` dispatch
/// loop over `$state`: state *k* runs its statements and then either **returns** (a `yield` → `some`,
/// or exhaustion → `none`) or **jumps** (`$state = j; continue`) to re-enter the loop at another
/// state. A construct with no `yield` and no escaping control flow is emitted **verbatim** (a `match`,
/// a self-contained `for`/`while`), so it runs whole within one step. A `yield` inside a `for` is
/// rejected by the checker (E0039) and never reaches here through a clean program.
///
/// Every top-level / flattened-level local is conservatively hoisted to a captured `mut` cell (the
/// prelude declares it; its in-body `let x = …` is rewritten to a bare assignment that reassigns the
/// cell), so a value computed before a `yield` survives into the next state. G.3 narrows this with
/// real liveness. All synthesized nodes carry the generator's `span`; reused source nodes keep theirs.
pub(super) fn desugar_state_machine(
    body: &[AstStmt],
    span: Span,
    stream_sites: &HashSet<Span>,
    mode: SuspendMode,
    module_globals: &HashSet<String>,
    params: &[String],
) -> StateMachineDesugar {
    // A bare reassignment of a module global that this function does not shadow (no param, no fresh
    // `mut` local of the same name) is a global store, never a capturable local — so it must not be
    // hoisted into a state-machine cell. Build that exclusion set once: a global name minus any that
    // a param shadows. (An in-body `mut g` re-shadow is handled by the `declaring` check at
    // hoist-decision time, since such a name is eligible rather than a bare-assign candidate.)
    let param_set: HashSet<&str> = params.iter().map(String::as_str).collect();
    let global_stores: HashSet<String> = module_globals
        .iter()
        .filter(|g| !param_set.contains(g.as_str()))
        .cloned()
        .collect();
    let mut flat = Flattener {
        blocks: Vec::new(),
        binds: Vec::new(),
        declaring: HashSet::new(),
        disqualified: HashSet::new(),
        stream_sites,
        mode,
        tmp: 0,
        global_stores,
    };
    // Track A.6: hoist mid-expression awaits to statement position before flattening (async only —
    // a generator has no awaits). After this the flattener only ever sees head/hoisted-binding awaits.
    let hoisted_body;
    let body = if mode == SuspendMode::Async {
        hoisted_body = hoist_await_body(body);
        &hoisted_body[..]
    } else {
        body
    };
    let entry = flat.new_block();
    let exit = flat.lower_seq(body, entry, None);
    // Falling off the end: a generator is exhausted (`Done` → `return none`); an async fn completes
    // with unit (`Complete(None)` → `return` = unit, wrapped into `some` at the future value level).
    flat.blocks[exit].term = match mode {
        SuspendMode::Gen => Term::Done,
        SuspendMode::Async => Term::Complete(None),
    };

    // Liveness (G.3): a candidate name that is eligible (a fresh `mut`/for-var declaration) and
    // referenced within a single state stays a block-local; everything else is hoisted to a captured
    // cell. Then rewrite every hoisted binding to a bare assignment against its prelude cell.
    let hoisted = flat.compute_hoisted();
    let hoisted_set: HashSet<&str> = hoisted.iter().map(String::as_str).collect();
    flat.rewrite_hoisted(&hoisted_set);

    // Render each state as `if $state == idx { <stmts> <terminator> }`, wrapped in a `while true`
    // dispatch loop. The trailing statement fires once `$state` reaches the terminal sentinel: a
    // generator returns `none` (exhausted); an async step returns `$pending` (unreachable — a completed
    // future is never re-polled — but fail-loud rather than silently mis-complete).
    let terminal = flat.blocks.len();
    let mut chain: Vec<AstStmt> = Vec::with_capacity(flat.blocks.len() + 1);
    for (idx, block) in flat.blocks.iter().enumerate() {
        let mut stmts = block.stmts.clone();
        block.term.render(&mut stmts, idx, terminal, span);
        chain.push(state_arm(idx as i64, stmts, span));
    }
    chain.push(AstStmt::Return {
        value: Some(match mode {
            SuspendMode::Gen => none_expr(span),
            SuspendMode::Async => pending_expr(span),
        }),
        span,
    });
    let dispatch = AstStmt::While {
        cond: Expr::Bool { value: true, span },
        body: chain,
        span,
    };

    let step = Expr::Closure {
        params: vec![Param {
            attrs: Vec::new(),
            name: RESUME_PARAM.to_string(),
            name_span: span,
            ty: None,
            default: None,
            span,
        }],
        ret: None,
        body: ClosureBody::Block(vec![dispatch]),
        span,
    };

    let mut prelude = vec![AstStmt::Binding {
        mut_decl: true,
        name: STATE_VAR.to_string(),
        name_span: span,
        ty: None,
        value: Expr::Int { value: 0, span },
        span,
    }];
    for name in &hoisted {
        prelude.push(AstStmt::Binding {
            mut_decl: true,
            name: name.clone(),
            name_span: span,
            ty: None,
            value: none_expr(span),
            span,
        });
    }

    StateMachineDesugar { prelude, step }
}

/// A state's terminator in the flattened CFG — how control leaves the state.
enum Term {
    /// Jump to another state and re-enter the dispatch loop (`$state = k; continue`).
    Goto(usize),
    /// Generator suspend: `$state = k; return some(<expr>)` — the next step resumes at state `k`.
    Yield(Expr, usize),
    /// Two-way branch on a condition (`if cond { $state = t } else { $state = e } continue`).
    Branch(Expr, usize, usize),
    /// End of iteration (generator): `$state = <terminal>; return none`.
    Done,
    /// Async completion: `$state = <terminal>; return <value>` — the raw completion value (or unit for a
    /// bare fall-off/`return;`), which `make_future` presents as a resolved `Future`. Left un-`some`-wrapped
    /// so `return e` and `?`'s injected error-return both flow through as the completion value.
    Complete(Option<Expr>),
    /// Async await suspend point (poll-state). `future` is the hoisted cell holding the awaited future;
    /// `result` is the hoisted cell the ready value binds to; `next` is the state to resume at once
    /// ready. Renders (at its own state `idx`): poll the future once; if `some(v)`, bind `result = v`,
    /// advance to `next`, and continue; if `none`, stay at `idx` and `return $pending` — so the next poll
    /// re-enters here and re-polls the same (state-preserving) future.
    AwaitPoll {
        future: String,
        result: String,
        next: usize,
    },
    /// Nested-`concurrent` join suspend point (poll-state, Track A.7). `scope` is the hoisted cell
    /// holding the index `$scope_begin()` returned for this block's scope; `next` is the state to resume
    /// at once every task in that scope has completed. Renders (at its own state `idx`): if
    /// `$scope_ready(scope)`, advance to `next` and continue; otherwise stay at `idx` and `return
    /// $pending` — so the outer scheduler round-robins this scope's tasks with the enclosing scope's
    /// siblings across polls, and the next poll re-enters here to re-test the (state-preserving) scope.
    JoinPoll { scope: String, next: usize },
}

impl Term {
    /// Emit the statements that realize this terminator, appended after a state's own statements. `idx`
    /// is this state's own index (an `AwaitPoll` re-enters itself while pending).
    fn render(&self, out: &mut Vec<AstStmt>, idx: usize, terminal: usize, span: Span) {
        match self {
            Term::Goto(k) => {
                out.push(assign_state(*k as i64, span));
                out.push(AstStmt::Continue { span });
            }
            Term::Yield(value, k) => {
                out.push(assign_state(*k as i64, span));
                out.push(AstStmt::Return {
                    value: Some(call_some(value.clone(), span)),
                    span,
                });
            }
            Term::Branch(cond, then_state, else_state) => {
                out.push(AstStmt::If {
                    cond: cond.clone(),
                    then_body: vec![assign_state(*then_state as i64, span)],
                    else_body: Some(vec![assign_state(*else_state as i64, span)]),
                    span,
                });
                out.push(AstStmt::Continue { span });
            }
            Term::Done => {
                out.push(assign_state(terminal as i64, span));
                out.push(AstStmt::Return {
                    value: Some(none_expr(span)),
                    span,
                });
            }
            Term::Complete(value) => {
                out.push(assign_state(terminal as i64, span));
                out.push(AstStmt::Return {
                    value: value.clone(),
                    span,
                });
            }
            Term::AwaitPoll {
                future,
                result,
                next,
            } => {
                // $pollN = $poll($future)   (block-local to this state)
                let poll_var = format!("$poll{idx}");
                out.push(bare_assign_expr(
                    &poll_var,
                    poll_call(ident(future, span), span),
                    span,
                ));
                // if is_some($pollN) { $result = $pollN ?? none; $state = next; continue }
                out.push(AstStmt::If {
                    cond: is_some_test(ident(&poll_var, span), span),
                    then_body: vec![
                        bare_assign_expr(
                            result,
                            Expr::Coalesce {
                                value: Box::new(ident(&poll_var, span)),
                                fallback: Box::new(none_expr(span)),
                                span,
                            },
                            span,
                        ),
                        assign_state(*next as i64, span),
                        AstStmt::Continue { span },
                    ],
                    else_body: None,
                    span,
                });
                // pending: stay here and yield control up as `$pending`.
                out.push(assign_state(idx as i64, span));
                out.push(AstStmt::Return {
                    value: Some(pending_expr(span)),
                    span,
                });
            }
            Term::JoinPoll { scope, next } => {
                // if $scope_ready($scope) { $state = next; continue }
                out.push(AstStmt::If {
                    cond: scope_ready_call(ident(scope, span), span),
                    then_body: vec![assign_state(*next as i64, span), AstStmt::Continue { span }],
                    else_body: None,
                    span,
                });
                // pending: stay here and yield control up as `$pending`, so the scheduler advances the
                // clock and re-polls this scope's tasks (and the outer scope's siblings) before we retry.
                out.push(assign_state(idx as i64, span));
                out.push(AstStmt::Return {
                    value: Some(pending_expr(span)),
                    span,
                });
            }
        }
    }
}

/// One state of the flattened generator: the statements it runs and how it leaves.
struct BlockBuf {
    stmts: Vec<AstStmt>,
    term: Term,
}

/// Flattens a generator body into a CFG of [`BlockBuf`] states (Track G.2). See
/// [`desugar_state_machine`].
struct Flattener<'a> {
    blocks: Vec<BlockBuf>,
    /// Every flattened-level binding name, deduped in first-seen order — the candidates for hoisting
    /// into captured cells. Which of these actually become cells is decided by liveness (G.3): a name
    /// referenced in more than one state must persist across a suspend/jump, so it is hoisted; a
    /// genuinely-fresh local (`mut x`/for-var) used within a single state stays a block-local.
    binds: Vec<String>,
    /// Candidate names that *declare* a fresh local (a `mut x = …` scalar or a single `for`-var) — the
    /// only names eligible to stay a block-local, since a block-local and a cell both shadow any outer
    /// binding identically, making the optimization behavior-preserving.
    declaring: HashSet<String>,
    /// Candidate names disqualified from the block-local optimization — kept on the always-hoist path
    /// (the pre-G.3 behavior) to avoid any semantic change. A name is disqualified when its first
    /// binding is a bare `x = …` (which may reassign an outer, so hoisting must keep shadowing it), or
    /// when it is a destructure/tuple target or a synthetic cursor (`$for`/`$next`).
    disqualified: HashSet<String>,
    /// The `for`-loop spans whose source is statically an `Iterator<T>` (Track I.2, computed by the
    /// checker). A `for` across a `yield` (G.4) uses its source directly when it is already an
    /// iterator, or calls `.iter()` on it when it is a collection.
    stream_sites: &'a HashSet<Span>,
    /// Which suspend primitive this machine is built for (generator `yield` vs async `.await`) — selects
    /// terminator flavours and the completion protocol. See [`SuspendMode`].
    mode: SuspendMode,
    /// Counter for the synthetic `$for`/`$next`/`$fut`/`$aw` cell names the flattener introduces.
    tmp: usize,
    /// Module-global names this function reassigns as global stores (no shadowing param). A bare
    /// `g = …`/`g.f = …` against such a name is a store to the global — not a fresh local — so it is
    /// excluded from cell-hoisting: the desugared body keeps a bare reassignment the compiler resolves
    /// to `StoreGlobal`, exactly as a synchronous function does. Without this a global reassignment
    /// would be mis-hoisted into a captured cell initialized to `none`, shadowing the real global.
    global_stores: HashSet<String>,
}

impl Flattener<'_> {
    /// A fresh index for a synthetic cell name (`$for{n}`, `$next{n}`, `$fut{n}`, `$aw{n}`).
    fn fresh(&mut self) -> usize {
        let n = self.tmp;
        self.tmp += 1;
        n
    }

    /// The terminator for "the machine ends here" — generator exhaustion (`Done`) or async completion
    /// with unit (`Complete(None)`). Used where control leaves with no explicit value (a `break`/
    /// `continue` outside any loop — itself an E0024 the checker rejects, so unreachable in practice).
    fn end_term(&self) -> Term {
        match self.mode {
            SuspendMode::Gen => Term::Done,
            SuspendMode::Async => Term::Complete(None),
        }
    }

    /// Allocate a fresh empty state (terminator filled in by the caller; `Done` is a safe default for
    /// an unreachable continuation).
    fn new_block(&mut self) -> usize {
        let idx = self.blocks.len();
        self.blocks.push(BlockBuf {
            stmts: Vec::new(),
            term: Term::Done,
        });
        idx
    }

    /// Record a flattened-level binding of `name`. `scalar` is false for destructure/tuple targets and
    /// synthetic cursors (always hoisted); `mutable` marks a `mut`-declared scalar. A scalar's *first*
    /// binding decides eligibility: a `mut` declaration is `declaring` (block-local-eligible), a bare
    /// assignment is `disqualified` (may reassign an outer — keep hoisting to preserve shadowing).
    fn record(&mut self, name: &str, mutable: bool, scalar: bool) {
        let first = !self.binds.iter().any(|n| n == name);
        if first {
            self.binds.push(name.to_string());
        }
        if !scalar {
            self.disqualified.insert(name.to_string());
        } else if first {
            if mutable {
                self.declaring.insert(name.to_string());
            } else {
                self.disqualified.insert(name.to_string());
            }
        }
    }

    /// Decide which candidate names become captured cells (G.3 liveness). A name is hoisted unless it
    /// is *eligible* (a fresh `mut`/for-var declaration, not disqualified) **and** referenced within a
    /// single state — in which case it stays a block-local, re-declared on each entry to its state.
    /// Preserves `binds`' first-seen order so the prelude is deterministic (both backends agree).
    fn compute_hoisted(&self) -> Vec<String> {
        self.binds
            .iter()
            .filter(|name| {
                // A bare reassignment of a module global (with no shadowing `mut` local here) is a
                // global store, not a hoistable local — keep it a bare reassignment so the compiler
                // resolves it to `StoreGlobal`, exactly as in a synchronous function.
                !self.is_global_store(name)
            })
            .filter(|name| {
                let eligible = self.declaring.contains(*name) && !self.disqualified.contains(*name);
                !eligible || self.ref_block_count(name) > 1
            })
            .cloned()
            .collect()
    }

    /// Whether `name` denotes a module-global store here: it is a reassigned global with no `mut`
    /// declaration in this body shadowing it. Such a name must never become a state-machine cell —
    /// its reads/writes go through the global, shared across suspensions like any other global.
    fn is_global_store(&self, name: &str) -> bool {
        self.global_stores.contains(name) && !self.declaring.contains(name)
    }

    /// The number of distinct states that reference `name` (read or written). A name referenced in
    /// more than one state must persist across a suspend/jump and so must be a cell.
    fn ref_block_count(&self, name: &str) -> usize {
        self.blocks
            .iter()
            .filter(|block| block_mentions(block, name))
            .count()
    }

    /// Rewrite every hoisted name's flattened-level `Binding`/`Destructure` to a bare assignment
    /// (`mut_decl: false`), so it reassigns the prelude cell rather than shadowing it. Non-hoisted
    /// (block-local) bindings keep their declaration form untouched.
    fn rewrite_hoisted(&mut self, hoisted: &HashSet<&str>) {
        for block in &mut self.blocks {
            for stmt in &mut block.stmts {
                match stmt {
                    AstStmt::Binding { mut_decl, name, .. } if hoisted.contains(name.as_str()) => {
                        *mut_decl = false
                    }
                    AstStmt::Destructure {
                        mut_decl, targets, ..
                    } if targets.iter().any(|(n, _)| hoisted.contains(n.as_str())) => {
                        *mut_decl = false
                    }
                    _ => {}
                }
            }
        }
    }

    /// Emit the binding(s) of a flattened `for` loop's pattern into state `block`, from the
    /// already-unwrapped `element` expression: a single name binds directly (a fresh declaration —
    /// block-local-eligible); a tuple pattern destructures positionally (always hoisted). A bound name
    /// used across a `yield` in the loop body is hoisted to a cell by liveness; one used within a
    /// single state stays a block-local (the loop re-binds it each iteration).
    fn bind_for_pattern(
        &mut self,
        block: usize,
        pattern: &AstForPattern,
        element: Expr,
        span: Span,
    ) {
        match pattern {
            AstForPattern::Single { name, name_span } => {
                self.record(name, true, true);
                self.blocks[block]
                    .stmts
                    .push(decl_expr(name, element, *name_span));
            }
            AstForPattern::Tuple { names, .. } => {
                for (name, _) in names {
                    self.record(name, true, false);
                }
                self.blocks[block].stmts.push(AstStmt::Destructure {
                    mut_decl: false,
                    targets: names.clone(),
                    value: element,
                    span,
                });
            }
        }
    }

    /// Lower a statement sequence starting at state `entry`, returning the state where control
    /// continues afterward. `loop_ctx` is `(continue_target, break_target)` of the innermost
    /// enclosing **flattened** loop (`None` at the top level), routing `continue`/`break`.
    fn lower_seq(
        &mut self,
        stmts: &[AstStmt],
        entry: usize,
        loop_ctx: Option<(usize, usize)>,
    ) -> usize {
        let mut cur = entry;
        for stmt in stmts {
            cur = self.lower_one(stmt, cur, loop_ctx);
        }
        cur
    }

    /// Lower one statement into the CFG at state `cur`, returning the state control continues at.
    fn lower_one(&mut self, stmt: &AstStmt, cur: usize, loop_ctx: Option<(usize, usize)>) -> usize {
        let mode = self.mode;
        // Async (Track A.3): a statement whose value is an `.await` (optionally under `?`) is a suspend
        // point. Evaluate the awaited future into a hoisted cell, add a poll-state that parks on
        // `Pending`, then rebuild the statement with the `.await` replaced by the ready value and lower
        // that (so an async `return e.await` still becomes a `Complete`, a `?` still propagates, etc.).
        // Awaits in non-statement position are rejected by the checker (E0040), so they never reach here
        // through a clean program.
        if mode == SuspendMode::Async
            && let Some(future) = stmt_await_future(stmt)
        {
            let fspan = future.span();
            let aw = format!("$aw{}", self.fresh());
            let futc = format!("$fut{}", self.fresh());
            self.record(&futc, true, false);
            self.record(&aw, true, false);
            self.blocks[cur]
                .stmts
                .push(bare_assign_expr(&futc, future, fspan));
            let poll = self.new_block();
            self.blocks[cur].term = Term::Goto(poll);
            let next = self.new_block();
            self.blocks[poll].term = Term::AwaitPoll {
                future: futc,
                result: aw.clone(),
                next,
            };
            let rebuilt = rebuild_stmt_await(stmt, &aw);
            return self.lower_one(&rebuilt, next, loop_ctx);
        }
        match stmt {
            AstStmt::Yield { value, .. } => {
                let next = self.new_block();
                self.blocks[cur].term = Term::Yield(value.clone(), next);
                next
            }
            // Generator: bare `return;` ends iteration; `return e;` is checker-rejected (E0039). Async:
            // `return e` completes the future with `e` (raw — `make_future` presents it as resolved).
            AstStmt::Return { value, .. } => {
                self.blocks[cur].term = match mode {
                    SuspendMode::Gen => Term::Done,
                    SuspendMode::Async => Term::Complete(value.clone()),
                };
                self.new_block()
            }
            AstStmt::Break { .. } => {
                // `break` to the enclosing flattened loop's exit (or end the machine if there is none —
                // a `break` outside a loop is already a checker error E0024, so this never runs).
                self.blocks[cur].term = match loop_ctx {
                    Some((_, brk)) => Term::Goto(brk),
                    None => self.end_term(),
                };
                self.new_block()
            }
            AstStmt::Continue { .. } => {
                self.blocks[cur].term = match loop_ctx {
                    Some((cont, _)) => Term::Goto(cont),
                    None => self.end_term(),
                };
                self.new_block()
            }
            // A nested `fn` declaration binds a name in this scope exactly as a local does, so it is
            // a hoist candidate for the same reason: the declaration executes in ONE state, and
            // every later state must still see the name. Without this the name never entered
            // `binds`, so it was never a cell and died at the first suspend — `fn helper()` declared
            // above a `yield` was gone below it (E0005), while the equivalent
            // `helper = fn() => …` worked, because that spelling *is* a `Binding`.
            //
            // It is rewritten into the binding form `name = fn(params) { body }` — a `Binding` of a
            // closure — because that spelling already works across suspends today, for exactly the
            // reason the declaration form does not: a `Binding` is a hoist candidate, so liveness
            // makes it a prelude cell and `rewrite_hoisted` turns it into an assignment against
            // that cell. Registering the name without rewriting is not enough — the declaration
            // would still shadow the cell inside its own state and leave it unset.
            //
            // Sound because a named fn is SEALED: its body sees its parameters and statics, never
            // the enclosing scope implicitly. A capture-free one therefore has nothing an
            // auto-capturing closure could newly bind, so no program that compiles today changes
            // meaning. `mut_decl: false` matches the hand-written spelling and lands the name on
            // the always-hoist path.
            //
            // Anything the closure form cannot faithfully carry stays on the old path (declared
            // in-state, unusable after a suspend as before, rather than silently mis-lowered): a
            // capturing fn (`use (x)`, whose upvalues are themselves cells here), an `async fn`, a
            // generic fn, and a nested generator.
            AstStmt::Fn(decl)
                if decl.captures.is_empty()
                    && !decl.is_async
                    && decl.type_params.is_empty()
                    && !body_has_yield(&decl.body) =>
            {
                self.record(&decl.name, false, true);
                self.blocks[cur].stmts.push(AstStmt::Binding {
                    mut_decl: false,
                    name: decl.name.clone(),
                    name_span: decl.name_span,
                    ty: None,
                    value: Expr::Closure {
                        params: decl.params.clone(),
                        ret: decl.ret.clone(),
                        body: ClosureBody::Block(decl.body.clone()),
                        span: decl.span,
                    },
                    span: decl.span,
                });
                cur
            }
            // A flattened-level local: record it as a hoist candidate, preserving its original
            // declaration form (`mut x` vs bare `x =`). Liveness later decides whether it becomes a
            // captured cell (used across a state) or stays a block-local (used within one state); a
            // hoisted binding is then rewritten to a bare assignment against the prelude cell.
            AstStmt::Binding {
                mut_decl,
                name,
                value,
                span,
                ..
            } => {
                self.record(name, *mut_decl, true);
                self.blocks[cur].stmts.push(AstStmt::Binding {
                    mut_decl: *mut_decl,
                    name: name.clone(),
                    name_span: *span,
                    ty: None,
                    value: value.clone(),
                    span: *span,
                });
                cur
            }
            AstStmt::Destructure {
                targets,
                value,
                span,
                ..
            } => {
                for (name, _) in targets {
                    self.record(name, true, false);
                }
                self.blocks[cur].stmts.push(AstStmt::Destructure {
                    mut_decl: false,
                    targets: targets.clone(),
                    value: value.clone(),
                    span: *span,
                });
                cur
            }
            AstStmt::If {
                cond,
                then_body,
                else_body,
                ..
            } if needs_flatten_if(then_body, else_body.as_deref(), mode) => {
                let then_entry = self.new_block();
                let else_entry = self.new_block();
                let join = self.new_block();
                self.blocks[cur].term = Term::Branch(cond.clone(), then_entry, else_entry);
                let then_exit = self.lower_seq(then_body, then_entry, loop_ctx);
                self.blocks[then_exit].term = Term::Goto(join);
                let else_exit =
                    self.lower_seq(else_body.as_deref().unwrap_or(&[]), else_entry, loop_ctx);
                self.blocks[else_exit].term = Term::Goto(join);
                join
            }
            AstStmt::While { cond, body, .. } if has_suspend(body, mode) => {
                let head = self.new_block();
                self.blocks[cur].term = Term::Goto(head);
                let body_entry = self.new_block();
                let after = self.new_block();
                self.blocks[head].term = Term::Branch(cond.clone(), body_entry, after);
                // The loop body's `break`/`continue` route to `after`/`head`; the body falls back to
                // `head` (the back-edge).
                let body_exit = self.lower_seq(body, body_entry, Some((head, after)));
                self.blocks[body_exit].term = Term::Goto(head);
                after
            }
            // A `for` whose body suspends (Track G.4) lowers to the iterator protocol so the source's
            // cursor becomes part of the machine state: a hoisted cell holds the iterator, and the loop
            // becomes a flattened `while` over `.next()`. `head` fetches the next element and branches
            // on `some`/`none`; the body binds the loop variable(s) from the unwrapped element. A `for`
            // with no `yield` is emitted verbatim (the catch-all below), running whole within one state.
            AstStmt::For {
                pattern,
                iterable,
                body,
                span,
            } if has_suspend(body, mode) => {
                let cursor = format!("$for{}", self.fresh());
                let next = format!("$next{}", self.fresh());
                // The cursor and next-element cells are always hoisted (they span the loop's states).
                self.record(&cursor, true, false);
                self.record(&next, true, false);
                // Initialize the cursor cell: a collection needs `.iter()`; a source that is already an
                // `Iterator<T>` (a stream site) is used directly (calling `.iter()` on it is not valid).
                let source = if self.stream_sites.contains(span) {
                    iterable.clone()
                } else {
                    method_call(iterable.clone(), "iter", *span)
                };
                self.blocks[cur]
                    .stmts
                    .push(bare_assign_expr(&cursor, source, *span));
                let head = self.new_block();
                self.blocks[cur].term = Term::Goto(head);
                let body_entry = self.new_block();
                let after = self.new_block();
                // head: `$next = $cursor.next()`; branch on whether an element remains.
                self.blocks[head].stmts.push(bare_assign_expr(
                    &next,
                    method_call(ident(&cursor, *span), "next", *span),
                    *span,
                ));
                self.blocks[head].term =
                    Term::Branch(is_some_test(ident(&next, *span), *span), body_entry, after);
                // body_entry: bind the loop variable(s) from the unwrapped element (the `none` arm of
                // the `??` is unreachable — we only branch here on `some`), then run the body.
                let element = Expr::Coalesce {
                    value: Box::new(ident(&next, *span)),
                    fallback: Box::new(none_expr(*span)),
                    span: *span,
                };
                self.bind_for_pattern(body_entry, pattern, element, *span);
                let body_exit = self.lower_seq(body, body_entry, Some((head, after)));
                self.blocks[body_exit].term = Term::Goto(head);
                after
            }
            // A `concurrent { }` block inside an async fn (Track A.7): split it across states so its
            // join becomes a genuine suspension point. `$scope_begin()` opens the scope (its index
            // bound to a hoisted cell); the body's `spawn`s land in it and the body's own `.await`s
            // become poll-states; a `JoinPoll` state then suspends until every task in the scope has
            // completed, so the inner scope's tasks interleave with the outer scope's siblings across
            // polls; finally `$scope_end()` closes the drained scope. Contrast the synchronous path
            // (`lower.rs`'s `AstStmt::Concurrent` for the top level / a non-async fn), which drives
            // the scope to completion in place — correct there, as nothing outer is left to interleave.
            AstStmt::Concurrent { body, span } if mode == SuspendMode::Async => {
                let scope_cell = format!("$scope{}", self.fresh());
                self.record(&scope_cell, true, false);
                self.blocks[cur].stmts.push(bare_assign_expr(
                    &scope_cell,
                    scope_begin_call(*span),
                    *span,
                ));
                let body_exit = self.lower_seq(body, cur, loop_ctx);
                let join = self.new_block();
                self.blocks[body_exit].term = Term::Goto(join);
                let next = self.new_block();
                self.blocks[join].term = Term::JoinPoll {
                    scope: scope_cell.clone(),
                    next,
                };
                self.blocks[next]
                    .stmts
                    .push(scope_end_stmt(&scope_cell, *span));
                next
            }
            // No `yield` and no escaping control flow: emit verbatim — it runs whole within this state
            // (a `match`, a self-contained `for`/`while`/`if`). Its own `break`/`continue` target
            // itself, so it needs no state interaction.
            other => {
                self.blocks[cur].stmts.push(other.clone());
                cur
            }
        }
    }
}

/// Whether an `if` must be flattened into the state machine: it carries a suspend point (a `yield` in a
/// generator, an `.await` in an async fn), or a `break`/`continue` that **escapes** the `if` (targeting
/// an enclosing flattened loop) — emitting it verbatim would make that jump a real `break`/`continue`
/// of the dispatch loop.
fn needs_flatten_if(
    then_body: &[AstStmt],
    else_body: Option<&[AstStmt]>,
    mode: SuspendMode,
) -> bool {
    let suspends = has_suspend(then_body, mode) || else_body.is_some_and(|b| has_suspend(b, mode));
    let escapes =
        body_has_escaping_ctrl(then_body) || else_body.is_some_and(body_has_escaping_ctrl);
    suspends || escapes
}

/// Whether a statement sequence contains a suspend point at this flattening level — a `yield`
/// (generator) or an `.await` (async fn). Descends control-flow bodies but not nested callables (a
/// closure resets both colorings), mirroring the checker's detection so the flattener desugars exactly
/// the bodies the checker treats as generators/async.
fn has_suspend(stmts: &[AstStmt], mode: SuspendMode) -> bool {
    match mode {
        SuspendMode::Gen => body_has_yield(stmts),
        SuspendMode::Async => body_has_await(stmts),
    }
}

/// Whether a statement sequence contains an `.await` at this callable level (Track A.3). Built on
/// [`Expr::has_await`], which already stops at closure boundaries.
fn body_has_await(stmts: &[AstStmt]) -> bool {
    stmts.iter().any(stmt_has_await)
}

fn stmt_has_await(stmt: &AstStmt) -> bool {
    match stmt {
        AstStmt::Echo { value, .. } | AstStmt::Expr { expr: value, .. } => value.has_await(),
        AstStmt::Binding { value, .. } | AstStmt::Destructure { value, .. } => value.has_await(),
        AstStmt::Return { value, .. } => value.as_ref().is_some_and(Expr::has_await),
        AstStmt::Yield { value, .. } => value.has_await(),
        AstStmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            cond.has_await()
                || body_has_await(then_body)
                || else_body.as_deref().is_some_and(body_has_await)
        }
        AstStmt::While { cond, body, .. } => cond.has_await() || body_has_await(body),
        AstStmt::For { iterable, body, .. } => iterable.has_await() || body_has_await(body),
        // A `concurrent { }` block is itself a suspend point (Track A.7): its join lowers to a poll-state
        // (see the `AstStmt::Concurrent` arm of `lower_one`), so an enclosing `if`/`while` must flatten
        // for the split to take effect — regardless of whether the body contains explicit `.await`s.
        AstStmt::Concurrent { .. } => true,
        _ => false,
    }
}

/// The awaited future of a **statement-position** `.await` (Track A.3): the operand of an `.await` that
/// is the whole value of a `Binding`/`Expr`/`Return`/`Echo`, optionally under one `?`. `None` for any
/// other statement (including one whose `.await` is buried in a sub-expression — the checker rejects
/// those with E0040, so they never reach the flattener through a clean program).
fn stmt_await_future(stmt: &AstStmt) -> Option<Expr> {
    let value = stmt_value(stmt)?;
    value_await_future(value)
}

/// Rebuild a statement-position-await statement with the `.await` replaced by a reference to `aw` (the
/// hoisted cell the poll-state binds the ready value into). The `?` wrapper, if any, is preserved, so
/// `x = e.await?` becomes `x = aw?` (still propagating on error) and `return e.await` becomes
/// `return aw` (still an async `Complete`). Assumes [`stmt_await_future`] returned `Some` for `stmt`.
fn rebuild_stmt_await(stmt: &AstStmt, aw: &str) -> AstStmt {
    let rebuilt = |v: &Expr| value_replace_await(v, aw);
    match stmt {
        AstStmt::Binding {
            mut_decl,
            name,
            name_span,
            value,
            span,
            ty,
        } => AstStmt::Binding {
            mut_decl: *mut_decl,
            name: name.clone(),
            name_span: *name_span,
            ty: ty.clone(),
            value: rebuilt(value),
            span: *span,
        },
        AstStmt::Expr { expr, span } => AstStmt::Expr {
            expr: rebuilt(expr),
            span: *span,
        },
        AstStmt::Return { value, span } => AstStmt::Return {
            value: value.as_ref().map(rebuilt),
            span: *span,
        },
        AstStmt::Echo { value, span } => AstStmt::Echo {
            value: rebuilt(value),
            span: *span,
        },
        // `stmt_await_future` only returns `Some` for the four kinds above.
        other => other.clone(),
    }
}

/// The value expression of a statement that can carry a statement-position `.await`, if any.
fn stmt_value(stmt: &AstStmt) -> Option<&Expr> {
    match stmt {
        AstStmt::Binding { value, .. }
        | AstStmt::Expr { expr: value, .. }
        | AstStmt::Echo { value, .. } => Some(value),
        AstStmt::Return { value, .. } => value.as_ref(),
        _ => None,
    }
}

/// The operand of an `.await` at the head of `value` (`e.await` → `e`), or under a single `?`
/// (`e.await?` → `e`); `None` otherwise.
fn value_await_future(value: &Expr) -> Option<Expr> {
    match value {
        Expr::Await { expr, .. } => Some((**expr).clone()),
        Expr::Try { expr, .. } => match expr.as_ref() {
            Expr::Await { expr, .. } => Some((**expr).clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `value` with a head `.await` replaced by a reference to `aw`, preserving a `?` wrapper. Assumes
/// [`value_await_future`] returned `Some` for `value`.
fn value_replace_await(value: &Expr, aw: &str) -> Expr {
    match value {
        Expr::Await { span, .. } => ident(aw, *span),
        Expr::Try { expr, span } if matches!(expr.as_ref(), Expr::Await { .. }) => Expr::Try {
            expr: Box::new(ident(aw, *span)),
            span: *span,
        },
        other => other.clone(),
    }
}

/// Track A.6 — the mid-expression-`.await` pre-pass. Rewrites an async fn body so that every `.await`
/// in an **unconditionally-evaluated** sub-expression position is hoisted to a preceding
/// statement-position `$hwN = <sub>.await`, left-to-right (evaluation order), and replaced by a
/// reference to `$hwN`. After this pass, every remaining `.await` is either such a hoisted binding or
/// the head await of a value-bearing statement — both of which the flattener already turns into
/// poll-states. The checker (E0040) has already rejected awaits in conditionally-evaluated positions
/// (short-circuit `&&`/`||`/`??` operands, `match`/`if…then…else` arm bodies) and in condition/loop
/// heads, so those never reach here; this pass mirrors that by not recursing into them.
fn hoist_await_body(stmts: &[AstStmt]) -> Vec<AstStmt> {
    let mut ctr = 0u32;
    hoist_await_body_ctr(stmts, &mut ctr)
}

/// [`hoist_await_body`] threading an existing synthetic-name counter, so a body hoisted *inside* an
/// ongoing rewrite (a `??`/`match` arm-body desugar) keeps its `$hw`/`$sc`/… names globally unique
/// rather than restarting at `0` and colliding with the outer pass's cells.
fn hoist_await_body_ctr(stmts: &[AstStmt], ctr: &mut u32) -> Vec<AstStmt> {
    let mut out = Vec::new();
    for stmt in stmts {
        hoist_await_stmt(stmt, ctr, &mut out);
    }
    out
}

/// Rewrite one statement, appending its hoisted-await preludes and then the rewritten statement to
/// `out`. Recurses into control-flow bodies so a mid-expression await deep inside an `if`/`while`/
/// `for`/`concurrent` is hoisted within that body.
fn hoist_await_stmt(stmt: &AstStmt, ctr: &mut u32, out: &mut Vec<AstStmt>) {
    let mut pre = Vec::new();
    let rewritten = match stmt {
        AstStmt::Binding {
            mut_decl,
            name,
            name_span,
            ty,
            value,
            span,
        } => {
            let mut value = value.clone();
            hoist_value_keep_head(&mut value, &mut pre, ctr);
            AstStmt::Binding {
                mut_decl: *mut_decl,
                name: name.clone(),
                name_span: *name_span,
                ty: ty.clone(),
                value,
                span: *span,
            }
        }
        AstStmt::Expr { expr, span } => {
            let mut expr = expr.clone();
            hoist_value_keep_head(&mut expr, &mut pre, ctr);
            AstStmt::Expr { expr, span: *span }
        }
        AstStmt::Echo { value, span } => {
            let mut value = value.clone();
            hoist_value_keep_head(&mut value, &mut pre, ctr);
            AstStmt::Echo { value, span: *span }
        }
        AstStmt::Return {
            value: Some(v),
            span,
        } => {
            let mut v = v.clone();
            hoist_value_keep_head(&mut v, &mut pre, ctr);
            AstStmt::Return {
                value: Some(v),
                span: *span,
            }
        }
        // A destructure has no head-await fast path in the flattener, so hoist *every* await in its
        // value (a bare `(a, b) = e.await` becomes `$hw = e.await; (a, b) = $hw`).
        AstStmt::Destructure {
            mut_decl,
            targets,
            value,
            span,
        } => {
            let mut value = value.clone();
            hoist_in_expr(&mut value, &mut pre, ctr);
            AstStmt::Destructure {
                mut_decl: *mut_decl,
                targets: targets.clone(),
                value,
                span: *span,
            }
        }
        AstStmt::If {
            cond,
            then_body,
            else_body,
            span,
        } => AstStmt::If {
            cond: cond.clone(),
            then_body: hoist_await_body(then_body),
            else_body: else_body.as_deref().map(hoist_await_body),
            span: *span,
        },
        AstStmt::While { cond, body, span } => AstStmt::While {
            cond: cond.clone(),
            body: hoist_await_body(body),
            span: *span,
        },
        AstStmt::For {
            pattern,
            iterable,
            body,
            span,
        } => AstStmt::For {
            pattern: pattern.clone(),
            iterable: iterable.clone(),
            body: hoist_await_body(body),
            span: *span,
        },
        AstStmt::Concurrent { body, span } => AstStmt::Concurrent {
            body: hoist_await_body(body),
            span: *span,
        },
        other => other.clone(),
    };
    out.append(&mut pre);
    out.push(rewritten);
}

/// Hoist mid-expression awaits in a value-bearing statement's value, **keeping a head `.await`**
/// (optionally under `?`) in place — the flattener turns that head await into the statement's own
/// poll-state, so only awaits *nested below* the head need hoisting.
fn hoist_value_keep_head(value: &mut Expr, pre: &mut Vec<AstStmt>, ctr: &mut u32) {
    match value {
        Expr::Await { expr, .. } => hoist_in_expr(expr, pre, ctr),
        Expr::Try { expr, .. } if matches!(expr.as_ref(), Expr::Await { .. }) => {
            if let Expr::Await { expr, .. } = expr.as_mut() {
                hoist_in_expr(expr, pre, ctr);
            }
        }
        other => hoist_in_expr(other, pre, ctr),
    }
}

/// Hoist **every** `.await` in `e` (in unconditionally-evaluated positions) into `pre` as a
/// `$hwN = <inner>.await` binding, replacing the await with a reference to `$hwN`. Inner awaits are
/// hoisted before their enclosing one and children are visited left-to-right, so the emitted bindings
/// run in source evaluation order. Conditionally-evaluated operands (short-circuit RHS, `??` fallback,
/// `match` arm bodies) and closures are not descended into (the checker guarantees no await there).
fn hoist_in_expr(e: &mut Expr, pre: &mut Vec<AstStmt>, ctr: &mut u32) {
    if let Expr::Await { .. } = e {
        if let Expr::Await { expr, .. } = e {
            hoist_in_expr(expr, pre, ctr);
        }
        let span = e.span();
        let name = format!("$hw{}", *ctr);
        *ctr += 1;
        let awaited = std::mem::replace(e, ident(&name, span));
        pre.push(bare_assign_expr(&name, awaited, span));
        return;
    }
    match e {
        Expr::Unary { operand, .. } => hoist_in_expr(operand, pre, ctr),
        Expr::Binary { .. } => {
            // Read the operator and whether the (conditional) RHS holds an await without keeping the
            // destructuring borrow alive across the rewrites below.
            let (is_short_circuit, rhs_await) = match &*e {
                Expr::Binary { op, rhs, .. } => {
                    (matches!(op, BinaryOp::And | BinaryOp::Or), rhs.has_await())
                }
                _ => unreachable!("matched Binary"),
            };
            // The LHS is always evaluated unconditionally — hoist its awaits.
            if let Expr::Binary { lhs, .. } = e {
                hoist_in_expr(lhs, pre, ctr);
            }
            if is_short_circuit && rhs_await {
                // Track A.6b — a short-circuit RHS holding an await becomes control flow so the await
                // runs only when the operator would evaluate it.
                desugar_short_circuit_await(e, pre, ctr);
            } else if !is_short_circuit {
                // A non-short-circuit binary evaluates both operands unconditionally.
                if let Expr::Binary { rhs, .. } = e {
                    hoist_in_expr(rhs, pre, ctr);
                }
            }
            // else: a short-circuit with an await-free RHS — leave it, it evaluates conditionally at
            // runtime with no suspension inside the guarded operand.
        }
        Expr::Pipeline { left, right, .. } => {
            hoist_in_expr(left, pre, ctr);
            hoist_in_expr(right, pre, ctr);
        }
        // `??`: the value is evaluated unconditionally (hoist its awaits); the fallback is
        // conditionally-evaluated (only on the `none`/`Err` path). A fallback holding an await becomes
        // control flow (Track A.6b-residual) so the guarded await runs only when `??` would evaluate it.
        Expr::Coalesce {
            value, fallback, ..
        } => {
            hoist_in_expr(value, pre, ctr);
            if fallback.has_await() {
                desugar_coalesce_await(e, pre, ctr);
            }
            // else: an await-free fallback stays lazy at runtime with no suspension inside it.
        }
        Expr::Index {
            receiver, index, ..
        } => {
            hoist_in_expr(receiver, pre, ctr);
            hoist_in_expr(index, pre, ctr);
        }
        Expr::Range { start, end, .. } => {
            hoist_in_expr(start, pre, ctr);
            hoist_in_expr(end, pre, ctr);
        }
        Expr::Call { callee, args, .. } => {
            hoist_in_expr(callee, pre, ctr);
            for a in args {
                hoist_in_expr(&mut a.value, pre, ctr);
            }
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => {
            for it in items {
                hoist_in_expr(it, pre, ctr);
            }
        }
        Expr::TupleIndex { receiver, .. } => hoist_in_expr(receiver, pre, ctr),
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                hoist_in_expr(k, pre, ctr);
                hoist_in_expr(v, pre, ctr);
            }
        }
        Expr::Member { receiver, .. } => hoist_in_expr(receiver, pre, ctr),
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(h) = part {
                    hoist_in_expr(h, pre, ctr);
                }
            }
        }
        // The scrutinee is evaluated unconditionally (hoist its awaits); each arm body is
        // conditionally-evaluated (only when its arm is selected). An arm body holding an await becomes
        // control flow (Track A.6b-residual) so the guarded await runs only when its arm is taken.
        Expr::Match {
            scrutinee, arms, ..
        } => {
            hoist_in_expr(scrutinee, pre, ctr);
            if arms.iter().any(|a| a.body.has_await()) {
                desugar_match_await(e, pre, ctr);
            }
            // else: no arm awaits — the match runs whole within one state (emitted verbatim).
        }
        Expr::Object(lit) => {
            for f in &mut lit.fields {
                hoist_in_expr(&mut f.value, pre, ctr);
            }
            if let Some(s) = &mut lit.spread {
                hoist_in_expr(s, pre, ctr);
            }
        }
        Expr::Try { expr, .. }
        | Expr::Spawn { future: expr, .. }
        | Expr::As { expr, .. }
        | Expr::TypeTest { expr, .. }
        | Expr::TypeOf { value: expr, .. }
        | Expr::FieldsOf { value: expr, .. }
        | Expr::TraitsOf { value: expr, .. }
        | Expr::ParamsOf { target: expr, .. }
        | Expr::ReturnsOf { target: expr, .. }
        | Expr::FromBytes { blob: expr, .. } => hoist_in_expr(expr, pre, ctr),
        Expr::Channel { capacity, .. } => hoist_in_expr(capacity, pre, ctr),
        // A turbofish operand is a type — no expression to hoist; a dynamic one is ordinary.
        Expr::FieldSpecsOf { name, .. } => {
            if let Some(e) = name.dynamic_mut() {
                hoist_in_expr(e, pre, ctr);
            }
        }
        Expr::Invoke {
            recv, name, args, ..
        } => {
            if let Some(recv) = recv {
                hoist_in_expr(recv, pre, ctr);
            }
            hoist_in_expr(name, pre, ctr);
            hoist_in_expr(args, pre, ctr);
        }
        Expr::Construct { name, fields, .. } => {
            if let Some(e) = name.dynamic_mut() {
                hoist_in_expr(e, pre, ctr);
            }
            hoist_in_expr(fields, pre, ctr);
        }
        Expr::TypedModuleCall { recv, args, .. } => {
            hoist_in_expr(recv, pre, ctr);
            for a in args {
                hoist_in_expr(&mut a.value, pre, ctr);
            }
        }
        Expr::TypedCall { args, .. } => {
            for a in args {
                hoist_in_expr(&mut a.value, pre, ctr);
            }
        }
        Expr::TypedMethodCall { recv, args, .. } => {
            hoist_in_expr(recv, pre, ctr);
            for a in args {
                hoist_in_expr(&mut a.value, pre, ctr);
            }
        }
        Expr::FieldSet {
            receiver, value, ..
        } => {
            hoist_in_expr(receiver, pre, ctr);
            hoist_in_expr(value, pre, ctr);
        }
        // A closure is a separate callable (an expression-tier block's holes desugar to
        // closures); leaves have no sub-expressions; `Await` is handled above.
        Expr::Closure { .. }
        | Expr::TierExpr { .. }
        | Expr::NativeFnRef { .. }
        | Expr::Await { .. }
        | Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::IntN { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::Bool { .. }
        | Expr::Ident { .. }
        | Expr::AttributesOf { .. }
        | Expr::RolesOf { .. } => {}
    }
}

/// `$poll(future)` — the async desugar's single-poll call (lowered to [`Rvalue::PollFuture`]).
fn poll_call(future: Expr, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(ident(POLL_FN, span)),
        args: vec![noeta_ast::CallArg::positional(future)],
        span,
    }
}

/// `$pending` — the async pending sentinel reference (lowered to [`Rvalue::Pending`]).
fn pending_expr(span: Span) -> Expr {
    ident(PENDING_IDENT, span)
}

/// `$scope_begin()` — open a concurrency scope and yield its index (lowered to [`Rvalue::ScopeBegin`]).
fn scope_begin_call(span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(ident(SCOPE_BEGIN_FN, span)),
        args: vec![],
        span,
    }
}

/// `$scope_ready(scope)` — the join poll-state's readiness test (lowered to [`Rvalue::ScopeReady`]).
fn scope_ready_call(scope: Expr, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(ident(SCOPE_READY_FN, span)),
        args: vec![noeta_ast::CallArg::positional(scope)],
        span,
    }
}

/// `$scope_end(scope);` — close the drained scope by index (lowered to [`Rvalue::ScopeEndAt`]), as an
/// expression statement. The index (not "innermost") because a sibling task's `concurrent` scope may
/// still be open above this one — they close out of structured-stack order under interleaving.
fn scope_end_stmt(scope: &str, span: Span) -> AstStmt {
    AstStmt::Expr {
        expr: Expr::Call {
            callee: Box::new(ident(SCOPE_END_FN, span)),
            args: vec![noeta_ast::CallArg::positional(ident(scope, span))],
            span,
        },
        span,
    }
}

/// Whether a statement sequence contains a `break`/`continue` that escapes to an **enclosing** loop —
/// i.e. one not absorbed by a `while`/`for` within the sequence. An `if` is transparent to control
/// flow (its `break` targets the outer loop); a `while`/`for` absorbs its own (no labels); a `match`
/// arm is an expression (no statements); nested callables are separate scopes.
fn body_has_escaping_ctrl(stmts: &[AstStmt]) -> bool {
    stmts.iter().any(stmt_has_escaping_ctrl)
}

fn stmt_has_escaping_ctrl(stmt: &AstStmt) -> bool {
    match stmt {
        AstStmt::Break { .. } | AstStmt::Continue { .. } => true,
        AstStmt::If {
            then_body,
            else_body,
            ..
        } => {
            body_has_escaping_ctrl(then_body)
                || else_body.as_deref().is_some_and(body_has_escaping_ctrl)
        }
        _ => false,
    }
}

/// `if $state == k { body }` — one state's dispatch arm inside the `while true` step loop.
fn state_arm(k: i64, body: Vec<AstStmt>, span: Span) -> AstStmt {
    AstStmt::If {
        cond: Expr::Binary {
            op: BinaryOp::Eq,
            lhs: Box::new(Expr::Ident {
                name: STATE_VAR.to_string(),
                span,
            }),
            rhs: Box::new(Expr::Int { value: k, span }),
            span,
        },
        then_body: body,
        else_body: None,
        span,
    }
}

/// `$state = k` — advance the dispatch discriminant (a bare assignment, so it reassigns the captured
/// cell rather than declaring a closure-local).
fn assign_state(k: i64, span: Span) -> AstStmt {
    AstStmt::Binding {
        mut_decl: false,
        name: STATE_VAR.to_string(),
        name_span: span,
        ty: None,
        value: Expr::Int { value: k, span },
        span,
    }
}

/// `name` — an identifier reference (a hoisted generator cell).
fn ident(name: &str, span: Span) -> Expr {
    Expr::Ident {
        name: name.to_string(),
        span,
    }
}

/// `name = value` — a bare assignment to a hoisted cell (`mut_decl: false`, so it reassigns the
/// pre-declared binding rather than shadowing it).
fn bare_assign_expr(name: &str, value: Expr, span: Span) -> AstStmt {
    AstStmt::Binding {
        mut_decl: false,
        name: name.to_string(),
        name_span: span,
        ty: None,
        value,
        span,
    }
}

/// A fresh `mut name = value` binding — the mutable head of a short-circuit-await desugar, later
/// re-assigned inside the guard.
fn mut_binding(name: &str, value: Expr, span: Span) -> AstStmt {
    AstStmt::Binding {
        mut_decl: true,
        name: name.to_string(),
        name_span: span,
        ty: None,
        value,
        span,
    }
}

/// Track A.6b — rewrite a short-circuit `lhs && rhs` / `lhs || rhs` whose RHS holds an `.await` into
/// control flow, so the guarded await runs exactly when the operator would evaluate the RHS:
///
/// ```text
/// x = a && b.await          x = a || b.await
/// ─────────────────         ─────────────────
/// mut $scN = a              mut $scN = a
/// if $scN { $scN = b.await } if !$scN { $scN = b.await }
/// x = $scN                  x = $scN
/// ```
///
/// `e` is the short-circuit expression (its LHS awaits already hoisted by the caller); it is replaced
/// with a reference to `$scN` and the two prelude statements are appended to `pre`. The RHS is run
/// through [`hoist_in_expr`] inside the guard body, so a *nested* short-circuit await (`a && (b &&
/// c.await)`) desugars recursively. Any other conditional await inside the RHS (a `??` fallback, a
/// `match` arm) was already rejected by the checker (E0040), so it never reaches here.
fn desugar_short_circuit_await(e: &mut Expr, pre: &mut Vec<AstStmt>, ctr: &mut u32) {
    let span = e.span();
    let name = format!("$sc{}", *ctr);
    *ctr += 1;
    let (op, lhs, rhs) = match std::mem::replace(e, ident(&name, span)) {
        Expr::Binary { op, lhs, rhs, .. } => (op, *lhs, *rhs),
        _ => unreachable!("caller guarantees a short-circuit Binary"),
    };
    // `mut $sc = lhs;` — the LHS's own awaits were already hoisted into `pre` before this call.
    pre.push(mut_binding(&name, lhs, span));
    // The guarded body: hoist the RHS's awaits (now in statement position) then `$sc = rhs`.
    let mut body = Vec::new();
    let mut rhs = rhs;
    hoist_in_expr(&mut rhs, &mut body, ctr);
    body.push(bare_assign_expr(&name, rhs, span));
    // `&&` runs the RHS iff `$sc` is true; `||` iff it is false.
    let cond = match op {
        BinaryOp::And => ident(&name, span),
        BinaryOp::Or => Expr::Unary {
            op: UnaryOp::Not,
            operand: Box::new(ident(&name, span)),
            span,
        },
        _ => unreachable!("caller guarantees `&&`/`||`"),
    };
    pre.push(AstStmt::If {
        cond,
        then_body: body,
        else_body: None,
        span,
    });
}

/// Track A.6b-residual — rewrite a `value ?? fallback` whose **fallback** holds an `.await` into control
/// flow, so the guarded await runs only on the `none`/`Err` path (exactly when `??` evaluates the
/// fallback). `e` is the `Coalesce` (its `value`'s awaits already hoisted by the caller); it is replaced
/// with a reference to the result cell `$coN` and the prelude is appended to `pre`:
///
/// ```text
/// x = value ?? fallback.await
/// ───────────────────────────
/// mut $mdN = 0
/// mut $coN = value ?? match 0 { _ => { $mdN = 1 } }   // success: $co = unwrapped; failure: $md=1, $co=unit
/// if $mdN == 1 { $coN = fallback.await }              // statement position → poll-state
/// x = $coN
/// ```
///
/// The Phase-1 `??` keeps the language's Option/Result-aware unwrap (success yields the unwrapped
/// value); its fallback is now a side-effecting `match` that only flips the discriminant, so no await
/// remains inside it — laziness holds by construction (the flip, and hence the real fallback, run only
/// when `??` takes the fallback path). The real fallback then runs in statement position, where the
/// flattener turns its await into a poll-state.
fn desugar_coalesce_await(e: &mut Expr, pre: &mut Vec<AstStmt>, ctr: &mut u32) {
    let span = e.span();
    let n = *ctr;
    *ctr += 1;
    let md = format!("$md{n}");
    let co = format!("$co{n}");
    let (value, fallback) = match std::mem::replace(e, ident(&co, span)) {
        Expr::Coalesce {
            value, fallback, ..
        } => (*value, *fallback),
        _ => unreachable!("caller guarantees a Coalesce"),
    };
    pre.push(mut_binding(&md, int_expr(0, span), span));
    // mut $co = value ?? (match 0 { _ => { $md = 1 } })  — the fallback only flips the discriminant.
    let flip = ClosureBody::Block(vec![bare_assign_expr(&md, int_expr(1, span), span)]);
    pre.push(mut_binding(
        &co,
        Expr::Coalesce {
            value: Box::new(value),
            fallback: Box::new(block_arm_expr(flip, span)),
            span,
        },
        span,
    ));
    // if $md == 1 { $co = <fallback, awaits hoisted to statement position> }
    let guarded = hoist_await_body_ctr(&[bare_assign_expr(&co, fallback, span)], ctr);
    pre.push(AstStmt::If {
        cond: eq_int(&md, 1, span),
        then_body: guarded,
        else_body: None,
        span,
    });
}

/// Track A.6b-residual — rewrite a `match` whose arm body/bodies hold an `.await` into a discriminant
/// dispatch plus guarded awaits, so each arm's await runs only when that arm is selected (in statement
/// position, where the flattener turns it into a poll-state). `if…then…else` desugars to a two-arm
/// `match` (parser), so this covers it too. `e` is the `Match` (its `scrutinee`'s awaits already hoisted
/// by the caller); it is replaced with a reference to the result cell `$mrN` and the prelude appended:
///
/// ```text
/// x = match scrut { p1 => a1, some(v) => f(v).await, p3 => a3 }
/// ────────────────────────────────────────────────────────────
/// mut $mrN = none                        // result cell (placeholder; the match is exhaustive so it
/// mut $mdN = 0                           //   is always reassigned). $md=0 ⇒ a non-awaiting arm ran.
/// match scrut {
///     p1 => { $mrN = a1 }                // non-awaiting arm: compute the value in place
///     some(v) => { $mb… = v; $mdN = 2 }  // awaiting arm: capture its bindings, then select it
///     p3 => { $mrN = a3 }
/// }
/// if $mdN == 2 { mut v = $mb…; $mrN = f(v).await }   // the selected awaiting arm, awaits hoisted
/// x = $mrN
/// ```
///
/// A non-awaiting arm keeps its body verbatim (it runs whole within one state). An awaiting arm binds
/// nothing in Phase-1 beyond capturing its pattern bindings into `$`-cells and flipping the
/// discriminant to its 1-based index; the real body runs guarded in statement position, its bindings
/// rebound from the cells so its references resolve unchanged, and any nested await (including a further
/// short-circuit / `??` / `match` await) is desugared recursively by the hoist. Laziness holds: only the
/// selected arm's guard fires, so only its await runs.
fn desugar_match_await(e: &mut Expr, pre: &mut Vec<AstStmt>, ctr: &mut u32) {
    let span = e.span();
    let n = *ctr;
    *ctr += 1;
    let mr = format!("$mr{n}");
    let md = format!("$md{n}");
    let (scrutinee, arms) = match std::mem::replace(e, ident(&mr, span)) {
        Expr::Match {
            scrutinee, arms, ..
        } => (*scrutinee, arms),
        _ => unreachable!("caller guarantees a Match"),
    };
    pre.push(mut_binding(&mr, none_expr(span), span));
    pre.push(mut_binding(&md, int_expr(0, span), span));

    let mut phase1_arms = Vec::with_capacity(arms.len());
    let mut phase2 = Vec::new();
    for (i, arm) in arms.into_iter().enumerate() {
        let arm_span = arm.span;
        if !arm.body.has_await() {
            // Non-awaiting arm: assign its value to the result cell, verbatim (no await inside).
            // The guard (await-free by the checker's guard rule) selects the arm exactly as in
            // the source.
            phase1_arms.push(MatchArm {
                pattern: arm.pattern,
                guard: arm.guard,
                body: ClosureBody::Block(arm_body_to_result(&mr, arm.body, arm_span)),
                span: arm_span,
            });
            continue;
        }
        // Awaiting arm: capture its pattern bindings into cells, select it by discriminant.
        let disc = (i + 1) as i64;
        let mut names = Vec::new();
        pattern_bound_names(&arm.pattern, &mut names);
        let mut select = Vec::new();
        let mut rebind = Vec::new();
        for (bname, bspan) in &names {
            let cell = format!("$mb{}", *ctr);
            *ctr += 1;
            // Declare the capture cell at the flattened level so it becomes a hoisted cell the
            // guarded Phase-2 body can read; the in-arm write is then a bare reassignment of it (a
            // fresh local written inside the verbatim Phase-1 match would not survive to Phase-2).
            pre.push(mut_binding(&cell, none_expr(*bspan), *bspan));
            select.push(bare_assign_expr(&cell, ident(bname, *bspan), *bspan));
            rebind.push(mut_binding(bname, ident(&cell, *bspan), *bspan));
        }
        select.push(bare_assign_expr(&md, int_expr(disc, arm_span), arm_span));
        // The guard (await-free by the checker's guard rule) stays on the Phase-1 selection arm:
        // pattern + guard together decide whether this arm's discriminant is flipped, so a false
        // guard falls through to the next Phase-1 arm exactly as in the source.
        phase1_arms.push(MatchArm {
            pattern: arm.pattern,
            guard: arm.guard,
            body: ClosureBody::Block(select),
            span: arm_span,
        });
        // Phase-2 guarded body: rebind the captured pattern names, then the arm body assigning $mr.
        let mut guarded = rebind;
        guarded.extend(arm_body_to_result(&mr, arm.body, arm_span));
        phase2.push(AstStmt::If {
            cond: eq_int(&md, disc, arm_span),
            then_body: hoist_await_body_ctr(&guarded, ctr),
            else_body: None,
            span: arm_span,
        });
    }
    // Phase-1 match: no awaits remain in any arm, so the flattener emits it verbatim (one state).
    pre.push(AstStmt::Expr {
        expr: Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms: phase1_arms,
            span,
        },
        span,
    });
    pre.extend(phase2);
}

/// The statements that assign a match arm body's value to the result cell `mr`. An expression arm
/// yields its value (`$mr = expr`); a statement-block arm (aether F1) runs its statements in the same
/// frame and yields unit (`stmts…; $mr = unit`).
fn arm_body_to_result(mr: &str, body: ClosureBody, span: Span) -> Vec<AstStmt> {
    match body {
        ClosureBody::Expr(e) => vec![bare_assign_expr(mr, *e, span)],
        ClosureBody::Block(mut stmts) => {
            stmts.push(bare_assign_expr(mr, unit_expr(span), span));
            stmts
        }
    }
}

/// Collect the names a pattern binds, in source order — the values that must survive from an awaiting
/// arm's selection to its guarded body (captured into `$`-cells). `Binding` binds its name; `Variant`
/// and `Tuple` recurse into their sub-patterns; the rest (`Wildcard`/literals/`IsType`) bind nothing —
/// an `IsType` only narrows an existing scrutinee identifier, which stays in scope in the guarded body.
fn pattern_bound_names(pat: &noeta_ast::Pattern, out: &mut Vec<(String, Span)>) {
    match pat {
        noeta_ast::Pattern::Binding { name, span } => out.push((name.clone(), *span)),
        noeta_ast::Pattern::Variant { bindings, .. } => {
            for b in bindings {
                pattern_bound_names(b, out);
            }
        }
        noeta_ast::Pattern::Tuple { elements, .. } => {
            for el in elements {
                pattern_bound_names(el, out);
            }
        }
        noeta_ast::Pattern::Wildcard { .. }
        | noeta_ast::Pattern::Int { .. }
        | noeta_ast::Pattern::Str { .. }
        | noeta_ast::Pattern::Bool { .. }
        | noeta_ast::Pattern::IsType { .. } => {}
    }
}

/// `n` — an integer literal expression.
fn int_expr(value: i64, span: Span) -> Expr {
    Expr::Int { value, span }
}

/// `lhs == k` — an equality test of the discriminant cell `name` against an integer literal.
fn eq_int(name: &str, k: i64, span: Span) -> Expr {
    Expr::Binary {
        op: BinaryOp::Eq,
        lhs: Box::new(ident(name, span)),
        rhs: Box::new(int_expr(k, span)),
        span,
    }
}

/// `match 0 { _ => <body> }` — a single-wildcard `match` used as a side-effecting expression whose value
/// is `body`'s (unit for a block body). Reused as `??`'s discriminant-flip fallback and as the `unit`
/// literal the AST otherwise lacks.
fn block_arm_expr(body: ClosureBody, span: Span) -> Expr {
    Expr::Match {
        scrutinee: Box::new(int_expr(0, span)),
        arms: vec![MatchArm {
            pattern: noeta_ast::Pattern::Wildcard { span },
            guard: None,
            body,
            span,
        }],
        span,
    }
}

/// `unit` — the unit value, expressed as an empty block arm (`match 0 { _ => {} }`), since the surface
/// AST has no unit literal node. Used where a desugared block arm's value must flow into the result cell.
fn unit_expr(span: Span) -> Expr {
    block_arm_expr(ClosureBody::Block(Vec::new()), span)
}

/// `mut name = value` — a fresh declaration (used for a block-local-eligible binding, e.g. a `for`
/// loop variable). If liveness later hoists the name, [`Flattener::rewrite_hoisted`] turns this back
/// into a bare assignment against the prelude cell.
fn decl_expr(name: &str, value: Expr, span: Span) -> AstStmt {
    AstStmt::Binding {
        mut_decl: true,
        name: name.to_string(),
        name_span: span,
        ty: None,
        value,
        span,
    }
}

/// Whether a flattened state references `name` — in any of its statements or its terminator's
/// condition/yielded expression. Conservative: never under-reports a reference (an over-report only
/// forgoes the block-local optimization for that name, never miscompiles). See [`stmt_mentions`].
fn block_mentions(block: &BlockBuf, name: &str) -> bool {
    block.stmts.iter().any(|s| stmt_mentions(s, name))
        || match &block.term {
            Term::Yield(e, _) | Term::Branch(e, _, _) => e.mentions(name),
            Term::Complete(value) => value.as_ref().is_some_and(|e| e.mentions(name)),
            // A poll-state reads its awaited-future cell and writes its result cell — so both are
            // referenced here (they also span states, which keeps them hoisted).
            Term::AwaitPoll { future, result, .. } => name == future || name == result,
            // A join poll-state reads its scope-index cell (which also spans states, keeping it hoisted).
            Term::JoinPoll { scope, .. } => name == scope,
            Term::Goto(_) | Term::Done => false,
        }
}

/// Whether a statement references `name` — as a binding target or anywhere in a contained expression,
/// descending into nested statement bodies. Total over `AstStmt` and **conservative**: any construct
/// that could carry a reference this walker does not fully traverse (a nested declaration) reports
/// `true`, so a reference is never missed (which would be a miscompile); over-reporting only skips the
/// optimization. Built on [`Expr::mentions`], which is itself conservative for closure bodies.
fn stmt_mentions(stmt: &AstStmt, name: &str) -> bool {
    match stmt {
        AstStmt::Echo { value, .. } | AstStmt::Expr { expr: value, .. } => value.mentions(name),
        AstStmt::Binding {
            name: target,
            value,
            ..
        } => target == name || value.mentions(name),
        AstStmt::Destructure { targets, value, .. } => {
            targets.iter().any(|(t, _)| t == name) || value.mentions(name)
        }
        AstStmt::Return { value, .. } => value.as_ref().is_some_and(|v| v.mentions(name)),
        AstStmt::Yield { value, .. } => value.mentions(name),
        AstStmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            cond.mentions(name)
                || then_body.iter().any(|s| stmt_mentions(s, name))
                || else_body
                    .as_deref()
                    .is_some_and(|b| b.iter().any(|s| stmt_mentions(s, name)))
        }
        AstStmt::While { cond, body, .. } => {
            cond.mentions(name) || body.iter().any(|s| stmt_mentions(s, name))
        }
        AstStmt::For { iterable, body, .. } => {
            iterable.mentions(name) || body.iter().any(|s| stmt_mentions(s, name))
        }
        AstStmt::Break { .. }
        | AstStmt::Continue { .. }
        | AstStmt::Namespace { .. }
        | AstStmt::Use { .. } => false,
        // Nested declarations (`fn`/`class`/…/tier blocks) are not fully walked; conservatively assume
        // they may reference `name` so it stays hoisted.
        _ => true,
    }
}

/// `receiver.method()` — a no-argument method call (used for `.iter()`/`.next()` in a flattened
/// `for`).
fn method_call(receiver: Expr, method: &str, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::Member {
            receiver: Box::new(receiver),
            name: method.to_string(),
            name_span: span,
            span,
        }),
        args: Vec::new(),
        span,
    }
}

/// `match opt { some(_) => true, _ => false }` — whether an `?T` iterator result holds an element. A
/// generator's flattened `for` branches on this to decide whether to run the body or exit the loop.
fn is_some_test(opt: Expr, span: Span) -> Expr {
    Expr::Match {
        scrutinee: Box::new(opt),
        arms: vec![
            noeta_ast::MatchArm {
                pattern: noeta_ast::Pattern::Variant {
                    type_name: None,
                    variant: "some".to_string(),
                    bindings: vec![noeta_ast::Pattern::Wildcard { span }],
                    span,
                },
                guard: None,
                body: noeta_ast::ClosureBody::Expr(Box::new(Expr::Bool { value: true, span })),
                span,
            },
            noeta_ast::MatchArm {
                pattern: noeta_ast::Pattern::Wildcard { span },
                guard: None,
                body: noeta_ast::ClosureBody::Expr(Box::new(Expr::Bool { value: false, span })),
                span,
            },
        ],
        span,
    }
}

/// `some(value)` — the per-element constructor of a generator step's `?T` result.
fn call_some(value: Expr, span: Span) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::Ident {
            name: "some".to_string(),
            span,
        }),
        args: vec![noeta_ast::CallArg::positional(value)],
        span,
    }
}

/// `none` — the end-of-iteration result and the placeholder a hoisted local is initialized to (it is
/// always reassigned before it is read in a well-formed generator).
fn none_expr(span: Span) -> Expr {
    Expr::Ident {
        name: "none".to_string(),
        span,
    }
}

/// Whether a statement sequence contains a `yield` (Track G), descending into control-flow bodies but
/// **not** into nested callables (a closure/`fn` resets the generator context). Mirrors the checker's
/// generator detection so the lowering desugars exactly the bodies the checker treats as generators.
pub(super) fn body_has_yield(stmts: &[AstStmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

fn stmt_has_yield(stmt: &AstStmt) -> bool {
    match stmt {
        AstStmt::Yield { .. } => true,
        AstStmt::If {
            then_body,
            else_body,
            ..
        } => body_has_yield(then_body) || else_body.as_deref().is_some_and(body_has_yield),
        AstStmt::For { body, .. } | AstStmt::While { body, .. } => body_has_yield(body),
        _ => false,
    }
}
