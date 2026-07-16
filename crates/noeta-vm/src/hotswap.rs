//! **Hot-swap and console-fragment installation** (tooling-unification T4,
//! server-hmr W1): the [`FragmentCompiler`] seam and [`HotFragment`] /
//! [`HotChannel`] mailbox vocabulary, plus the `impl Vm` apply/install/eval
//! cluster (`apply_pending_hotswap`, `install_fragment`,
//! `debug_eval_fragment`, `debug_set_variable`). Deliberately NOT
//! `compile`-gated as a module: these paths are compiler-free by design
//! (native-size slice 2) — the seam keeps `noeta-compiler` out of the VM core,
//! and each item keeps exactly the `#[cfg]`s it had at the crate root. Moved
//! verbatim purely to shrink `lib.rs` — no behavior change.

use crate::*;

/// The **live incremental compiler** behind a debug console / REPL / hot-reload session
/// (tooling-unification T4, server-hmr W1) — the seam that keeps the VM core free of the compiler
/// crate (native-size slice 2). `noeta_compiler::SessionCompiler` implements it (behind the
/// `compile` feature); a shipped AOT binary, which runs a pre-compiled bundle and never compiles a
/// fragment, links no implementor and sheds the whole compiler front-end. Names only always-present
/// types ([`Program`], [`Module`]) so the trait — and every install path that drives it — compiles
/// without `noeta-compiler`.
pub trait FragmentCompiler: std::fmt::Debug {
    /// Compile `fragment` as a stable-prefix extension of the running program, returning the
    /// extended module (a superset — every index minted under an earlier module stays valid).
    /// `Err` is the rendered reason.
    fn extend(&mut self, fragment: &Program) -> Result<Module, String>;
    /// The global slot a name currently binds, if any (used to collect re-bound top-level slots
    /// before a hot re-run).
    fn global_slot(&self, name: &str) -> Option<u32>;
    /// Declare a global into the session's name-space (a fragment's new/overwritten binding).
    fn declare_global(&mut self, name: &str, mutable: bool, overwrite: bool);
}

/// The binding names a top-level statement (re)binds — the globals a re-running swap overwrites.
/// A pure-AST helper shared by the live-VM hot path ([`Vm::apply_pending_hotswap`]) and the session
/// module; lives here (not in the feature-gated `session`) so the compiler-free hot apply can use it.
pub(crate) fn binding_targets(stmt: &Stmt) -> Vec<&str> {
    match stmt {
        Stmt::Binding { name, .. } => vec![name.as_str()],
        Stmt::Destructure { targets, .. } => targets.iter().map(|(n, _)| n.as_str()).collect(),
        _ => Vec::new(),
    }
}

/// A ready-to-apply hot-reload fragment (server-hmr W1) — the compiler-free hand-off the VM applies.
/// The watcher thread (which owns parsing, checking, and diffing) turns its `SwapPlan` into this
/// plain record when depositing, so [`HotChannel`] — and thus the VM core — never names the compiler.
#[derive(Debug, Clone)]
pub struct HotFragment {
    /// The edit's fragment program (new/changed top-level items, re-run initializers).
    pub fragment: Program,
    /// Whether the fragment's top-level statements re-run (initializers), rebinding their slots.
    pub rerun_top_level: bool,
    /// Names added by this edit (for the `[hot] swapped: …` report).
    pub added: Vec<String>,
    /// Names changed by this edit (preferred over `added` in the report when non-empty).
    pub changed: Vec<String>,
}

/// The hot-reload mailbox (server-hmr W1): a watcher thread — which owns parsing, checking
/// (transactional gate), and diffing — deposits a ready-to-apply [`HotFragment`]; the run thread
/// takes it at the next scheduler tick and applies it to the live program. A deposit replaces an
/// unconsumed predecessor (the depositor is responsible for diffing against the last *consumed*
/// version — see the CLI's hot-serve driver).
pub type HotSwapMailbox = Arc<HotChannel>;

