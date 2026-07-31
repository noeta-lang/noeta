//! The crate's **entry points**: [`VmBackend`] with the [`RunOptions`] →
//! [`RunOutcome`] core runner and its `run_module_*` presets, [`Vm::run_top`] +
//! [`run_and_teardown`] (the two phases a session splits), and the
//! `--jit-stats` report types ([`JitStats`], [`JitReport`], [`JitBailSite`],
//! [`JitDeclinedLoop`]).

#[cfg(feature = "compile")]
use crate::{DebugSession, Program, Unsupported, compile};
use crate::{
    Debugger, Frame, HotSwapMailbox, IsolateFactory, Module, ProfileHook, ProfileHookFactory,
    ProfileSink, RetTransform, Vm, release,
};
#[cfg(feature = "compile")]
use noeta_backend::Backend;
use noeta_backend::{RunResult, TraceFrame};
#[cfg(feature = "compile")]
use std::collections::HashMap;
use std::sync::Arc;

/// The bytecode-VM backend.
#[derive(Debug, Clone, Default)]
pub struct VmBackend;

/// How the tier-1 JIT participates in a run — one axis instead of a `run_module_*`
/// variant per combination (audit-1 finding 14).
///
/// - `Off` pins tier-0. The debugger/profiler paths **require** this: every frame stays
///   interpreter-executed and therefore observable (a JIT'd region has no readable pc or
///   register file mid-execution); tier-0 is held observably identical to tier-1 by the
///   JIT's bail-before-mutate contract, so turning the perf tier off changes speed, not
///   behavior. Also the `--jit-differential` oracle's pure tier-0 baseline.
/// - `Hot` is production tiering: hot-counter + OSR promotion, compiling OFF-THREAD
///   (P-PAR S4) so the mutator never pauses for Cranelift.
/// - `Forced` compiles every eligible prototype synchronously from the first dispatch —
///   the oracle's tier-1 side and the deterministic hot-swap stress entry.
///
/// Without the `jit` feature, `Hot` and `Forced` are no-ops (everything interprets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tiering {
    #[default]
    Off,
    Hot,
    Forced,
}

/// A **cooperative stop request** a run polls at its safepoints (isolate-cancel): the parent stores
/// `true`, the runner notices at its next frame transfer / loop back-edge / scheduler round and
/// unwinds, running the same teardown a completed run does.
///
/// It is the same `Arc<AtomicBool>` a real worker isolate carries — one mechanism, two callers.
/// `h.cancel()` reaches a worker through its isolate slot; [`RunOptions::cancel`] arms the
/// *top-level* run of a whole program with one, which is what lets `noeta test` ask an overrunning
/// case to stop instead of only abandoning it.
pub type CancelFlag = Arc<std::sync::atomic::AtomicBool>;

