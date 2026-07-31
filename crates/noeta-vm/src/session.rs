//! The REPL session on the VM (REPL-on-VM R1).
//!
//! A [`VmSession`] runs many REPL entries against **one persistent runtime** so bindings, `fn` /
//! `type` / `enum` / `class` declarations, channels, the reactive graph, and the host's id / entropy /
//! clock state survive between entries — the same continuity the tree-walker `noeta_eval::Session`
//! gave the REPL, now on the production VM so the oracle backend can be cut from the shipped binary.
//! (The `next_id()` counter now lives on the `Host`, which the session persists, so it carries across
//! entries for free.)
//!
//! The design keeps the hot path untouched (see `plans/repl-on-vm`): the `Vm` struct and its dispatch
//! loop are unchanged. Each entry builds an **ephemeral** [`Vm`] over the session's persistent
//! [`SessionState`] via [`Vm::load_seeded`], runs the entry chunk with [`Vm::run_top`] (no teardown),
//! then moves the state back out with [`Vm::into_state`]. One [`Vm::teardown`] runs at session end,
//! bringing heap residency to zero.

use std::rc::Rc;

// The trailing-expression desugar + its sentinel live in `noeta_ast::desugar` (audit-3
// finding 10), shared with the `noeta-eval` oracle session so the two backends agree by
// construction (the `session_parity` differential gates).
use noeta_ast::Program;
use noeta_ast::desugar::{REPL_VALUE as SENTINEL, rewrite_trailing_expr};
use noeta_backend::TraceFrame;
use noeta_bytecode::{Module, PackedFieldDef};
use noeta_compiler::SessionCompiler;
use noeta_diagnostics::Diagnostic;
use noeta_object::{PackedKind, PackedSchema};
use noeta_span::{SourceId, Span};
use noeta_stdlib::{Executor, Host};
use noeta_value::Value;

use crate::{SessionState, Vm, release, retain};

/// A factory for a fresh host + executor pair — the session builds one at construction and again on
/// `:reset`, so a reset REPL starts against the same *kind* of environment (a real host, or the
/// deterministic sandbox) without the session having to know which. Mirrors the isolate factory.
pub type HostFactory = Box<dyn Fn() -> (Box<dyn Host>, Box<dyn Executor>)>;

impl SessionState {
    /// A fresh runtime: empty tables, id counter at 1, and the given host + executor.
    fn fresh(host: Box<dyn Host>, executor: Box<dyn Executor>) -> SessionState {
        SessionState {
            globals: Vec::new(),
            global_order: Vec::new(),
            channels: Vec::new(),
            channel_progress: 0,
            ext_arena: Vec::new(),
            ext_arena_free: Vec::new(),
            embed_handles: Vec::new(),
            embed_handles_free: Vec::new(),
            ext_state: Vec::new(),
            ext_closed_gates: Vec::new(),
            shapes: Vec::new(),
            packed_schemas: Vec::new(),
            type_reprs: Vec::new(),
            host,
            executor,
            registry: None,
        }
    }

    /// Grow the append-only derived tables to match `module` after an entry added new types. New
    /// shapes / packed-schemas / type-reprs are built here (fresh `Rc`s); the existing prefix is
    /// untouched, so old values keep their shape identity. New global slots start unbound.
    fn sync_to(&mut self, module: &Module) {
        if module.global_names.len() > self.globals.len() {
            self.globals
                .resize(module.global_names.len(), Value::unbound());
        }
        for shape in &module.shapes[self.shapes.len()..] {
            self.shapes.push(noeta_object::intern_shape(shape.clone()));
        }
        // Packed schemas are interned inner-before-outer, so a new schema's referenced shape index and
        // nested-schema index are already present in the grown tables above / earlier in this loop.
        for def in &module.packed_schemas[self.packed_schemas.len()..] {
            let fields = def
                .fields
                .iter()
                .map(|f| match f {
                    PackedFieldDef::Int => PackedKind::Int,
                    PackedFieldDef::Float => PackedKind::Float,
                    PackedFieldDef::F32 => PackedKind::F32,
                    PackedFieldDef::F64 => PackedKind::F64,
                    PackedFieldDef::IntN { bits, signed } => PackedKind::IntN {
                        bits: *bits,
                        signed: *signed,
                    },
                    PackedFieldDef::Bool => PackedKind::Bool,
                    PackedFieldDef::Struct(idx) => {
                        PackedKind::Struct(self.packed_schemas[*idx as usize])
                    }
                })
                .collect();
            self.packed_schemas
                .push(noeta_object::intern_schema(PackedSchema {
                    // A bare-scalar element carries no shape (`None`) — see `PackedSchema::shape`.
                    shape: def.shape.map(|i| self.shapes[i as usize]),
                    fields,
                    byte_size: def.byte_size as usize,
                    column: def.column,
                }));
        }
        for repr in &module.type_reprs[self.type_reprs.len()..] {
            self.type_reprs.push(Rc::new(repr.clone()));
        }
    }
}