/// The hot-reload channel shared by the watcher thread, the VM, and (through the [`NativeCtx`]
/// accessors) the serve loop (server-hmr L3).
///
/// - `plan` — the swap mailbox (see [`HotSwapMailbox`]).
/// - `error` — the last **rejected** edit's rendered diagnostics: the watcher deposits on a red
///   check (replacing an older error; a green deposit clears it), the serve loop takes it and
///   pushes an `error` frame to live LiveView clients for the browser overlay.
/// - `swaps` — the swap **generation**: incremented after each successfully applied swap, so the
///   serve loop can detect "a swap landed since my last iteration" and push `reload` to clients.
///
/// [`NativeCtx`]: noeta_stdlib::NativeCtx
#[derive(Debug, Default)]
pub struct HotChannel {
    /// The **append-only queue** of swap plans, generation = index (server-hmr F5). A single
    /// depositor (the watcher) pushes; every consuming VM drains the tail it has not yet applied,
    /// so a swap **broadcasts** to every worker isolate (`serve --parallel --watch`) rather than
    /// being taken by one. Each plan is diffed against the previous *deposited* version, so
    /// applying them in order keeps each worker's session in lockstep.
    pub plans: std::sync::Mutex<Vec<HotFragment>>,
    pub error: std::sync::Mutex<Option<String>>,
}

impl<'m> Vm<'m> {
    /// Install a debug-console **fragment** into this running Vm (tooling-unification T4). The
    /// fragment compiles through the adopted session compiler — checkerless, stable-prefix id
    /// accumulation, exactly a REPL entry — and the Vm then:
    ///
    /// 1. **Relocates the fragment's entry chunk** to a fresh proto index at the end of the table.
    ///    `SessionCompiler::extend` rewrites proto 0 per entry, but proto 0 of the *running* module
    ///    is the program's `main`, which live frame 0 is still executing — so the snapshot's proto 0
    ///    is restored to `main` and the fragment's statements get their own index. (Entry chunks
    ///    never self-reference index 0; every callee/closure index inside is absolute and new.)
    /// 2. **Grows the derived tables** the fragment introduced — the same appends `Vm::load` /
    ///    `SessionState::sync_to` perform: interned shapes and packed schemas (global interning
    ///    makes identity hold by construction), shared `TypeRepr`s, method / destructor /
    ///    field-default entries, derive sets, destruct-reachability, and the globals vector (new
    ///    slots start unbound).
    /// 3. **Swaps `self.module` to the extended snapshot**, kept alive in the session's arena for
    ///    the rest of the run. Every snapshot is a stable-prefix superset of every earlier one, so
    ///    old frames keep executing identical code and an escaped fragment value (a closure's raw
    ///    proto index) stays resolvable after the program resumes — the dispatch loop re-reads the
    ///    module at each frame transfer.
    ///
    /// Returns the relocated entry's proto index; the caller runs it via [`Vm::run_thunk`]. Debug
    /// runs keep the JIT unarmed (asserted): tier-1 mirror tables never see a swapped module.
    /// Apply a pending hot-swap plan, if one is waiting in the mailbox (server-hmr W1). Called at
    /// the scheduler tick (`advance_tasks` — every ctx-driven loop's per-iteration safepoint), so
    /// a `noeta serve --watch` process absorbs edits between polls. Mirrors
    /// [`VmSession::hot_swap`]'s semantics on the live VM: on a re-running swap, dispose the
    /// previous effect epoch and the reactive nodes the re-run re-binds ([`Vm::hotswap_prepare`]),
    /// then install the fragment ([`Vm::install_fragment`] — the debug console's stable-prefix
    /// module swap) and run its entry under the console's evaluation budget. In-flight frames keep
    /// executing old code (stable-prefix invariant); every slot-routed call dispatches to the new
    /// bodies from now on.
    ///
    /// Failures are reported to stderr and leave the program serving its previous version — the
    /// watcher thread already gated on parse/check, so a failure here is compile-internal (or the
    /// fragment's own top-level code erroring at run time, e.g. a re-run initializer panicking).
    pub(crate) fn apply_pending_hotswap(&mut self) {
        let Some(mailbox) = &self.hot_mailbox else {
            return;
        };
        // Drain the broadcast queue from this VM's own generation (server-hmr F5): clone the
        // plans it has not applied yet (the lock is held only for the clone, never across the
        // apply), then apply each in order. N workers each drain the same queue independently.
        let pending: Vec<HotFragment> = match mailbox.plans.try_lock() {
            Ok(plans) if plans.len() > self.applied_swaps => plans[self.applied_swaps..].to_vec(),
            _ => return,
        };
        for plan in pending {
            self.apply_one_swap(plan);
            // Count the generation as applied even if the fragment errored at run time (the
            // watcher already gated parse/check): a re-attempt would re-run the same fragment.
            self.applied_swaps += 1;
        }
    }