/// Everything one VM run can vary, in one place (audit-1 finding 14 / the `CheckOptions`
/// pattern) — instead of the `run_module_*` family growing a method per host × executor ×
/// tiering × debugger × session × stats combination. The presets below cover the
/// established entry points; a new combination is a struct literal, not a new method.
///
/// Defaults are the conformance differential's deterministic sandbox run: sandbox host +
/// sandbox executor, [`CollectorMode::Trace`], tier-0, nothing attached.
///
/// [`CollectorMode::Trace`]: noeta_value::CollectorMode::Trace
pub struct RunOptions {
    pub host: Box<dyn noeta_stdlib::Host>,
    pub executor: Box<dyn noeta_stdlib::Executor>,
    pub collector: noeta_value::CollectorMode,
    /// The safepoint-GC step (memory-management 6.x): how many further live objects accumulate
    /// before an in-run cycle collection is requested at the next safepoint. `None` = the
    /// process default (`NOETA_GC_THRESHOLD`, else 10k). Tests pin small values to exercise
    /// mid-run collection deterministically on tiny heaps.
    pub gc_threshold: Option<usize>,
    pub tiering: Tiering,
    /// Attached debugger (`noeta dap`). Callers pair this with [`Tiering::Off`] — see the
    /// observability contract on [`Tiering`].
    pub debugger: Option<Box<dyn Debugger>>,
    /// Attached profiler (`noeta profile`), consulted before every instruction; handed back
    /// in [`RunOutcome::profiler`] so its accumulated counters can be reclaimed. Tier-0 only,
    /// like the debugger.
    pub profiler: Option<Box<dyn ProfileHook>>,
    /// Debug-console / hot-reload session (tooling-unification T5): the live compiler
    /// returned alongside `module`; every console fragment or swap plan compiles through it
    /// and installs into the running Vm. The arena owning each extended module snapshot
    /// lives inside the core runner, for exactly the run's duration.
    #[cfg(feature = "compile")]
    pub session: Option<noeta_compiler::SessionCompiler>,
    /// Hot-reload mailbox the run thread polls at every scheduler tick (server-hmr W1);
    /// requires `session`.
    pub hot_mailbox: Option<HotSwapMailbox>,
    /// Real OS-thread isolates (isolates I.4b): the module by `Arc` (worker threads own it)
    /// plus the fresh-VM factory. CLI-only / out-of-oracle.
    pub isolates: Option<(Arc<Module>, IsolateFactory)>,
    /// A cooperative **stop request** for this whole run (test-timeout): the run polls it at the
    /// same safepoints a worker isolate polls its own — the dispatch loop's frame transfers and
    /// taken loop back-edges, plus each scheduler round — and unwinds when it is set, running the
    /// ordinary teardown (destructors, cycle collection, joining any isolates it spawned).
    ///
    /// `None` on every ordinary run, which is the case the safepoints must not pay for: the poll
    /// is then a null test on a cached field. The run's [`RunResult`] after an honored cancel is
    /// **not meaningful** — the body did not finish, so there is no value and no diagnostic; the
    /// caller asked for the stop and already knows what it means. `noeta test` is the one caller,
    /// and it discards the result (it has already reported the case as timed out).
    pub cancel: Option<CancelFlag>,
    /// Per-isolate profiling (`noeta profile` over a program with real isolates): each spawned
    /// worker gets its own hook from the factory and deposits it, named, in the sink at finish.
    /// Meaningful only alongside `isolates`.
    pub isolate_profiler: Option<(ProfileHookFactory, ProfileSink)>,
    /// Record the `--jit-stats` bail histogram; the assembled [`JitReport`] rides back in
    /// [`RunOutcome::report`]. Costs one branch per bail *event* (a tier transition), so it
    /// observes without perturbing what it measures.
    #[cfg(feature = "jit")]
    pub bail_histogram: bool,
    /// Stats determinism for hot-counter runs: compile the outstanding queue at exit so
    /// promotion counts don't race the program's runtime (the OSR tests assert them exactly).
    #[cfg(feature = "jit")]
    pub drain_at_exit: bool,
}

// Hand-written: the host/executor/debugger/profiler/session fields are trait objects with
// no Debug bound, so presence is the honest thing to print.
impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("RunOptions");
        d.field("collector", &self.collector)
            .field("tiering", &self.tiering)
            .field("debugger", &self.debugger.is_some())
            .field("profiler", &self.profiler.is_some())
            .field("hot_mailbox", &self.hot_mailbox.is_some())
            .field("isolates", &self.isolates.is_some())
            .field("isolate_profiler", &self.isolate_profiler.is_some())
            .field("cancel", &self.cancel.is_some());
        #[cfg(feature = "compile")]
        d.field("session", &self.session.is_some());
        #[cfg(feature = "jit")]
        d.field("bail_histogram", &self.bail_histogram)
            .field("drain_at_exit", &self.drain_at_exit);
        d.finish_non_exhaustive()
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            host: Box::new(noeta_stdlib::SandboxHost::new()),
            executor: Box::new(noeta_stdlib::SandboxExecutor::new()),
            collector: noeta_value::CollectorMode::Trace,
            gc_threshold: None,
            tiering: Tiering::default(),
            debugger: None,
            profiler: None,
            #[cfg(feature = "compile")]
            session: None,
            hot_mailbox: None,
            isolates: None,
            isolate_profiler: None,
            cancel: None,
            #[cfg(feature = "jit")]
            bail_histogram: false,
            #[cfg(feature = "jit")]
            drain_at_exit: false,
        }
    }
}

/// What one VM run produced. `result` is the differential's compared unit; the trace is
/// deliberately not part of it (yet) — the oracle grows its own traceback first.
// Debug by hand: the handed-back profiler is a trait object with no Debug bound.
pub struct RunOutcome {
    pub result: RunResult,
    /// The abort traceback (empty for a clean run).
    pub trace: Vec<TraceFrame>,
    /// The profiler handed back (present iff one was attached), so the concrete collector's
    /// accumulated counters/samples can be reclaimed via [`ProfileHook::into_any`].
    pub profiler: Option<Box<dyn ProfileHook>>,
    /// JIT-coverage counts: the synchronous engine's live accounting for [`Tiering::Forced`]
    /// runs, the service's parked final accounting for [`Tiering::Hot`] runs (meaningful
    /// there only with [`RunOptions::drain_at_exit`]). Default-empty when the report below
    /// already consumed the accounting — the two are never requested together today.
    #[cfg(feature = "jit")]
    pub stats: JitStats,
    /// The assembled `--jit-stats` report, iff [`RunOptions::bail_histogram`] was set.
    #[cfg(feature = "jit")]
    pub report: Option<JitReport>,
}

