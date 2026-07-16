//! The crate's **entry points**: [`VmBackend`] and its `run_module_*` family,
//! the `execute*` drivers, [`Vm::run_top`] + [`run_and_teardown`] (the two
//! phases a session splits), and the `--jit-stats` report types ([`JitStats`],
//! [`JitReport`], [`JitBailSite`], [`JitDeclinedLoop`]). Every item is moved
//! verbatim from the crate root (re-exported there, so the public API is
//! unchanged) purely to shrink `lib.rs` — no behavior change.

use crate::*;

/// The bytecode-VM backend.
#[derive(Debug, Clone, Default)]
pub struct VmBackend;

impl VmBackend {
    pub fn new() -> VmBackend {
        VmBackend
    }

    /// Compile and run a program, or report that it falls outside the supported subset.
    #[cfg(feature = "compile")]
    pub fn try_run(&self, program: &Program) -> Result<RunResult, Unsupported> {
        let module = compile(program)?;
        // The differential harness path stays pure tier-0 (see `run_module`).
        Ok(execute(
            &module,
            Box::new(noeta_stdlib::SandboxHost::new()),
            false,
        ))
    }

    /// Execute an already-compiled [`Module`]. This is the seam the salsa graph (`noeta-db`)
    /// drives: it produces the `Module` via the memoized `bytecode` query, then hands it here.
    /// Splitting compilation from execution is what lets the VM "consume `chunk(db)`" (M1.1)
    /// without the VM crate depending on the database. Runs against a deterministic
    /// [`noeta_stdlib::SandboxHost`] — the host the conformance differential always uses.
    pub fn run_module(&self, module: &Module) -> RunResult {
        // The sandbox path is the `--jit-differential` oracle's pure tier-0 baseline, so it never
        // auto-JITs (the oracle's tier-1 tier is `run_module_jit`'s explicit `force_jit`).
        execute(module, Box::new(noeta_stdlib::SandboxHost::new()), false)
    }