    /// Apply a single swap plan to the live session (server-hmr W1/H1; the per-plan body the F5
    /// queue drain calls). Failures report to stderr and keep the previous version serving.
    fn apply_one_swap(&mut self, plan: HotFragment) {
        // Slots the re-run overwrites — resolved against the session compiler BEFORE the fragment
        // extends it, so only pre-existing bindings (the ones with old nodes) are collected.
        let rebound: Vec<u32> = if plan.rerun_top_level {
            let Some(session) = self.debug_session.as_ref() else {
                eprintln!("[hot] no live session — swap skipped");
                return;
            };
            plan.fragment
                .stmts
                .iter()
                .flat_map(binding_targets)
                .filter_map(|name| session.compiler.global_slot(name))
                .collect()
        } else {
            Vec::new()
        };
        if plan.rerun_top_level {
            self.hotswap_prepare(&rebound);
        }
        let entry = match self.install_fragment(&plan.fragment) {
            Ok(entry) => entry,
            Err(msg) => {
                eprintln!("[hot] swap failed to compile: {msg} — still serving the old version");
                return;
            }
        };
        // Run the fragment's entry under the console's budget (a runaway re-run initializer must
        // not wedge the server); the budget debugger swap is the same dance the console does.
        let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saved = self.debugger.take();
        self.debugger = Some(Box::new(EvalBudget {
            steps: 0,
            deadline: std::time::Instant::now()
                + std::time::Duration::from_millis(DEBUG_EVAL_TIMEOUT_MS),
            tripped: Arc::clone(&tripped),
        }));
        let outcome = self.run_thunk(entry, &[]);
        self.debugger = saved;
        match outcome {
            Ok(v) => {
                release(v);
                let what = if plan.changed.is_empty() {
                    plan.added.join(", ")
                } else {
                    plan.changed.join(", ")
                };
                eprintln!(
                    "[hot] swapped{}{}",
                    if what.is_empty() { "" } else { ": " },
                    what
                );
                // The serve loop detects this via `NativeCtx::hot_swap_count` (this VM's
                // `applied_swaps`, bumped by the drain) and pushes `reload` to its live clients
                // (server-hmr L3).
            }
            Err(Abort) => {
                let msg = self.last_diag_message();
                eprintln!("[hot] swap fragment aborted: {msg} — the program keeps running");
            }
        }
    }

    pub(crate) fn install_fragment(&mut self, fragment: &Program) -> Result<u32, String> {
        // Tier-1 across a swap (server-hmr H3): retire the armed engine (pages parked in the
        // graveyard so in-flight native frames stay executable), install the fragment against a
        // clean tier-0 world, then re-arm fresh against the swapped module and let tiering
        // re-warm. Unarmed runs (the debug console, hover, the differential) skip both halves.
        #[cfg(feature = "jit")]
        let rearm = self.hotswap_retire_tier1();
        let Some(session) = self.debug_session.as_mut() else {
            return Err("this run has no debug session (fragments need a session launch)".into());
        };
        let arena = session.arena;
        let mut extended = session.compiler.extend(fragment)?;
        // (1) Relocate the entry; proto 0 stays the program's `main`.
        let entry = std::mem::replace(&mut extended.protos[0], self.module.protos[0].clone());
        extended.protos.push(entry);
        let entry_idx = (extended.protos.len() - 1) as u32;
        // The checkerless snapshot carries no `map(...)`-packed pairs; keep the base compile's
        // precise ones so the swapped module stays self-consistent (`vm.map_packed` already holds
        // the resolved schemas either way).
        extended.map_packed_sites = self.module.map_packed_sites.clone();

        // (2) Grow the derived tables from the snapshot's tails (all appends are prefix-stable).
        for shape in &extended.shapes[self.shapes.len()..] {
            self.shapes.push(noeta_object::intern_shape(shape.clone()));
        }
        for def in &extended.packed_schemas[self.packed_schemas.len()..] {
            let fields = def
                .fields
                .iter()
                .map(|f| match f {
                    noeta_bytecode::PackedFieldDef::Int => noeta_object::PackedKind::Int,
                    noeta_bytecode::PackedFieldDef::Float => noeta_object::PackedKind::Float,
                    noeta_bytecode::PackedFieldDef::F32 => noeta_object::PackedKind::F32,
                    noeta_bytecode::PackedFieldDef::Bool => noeta_object::PackedKind::Bool,
                    noeta_bytecode::PackedFieldDef::Struct(idx) => {
                        noeta_object::PackedKind::Struct(self.packed_schemas[*idx as usize])
                    }
                })
                .collect();
            self.packed_schemas
                .push(noeta_object::intern_schema(noeta_object::PackedSchema {
                    shape: self.shapes[def.shape as usize],
                    fields,
                    byte_size: def.byte_size as usize,
                    column: def.column,
                }));
        }
        for repr in &extended.type_reprs[self.type_reprs.len()..] {
            self.type_reprs.push(Rc::new(repr.clone()));
        }
        for m in &extended.methods[self.module.methods.len()..] {
            self.methods
                .insert((m.type_name.clone(), m.method.clone()), m.proto);
        }
        for (ty, proto) in &extended.destructors[self.module.destructors.len()..] {
            self.destructors.insert(ty.clone(), *proto);
        }
        for (ty, field, proto) in &extended.field_defaults[self.module.field_defaults.len()..] {
            self.field_defaults
                .insert((ty.clone(), field.clone()), *proto);
        }
        self.comparable_derives
            .extend(extended.comparable_derives.iter().cloned());
        self.tojson_derives
            .extend(extended.tojson_derives.iter().cloned());
        self.destruct_reachable
            .extend(extended.destruct_reachable.iter().cloned());
        self.globals
            .resize(extended.global_names.len(), Value::unbound());

        // (3) Swap to the arena'd snapshot; the dispatch loop picks it up at the next frame transfer.
        self.module = arena.alloc(extended);
        #[cfg(feature = "jit")]
        if rearm {
            self.hotswap_rearm_tier1();
        }
        Ok(entry_idx)
    }