impl std::fmt::Debug for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("RunOutcome");
        d.field("result", &self.result)
            .field("trace", &self.trace)
            .field("profiler", &self.profiler.is_some());
        #[cfg(feature = "jit")]
        d.field("stats", &self.stats).field("report", &self.report);
        d.finish_non_exhaustive()
    }
}

impl VmBackend {
    pub fn new() -> VmBackend {
        VmBackend
    }

    /// Compile and run a program, or report that it falls outside the supported subset.
    #[cfg(feature = "compile")]
    pub fn try_run(&self, program: &Program) -> Result<RunResult, Unsupported> {
        let module = compile(program)?;
        // The differential harness path stays pure tier-0 (see `run_module`).
        Ok(self.run_module_with(&module, RunOptions::default()).result)
    }

    /// The core runner every preset delegates to: load a [`Vm`] configured by `opts`, run
    /// `main` to completion, tear down, and hand back everything the run produced. One body
    /// owns the load→attach→arm→run→collect protocol, so a new run mode is a [`RunOptions`]
    /// combination — not another copy of the protocol (audit-1 finding 14).
    pub fn run_module_with(&self, module: &Module, opts: RunOptions) -> RunOutcome {
        noeta_value::set_collector_mode(opts.collector);
        // Arm the in-run safepoint-GC trigger for this run (thread-local = per-isolate); teardown
        // disarms it. See `Vm::maybe_safepoint_gc`.
        noeta_value::safepoint_gc_arm(
            opts.gc_threshold
                .unwrap_or_else(noeta_value::safepoint_gc_default_threshold),
        );
        // The arena owning each debug-session/hot-swap module snapshot lives here, for
        // exactly the run's duration: an escaped fragment value stays resolvable until the
        // program exits.
        #[cfg(feature = "compile")]
        let arena = typed_arena::Arena::new();
        let mut vm = Vm::load(module, opts.host, opts.executor);
        vm.debugger = opts.debugger;
        vm.profiler = opts.profiler;
        #[cfg(feature = "compile")]
        if let Some(session) = opts.session {
            vm.debug_session = Some(DebugSession {
                compiler: Box::new(session),
                arena: &arena,
                memo: HashMap::new(),
                result_memo: HashMap::new(),
                stop_generation: 0,
            });
        }
        // Claim this VM's consumer cursor before the run can drain (server-hmr H5 retention): the
        // channel reclaims a plan's payload only once every declared consumer has passed it, so a
        // worker still arming here holds the prefix back rather than losing a swap.
        vm.hot_mailbox = opts.hot_mailbox.map(|mailbox| {
            let slot = mailbox.register();
            crate::hotswap::HotConsumer { mailbox, slot }
        });
        // The isolate module `Arc` doubles as the hot tier's module handle below, saving the
        // `module.clone()` when a parallel run arms the JIT service.
        #[cfg(feature = "jit")]
        let isolate_arc = opts.isolates.as_ref().map(|(m, _)| Arc::clone(m));
        if let Some((arc, factory)) = opts.isolates {
            vm.isolates.parallel_isolates = true;
            vm.isolates.isolate_module = Some(arc);
            vm.isolates.isolate_factory = Some(factory);
            vm.isolates.profile_seam = opts.isolate_profiler;
        }
        // Arm the top-level run's cancellation poll (test-timeout) — the identical field a worker
        // isolate installs on its own VM, so one mechanism serves `h.cancel()` and `noeta test`'s
        // "ask the case to stop" alike. Set *before* the run so a request that arrives during
        // startup is honored at the body's first safepoint.
        vm.isolates.cancel_flag = opts.cancel;
        match opts.tiering {
            Tiering::Off => {}
            // Without the `jit` feature both arms are no-ops: everything interprets.
            Tiering::Hot => {
                #[cfg(feature = "jit")]
                vm.init_jit_service(isolate_arc.unwrap_or_else(|| Arc::new(module.clone())));
            }
            Tiering::Forced => {
                #[cfg(feature = "jit")]
                {
                    vm.tier1.force_jit = true;
                    vm.init_jit();
                }
            }
        }
        #[cfg(feature = "jit")]
        if opts.bail_histogram {
            vm.tier1.jit_bail_counts = Some(std::collections::HashMap::new());
        }
        #[cfg(feature = "jit")]
        if opts.drain_at_exit {
            vm.tier1.jit_drain_at_exit = true;
        }
        let result = run_and_teardown(&mut vm, opts.collector);
        // This consumer is done: release the prefix it was holding back, so a worker that exits
        // (or panics its way out of the fleet) cannot pin the queue for the rest of the session.
        if let Some(consumer) = &vm.hot_mailbox {
            consumer.mailbox.retire(consumer.slot);
        }
        let trace = std::mem::take(&mut vm.out.abort_trace);
        let profiler = vm.profiler.take();
        // Report before stats: assembling the report consumes the service's parked final
        // accounting, which is also the `Hot` path's stats source — no caller asks for both.
        #[cfg(feature = "jit")]
        let report = opts.bail_histogram.then(|| vm.take_jit_report());
        #[cfg(feature = "jit")]
        let stats = vm
            .tier1
            .jit
            .as_ref()
            .map(|j| JitStats {
                native: j.native_count(),
                compiled: j.compiled_count(),
                compile_ns_total: j.compile_ns_total(),
                compile_ns_max: j.compile_ns_max(),
                breakdown: j.compile_breakdown(),
            })
            .unwrap_or_else(|| vm.tier1.jit_final_stats.take().unwrap_or_default());
        RunOutcome {
            result,
            trace,
            profiler,
            #[cfg(feature = "jit")]
            stats,
            #[cfg(feature = "jit")]
            report,
        }
    }