    /// [`VmBackend::run_module`] plus the abort traceback (empty for a clean run) — the sandboxed,
    /// deterministic entry the traceback's own tests drive.
    pub fn run_module_traced(&self, module: &Module) -> (RunResult, Vec<TraceFrame>) {
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let mut vm = Vm::load(
            module,
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        );
        let result = run_and_teardown(&mut vm, mode);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, trace)
    }

    /// Execute a module against a caller-provided [`noeta_stdlib::Host`] (M2.3). The CLI/REPL pass
    /// a real host here; the conformance harness keeps using the sandbox default via
    /// [`VmBackend::run_module`], so the differential stays deterministic.
    pub fn run_module_with_host(
        &self,
        module: &Module,
        host: Box<dyn noeta_stdlib::Host>,
    ) -> RunResult {
        // A real-host production run (`lang bench`, single-isolate CLI): drive the tier-1 JIT under
        // ordinary hot-counter promotion (P-JIT). A no-op without the `jit` feature.
        execute(module, host, true)
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
        // Real-host production run with a real async executor: hot-counter JIT (P-JIT).
        execute_with_collector(
            module,
            host,
            executor,
            noeta_value::CollectorMode::Trace,
            true,
        )
    }

    /// Execute a module against a real host + executor **with the JIT unarmed** — the debugger's run
    /// path (`noeta dap`). A debug session pins tier-0 so every frame stays interpreter-executed and
    /// therefore observable (a JIT'd region has no readable pc or register file mid-execution); tier-0
    /// is held observably identical to tier-1 by the JIT's bail-before-mutate contract, so turning the
    /// perf tier off changes speed, not behavior. Single-isolate/cooperative (real OS-thread isolate
    /// debugging is a later milestone); the differential never calls this, so it is out-of-oracle.
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
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let mut vm = Vm::load(module, host, executor);
        vm.debugger = debugger;
        let result = run_and_teardown(&mut vm, mode);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, trace)
    }

    /// Like [`VmBackend::run_module_debug`], but with the **debug console armed** (tooling-
    /// unification T5): `session` is the live compiler
    /// [`noeta_compiler::compile_with_sites_session`] returned alongside `module`, and every
    /// console fragment the debugger sends compiles through it and installs into the running Vm —
    /// full language, closures included. The arena owning each extended module snapshot lives
    /// here, for exactly the run's duration; an escaped fragment value stays resolvable until the
    /// program exits.
    #[cfg(feature = "compile")]
    pub fn run_module_debug_session(
        &self,
        module: &Module,
        session: noeta_compiler::SessionCompiler,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        debugger: Option<Box<dyn Debugger>>,
    ) -> (RunResult, Vec<TraceFrame>) {
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let arena = typed_arena::Arena::new();
        let mut vm = Vm::load(module, host, executor);
        vm.debugger = debugger;
        vm.debug_session = Some(DebugSession {
            compiler: Box::new(session),
            arena: &arena,
            memo: HashMap::new(),
        });
        let result = run_and_teardown(&mut vm, mode);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, trace)
    }

    /// Run a module with **in-process hot reload armed** (server-hmr W1): the debug-session
    /// machinery (live [`SessionCompiler`] + module arena — the same stable-prefix swap the debug
    /// console uses) plus a [`HotSwapMailbox`] the run thread polls at every scheduler tick. This
    /// is `noeta serve --watch`'s hot mode: the CLI's watcher thread deposits [`SwapPlan`]s and the
    /// serving program absorbs them between polls without restarting. JIT stays unarmed (the
    /// debug-path contract `install_fragment` asserts; H3 lifts this).
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
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let arena = typed_arena::Arena::new();
        let mut vm = Vm::load(module, host, executor);
        vm.debug_session = Some(DebugSession {
            compiler: Box::new(session),
            arena: &arena,
            memo: HashMap::new(),
        });
        vm.hot_mailbox = Some(mailbox);
        // Hot serving runs tier-1 like any production serve (server-hmr H3): the hot-counter
        // service compiles off-thread, and a swap retires + re-arms it (`install_fragment`).
        #[cfg(feature = "jit")]
        vm.init_jit_service(Arc::new(module.clone()));
        let result = run_and_teardown(&mut vm, mode);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, trace)
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
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let arena = typed_arena::Arena::new();
        let mut vm = Vm::load(module, host, executor);
        vm.debug_session = Some(DebugSession {
            compiler: Box::new(session),
            arena: &arena,
            memo: HashMap::new(),
        });
        vm.hot_mailbox = Some(mailbox);
        vm.tier1.force_jit = true;
        vm.init_jit();
        let result = run_and_teardown(&mut vm, mode);
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, trace)
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
        let mode = noeta_value::CollectorMode::Trace;
        noeta_value::set_collector_mode(mode);
        let mut vm = Vm::load(module, host, executor);
        vm.profiler = Some(profiler);
        let result = run_and_teardown(&mut vm, mode);
        let profiler = vm
            .profiler
            .take()
            .expect("the profiler stays attached for the whole run");
        let trace = std::mem::take(&mut vm.out.abort_trace);
        (result, profiler, trace)
    }

    /// Execute a module with **real OS-thread isolates** (isolates I.4b), CLI-only / out-of-oracle.
    /// `module` is an `Arc` (the compiled module is `Send + Sync`) so worker threads can own it; each
    /// `isolate f(args)` with `Send`, channel-free arguments runs on its own thread with a fresh VM +
    /// host + executor from `factory`, communicating by copied [`isolate::Wire`] values. Channel-shipping
    /// isolates fall back to cooperative tasks (cross-thread channels are I.4c). The differential never
    /// calls this (it keeps the deterministic cooperative sandbox), so it stays out-of-oracle.
    pub fn run_module_with_host_and_executor_parallel(
        &self,
        module: Arc<Module>,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
        factory: IsolateFactory,
        jit_report: bool,
    ) -> (RunResult, Vec<TraceFrame>, Option<JitReport>) {
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load(&module, host, executor);
        vm.isolates.parallel_isolates = true;
        vm.isolates.isolate_module = Some(Arc::clone(&module));
        vm.isolates.isolate_factory = Some(factory);
        // The main isolate is a real-host production run: enable the hot-counter JIT (P-JIT),
        // compiling off-thread (P-PAR S4). Worker isolates load through `Vm::load` and stay
        // tier-0 (the engine lives on the compile-service thread).
        #[cfg(feature = "jit")]
        vm.init_jit_service(Arc::clone(&module));
        // `--jit-stats`: arm the bail histogram before the run. Recording costs one branch per
        // bail *event* (a tier transition), so it observes without perturbing what it measures.
        #[cfg(feature = "jit")]
        if jit_report {
            vm.tier1.jit_bail_counts = Some(std::collections::HashMap::new());
        }
        #[cfg(not(feature = "jit"))]
        let _ = jit_report;
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
        // The abort traceback (empty for a clean run) rides beside the result — `RunResult` itself
        // stays the differential's compared unit, which the trace is deliberately not part of (yet):
        // the oracle grows its own traceback first.
        let trace = std::mem::take(&mut vm.out.abort_trace);
        #[cfg(feature = "jit")]
        let report = jit_report.then(|| vm.take_jit_report());
        #[cfg(not(feature = "jit"))]
        let report = None;
        (result, trace, report)
    }

    /// Run a module whose native prototype entries were **compiled ahead of time and linked in**
    /// (P-AOT L3.2b). Instead of arming the JIT compiler, bind the entries from `dispatch` — the
    /// [`noeta_jit_abi::AOT_DISPATCH_SYMBOL`] table (`[count][main_0, fast_0, …]`, pointer-width words the
    /// linker resolved to real code addresses) — into the mutable per-proto mirror tables, then run.
    /// Prototypes with a null slot (ineligible, or no fast body) interpret. Real host + executor +
    /// isolate factory, exactly like the production `parallel` path; out-of-oracle.
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
    pub fn run_module_with_collector(
        &self,
        module: &Module,
        mode: noeta_value::CollectorMode,
    ) -> RunResult {
        execute_with_collector(
            module,
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
            mode,
            false,
        )
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
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load(
            module,
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        );
        vm.tier1.force_jit = true;
        vm.init_jit();
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
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
            .unwrap_or_default();
        (result, stats)
    }

    /// Execute a module with **ordinary hot-counter promotion** (the production tiering) — like the
    /// real `lang run`, a prototype goes native only once hot. Used by the OSR bench.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_hot(&self, module: &Module) -> RunResult {
        self.run_module_jit_hot_with_stats(module).0
    }

    /// Like [`VmBackend::run_module_jit_with_stats`] but with **ordinary hot-counter promotion**
    /// (`force_jit` off) — the real production tiering. A prototype compiles only once it crosses
    /// [`JIT_HOT_THRESHOLD`] entries *or back-edges* (P-JIT J5 OSR), so this exercises the promotion
    /// path itself: a top-level loop entered once must still go native via its loop back-edges.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_hot_with_stats(&self, module: &Module) -> (RunResult, JitStats) {
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load(
            module,
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        );
        // force_jit stays false → hot-counter + OSR promotion, compiled OFF-THREAD (P-PAR S4).
        vm.init_jit_service(Arc::new(module.clone()));
        // Stats determinism: compile the outstanding queue at exit so promotion counts don't
        // race the program's runtime (the OSR tests assert them exactly).
        vm.tier1.jit_drain_at_exit = true;
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
        // Teardown shut the service down and parked its final accounting.
        let stats = vm.tier1.jit_final_stats.take().unwrap_or_default();
        (result, stats)
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