    /// Retire the armed tier-1 engine ahead of a module swap (server-hmr H3). The engine (or the
    /// off-thread service) moves to the graveyard — its executable pages must outlive any native
    /// frame still on the machine stack beneath the swap safepoint — and every mirror entry is
    /// cleared, so no *new* dispatch enters retired code; everything falls back to the
    /// interpreter (whose dispatch reads the live, post-swap tables) until re-warmed. Counters
    /// and request state reset with them. Returns whether tier 1 was armed (the caller re-arms
    /// after the module is swapped).
    #[cfg(feature = "jit")]
    fn hotswap_retire_tier1(&mut self) -> bool {
        let was_armed = self.tier1.jit.is_some() || self.tier1.jit_service.is_some();
        if let Some(engine) = self.tier1.jit.take() {
            self.tier1.jit_graveyard.push(engine);
        }
        if let Some(service) = self.tier1.jit_service.take() {
            // Parked, not shut down: shutdown joins the thread and frees the pages. Stale ready
            // responses die with the handle at teardown — `jit_pending` resets below, so the
            // drain never looks for them.
            self.tier1.jit_service_graveyard.push(service);
        }
        self.tier1.jit_entries.iter_mut().for_each(|e| *e = None);
        self.tier1.jit_fast.iter_mut().for_each(|f| *f = None);
        self.tier1.jit_counters.iter_mut().for_each(|c| *c = 0);
        self.tier1.jit_declined.iter_mut().for_each(|d| *d = false);
        self.tier1.jit_requested.iter_mut().for_each(|r| *r = false);
        self.tier1
            .jit_osr_pending
            .iter_mut()
            .for_each(|o| *o = false);
        self.tier1.jit_pending = 0;
        // `jit_cache_pins` stay: retired code's call-site caches still guard on those closures'
        // bits, and in-flight old frames may consult them. Released at teardown, as always.
        was_armed
    }

    /// Re-arm tier 1 against the freshly swapped module (server-hmr H3): the `force_jit` oracle
    /// re-creates the synchronous engine (eager-compiling every proto of the NEW module, added
    /// ones included); production re-spawns the off-thread service around a clone of the new
    /// module — hot-counter promotion re-warms exactly what the program still runs.
    #[cfg(feature = "jit")]
    fn hotswap_rearm_tier1(&mut self) {
        if self.tier1.force_jit {
            self.init_jit();
        } else {
            self.init_jit_service(Arc::new(self.module.clone()));
        }
    }