    /// Execute an already-compiled [`Module`]. This is the seam the salsa graph (`noeta-db`)
    /// drives: it produces the `Module` via the memoized `bytecode` query, then hands it here.
    /// Splitting compilation from execution is what lets the VM "consume `chunk(db)`" (M1.1)
    /// without the VM crate depending on the database. Runs against a deterministic
    /// [`noeta_stdlib::SandboxHost`] — the host the conformance differential always uses; pure
    /// tier-0, so it never auto-JITs (the oracle's tier-1 tier is `run_module_jit`'s explicit
    /// `Forced`).
    pub fn run_module(&self, module: &Module) -> RunResult {
        self.run_module_with(module, RunOptions::default()).result
    }

    /// [`VmBackend::run_module`] plus the abort traceback (empty for a clean run) — the sandboxed,
    /// deterministic entry the traceback's own tests drive.
    pub fn run_module_traced(&self, module: &Module) -> (RunResult, Vec<TraceFrame>) {
        let out = self.run_module_with(module, RunOptions::default());
        (out.result, out.trace)
    }

    /// Execute a module against a caller-provided [`noeta_stdlib::Host`] (M2.3). The CLI/REPL pass
    /// a real host here; the conformance harness keeps using the sandbox default via
    /// [`VmBackend::run_module`], so the differential stays deterministic. A real-host production
    /// run drives the tier-1 JIT under ordinary hot-counter promotion (P-JIT).
    pub fn run_module_with_host(
        &self,
        module: &Module,
        host: Box<dyn noeta_stdlib::Host>,
    ) -> RunResult {
        self.run_module_with(
            module,
            RunOptions {
                host,
                tiering: Tiering::Hot,
                ..RunOptions::default()
            },
        )
        .result
    }

    /// Execute a module against a caller-provided host *and* async executor (Track A.4). The CLI
    /// pairs a real host with a real wall-clock executor so `sleep`/`concurrent` run against real
    /// time; the differential never calls this (it keeps the sandbox pair), so it is out-of-oracle.
    pub fn run_module_with_host_and_executor(
        &self,
        module: &Module,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
    ) -> RunResult {
        self.run_module_with(
            module,
            RunOptions {
                host,
                executor,
                tiering: Tiering::Hot,
                ..RunOptions::default()
            },
        )
        .result
    }

    /// Execute a module against a real host + executor **with the JIT unarmed** — the debugger's run
    /// path (`noeta dap`); see the tier-0 observability contract on [`Tiering`]. Single-isolate /
    /// cooperative (real OS-thread isolate debugging is a later milestone); the differential never
    /// calls this, so it is out-of-oracle.
    pub fn run_module_with_host_and_executor_no_jit(
        &self,
        module: &Module,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
    ) -> RunResult {
        self.run_module_debug(module, host, executor, None).0
    }

    /// Like [`VmBackend::run_module_with_host_and_executor_no_jit`], but with a [`Debugger`] attached
    /// (the `noeta dap` run path). Tier-0 throughout so every frame is interpreter-executed and the
    /// debugger's `before_op` sees a real pc; the JIT is never armed. `debugger = None` is exactly the
    /// plain no-JIT run.
    pub fn run_module_debug(
        &self,
        module: &Module,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        debugger: Option<Box<dyn Debugger>>,
    ) -> (RunResult, Vec<TraceFrame>) {
        let out = self.run_module_with(
            module,
            RunOptions {
                host,
                executor,
                debugger,
                ..RunOptions::default()
            },
        );
        (out.result, out.trace)
    }