impl<'m> Vm<'m> {
    /// Build a `Vm` for one REPL entry, seeded with the session's persistent `state` instead of a
    /// fresh runtime — **one move** into [`Vm::load_with`] (audit-1 finding 4). The caller has
    /// already `sync_to`'d `state`'s derived tables to `module`; `load_with` builds only the
    /// module-derived *name* tables and per-entry scratch, and rebuilds `map_packed` against the
    /// seeded (identity-preserving) schemas.
    fn load_seeded(module: &'m Module, state: SessionState) -> Vm<'m> {
        Vm::load_with(module, state)
    }

    /// Move the persistent runtime state back out of the `Vm` after an entry ran (the ephemeral `Vm`
    /// is then dropped; its per-entry scratch — empty scopes, drained stdout/diagnostics, no isolates
    /// — drops cleanly, `Vm` having no `Drop`). The next entry re-seeds from this. One move: a
    /// persistent field added to [`SessionState`] rides along by construction (audit-1 finding 4).
    fn into_state(self) -> SessionState {
        self.persist
    }
}

/// The outcome of one [`VmSession::eval`]: this entry's stdout, diagnostics, the display form of a
/// trailing bare expression (for the REPL to echo), and the abort traceback if it panicked. Mirrors
/// `noeta_eval::SessionOutput` field-for-field so the CLI's REPL rendering is backend-agnostic and the
/// R2 session differential can compare the two directly.
#[derive(Debug, Clone)]
pub struct SessionOutput {
    pub stdout: String,
    /// This entry's standard-error output (`std.io`'s `err`/`errln`), the stderr twin of `stdout`.
    pub stderr: String,
    pub diagnostics: Vec<Diagnostic>,
    pub value: Option<String>,
    /// The abort traceback if this entry panicked (empty otherwise), innermost frame first. A frame
    /// from a function defined in an *earlier* entry carries a span into that entry's now-gone text;
    /// the CLI's renderer degrades such a frame to name-only.
    pub trace: Vec<TraceFrame>,
}

/// An opaque, GC-rooted reference to a live language value the embedding host keeps across calls
/// (server-hmr F3). Minted by [`VmSession::call_retaining`], read via [`VmSession::read_handle`],
/// freed via [`VmSession::release_handle`]. A plain index — meaningful only until released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmbedHandle(u32);

impl EmbedHandle {
    pub(crate) fn from_index(idx: u32) -> EmbedHandle {
        EmbedHandle(idx)
    }
    pub(crate) fn index(self) -> u32 {
        self.0
    }
}

/// One argument to [`VmSession::call_retaining`]: either a value marshalled in, or a handle to a
/// value the session already holds (passed back without a round-trip through the host).
#[derive(Debug)]
pub enum EmbedArg {
    Value(noeta_stdlib::NativeOut),
    Handle(EmbedHandle),
}

enum CallResult {
    Value(noeta_stdlib::NativeValue),
    Handle(EmbedHandle),
}

/// Why a [`VmSession::call_by_name`] returned no value (server-hmr E0).
#[derive(Debug)]
pub enum CallError {
    /// No top-level binding of this name exists in the session.
    NoSuchFunction(String),
    /// The call aborted (a panic, or the binding was not callable): stdout-so-far, the
    /// diagnostics, and the traceback ride in the output. The session survives.
    Aborted(Box<SessionOutput>),
}

/// A persistent REPL session on the bytecode VM. Owns the incremental [`SessionCompiler`] and the
/// [`SessionState`]; each [`VmSession::eval`] compiles one entry against the accumulated tables and
/// runs it against the persistent globals.
pub struct VmSession {
    compiler: SessionCompiler,
    factory: HostFactory,
    /// `Some` between entries; taken (and put back) transiently inside [`VmSession::eval`].
    state: Option<SessionState>,
    /// An optional liveness/observation [`Debugger`](crate::Debugger) installed on every entry's
    /// ephemeral [`Vm`] (see [`VmSession::set_debugger`]). Held here between entries and lent to the
    /// entry's `Vm` for the duration of its run — a runaway loop inside a session entry (an `eval`
    /// fragment, a `test` case) is bounded exactly as a `run` is, over the same per-op seam. `None`
    /// on the REPL / differential paths, where it costs one predicted branch per entry.
    debugger: Option<Box<dyn crate::Debugger>>,
}

impl std::fmt::Debug for VmSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmSession")
            .field("compiler", &self.compiler)
            .finish_non_exhaustive()
    }
}

impl VmSession {
    /// Start a session whose entries run against host + executor pairs from `factory` (a real host for
    /// `noeta repl`, the deterministic sandbox for tests / the session differential). The factory is
    /// called once now and again on [`VmSession::reset`].
    pub fn new(factory: HostFactory) -> VmSession {
        let (host, executor) = factory();
        // Register a live session heap-owner on this thread (bugfix): so a *sibling* session's
        // teardown does not mark this session's live objects as garbage. Retired in `teardown`.
        crate::session_owner_enter();
        VmSession {
            compiler: SessionCompiler::new(),
            factory,
            state: Some(SessionState::fresh(host, executor)),
            debugger: None,
        }
    }

    /// Install (or clear with `None`) a [`Debugger`](crate::Debugger) consulted before every
    /// instruction of every subsequent entry. The debugger is held on the session between entries
    /// and lent to each entry's ephemeral [`Vm`] for its run, so a single instance accumulates
    /// across the entries of one session — the seam an embedding uses to bound a session's liveness
    /// (an MCP `eval`/`test` arms a step-count + wall-clock limit debugger here, the same one a
    /// `run` installs, so a runaway loop terminates in-VM instead of hanging the caller).
    pub fn set_debugger(&mut self, debugger: Option<Box<dyn crate::Debugger>>) {
        self.debugger = debugger;
    }

    /// A session **adopted from a checked compile** (tooling-unification T3): run `module` — the
    /// snapshot [`noeta_compiler::compile_with_sites_session`] returned alongside `compiler` — to
    /// completion as entry 0, then continue the session incrementally from its final state. A
    /// fragment evaluated afterwards resolves the checked program's globals, functions, types, and
    /// methods by their **original ids** (the compiler's tables are the module's own id-spaces), and
    /// values entry 0 created keep full interned `&'static Shape` identity in later entries. The initial run is
    /// fully checked; fragments are checkerless, exactly like REPL entries.
    ///
    /// Returns the session plus entry 0's output (its stdout/diagnostics/trace — a debug console or
    /// a future `repl --load` replays these before the first prompt).
    pub fn adopted(
        module: &Module,
        compiler: SessionCompiler,
        factory: HostFactory,
    ) -> (VmSession, SessionOutput) {
        VmSession::adopted_with_registry(module, compiler, factory, None)
    }