    /// Evaluate a debug-console **fragment** against a paused frame by *compiling* it — the T5
    /// evaluator: the console is a REPL over the paused program. The fragment is wrapped as a
    /// closure whose parameters are the frame's in-scope locals (frame-scope binding via the
    /// ordinary call protocol — the REPL's sentinel idea in frame scope), its trailing bare
    /// expression rewritten to a `return`; the wrapper is bound to an unforgeable sentinel global
    /// by [`Vm::install_fragment`] + one entry run, then called with the values read straight from
    /// the paused register window. Everything the language can do works — calls, closures
    /// (`xs.filter(fn(x) => x > 15)`), statements, assignment to program globals — with semantics
    /// that are the compiler's by construction. An escaped value (a closure stored into program
    /// state) stays callable after resume because the installed module lives for the rest of the
    /// run.
    ///
    /// A fragment's transient diagnostics/abort-trace are rolled back afterwards, so a failed
    /// console entry never pollutes the debugged run.
    ///
    /// With `pure` set (a **hover** — T6), the same engine runs gated to the read-only surface:
    /// the fragment must be a single expression built from names / members / indexing / operators /
    /// literals ([`is_pure_expr`]), and [`Vm::pure_eval`] backstops the receiver-dependent
    /// dispatches the AST cannot decide (an object's `Index` impl, a user ordering method) by
    /// refusing any frame push during the run. One evaluator for hover, watch, and console.
    pub(crate) fn debug_eval_fragment(
        &mut self,
        program: &Program,
        frame: usize,
        pure: bool,
        text: &str,
        frames: &[Frame],
        regs: &[Value],
    ) -> DebugEvalOutcome {
        match self.eval_fragment_owned(program, frame, pure, Some(text), frames, regs) {
            Ok(v) => {
                let text = v.display();
                let ty = v.type_display();
                release(v);
                DebugEvalOutcome::Value { text, ty }
            }
            Err(msg) => DebugEvalOutcome::Error(msg),
        }
    }

    /// The value-returning core of [`Vm::debug_eval_fragment`]: evaluate the fragment and hand back
    /// the resulting **owned** [`Value`] (one reference the caller must consume — render + release
    /// for an `evaluate`, store into a register for a `setVariable`).
    fn eval_fragment_owned(
        &mut self,
        program: &Program,
        frame: usize,
        pure: bool,
        text: Option<&str>,
        frames: &[Frame],
        regs: &[Value],
    ) -> Result<Value, String> {
        if pure {
            // Hover: exactly one expression, from the side-effect-free surface.
            let gated = match &program.stmts[..] {
                [Stmt::Expr { expr, .. }] => is_pure_expr(expr),
                _ => false,
            };
            if !gated {
                return Err(
                    "hover stays read-only — supported: names, `.field`, `[index]`, operators, \
                     and literals (use a watch or the debug console to run code)"
                        .to_string(),
                );
            }
        }
        // Resolve the target frame (snapshot indices are innermost-first, the view bottom-first)
        // and collect its in-scope locals — the same declared-before-the-paused-instruction filter
        // the Variables view applies, so the console sees exactly what the panel shows.
        let (params, args): (Vec<String>, Vec<Value>) = {
            let view = DebugView {
                module: self.module,
                frames,
                regs,
            };
            let Some(view_idx) = view.depth().checked_sub(frame + 1) else {
                return Err(format!("no frame {frame} in the paused stack"));
            };
            let f = view.frame(view_idx);
            let here = f.line_span();
            f.locals()
                .filter(|(_, def_span, _)| match here {
                    Some(h) => def_span.start < h.start,
                    None => true,
                })
                .map(|(name, _, value)| (name.to_string(), value))
                .unzip()
        };
        let span = program.span;
        // Compiled-wrapper memo (U3): a watch panel re-evaluates its expressions on every step —
        // key on (raw text, in-scope local names) and reuse the installed entry on a hit, so a
        // repeated watch appends nothing to the session. Values stay fresh (they are call
        // arguments). Only a successful compile is memoized, and the param names are part of the
        // key, so a hit is exactly a replay of a compile that succeeded in this same scope shape.
        let memo_key = text.map(|t| (t.to_string(), params.clone()));
        let cached = memo_key.as_ref().and_then(|k| {
            self.debug_session
                .as_ref()
                .and_then(|s| s.memo.get(k).copied())
        });
        let entry = if let Some(entry) = cached {
            entry
        } else {
            self.compile_fragment_entry(program, pure, &params, memo_key, span)?
        };
        let diag_mark = self.out.diagnostics.len();
        let trace_mark = self.out.abort_trace.len();
        self.pure_eval = pure;
        let outcome = self.run_installed_fragment(entry, args, span);
        self.pure_eval = false;
        // A console entry is a side query — its errors must not leak into the run being debugged.
        self.out.diagnostics.truncate(diag_mark);
        self.out.abort_trace.truncate(trace_mark);
        outcome
    }