    /// Like [`VmBackend::run_module_debug`], but with the **debug console armed** (tooling-
    /// unification T5): `session` is the live compiler
    /// [`noeta_compiler::compile_with_sites_session`] returned alongside `module`, and every
    /// console fragment the debugger sends compiles through it and installs into the running Vm —
    /// full language, closures included.
    #[cfg(feature = "compile")]
    pub fn run_module_debug_session(
        &self,
        module: &Module,
        session: noeta_compiler::SessionCompiler,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        debugger: Option<Box<dyn Debugger>>,
    ) -> (RunResult, Vec<TraceFrame>) {
        let out = self.run_module_with(
            module,
            RunOptions {
                host,
                executor,
                debugger,
                session: Some(session),
                ..RunOptions::default()
            },
        );
        (out.result, out.trace)
    }

    /// Run a module with **in-process hot reload armed** (server-hmr W1): the debug-session
    /// machinery (live [`SessionCompiler`] + module arena — the same stable-prefix swap the debug
    /// console uses) plus a [`HotSwapMailbox`] the run thread polls at every scheduler tick. This
    /// is `noeta serve --watch`'s hot mode: the CLI's watcher thread deposits [`SwapPlan`]s and the
    /// serving program absorbs them between polls without restarting. Hot serving runs tier-1 like
    /// any production serve (server-hmr H3): the hot-counter service compiles off-thread, and a
    /// swap retires + re-arms it (`install_fragment`).
    ///
    /// [`SessionCompiler`]: noeta_compiler::SessionCompiler
    /// [`SwapPlan`]: noeta_compiler::hotswap::SwapPlan
    #[cfg(feature = "compile")]
    pub fn run_module_hot(
        &self,
        module: &Module,
        session: noeta_compiler::SessionCompiler,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        mailbox: HotSwapMailbox,
    ) -> (RunResult, Vec<TraceFrame>) {
        let out = self.run_module_with(
            module,
            RunOptions {
                host,
                executor,
                session: Some(session),
                hot_mailbox: Some(mailbox),
                tiering: Tiering::Hot,
                ..RunOptions::default()
            },
        );
        (out.result, out.trace)
    }

    /// [`VmBackend::run_module_hot`] with the **synchronous force-JIT engine** — the H3 oracle
    /// entry: every prototype (the in-flight `main` included) executes tier-1 from the first
    /// dispatch, so a swap deposited in `mailbox` deterministically exercises retire→re-arm
    /// under live native frames (the off-thread service would race the program's runtime). A
    /// dropped — rather than graveyard-parked — engine fails this by unwinding into freed pages.
    #[cfg(all(feature = "jit", feature = "compile"))]
    pub fn run_module_hot_forced_jit(
        &self,
        module: &Module,
        session: noeta_compiler::SessionCompiler,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        mailbox: HotSwapMailbox,
    ) -> (RunResult, Vec<TraceFrame>) {
        let out = self.run_module_with(
            module,
            RunOptions {
                host,
                executor,
                session: Some(session),
                hot_mailbox: Some(mailbox),
                tiering: Tiering::Forced,
                ..RunOptions::default()
            },
        );
        (out.result, out.trace)
    }

    /// Run a module **tier-0 under a profiler** (`noeta profile`): the JIT is never armed and a
    /// [`ProfileHook`] is consulted before every instruction (the same seam the debugger uses, minus
    /// the pause). Returns the run result, the hook handed back — so the concrete collector's
    /// accumulated counters/samples can be reclaimed via [`ProfileHook::into_any`] — and any abort
    /// trace. Mirrors [`VmBackend::run_module_debug`] with a profiler in place of the debugger.
    pub fn run_module_profiled(
        &self,
        module: &Module,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        profiler: Box<dyn ProfileHook>,
    ) -> (RunResult, Box<dyn ProfileHook>, Vec<TraceFrame>) {
        let out = self.run_module_with(
            module,
            RunOptions {
                host,
                executor,
                profiler: Some(profiler),
                ..RunOptions::default()
            },
        );
        let profiler = out
            .profiler
            .expect("the profiler stays attached for the whole run");
        (out.result, profiler, out.trace)
    }