impl VmBackend {
    /// Like [`VmBackend::run_module_jit`] (forced tier-1, sandbox host) but returning the **bail
    /// histogram** — the `--jit-stats` recording seam under the oracle's deterministic conditions,
    /// so tests can pin exactly which (proto, pc) sites bail and how often.
    #[cfg(feature = "jit")]
    pub fn run_module_jit_bails(&self, module: &Module) -> (RunResult, Vec<JitBailSite>) {
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load(
            module,
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        );
        vm.tier1.force_jit = true;
        vm.init_jit();
        vm.tier1.jit_bail_counts = Some(std::collections::HashMap::new());
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
        let report = vm.take_jit_report();
        (result, report.bails)
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

/// Execute a compiled module, capturing stdout, exit code, and diagnostics. `jit` enables the
/// hot-counter tier-1 JIT (real-host production paths); the sandbox differential passes `false`.
fn execute(module: &Module, host: Box<dyn noeta_stdlib::Host>, jit: bool) -> RunResult {
    execute_with_collector(
        module,
        host,
        Box::new(noeta_stdlib::SandboxExecutor::new()),
        noeta_value::CollectorMode::Trace,
        jit,
    )
}

/// Execute a module under an explicit cycle-collector mode (Phase 6.4). The default path uses the
/// backup mark-sweep [`CollectorMode::Trace`]; the benchmark drives [`CollectorMode::TrialDeletion`]
/// to compare the two. The mode is set on this isolate's release path before the first allocation
/// and the matching collector runs at clean exit (the trace marks from the live globals *before*
/// teardown; trial-deletion reaps its buffered candidates *after*, once every release has fed them).
fn execute_with_collector(
    module: &Module,
    host: Box<dyn noeta_stdlib::Host>,
    executor: Box<dyn noeta_stdlib::Executor>,
    mode: noeta_value::CollectorMode,
    jit: bool,
) -> RunResult {
    noeta_value::set_collector_mode(mode);
    let mut vm = Vm::load(module, host, executor);
    // Real-host production paths pass `jit = true` to arm the hot-counter tier-1 JIT (P-JIT). A
    // no-op without the `jit` feature; the `jit` binding is unused there, so quiet the warning.
    #[cfg(feature = "jit")]
    if jit {
        // Production hot-counter tiering compiles OFF-THREAD (P-PAR S4): the mutator never
        // pauses for Cranelift. The compile thread outlives every `&Module` borrow, so it takes
        // the module by `Arc` (a one-time table clone at startup).
        vm.init_jit_service(Arc::new(module.clone()));
    }
    #[cfg(not(feature = "jit"))]
    let _ = jit;
    run_and_teardown(&mut vm, mode)
}

/// Run `main` and tear the VM down (globals, cycle collection, channel drain), returning the program's
/// [`RunResult`]. Split from [`Vm::load`] so a worker isolate can load the module without running
/// `main` (isolates I.4b). Two phases — [`Vm::run_top`] then [`Vm::teardown`] — so a persistent
/// session (REPL-on-VM) can run one entry's `main` against the shared globals *without* the teardown a
/// later entry's bindings still depend on; the single-shot path just runs them back to back.
pub(crate) fn run_and_teardown(vm: &mut Vm, mode: noeta_value::CollectorMode) -> RunResult {
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
        let regs = vec![Value::unit(); self.module.main().num_registers as usize];
        let top = Frame {
            proto: 0,
            base: 0,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        };
        // The top-level frame's `Return`/`Halt` yields the program's (discarded) value; release
        // it. On abort `run` has already released every frame register.
        if let Ok(v) = self.run(vec![top], regs) {
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