    /// Compile-and-install one fragment wrapper (the memo-miss path of
    /// [`Vm::eval_fragment_owned`]): apply the U2 binding promotion, rewrite the trailing bare
    /// expression to a `return`, wrap as the sentinel-bound closure whose parameters are `params`,
    /// install through the session, and memoize the entry under `memo_key`.
    fn compile_fragment_entry(
        &mut self,
        program: &Program,
        pure: bool,
        params: &[String],
        memo_key: Option<(String, Vec<String>)>,
        span: Span,
    ) -> Result<u32, String> {
        // Wrap: `mut <sentinel> = fn(<locals>) { <fragment>; return <trailing expr> };`
        let mut body = program.stmts.clone();
        // Persistent console bindings (U2): a fragment's top-level `mut x = e` — and a bare
        // `x = e` introducing a NEW name — binds a SESSION GLOBAL, the console analogue of a REPL
        // binding, not a closure-local that dies with the entry. Each such name is pre-registered
        // in the session compiler (so the assignment inside the wrapper resolves globally) and the
        // `mut` declaration is rewritten to a plain assignment. A name that is a frame local here
        // is refused: the language forbids shadowing, and silently diverging from what the
        // Variables panel shows would be worse. (Hover never reaches this — the purity gate above
        // admits only a single expression.) Nested `mut`s — inside a loop or closure in the
        // fragment — stay fragment-local, as they would in any function body.
        if !pure {
            for stmt in body.iter_mut() {
                let Stmt::Binding { mut_decl, name, .. } = stmt else {
                    continue;
                };
                if *mut_decl {
                    if params.iter().any(|p| p == name) {
                        return Err(format!(
                            "`{name}` is a frame local here — pick another name to bind at the \
                             console"
                        ));
                    }
                    if let Some(session) = self.debug_session.as_mut() {
                        session.compiler.declare_global(name, true, true);
                    }
                    *mut_decl = false;
                } else if !params.iter().any(|p| p == name) {
                    // A bare `x = e` on a brand-new name: register it (re-assignable, like a REPL
                    // binding) so it persists too — an existing global's declared mutability stands.
                    if let Some(session) = self.debug_session.as_mut() {
                        session.compiler.declare_global(name, true, false);
                    }
                }
            }
        }
        if let Some(Stmt::Expr { expr, span }) = body.last() {
            let (expr, span) = (expr.clone(), *span);
            *body.last_mut().expect("non-empty: matched last") = Stmt::Return {
                value: Some(expr),
                span,
            };
        }
        let wrapper = Program {
            stmts: vec![Stmt::Binding {
                mut_decl: true,
                name: FRAGMENT_SENTINEL.to_string(),
                name_span: span,
                ty: None,
                value: Expr::Closure {
                    params: params
                        .iter()
                        .map(|name| Param {
                            name: name.clone(),
                            name_span: span,
                            ty: None,
                            default: None,
                            span,
                        })
                        .collect(),
                    ret: None,
                    body: ClosureBody::Block(body),
                    span,
                },
                span,
            }],
            span,
        };
        let entry = self.install_fragment(&wrapper)?;
        if let (Some(key), Some(session)) = (memo_key, self.debug_session.as_mut()) {
            session.memo.insert(key, entry);
        }
        Ok(entry)
    }