    /// Execute a module with **real OS-thread isolates** (isolates I.4b), CLI-only / out-of-oracle.
    /// `module` is an `Arc` (the compiled module is `Send + Sync`) so worker threads can own it; each
    /// `isolate f(args)` with `Send`, channel-free arguments runs on its own thread with a fresh VM +
    /// host + executor from `factory`, communicating by copied [`isolate::Wire`] values. Channel-shipping
    /// isolates fall back to cooperative tasks (cross-thread channels are I.4c). The main isolate is a
    /// real-host production run (hot-counter JIT, compiled off-thread); worker isolates load through
    /// `Vm::load` and stay tier-0 (the engine lives on the compile-service thread). The differential
    /// never calls this (it keeps the deterministic cooperative sandbox), so it stays out-of-oracle.
    ///
    /// `cancel` arms this run's own cooperative stop request (see [`RunOptions::cancel`]) — `None`
    /// for an ordinary `noeta run`, `Some` for a `noeta test` case, which is how the timeout rail
    /// asks an overrunning case to stop rather than only abandoning its thread.
    pub fn run_module_with_host_and_executor_parallel(
        &self,
        module: Arc<Module>,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        factory: IsolateFactory,
        jit_report: bool,
        cancel: Option<CancelFlag>,
    ) -> (RunResult, Vec<TraceFrame>, Option<JitReport>) {
        let out = self.run_module_with(
            &Arc::clone(&module),
            RunOptions {
                host,
                executor,
                isolates: Some((module, factory)),
                tiering: Tiering::Hot,
                cancel,
                #[cfg(feature = "jit")]
                bail_histogram: jit_report,
                ..RunOptions::default()
            },
        );
        #[cfg(not(feature = "jit"))]
        let _ = jit_report;
        #[cfg(feature = "jit")]
        let report = out.report;
        #[cfg(not(feature = "jit"))]
        let report = None;
        (out.result, out.trace, report)
    }

    /// Run a module whose native prototype entries were **compiled ahead of time and linked in**
    /// (P-AOT L3.2b). Instead of arming the JIT compiler, bind the entries from `dispatch` — the
    /// [`noeta_jit_abi::AOT_DISPATCH_SYMBOL`] table (`[count][main_0, fast_0, …]`, pointer-width words the
    /// linker resolved to real code addresses) — into the mutable per-proto mirror tables, then run.
    /// Prototypes with a null slot (ineligible, or no fast body) interpret. Real host + executor +
    /// isolate factory, exactly like the production `parallel` path; out-of-oracle. Stays off the
    /// [`RunOptions`] core: the dispatch bind is an unsafe pre-run step no other mode has.
    ///
    /// # Safety
    /// `dispatch` must point at a valid dispatch table of that layout whose function pointers stay
    /// valid for the whole run — in a linked AOT binary they live in the executable's text, so this
    /// always holds. A null `dispatch` is allowed (binds nothing; everything interprets).
    #[cfg(feature = "jit-rt")]
    #[allow(unsafe_code)]
    pub unsafe fn run_module_aot(
        &self,
        module: Arc<Module>,
        dispatch: *const usize,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        factory: IsolateFactory,
    ) -> (RunResult, Vec<TraceFrame>) {
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        noeta_value::safepoint_gc_arm(noeta_value::safepoint_gc_default_threshold());
        let mut vm = Vm::load(&module, host, executor);
        vm.isolates.parallel_isolates = true;
        vm.isolates.isolate_module = Some(Arc::clone(&module));
        vm.isolates.isolate_factory = Some(factory);
        vm.tier1.aot = true;
        // SAFETY: the caller guarantees `dispatch` is a valid, live dispatch table (contract above).
        unsafe { vm.bind_aot_dispatch(dispatch) };
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, trace)
    }

    /// Execute under an explicit cycle-collector mode (Phase 6.4 benchmark seam). Production paths
    /// use the default [`CollectorMode::Trace`]; the head-to-head benchmark drives both.
    ///
    /// [`CollectorMode::Trace`]: noeta_value::CollectorMode::Trace
    pub fn run_module_with_collector(
        &self,
        module: &Module,
        mode: noeta_value::CollectorMode,
    ) -> RunResult {
        self.run_module_with(
            module,
            RunOptions {
                collector: mode,
                ..RunOptions::default()
            },
        )
        .result
    }

    /// Execute a module through the tier-1 JIT (milestone P-JIT), forcing every eligible prototype
    /// through native code — the `--jit-differential` and leak-under-JIT oracle path. It keeps the
    /// deterministic [`noeta_stdlib::SandboxHost`], so its [`RunResult`] is directly comparable to
    /// [`VmBackend::run_module`]: the only variable is tier 0 vs tier 1 — which is precisely what the
    /// oracle asserts.
    #[cfg(feature = "jit")]
    pub fn run_module_jit(&self, module: &Module) -> RunResult {
        self.run_module_jit_with_stats(module).0
    }

    /// Like [`VmBackend::run_module_jit`] but also returns how many prototypes were compiled (native
    /// vs total) — the JIT-coverage numbers the oracle reports and the tests assert on.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_with_stats(&self, module: &Module) -> (RunResult, JitStats) {
        let out = self.run_module_with(
            module,
            RunOptions {
                tiering: Tiering::Forced,
                ..RunOptions::default()
            },
        );
        (out.result, out.stats)
    }

    /// Execute a module with **ordinary hot-counter promotion** (the production tiering) — like the
    /// real `lang run`, a prototype goes native only once hot. Used by the OSR bench.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_hot(&self, module: &Module) -> RunResult {
        self.run_module_jit_hot_with_stats(module).0
    }

    /// Like [`VmBackend::run_module_jit_with_stats`] but with **ordinary hot-counter promotion**
    /// (`Forced` off) — the real production tiering. A prototype compiles only once it crosses
    /// [`JIT_HOT_THRESHOLD`] entries *or back-edges* (P-JIT J5 OSR), so this exercises the promotion
    /// path itself: a top-level loop entered once must still go native via its loop back-edges.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_hot_with_stats(&self, module: &Module) -> (RunResult, JitStats) {
        let out = self.run_module_with(
            module,
            RunOptions {
                tiering: Tiering::Hot,
                drain_at_exit: true,
                ..RunOptions::default()
            },
        );
        (out.result, out.stats)
    }

    /// Like [`VmBackend::run_module_jit`] (forced tier-1, sandbox host) but returning the **bail
    /// histogram** — the `--jit-stats` recording seam under the oracle's deterministic conditions,
    /// so tests can pin exactly which (proto, pc) sites bail and how often.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_bails(&self, module: &Module) -> (RunResult, Vec<JitBailSite>) {
        let out = self.run_module_with(
            module,
            RunOptions {
                tiering: Tiering::Forced,
                bail_histogram: true,
                ..RunOptions::default()
            },
        );
        let bails = out.report.map(|r| r.bails).unwrap_or_default();
        (out.result, bails)
    }
}