    /// As [`VmSession::adopted`], but resolving native names against an explicit `registry`
    /// (instance-registry IR5) — the seam an embedding host with its own assembled extension set
    /// threads in so **every** entry of the session (the launch run and each later fragment /
    /// hot-swap) dispatches against *its* extensions. The registry rides in the persistent
    /// [`SessionState`], so it survives the state's round-trip through every entry. `None` keeps the
    /// session on the process-global default — exactly what [`VmSession::adopted`] passes.
    pub fn adopted_with_registry(
        module: &Module,
        compiler: SessionCompiler,
        factory: HostFactory,
        registry: Option<&'static noeta_stdlib::registry::Registry>,
    ) -> (VmSession, SessionOutput) {
        // Register a live session heap-owner on this thread (bugfix) — see `VmSession::new`.
        crate::session_owner_enter();
        let (host, executor) = factory();
        let mut state = SessionState::fresh(host, executor);
        state.registry = registry;
        state.sync_to(module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        // Arm the safepoint-GC trigger for this entry (step relative to the session's current
        // residency, so persistent state is never charged against the watermark).
        noeta_value::safepoint_gc_arm(noeta_value::safepoint_gc_default_threshold());
        let mut vm = Vm::load_seeded(module, state);
        vm.run_top();
        let stdout = std::mem::take(&mut vm.out.stdout);
        let stderr = std::mem::take(&mut vm.out.stderr);
        let diagnostics = std::mem::take(&mut vm.out.diagnostics);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        let session = VmSession {
            compiler,
            factory,
            state: Some(vm.into_state()),
            debugger: None,
        };
        (
            session,
            SessionOutput {
                stdout,
                stderr,
                diagnostics,
                value: None,
                trace,
            },
        )
    }

    /// Evaluate one REPL entry against the persistent scope. Returns this entry's stdout, diagnostics,
    /// and — when the final statement is a bare expression — the display form of its non-unit value.
    ///
    /// The entry is compiled by the incremental [`SessionCompiler`] (checkerless, matching the
    /// tree-walker REPL), its derived tables are appended to the persistent state, and its entry chunk
    /// runs against the shared globals with **no teardown**, so bindings and declarations survive into
    /// the next entry.
    pub fn eval(&mut self, program: &Program) -> SessionOutput {
        // Echo a trailing bare expression's **non-unit** value in its display form (`1 + 2` → `3`).
        self.run_capturing(program, None, |v| (!v.is_unit()).then(|| v.display()))
    }

    /// [`VmSession::eval`] with the checker's accumulated [`Sites`] bundle (session-checker C5):
    /// the entry compiles through [`SessionCompiler::extend_checked`] — the same site-driven
    /// codegen the file pipeline runs (packed lists, `type_of` full fidelity, method handles,
    /// precise destructor relevance). The caller is responsible for the soundness gate: only a
    /// session the checker has seen IN FULL may take this path (precise relevance from a registry
    /// that missed an unchecked entry's destructor class could skip a destructor).
    ///
    /// [`Sites`]: noeta_compiler::Sites
    pub fn eval_checked(
        &mut self,
        program: &Program,
        sites: &noeta_compiler::Sites,
    ) -> SessionOutput {
        self.run_capturing(program, Some(sites), |v| {
            (!v.is_unit()).then(|| v.display())
        })
    }

    /// Apply a hot-swap plan (server-hmr H0/H1): re-evaluate the plan's fragment — added/changed
    /// `use` imports, changed/added `fn` declarations, method-level type re-declarations, and (on
    /// a re-running swap) the new top-level statements — as one session entry. Running the
    /// fragment *is* the swap: each `fn` declaration stores a fresh closure into its **existing**
    /// global slot, so every live `Op::CallGlobal` site (old and new code alike) dispatches to the
    /// new body from now on; a re-declared type re-registers its methods against the same
    /// content-interned shape, so existing instances flow into the new bodies.
    ///
    /// A **re-running** swap (`plan.rerun_top_level`) implements the HMR state rule — *reactive
    /// state survives edits; plain state re-initializes*: the plan withholds unchanged reactive
    /// anchors (their live nodes survive untouched in their global slots), and before the
    /// fragment runs, the previous epoch's effects are disposed (the re-run re-creates them) and
    /// the reactive nodes held by re-bound globals are disposed (their replacements arrive with
    /// the re-run). Top-level side effects (an `echo`, a write) DO re-run — their output lands in
    /// the returned [`SessionOutput`] for the driver to surface.
    ///
    /// The caller owns the two gates the plan's existence implies (see
    /// [`noeta_compiler::hotswap::diff_programs`]): the NEW program checked green (transactional —
    /// never swap red code), and the differ found no blockers.
    ///
    /// `sites` is that green check's **whole-program** bundle (server-hmr H5). Supplying it
    /// compiles the fragment exactly as a cold start of the new version compiles it — packed
    /// lists, `type_of` full fidelity, decode recipes, call-site-typed native calls, precise
    /// destructor relevance — because the fragment's statements are cloned from the checked
    /// program with their real spans and a bundle is span-keyed. `None` is the checkerless,
    /// conservative compile: always sound, silently degraded. Only a session the checker has seen
    /// IN FULL may pass `Some` (a driver that also runs unchecked [`VmSession::eval`] entries has
    /// not — precise relevance derived without them could skip a destructor).
    ///
    /// A function *value* captured before the swap (`mut h = f`) keeps the old body by design —
    /// closures hold their proto directly; only slot-routed calls rebind.
    pub fn hot_swap(
        &mut self,
        plan: &noeta_compiler::hotswap::SwapPlan,
        sites: Option<&noeta_compiler::Sites>,
    ) -> SessionOutput {
        // Slots the fragment's binding statements will overwrite — resolved BEFORE the fragment
        // compiles, so only names that already exist (v1 bindings being replaced) are collected;
        // genuinely new bindings have no old node to dispose.
        let rebound: Vec<u32> = if plan.rerun_top_level {
            plan.fragment
                .stmts
                .iter()
                .flat_map(crate::binding_targets)
                .filter_map(|name| self.compiler.global_slots().get(name).copied())
                .collect()
        } else {
            Vec::new()
        };
        let prepare = plan.rerun_top_level;
        self.run_capturing_with(
            &plan.fragment,
            sites,
            |vm| {
                if prepare {
                    vm.hotswap_prepare(&rebound);
                }
            },
            |_| None,
        )
    }

    /// `:type <expr>` — evaluate `program`'s trailing expression and report its **runtime** type. The
    /// REPL runs no checker across entries, so the type is read from the produced value (like the
    /// language's `type_of`), which means the expression is evaluated and any side effects run. Uses
    /// reflection + the `TypeRepr` surface spelling (`List<int>`), falling back to the runtime kind
    /// name for an untagged primitive — the same rendering the debugger shows.
    pub fn type_of(&mut self, program: &Program) -> SessionOutput {
        // Conservative codegen deliberately: a `:type` query defines nothing worth optimizing, and
        // the conservative path is always sound regardless of the session's checkedness.
        self.run_capturing(program, None, |v| Some(v.type_display()))
    }

    /// Compile and run one entry, then — if the final statement was a bare expression — hand its
    /// captured value to `describe` for the returned `value` field. Shared by [`VmSession::eval`]
    /// (which displays) and [`VmSession::type_of`] (which reports the type). The value is **unbound +
    /// released** after `describe`, so an evaluated value neither lingers as a binding nor pins a
    /// refcount across entries (which would leak it and suppress a later `:drop`'s destructor);
    /// releasing runs its destructor now, matching the tree-walker, which drops it at batch end.
    fn run_capturing(
        &mut self,
        program: &Program,
        sites: Option<&noeta_compiler::Sites>,
        describe: impl FnOnce(Value) -> Option<String>,
    ) -> SessionOutput {
        self.run_capturing_with(program, sites, |_| {}, describe)
    }

    /// [`VmSession::run_capturing`] with a pre-run hook, invoked on the seeded ephemeral `Vm`
    /// after the entry compiled but before it runs — the hot-swap disposal window
    /// ([`VmSession::hot_swap`]).
    fn run_capturing_with(
        &mut self,
        program: &Program,
        sites: Option<&noeta_compiler::Sites>,
        pre_run: impl FnOnce(&mut Vm),
        describe: impl FnOnce(Value) -> Option<String>,
    ) -> SessionOutput {
        // A trailing bare expression is rewritten to `mut <sentinel> = expr;` so the IR path captures
        // its value in a global slot we read back below — pure AST surgery, backend-agnostic.
        let (lowerable, captures_value) = rewrite_trailing_expr(program);
        let module = match sites {
            None => self.compiler.extend(&lowerable),
            Some(sites) => self.compiler.extend_checked(&lowerable, sites),
        }
        .expect(
            "checkerless lowering is total over parsed programs (the REPL only feeds parsed input)",
        );

        let mut state = self
            .state
            .take()
            .expect("session state is present between entries");
        state.sync_to(&module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        // Arm the safepoint-GC trigger for this entry (step relative to the session's current
        // residency, so persistent state is never charged against the watermark).
        noeta_value::safepoint_gc_arm(noeta_value::safepoint_gc_default_threshold());
        let mut vm = Vm::load_seeded(&module, state);
        // Lend the session's debugger to this entry's ephemeral Vm for the run, then take it back
        // (the dispatch loop restores it into `vm.debugger` on both a clean finish and a
        // `Terminate`, so it is always present here). A single instance thus accumulates its
        // step/deadline budget across the session's entries.
        vm.debugger = self.debugger.take();
        pre_run(&mut vm);
        vm.run_top();
        self.debugger = vm.debugger.take();

        let value = if captures_value {
            self.sentinel_slot().and_then(|slot| {
                let v = std::mem::replace(&mut vm.persist.globals[slot as usize], Value::unbound());
                // Unbound means the entry errored (or returned) before the sentinel binding ran.
                if v.is_unbound() {
                    return None;
                }
                let described = describe(v);
                // Free the discarded trailing value with a **plain** release (no `destruct`), matching
                // the tree-walker REPL, which drops the extracted value as a host value rather than
                // through the interpreter's destructor path. A *bound* value's destructor still fires —
                // at `:drop` or teardown (which use `release_value`) — just not on a bare-expression echo.
                release(v);
                described
            })
        } else {
            None
        };

        let stdout = std::mem::take(&mut vm.out.stdout);
        let stderr = std::mem::take(&mut vm.out.stderr);
        let diagnostics = std::mem::take(&mut vm.out.diagnostics);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        self.state = Some(vm.into_state());
        SessionOutput {
            stdout,
            stderr,
            diagnostics,
            value,
            trace,
        }
    }

    /// Call a top-level function **by name** (the embed seam, server-hmr E0): arguments arrive as
    /// neutral [`NativeOut`]s (materialized into fresh values the callee's frame consumes), the
    /// result returns as the neutral deep [`NativeValue`] view — no fragment compilation per
    /// call, so a host loop (a game engine's `update(dt)`) pays lookup + call, not a compile.
    /// Anything the callee printed rides in the returned [`SessionOutput`]; a panic inside it
    /// comes back as [`CallError::Aborted`] with the traceback, the session intact.
    ///
    /// [`NativeOut`]: noeta_stdlib::NativeOut
    /// [`NativeValue`]: noeta_stdlib::NativeValue
    pub fn call_by_name(
        &mut self,
        name: &str,
        args: Vec<noeta_stdlib::NativeOut>,
    ) -> Result<(noeta_stdlib::NativeValue, SessionOutput), CallError> {
        let args = args.into_iter().map(EmbedArg::Value).collect();
        let (result, out) = self.call_internal(name, args, false)?;
        match result {
            CallResult::Value(v) => Ok((v, out)),
            CallResult::Handle(_) => unreachable!("retain_result was false"),
        }
    }

    /// Call `name` with a mix of marshalled values and handles, returning the deep result value
    /// (server-hmr F3): the handle-argument twin of [`Self::call_by_name`].
    pub fn call_mixed(
        &mut self,
        name: &str,
        args: Vec<EmbedArg>,
    ) -> Result<(noeta_stdlib::NativeValue, SessionOutput), CallError> {
        let (result, out) = self.call_internal(name, args, false)?;
        match result {
            CallResult::Value(v) => Ok((v, out)),
            CallResult::Handle(_) => unreachable!("retain_result was false"),
        }
    }

    /// Call `name`, **retaining** its result as an embed handle (server-hmr F3) — the host keeps
    /// the live value across frames without marshalling it out, and passes it back via
    /// [`EmbedArg::Handle`]. Read a handle's current value with [`Self::read_handle`]; free it with
    /// [`Self::release_handle`] (a forgotten handle reclaims at teardown). Arguments may mix
    /// marshalled values and handles.
    pub fn call_retaining(
        &mut self,
        name: &str,
        args: Vec<EmbedArg>,
    ) -> Result<(EmbedHandle, SessionOutput), CallError> {
        let (result, out) = self.call_internal(name, args, true)?;
        match result {
            CallResult::Handle(h) => Ok((h, out)),
            CallResult::Value(_) => unreachable!("retain_result was true"),
        }
    }

    fn call_internal(
        &mut self,
        name: &str,
        args: Vec<EmbedArg>,
        retain_result: bool,
    ) -> Result<(CallResult, SessionOutput), CallError> {
        let Some(slot) = self.compiler.global_slots().get(name).copied() else {
            return Err(CallError::NoSuchFunction(name.to_string()));
        };
        let module = self
            .compiler
            .extend(&empty_program())
            .expect("an empty program compiles");
        let mut state = self.state.take().expect("state present between entries");
        state.sync_to(&module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        // Arm the safepoint-GC trigger for this entry (step relative to the session's current
        // residency, so persistent state is never charged against the watermark).
        noeta_value::safepoint_gc_arm(noeta_value::safepoint_gc_default_threshold());
        let mut vm = Vm::load_seeded(&module, state);
        let callee = vm.persist.globals[slot as usize];
        if callee.is_unbound() {
            self.state = Some(vm.into_state());
            return Err(CallError::NoSuchFunction(name.to_string()));
        }
        // Materialize each argument: a marshalled value is built fresh; a handle's stored value is
        // read and retained (`+1`) so the callee's frame consumes its own reference and the
        // host's handle survives.
        let arg_values: Vec<Value> = args
            .into_iter()
            .map(|a| match a {
                EmbedArg::Value(out) => crate::values::materialize_native(out),
                EmbedArg::Handle(h) => {
                    let v = vm.persist.embed_handles[h.0 as usize].expect("a live embed handle");
                    retain(v);
                    v
                }
            })
            .collect();
        let outcome = vm.call_value(callee, arg_values, Span::new(0, 0));
        let result = match outcome {
            Ok(v) if retain_result => Some(CallResult::Handle(vm.embed_handle_store(v))),
            Ok(v) => {
                let native = v.to_native_deep();
                release(v);
                Some(CallResult::Value(native))
            }
            Err(_) => None,
        };
        let output = SessionOutput {
            stdout: std::mem::take(&mut vm.out.stdout),
            stderr: std::mem::take(&mut vm.out.stderr),
            diagnostics: std::mem::take(&mut vm.out.diagnostics),
            value: None,
            trace: std::mem::take(&mut vm.out.abort_trace),
        };
        self.state = Some(vm.into_state());
        match result {
            Some(r) => Ok((r, output)),
            None => Err(CallError::Aborted(Box::new(output))),
        }
    }

    /// The current value behind an embed handle, as the neutral deep view (F3). Reading does not
    /// consume the handle.
    pub fn read_handle(&mut self, handle: EmbedHandle) -> noeta_stdlib::NativeValue {
        let state = self.state.as_ref().expect("state present between entries");
        state.embed_handles[handle.0 as usize]
            .expect("a live embed handle")
            .to_native_deep()
    }

    /// Release an embed handle (F3): drop the host's reference, destructor-aware. Its slot is
    /// reused by a later retain. Releasing a handle twice is a misuse the debug build catches.
    pub fn release_handle(&mut self, handle: EmbedHandle) {
        let module = self
            .compiler
            .extend(&empty_program())
            .expect("an empty program compiles");
        let mut state = self.state.take().expect("state present between entries");
        state.sync_to(&module);
        let mut vm = Vm::load_seeded(&module, state);
        vm.embed_handle_release(handle);
        self.state = Some(vm.into_state());
    }

    /// Run a binding's destructor now and unbind it (`:drop` / `:free`), returning whether a binding
    /// existed. The REPL's top-level bindings are globals with extended lifetime (they never auto-fire
    /// a destructor), so this is how one is observed or an object reclaimed interactively; any
    /// destructor output lands in the returned [`SessionOutput`].
    pub fn drop_binding(&mut self, name: &str) -> (bool, SessionOutput) {
        let Some(slot) = self.compiler.global_slots().get(name).copied() else {
            return (false, SessionOutput::empty());
        };
        // Compile an empty entry to snapshot a module with the accumulated types (so a destructor
        // resolves), then release the bound slot on an ephemeral Vm and extract the state back.
        let module = self
            .compiler
            .extend(&empty_program())
            .expect("an empty program compiles");
        let mut state = self.state.take().expect("state present between entries");
        state.sync_to(&module);
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        // Arm the safepoint-GC trigger for this entry (step relative to the session's current
        // residency, so persistent state is never charged against the watermark).
        noeta_value::safepoint_gc_arm(noeta_value::safepoint_gc_default_threshold());
        let mut vm = Vm::load_seeded(&module, state);
        let found = {
            let v = std::mem::replace(&mut vm.persist.globals[slot as usize], Value::unbound());
            if v.is_unbound() {
                false
            } else {
                vm.release_value(v);
                true
            }
        };
        let stdout = std::mem::take(&mut vm.out.stdout);
        let stderr = std::mem::take(&mut vm.out.stderr);
        let diagnostics = std::mem::take(&mut vm.out.diagnostics);
        self.state = Some(vm.into_state());
        (
            found,
            SessionOutput {
                stdout,
                stderr,
                diagnostics,
                value: None,
                trace: Vec::new(),
            },
        )
    }

    /// The live **user** binding names (`:bindings`) — every global slot that is currently bound,
    /// excluding the trailing-expression sentinel. (The VM has no prelude *globals* — prelude values
    /// like `Ok` / `panic` are intrinsic, not slots — so unlike the tree-walker there is nothing else
    /// to filter out.)
    pub fn binding_names(&self) -> Vec<String> {
        let state = self.state.as_ref().expect("state present between entries");
        self.compiler
            .global_names()
            .iter()
            .enumerate()
            .filter(|(slot, name)| {
                name.as_str() != SENTINEL
                    && state.globals.get(*slot).is_some_and(|v| !v.is_unbound())
            })
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// Reset to a fresh session — a new compiler, globals, id counter, and reactive graph (`:reset`).
    /// Tears the current state down first so residency returns to zero, then rebuilds from the factory.
    pub fn reset(&mut self) {
        self.teardown();
        let (host, executor) = (self.factory)();
        self.compiler = SessionCompiler::new();
        self.state = Some(SessionState::fresh(host, executor));
        // The teardown above retired the old owner; the fresh state is a new one (bugfix).
        crate::session_owner_enter();
    }

    /// Tear the session's runtime down, bringing heap residency to zero (destructors fire on the live
    /// globals in reverse binding order). Leaves the session with no state; call at session end, or
    /// before rebuilding in [`VmSession::reset`]. Idempotent — a second call is a no-op.
    pub fn teardown(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        // Retire this session's heap-owner *before* `Vm::teardown` (bugfix): so the remaining count
        // the sweep gates on reflects only the SIBLING sessions still alive on this thread. If a
        // sibling is alive, the sweep is skipped (its live objects must survive); this session's own
        // cycle garbage is reaped by the last owner's teardown instead — never double-freed.
        crate::session_owner_exit();
        // Snapshot a module with the accumulated types so each global's destructor resolves during
        // teardown, then run the VM's end-of-program teardown on it (globals destroyed in reverse
        // binding order, cycles reaped, channels drained, reactive graph cleared).
        let module = self
            .compiler
            .extend(&empty_program())
            .expect("an empty program compiles");
        state.sync_to(&module);
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let mut vm = Vm::load_seeded(&module, state);
        vm.teardown(mode);
    }

    /// The global slot the trailing-expression sentinel is interned at, once any entry has used one.
    fn sentinel_slot(&self) -> Option<u32> {
        self.compiler.global_slots().get(SENTINEL).copied()
    }
}

impl SessionOutput {
    fn empty() -> SessionOutput {
        SessionOutput {
            stdout: String::new(),
            stderr: String::new(),
            diagnostics: Vec::new(),
            value: None,
            trace: Vec::new(),
        }
    }
}

/// The compiler-free seam the VM core drives fragment installs through (native-size slice 2): the
/// real incremental compiler, adapted to [`crate::FragmentCompiler`] so `DebugSession`,
/// `install_fragment`, and the hot-swap apply never name `noeta-compiler`. This impl — the only
/// implementor — lives in the `compile`-gated module, so a shipped AOT binary links none and sheds
/// the whole front-end.
impl crate::FragmentCompiler for SessionCompiler {
    fn extend(&mut self, fragment: &Program) -> Result<Module, String> {
        SessionCompiler::extend(self, fragment).map_err(|u| u.reason.clone())
    }

    /// Recover the checker's own [`noeta_compiler::Sites`] from the opaque handle the VM core
    /// ferried (see [`crate::FragmentSites`]) and lower the fragment against it. The downcast
    /// cannot fail in practice — this impl is the only consumer and the `Sites` impl below the only
    /// producer — so a mismatch is an internal error reported as a *failed swap*: the old version
    /// keeps serving rather than a degraded compile landing unannounced.
    fn extend_checked(
        &mut self,
        fragment: &Program,
        sites: &dyn crate::FragmentSites,
    ) -> Result<Module, String> {
        let sites = sites
            .as_any()
            .downcast_ref::<noeta_compiler::Sites>()
            .ok_or_else(|| {
                "internal error: the fragment's site bundle is not this compiler's `Sites`"
                    .to_string()
            })?;
        SessionCompiler::extend_checked(self, fragment, sites).map_err(|u| u.reason.clone())
    }

    fn global_slot(&self, name: &str) -> Option<u32> {
        self.global_slots().get(name).copied()
    }

    fn declare_global(&mut self, name: &str, mutable: bool, overwrite: bool) {
        SessionCompiler::declare_global(self, name, mutable, overwrite);
    }
}

/// The checker's bundle, viewed through the VM core's opaque site seam (server-hmr H5): a hot
/// deposit travels the compiler-free mailbox as a [`crate::FragmentSites`] handle and is recovered
/// here — the one place that may name `noeta_compiler`. Lives in the `compile`-gated module for the
/// same reason the [`crate::FragmentCompiler`] impl above does.
impl crate::FragmentSites for noeta_compiler::Sites {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// An empty program — used to snapshot a module carrying the session's accumulated types (for `:drop`
/// / teardown) without running any new top-level code.
fn empty_program() -> Program {
    Program {
        stmts: Vec::new(),
        span: Span::empty_at_in(SourceId::FIRST, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::Source;

    fn sandbox_session() -> VmSession {
        noeta_stdlib::registry::default_seeded();
        VmSession::new(Box::new(|| {
            (
                Box::new(noeta_stdlib::SandboxHost::new()),
                Box::new(noeta_stdlib::SandboxExecutor::new()),
            )
        }))
    }

    fn program(src: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "<repl>", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "test source should parse cleanly: {src:?}"
        );
        parsed.program
    }

    /// Evaluate `src` as one entry and return its stdout.
    fn eval(session: &mut VmSession, src: &str) -> String {
        session.eval(&program(src)).stdout
    }

    /// Compile `src` as a **checked** program keeping the compiler alive — the dance the debug
    /// adapter's launch path runs (T3). Panics on any parse/check/compile failure.
    fn checked_session(src: &str) -> (noeta_bytecode::Module, noeta_compiler::SessionCompiler) {
        noeta_stdlib::registry::default_seeded();
        let source = Source::new(SourceId::FIRST, "<file>", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "test source should parse cleanly: {src:?}"
        );
        let checked = noeta_check::check_all(&parsed.program);
        assert!(
            checked.diagnostics.is_empty(),
            "test source should check cleanly: {:?}",
            checked.diagnostics
        );
        noeta_compiler::compile_with_sites_session(&parsed.program, checked.sites, false, true)
            .expect("a checked program compiles")
    }

    #[test]
    fn an_adopted_session_extends_a_checked_program_with_stable_ids() {
        let before = noeta_value::live_count();
        let (module, compiler) = checked_session(
            "struct P { x: int }\n\
             fn twice(n: int): int { return n * 2 }\n\
             mut base = 10\n\
             mut p0 = P { x: 3 }\n\
             echo twice(base)\n",
        );
        // Entry 0: the checked program runs to completion under the session.
        let (mut session, out0) = VmSession::adopted(
            &module,
            compiler,
            Box::new(|| {
                (
                    Box::new(noeta_stdlib::SandboxHost::new()),
                    Box::new(noeta_stdlib::SandboxExecutor::new()),
                )
            }),
        );
        assert_eq!(out0.stdout, "20\n");
        assert!(out0.diagnostics.is_empty(), "{:?}", out0.diagnostics);

        // A fragment calls the checked program's function and reads its global — resolved by the
        // ORIGINAL proto index / global slot (stable-prefix accumulation).
        assert_eq!(eval(&mut session, "echo twice(base + 1);"), "22\n");
        // A fragment constructs the checked program's type; structural equality against a value
        // entry 0 built proves the `&'static Shape` is the SAME shape (pointer identity), not a re-wrap.
        assert_eq!(eval(&mut session, "echo p0 == P { x: 3 };"), "true\n");
        // A fragment-defined closure captures a checked-program global and calls a checked-program
        // function — new code (a new proto) composed with original ids.
        assert_eq!(
            eval(&mut session, "mut f = fn(n: int) => twice(n) + base;"),
            ""
        );
        assert_eq!(eval(&mut session, "echo f(5);"), "20\n");
        // Rebinding the checked program's global reuses its slot.
        assert_eq!(eval(&mut session, "base = 1; echo twice(base);"), "2\n");

        session.teardown();
        assert_eq!(
            noeta_value::live_count(),
            before,
            "teardown returns residency to the pre-session baseline"
        );
    }

    /// Two sessions LIVE on ONE thread share the thread-local value heap. The backup mark-sweep in
    /// [`Vm::teardown`] reclaims everything unreachable from the tearing-down VM's roots — so under
    /// the bug it freed the *sibling* session's live objects (a cross-session double-free / heap
    /// corruption at the sibling's own teardown). The fix runs the destructive sweeps only for the
    /// LAST owner on the thread. This pins: (a) tearing `a` down while `b` is alive does not corrupt
    /// `b` (it still runs), and (b) residency returns to the baseline once the last owner tears down
    /// (an earlier session's cycle garbage is reaped then, never double-freed).
    #[test]
    fn two_sessions_sharing_a_thread_tear_down_without_cross_session_free() {
        let before = noeta_value::live_count();
        let mut a = sandbox_session();
        let mut b = sandbox_session();
        // Each session binds a native-module value AND a heap global (a list) into its persistent
        // state — the live objects a sibling's sweep wrongly reclaimed under the bug.
        assert_eq!(
            eval(
                &mut a,
                "use std.{math}\nmut xs = [\"a\"]\necho math.abs(-5);"
            ),
            "5\n"
        );
        assert_eq!(
            eval(
                &mut b,
                "use std.{math}\nmut ys = [\"b\"]\necho math.abs(-9);"
            ),
            "9\n"
        );
        // `a` is NOT the last owner (b alive), so its teardown must SKIP the destructive sweep — not
        // free b's live `math`/`ys`. Under the bug this corrupted the heap.
        a.teardown();
        // b's objects survived a's teardown — it still resolves and dispatches its native module.
        assert_eq!(eval(&mut b, "echo math.abs(-3);"), "3\n");
        // b is the last owner: its teardown reaps everything (including a's deferred cycle garbage).
        b.teardown();
        assert_eq!(
            noeta_value::live_count(),
            before,
            "residency returns to baseline once the last session on the thread tears down"
        );
    }

    #[test]
    fn an_adopted_sessions_module_snapshots_keep_a_stable_prefix() {
        let (module, mut compiler) = checked_session(
            "fn twice(n: int): int { return n * 2 }\n\
             mut base = 10\n\
             echo twice(base)\n",
        );
        let entry = {
            let source = Source::new(SourceId::FIRST, "<fragment>", "echo twice(base);");
            let lexed = lex(&source);
            parse(&source, &lexed.tokens).program
        };
        let extended = compiler.extend(&entry).expect("fragment compiles");
        // Ids are append-only: everything the checked module assigned keeps its index.
        assert!(extended.protos.len() >= module.protos.len());
        assert_eq!(
            &extended.global_names[..module.global_names.len()],
            &module.global_names[..],
            "global slots are a stable prefix"
        );
        assert_eq!(
            &extended.names[..module.names.len()],
            &module.names[..],
            "interned names are a stable prefix"
        );
        assert!(
            extended.shapes.len() >= module.shapes.len(),
            "shapes only append"
        );
    }

    #[test]
    fn bindings_and_functions_persist_across_entries() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        assert_eq!(eval(&mut session, "mut x = 10;"), "");
        assert_eq!(
            eval(&mut session, "fn double(n: int): int { return n * 2; }"),
            ""
        );
        // A later entry sees the earlier binding and the earlier function.
        assert_eq!(eval(&mut session, "echo double(x);"), "20\n");
        // Rebinding a name in a later entry updates it (same slot).
        assert_eq!(eval(&mut session, "x = 5;"), "");
        assert_eq!(eval(&mut session, "echo double(x);"), "10\n");
        session.teardown();
        assert_eq!(
            noeta_value::live_count(),
            before,
            "teardown returns residency to the pre-session baseline"
        );
    }

    #[test]
    fn a_trailing_bare_expression_echoes_its_value() {
        let mut session = sandbox_session();
        let out = session.eval(&program("1 + 2"));
        assert_eq!(out.value.as_deref(), Some("3"));
        assert_eq!(out.stdout, "");
        // A statement (not a bare trailing expression) echoes nothing.
        let out = session.eval(&program("mut y = 7;"));
        assert_eq!(out.value, None);
        // The sentinel does not leak into the user's bindings.
        assert_eq!(session.binding_names(), vec!["y".to_string()]);
        session.teardown();
    }

    #[test]
    fn the_id_counter_is_continuous_across_entries() {
        let mut session = sandbox_session();
        // The `use` import in the first entry binds `next_id` as a global that persists into later
        // entries, and the deterministic counter carries across them (it rides on the session state).
        assert_eq!(
            eval(&mut session, "use std.id.{next_id}\necho next_id();"),
            "1\n"
        );
        assert_eq!(eval(&mut session, "echo next_id();"), "2\n");
        assert_eq!(eval(&mut session, "echo next_id();"), "3\n");
        session.teardown();
    }

    #[test]
    fn a_type_and_its_methods_defined_in_one_entry_work_in_the_next() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        eval(
            &mut session,
            "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return self.v * 2; }\n}",
        );
        // Construct in one entry, store in a global...
        assert_eq!(eval(&mut session, "mut b = Box.new(21);"), "");
        // ...and call a method on it in a later entry (cross-entry object identity + dispatch).
        assert_eq!(eval(&mut session, "echo b.doubled();"), "42\n");
        session.teardown();
        assert_eq!(noeta_value::live_count(), before);
    }

    #[test]
    fn drop_runs_a_destructor_and_unbinds() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        eval(
            &mut session,
            "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}",
        );
        assert_eq!(eval(&mut session, "mut r = Res.new(7);"), "");
        assert_eq!(session.binding_names(), vec!["r".to_string()]);
        // Dropping the binding runs its destructor now and unbinds it.
        let (found, out) = session.drop_binding("r");
        assert!(found);
        assert_eq!(out.stdout, "drop 7\n");
        assert!(session.binding_names().is_empty());
        // Dropping a name that is not bound reports absence.
        let (found, _) = session.drop_binding("nope");
        assert!(!found);
        session.teardown();
        assert_eq!(noeta_value::live_count(), before);
    }

    #[test]
    fn reset_clears_bindings_and_zeroes_residency() {
        let before = noeta_value::live_count();
        let mut session = sandbox_session();
        eval(&mut session, "mut xs = [1, 2, 3];");
        assert_eq!(session.binding_names(), vec!["xs".to_string()]);
        session.reset();
        assert!(session.binding_names().is_empty());
        assert_eq!(
            noeta_value::live_count(),
            before,
            "reset tears the old state down to zero residency"
        );
        // The session is usable again after reset.
        assert_eq!(eval(&mut session, "echo 1 + 1;"), "2\n");
        session.teardown();
    }
}