    /// Run one **installed** fragment entry: the entry run binds the sentinel closure, which is
    /// then taken out of its slot (one-shot — the sentinel never lingers) and called with the
    /// paused frame's local values. `args` are borrowed from the paused register window; they are
    /// retained only at the call (which consumes one reference each). Returns the fragment's
    /// **owned** result value.
    fn run_installed_fragment(
        &mut self,
        entry: u32,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, String> {
        // Liveness bound (MCP M6b): the nested run executes with the session's own debugger held
        // out of `self` (so a fragment never trips a breakpoint), which also meant nothing could
        // stop it — an infinite-loop watch expression hung the paused session. Arm a budget-only
        // debugger over the same per-op seam for exactly this run: on a trip it terminates the
        // *fragment* (an ordinary nested abort — the paused program is untouched and resumable).
        let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saved = self.debugger.take();
        self.debugger = Some(Box::new(EvalBudget {
            steps: 0,
            deadline: std::time::Instant::now()
                + std::time::Duration::from_millis(DEBUG_EVAL_TIMEOUT_MS),
            tripped: Arc::clone(&tripped),
        }));
        let result = self.run_installed_fragment_inner(entry, args, span);
        self.debugger = saved;
        if tripped.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(format!(
                "the evaluation was stopped after exceeding the debug-console budget \
                 ({DEBUG_EVAL_TIMEOUT_MS} ms / {DEBUG_EVAL_MAX_STEPS} steps — a runaway loop?); \
                 the program is still paused"
            ));
        }
        result
    }

    /// The unbudgeted core of [`Vm::run_installed_fragment`].
    fn run_installed_fragment_inner(
        &mut self,
        entry: u32,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, String> {
        match self.run_thunk(entry, &[]) {
            Ok(v) => release(v),
            Err(Abort) => return Err(self.last_diag_message()),
        }
        let Some(slot) = self
            .module
            .global_names
            .iter()
            .position(|n| n == FRAGMENT_SENTINEL)
        else {
            return Err("internal error: fragment sentinel not bound".into());
        };
        let closure = std::mem::replace(&mut self.globals[slot], Value::unbound());
        if closure.is_unbound() {
            return Err("the fragment did not produce a value".into());
        }
        for a in &args {
            retain(*a);
        }
        let result = self.call_value(closure, args, span);
        release(closure);
        result.map_err(|Abort| self.last_diag_message())
    }

    /// Service a paused-frame **`setVariable`** (U1): evaluate `value` as a console fragment (frame
    /// locals visible), then overwrite the named in-scope local's register in the selected frame.
    /// The old value is released with destructor semantics (this is a reassignment); on any error
    /// the frame is untouched. `self` is refused — replacing a method's receiver mid-body is a
    /// footgun, not a feature.
    pub(crate) fn debug_set_variable(
        &mut self,
        name: &str,
        value: &Program,
        frame: usize,
        frames: &[Frame],
        regs: &mut [Value],
    ) -> DebugEvalOutcome {
        if name == "self" {
            return DebugEvalOutcome::Error("`self` cannot be reassigned".to_string());
        }
        // Resolve the target register first, so an unknown name fails before the value evaluates.
        let Some(view_idx) = frames.len().checked_sub(frame + 1) else {
            return DebugEvalOutcome::Error(format!("no frame {frame} in the paused stack"));
        };
        let target = &frames[view_idx];
        let chunk = &self.module.protos[target.proto as usize];
        // The frame's current line, with the same innermost/caller pc adjustment `DebugView::frame`
        // makes — the in-scope filter must match what the Variables panel showed.
        let pc = if view_idx + 1 == frames.len() {
            target.pc
        } else {
            target.pc.saturating_sub(1)
        };
        let here = chunk.line_span(pc);
        let Some(reg) = chunk
            .debug_locals
            .iter()
            .find(|ld| {
                ld.name == name
                    && match here {
                        Some(h) => ld.def_span.start < h.start,
                        None => true,
                    }
            })
            .map(|ld| ld.reg as usize)
        else {
            return DebugEvalOutcome::Error(format!("no variable `{name}` in scope"));
        };
        let slot = target.base + reg;
        match self.eval_fragment_owned(value, frame, false, None, frames, regs) {
            Ok(v) => {
                let text = v.display();
                let ty = v.type_display();
                let old = std::mem::replace(&mut regs[slot], v);
                self.release_value(old);
                DebugEvalOutcome::Value { text, ty }
            }
            Err(msg) => DebugEvalOutcome::Error(msg),
        }
    }

    /// The message of the most recently recorded diagnostic, to surface a watch call's abort as text.
    fn last_diag_message(&self) -> String {
        self.out
            .diagnostics
            .last()
            .map(|d| d.message.clone())
            .unwrap_or_else(|| "the call could not be evaluated".to_string())
    }
}