impl<'m> Vm<'m> {
    /// Assemble the `--jit-stats` [`JitReport`] after a run: the service's final compile accounting
    /// (parked at teardown), the bail histogram (sorted most-frequent first, ties by site for a
    /// deterministic report), and the OSR-declined prototypes. Consumes both parked pieces.
    #[cfg(feature = "jit")]
    fn take_jit_report(&mut self) -> JitReport {
        let stats = self.tier1.jit_final_stats.take().unwrap_or_default();
        let mut bails: Vec<JitBailSite> = self
            .tier1
            .jit_bail_counts
            .take()
            .unwrap_or_default()
            .into_iter()
            .map(|((proto, pc), count)| JitBailSite { proto, pc, count })
            .collect();
        bails.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| (a.proto, a.pc).cmp(&(b.proto, b.pc)))
        });
        let declined = self
            .tier1
            .jit_declined
            .iter()
            .enumerate()
            .filter(|(_, d)| **d)
            .map(|(i, _)| JitDeclinedLoop {
                proto: i as u32,
                bail_pcs: noeta_jit::loop_bail_pcs(&self.module.protos[i])
                    .into_iter()
                    .map(|pc| pc as u32)
                    .collect(),
            })
            .collect();
        JitReport {
            native: stats.native,
            compiled: stats.compiled,
            compile_ns_total: stats.compile_ns_total,
            bails,
            declined,
        }
    }
}

/// JIT-coverage counts for one forced-JIT run: how many prototypes were compiled to real native code
/// (`native`) out of the total that were compiled at all (`compiled`, native + bail stubs), plus the
/// compile-pause accounting (P-PAR S0c) — compilation runs synchronously on the mutator thread, so
/// `compile_ns_max` is the worst single pause the program felt and `compile_ns_total` the sum.
#[cfg(feature = "jit")]
#[derive(Debug, Clone, Copy, Default)]
pub struct JitStats {
    pub native: usize,
    pub compiled: usize,
    pub compile_ns_total: u64,
    pub compile_ns_max: u64,
    /// Where `compile_ns_total` goes + compiled volume (P-JCT C0).
    pub breakdown: noeta_jit::CompileBreakdown,
}

/// One site in the `--jit-stats` bail histogram: native code for prototype `proto` bailed back to
/// the interpreter at instruction `pc`, `count` times. The pc is the bailing op's own pc
/// (bail-before-mutate), so the consumer resolves it to an exact op and source line. Counts are per
/// **native entry**, not per loop iteration (see `Vm::jit_bail_counts`). Un-gated plain data so a
/// JIT-less build still names the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitBailSite {
    pub proto: u32,
    pub pc: u32,
    pub count: u64,
}

/// The `--jit-stats` report for one production run: tier-1 compile coverage, the bail histogram
/// (sorted most-frequent first), and the prototypes whose loops were **declined** OSR because every
/// loop contains a non-native op (`noeta_jit::worth_osr` said no — those run tier-0 and produce no
/// bail events, which is why they are reported separately; `noeta_jit::loop_bail_pcs` names the ops
/// responsible). Un-gated plain data; a JIT-less build simply never produces one.
#[derive(Debug, Clone, Default)]
pub struct JitReport {
    /// Prototypes compiled to real native code.
    pub native: usize,
    /// Prototypes compiled at all (native + bail stubs).
    pub compiled: usize,
    /// Total off-thread compile time, ns.
    pub compile_ns_total: u64,
    /// Bail histogram, most-frequent first.
    pub bails: Vec<JitBailSite>,
    /// Prototypes declined OSR (heap-op-dominated loops): they ran tier-0 and produced no bail
    /// events, so they are reported separately, each with the pcs of the loop ops that blocked it.
    pub declined: Vec<JitDeclinedLoop>,
}

/// One OSR-declined prototype in the [`JitReport`]: every loop in `proto` contains a non-native op
/// (`bail_pcs` — resolved by `noeta_jit::loop_bail_pcs` at report assembly, so the renderer needs no
/// JIT dependency), which would make native code bounce tiers every iteration; the whole prototype
/// ran interpreted instead.
#[derive(Debug, Clone, Default)]
pub struct JitDeclinedLoop {
    pub proto: u32,
    pub bail_pcs: Vec<u32>,
}

// The [`Backend`] contract compiles source → bytecode, so it rides the `compile` feature (native-size
// slice 2). A shipped AOT runtime drives the VM through `run_module_aot` on a pre-compiled bundle and
// never needs this, so it links without the compiler.
#[cfg(feature = "compile")]
impl Backend for VmBackend {
    /// The [`Backend`] contract. The VM is only driven through [`VmBackend::try_run`] (the
    /// differential harness), so reaching this on an unsupported program is a caller bug.
    fn run(&self, program: &Program) -> RunResult {
        self.try_run(program)
            .expect("VmBackend::run on a program outside the VM subset; use try_run")
    }
}

/// Run `main` and tear the VM down (globals, cycle collection, channel drain), returning the program's
/// [`RunResult`]. Split from [`Vm::load`] so a worker isolate can load the module without running
/// `main` (isolates I.4b). Two phases — [`Vm::run_top`] then [`Vm::teardown`] — so a persistent
/// session (REPL-on-VM) can run one entry's `main` against the shared globals *without* the teardown a
/// later entry's bindings still depend on; the single-shot path just runs them back to back.
pub(crate) fn run_and_teardown(vm: &mut Vm, mode: noeta_value::CollectorMode) -> RunResult {
    // Register the root parent in the stall registry for its driving lifetime, so a genuine
    // real-path cross-isolate deadlock resolves to E0010 instead of spinning (isolates I.4c). Inert
    // in the deterministic sandbox (non-parallel).
    let _stall = if vm.isolates.parallel_isolates {
        vm.stall_active = true;
        Some(crate::isolate::STALL.scheduler())
    } else {
        None
    };
    vm.run_top();
    vm.teardown(mode)
}

impl<'m> Vm<'m> {
    /// Run the module's entry chunk (proto 0 = `main`) to completion and release the frame-local state
    /// it leaves behind — the returned top value, any open `concurrent` scopes, and the JIT
    /// inline-cache closure pins. **Does not** touch the globals, channels, reactive graph, or run any
    /// collector: those are [`Vm::teardown`]'s job, deferred so a session can run many entries between
    /// one load and one teardown (REPL-on-VM R0).
    pub(crate) fn run_top(&mut self) {
        let (mut frames, regs) = self.pooled_run_stacks(self.module.main().num_registers as usize);
        frames.push(Frame {
            proto: 0,
            base: 0,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        });
        // The top-level frame's `Return`/`Halt` yields the program's (discarded) value; release
        // it. On abort `run` has already released every frame register.
        if let Ok(v) = self.run(frames, regs) {
            release(v);
        }
        // An abort (e.g. a detected deadlock, E0010) can leave open `concurrent` scopes whose tasks
        // were never joined — each scope still owns its tasks' futures (and any parked results).
        // Release them exactly as `ScopeEnd` would, so an aborted program's teardown stays
        // refcount-exact (the anomaly oracle checks) and destructors on captured locals still run.
        for scope in std::mem::take(&mut self.sched.scopes) {
            for task in scope {
                self.release_value(task.future);
                if let Some(result) = task.result {
                    self.release_value(result);
                }
            }
        }
        // Release the JIT inline caches' closure pins (S4.2) before any collector accounting: a
        // pinned closure the program itself dropped must read as garbage now, not as an anomaly.
        // Native code can no longer run (the run above is over), so the caches are dead.
        #[cfg(feature = "jit")]
        for v in std::mem::take(&mut self.tier1.jit_cache_pins) {
            release(v);
        }
    }
}
