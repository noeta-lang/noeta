//! The Tier-0 register VM: executes a [`Module`] into a [`RunResult`].
//!
//! `VmBackend` is the second [`Backend`] (the M0 tree-walker is the first). The conformance
//! harness runs both over the corpus and asserts identical `RunResult`s — the differential
//! oracle. The VM compiles only a subset of the language, so [`VmBackend::try_run`] returns
//! [`Unsupported`] for programs it can't lower yet; the harness skips those and tracks a
//! climbing coverage percentage.
//!
//! ## Call frames and globals
//!
//! Each prototype runs in its own [`Frame`]: a register file, a program counter, and the
//! caller register its return value flows back into. `Call` pushes a frame; `Return` (or
//! falling off the end, an implicit unit return) pops one and threads the value into the
//! caller. The top-level program is the bottom frame; its `Halt`/`Return` ends the program.
//! Top-level bindings and function names live in a by-name `globals` table that every frame
//! shares — the runtime half of the compiler's two-level scope model.
//!
//! Memory is refcounted (`noeta-gc`): every register and every global owns one reference to
//! its value. The invariants are local — overwriting a slot releases the old occupant, a
//! `Move`/`LoadGlobal`/`Call`-argument retains the source, a returned value is retained
//! across its frame's teardown, and on exit every frame register and global is released — so
//! no value leaks and none is freed twice. A heap collection owns one reference to each of
//! its elements (the `MakeList`/`MakeMap`/iteration ops retain into it); freeing it releases
//! them. `miri` checks all of this over the unit tests.
//!
//! ## Re-entrant builtins
//!
//! `map`/`filter` are native, yet must call a *user* closure once per element. The dispatch
//! loop runs over an explicit frame stack ([`Vm::run`]); a native builtin re-enters the VM
//! by running a fresh single-frame stack to completion ([`Vm::call_value`]). The frame stack
//! is a local of `run`, never a field of [`Vm`], so this nesting is just ordinary Rust
//! recursion over the shared `globals`/`stdout`/`diagnostics`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use noeta_ast::{BinaryOp, Program};
use noeta_backend::{Backend, RunResult};
use noeta_bytecode::{
    BoolSide, Builtin, CaptureFrom, Const, Module, NarrowTarget, Op, ReuseCheck, StrPart,
};
use noeta_compiler::{Unsupported, compile};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_gc::{collect_trace, release, retain};
use noeta_object::{Shape, ShapeKind};
use noeta_reactive::{NodeKind, ReactiveGraph};
use noeta_span::Span;
use noeta_value::{
    ChannelId, ScopeId, TaskId, Value, apply_binary, apply_binary_wide, apply_unary,
    compare_primitive, structural_compare,
};

mod isolate;
mod values;
pub(crate) use values::*;
mod methods;
mod scheduler;

/// The bytecode-VM backend.
#[derive(Debug, Clone, Default)]
pub struct VmBackend;

impl VmBackend {
    pub fn new() -> VmBackend {
        VmBackend
    }

    /// Compile and run a program, or report that it falls outside the supported subset.
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
    ) -> RunResult {
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        let mut vm = Vm::load(&module, host, executor);
        vm.parallel_isolates = true;
        vm.isolate_module = Some(Arc::clone(&module));
        vm.isolate_factory = Some(factory);
        // The main isolate is a real-host production run: enable the hot-counter JIT (P-JIT). Worker
        // isolates load through `Vm::load` and stay tier-0 (Cranelift's `JITModule` is `!Send`).
        #[cfg(feature = "jit")]
        vm.init_jit();
        run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace)
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
        vm.force_jit = true;
        vm.init_jit();
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
        let stats = vm
            .jit
            .as_ref()
            .map(|j| JitStats {
                native: j.native_count(),
                compiled: j.compiled_count(),
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
        vm.init_jit(); // force_jit stays false → hot-counter + OSR promotion
        let result = run_and_teardown(&mut vm, noeta_value::CollectorMode::Trace);
        let stats = vm
            .jit
            .as_ref()
            .map(|j| JitStats {
                native: j.native_count(),
                compiled: j.compiled_count(),
            })
            .unwrap_or_default();
        (result, stats)
    }
}

/// JIT-coverage counts for one forced-JIT run: how many prototypes were compiled to real native code
/// (`native`) out of the total that were compiled at all (`compiled`, native + bail stubs).
#[cfg(feature = "jit")]
#[derive(Debug, Clone, Copy, Default)]
pub struct JitStats {
    pub native: usize,
    pub compiled: usize,
}

impl Backend for VmBackend {
    /// The [`Backend`] contract. The VM is only driven through [`VmBackend::try_run`] (the
    /// differential harness), so reaching this on an unsupported program is a caller bug.
    fn run(&self, program: &Program) -> RunResult {
        self.try_run(program)
            .expect("VmBackend::run on a program outside the VM subset; use try_run")
    }
}

/// One activation record: a prototype index, its register file, the program counter, the caller
/// register the return value flows into (irrelevant for the bottom/top-level frame), and an
/// optional transform applied to the return value as it lands in the caller.
struct Frame {
    proto: u32,
    /// This frame's register file occupies `regs[base .. base + proto.num_registers]` in the
    /// dispatch stack's single contiguous `regs: Vec<Value>` (P-VMT-FRAME). A call pushes a frame
    /// by extending that stack; a return truncates back to `base`. No frame owns its registers, so
    /// an ordinary call allocates nothing once the stack has grown to the run's deepest depth.
    base: usize,
    pc: usize,
    ret_dst: u16,
    ret_transform: RetTransform,
    /// The closure's captured upvalue cells, one owned reference each (released at frame
    /// teardown). Empty for top-level functions, methods, and operator-dispatch frames — only a
    /// closure built with captures carries any.
    upvalues: Vec<Value>,
}

/// A transform applied to a frame's return value as it flows into the caller's destination
/// register. Used by operator dispatch where the called trait method's raw result needs
/// post-processing: `!=` calls `Equatable::eq` and negates the resulting `bool`; `< <= > >=` call
/// `Comparable::compare` and map the resulting `Ordering` variant to a `bool`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RetTransform {
    /// Pass the value through unchanged (every ordinary call/return).
    None,
    /// Negate a `bool` result (for `!=` dispatched to `eq`); a non-bool passes through.
    Negate,
    /// Map a returned `Ordering` enum to this operator's `bool` (for `< <= > >=` dispatched to
    /// `compare`); a non-`Ordering` value passes through (an ill-typed `compare`).
    Ordering(BinaryOp),
    /// Wrap a by-name invocation's return value in `Result.Ok` (P2.6). The shape is the `Result.Ok`
    /// variant shape, baked into `Op::Invoke` and cloned in at frame setup; the raw return's
    /// reference transfers into the enum payload, so the original is *not* released afterward.
    WrapOk(Rc<Shape>),
}

impl RetTransform {
    /// Map the frame's raw return value. Returns the transformed value and whether the original
    /// `v` was *replaced* (so the caller must release `v`'s keep-alive reference — the transformed
    /// result is always a fresh immediate `bool`, holding no heap reference of its own). A
    /// pass-through (`None`, or an ill-typed value the transform doesn't recognize) returns `v`
    /// unchanged with `false`, so the caller transfers `v`'s reference onward as usual.
    fn apply(self, v: Value) -> (Value, bool) {
        match self {
            RetTransform::None => (v, false),
            RetTransform::Negate => match v.as_bool() {
                Some(b) => (Value::bool(!b), true),
                None => (v, false),
            },
            RetTransform::Ordering(op) => match v.shape() {
                Some(shape) if shape.kind == ShapeKind::Enum && shape.name == "Ordering" => {
                    let variant = shape.variant.as_deref().unwrap_or("");
                    (Value::bool(op.ordering_satisfies(variant)), true)
                }
                _ => (v, false),
            },
            // `v`'s reference transfers into the enum payload, so it is *not* a replacement (the
            // returned `Ok` carries it onward); the caller must not release `v`.
            RetTransform::WrapOk(shape) => (Value::enum_value(shape, vec![v]), false),
        }
    }
}

/// Signals that a diagnostic has been recorded and execution must unwind. The diagnostic
/// itself lives on [`Vm::diagnostics`]; this is just the propagation token.
struct Abort;

/// One program's worth of execution state, shared across every (possibly re-entrant) frame
/// stack: the compiled module, the shared shape handles and instance-method table, the by-name
/// global environment, captured stdout, and the diagnostics recorded so far.
struct Vm<'m> {
    module: &'m Module,
    /// One shared `Rc<Shape>` per shape-table entry — cloned into every value of that shape,
    /// so equal-built aggregates point at one shape (identity is a pointer comparison).
    shapes: Vec<Rc<Shape>>,
    /// One shared `Rc<PackedSchema>` per compiled packed-list layout (P-PACK 2.4), resolved at load
    /// from [`Module::packed_schemas`] against `shapes` — so `Op::MakePackedList` packs/materializes
    /// elements that share shape identity with directly-constructed instances.
    packed_schemas: Vec<Rc<noeta_object::PackedSchema>>,
    /// One shared `Rc<TypeRepr>` per interned reflected element type (runtime type-argument
    /// reflection, R1), built once at load from [`Module::type_reprs`]. `Op::MakeList` stamps a cheap
    /// `Rc` clone of its indexed entry onto the built list, so `type_of` recovers the element type
    /// after a `dyn` launder. Empty for a program with no tagged list literal.
    type_reprs: Vec<Rc<noeta_ast::reflect::TypeRepr>>,
    /// `map(...)` call span → the result element's `Rc<PackedSchema>` (P-PACK 2.6 category B), resolved
    /// at load from [`Module::map_packed_sites`]. The `map` builtin looks up its call span here to build
    /// a flat result instead of N boxed objects.
    map_packed: HashMap<Span, Rc<noeta_object::PackedSchema>>,
    /// Instance-method dispatch: `(type_name, method)` to the method's prototype index.
    methods: HashMap<(String, String), u32>,
    /// `type_name` to its `destruct` prototype, for classes with a destructor.
    destructors: HashMap<String, u32>,
    /// `(type_name, field_name)` to the field's default-value thunk prototype (object-model
    /// slice 5). `MakeStruct` runs the thunk (in global scope, empty upvalues) to fill a field the
    /// literal omits — mirroring the tree-walker's `TypeDef` field-default fill.
    field_defaults: HashMap<(String, String), u32>,
    /// Type names whose value, when destroyed, can run *some* `destruct` block — its own or a
    /// transitively-owned field / variant-payload / collection element (the checker's
    /// destruct-reachability fixpoint, threaded through the module). The container-before-contained
    /// field-walk gate (Phase 4.3, spec §4): a value whose shape name is absent here owns no
    /// destructor in its subtree and frees on the plain-release fast path.
    destruct_reachable: HashSet<String>,
    /// Type names that `@derive(Comparable)` (without a hand-written `compare`): their instances
    /// get structural field-wise ordering for `< <= > >=`.
    comparable_derives: HashSet<String>,
    /// Type names that `@derive(Serialize<Json>)` (without a hand-written `to_json`): `o.to_json()` on
    /// their instances synthesizes a structural JSON serializer.
    tojson_derives: HashSet<String>,
    /// The per-run global slots (P-VMT-GSLOT), indexed by [`GlobalId`] — sized to
    /// `module.global_names.len()`. A slot holds [`Value::unbound`] until first bound (a
    /// `LoadGlobal`/`TakeGlobal` of an unbound slot raises E0005); the compiler assigns a dense slot
    /// to every top-level binding and `fn` name so access is a `Vec` index, not a `HashMap`
    /// hash+probe. `Vec<Value>` (not `Vec<Option<Value>>`) so a slot is a single 8-byte word with a
    /// layout the JIT can access soundly (P-JIT globals) — and half the size / a cheaper unbound check.
    globals: Vec<Value>,
    /// Global slots in **binding order** (each pushed the first time its slot is stored), so globals
    /// are destroyed at program end in reverse binding order (the deterministic "program order" the
    /// spec requires) — the same order the pre-slot name-keyed `global_order` produced.
    global_order: Vec<u32>,
    /// The deterministic `next_id()` counter, seeded at 1 (matching the M0 `IdGen`).
    next_id: u64,
    /// All host-coupled effects (filesystem, seeded PRNG, logical clock) behind the M2.1
    /// [`noeta_stdlib::Host`] seam. The conformance harness constructs a deterministic
    /// [`noeta_stdlib::SandboxHost`]; a real host (later M2 slices) swaps in without touching
    /// this struct. See the eval backend's field of the same name.
    host: Box<dyn noeta_stdlib::Host>,
    /// The async executor (Track A.2): the clock + pending-timer set that `sleep(ms)` and
    /// drive-to-completion `.await` consult, behind the [`noeta_stdlib::Executor`] seam. The
    /// conformance harness keeps a deterministic [`noeta_stdlib::SandboxExecutor`] (identical to the
    /// tree-walker's by construction, so the differential holds); the CLI swaps in a real wall-clock
    /// executor (Track A.4). See the eval backend's field of the same name.
    executor: Box<dyn noeta_stdlib::Executor>,
    /// The structured-concurrency scope stack (Track A.3b): one entry per open `concurrent { }` block,
    /// each a list of the tasks `spawn`ed in it. The scope owns one reference to each task's future (and
    /// its result once ready), released when the scope is joined and popped. Mirrors the tree-walker's
    /// `scopes`; both round-robin identically, so the differential holds by construction.
    scopes: Vec<Vec<Task>>,
    /// The channel table (isolates I.1): every `channel::<T>(cap)` appends a [`Channel`]; endpoint
    /// values (`Sender`/`Receiver`) reference one by index. A queued message is owned by the channel
    /// (retained on enqueue, transferred out on dequeue). `channel_progress` counts successful queue
    /// operations (a `send` push, a `recv` pop, a `close`) so the scheduler treats a channel op that
    /// unblocks a sibling as progress even when no task completes. Mirrors the tree-walker's fields.
    channels: Vec<Channel>,
    channel_progress: u64,
    /// The reactive graph (reactivity S1): every `signal(v)`/`computed(fn)`/`effect(fn)` allocates a
    /// node here; a `Reactive` handle value references one by [`NodeId`]. Held behind `Rc` so a flush
    /// (`self.reactive.flush(|body| self.call_value(body, …))`) can borrow the graph and `self`
    /// independently — the graph's own methods are `&self` (interior-mutable), so this sidesteps the
    /// borrow conflict without `unsafe`. The graph stores [`GcVal`]s so its internal clones/drops keep
    /// the VM's manual refcounts exact; it is cleared at program end so held values are released
    /// (the leak oracle's residency returns to 0).
    reactive: Rc<ReactiveGraph<GcVal>>,
    /// Real OS-thread isolates (isolates I.4b), CLI-only / out-of-oracle. `parallel_isolates` selects
    /// the real path in the `Op::SpawnIsolate` handler; `isolate_module` is an `Arc` clone of the
    /// compiled module (`Send + Sync`) the entry point holds *alongside* the `&Module` borrow, so a
    /// worker thread can own the module for its lifetime; `isolate_factory` builds a fresh host +
    /// executor per worker (injected by the CLI so `noeta-vm` needs no `noeta-runtime`/tokio dependency);
    /// `isolates` holds each spawned worker's result channel + join handle; `inflight_isolates` counts
    /// workers whose result has not yet been harvested (so the scheduler treats a pending isolate as
    /// progress, not a deadlock). All inert in the sandbox (`parallel_isolates` false).
    parallel_isolates: bool,
    isolate_module: Option<Arc<Module>>,
    isolate_factory: Option<IsolateFactory>,
    isolates: Vec<IsolateSlot>,
    inflight_isolates: usize,
    stdout: String,
    diagnostics: Vec<Diagnostic>,
    /// The tier-1 JIT engine (milestone P-JIT), present only when the `jit` feature is on *and* the
    /// host ISA is available. `None` = interpret everything (tier 0). Never populated on a worker
    /// isolate — Cranelift's `JITModule` is `!Send`, and the deterministic path stays tier 0.
    #[cfg(feature = "jit")]
    jit: Option<noeta_jit::Jit>,
    /// When set, every eligible prototype is compiled eagerly and dispatched through tier 1 (the
    /// `--jit-differential` / leak-under-JIT oracle's "force JIT" switch). Off = ordinary hot-counter
    /// promotion.
    #[cfg(feature = "jit")]
    force_jit: bool,
    /// Per-prototype tier-1 entry counter, indexed by prototype index; a prototype is compiled once
    /// its count crosses [`JIT_HOT_THRESHOLD`] (or immediately under `force_jit`).
    #[cfg(feature = "jit")]
    jit_counters: Vec<u32>,
    /// Prototypes whose loops native code cannot sustain (every loop bails — see
    /// [`noeta_jit::worth_osr`]), so OSR was declined and must not be re-evaluated every back-edge.
    /// Checked once when a proto first goes hot; keeps a heap-op-dominated loop in the interpreter
    /// (which is faster for it than the tier-0↔tier-1 bounce) without a per-iteration re-scan.
    #[cfg(feature = "jit")]
    jit_declined: Vec<bool>,
    /// The value the bottom frame produced when it returned inside native code (J3): `jit_return`
    /// parks it here for the dispatch loop to yield as the run's result.
    #[cfg(feature = "jit")]
    jit_ret: Value,
    /// Closures pinned by the JIT's per-call-site inline caches (P-JSSA S4.2): `jit_prepare_call`
    /// retains a closure when it caches it, so bits-equality at the site stays a proof of
    /// identity (no free/reuse while cached). Only 0-upvalue closures are cacheable — they hold
    /// nothing, so delaying their free to teardown is observably inert. Released (and the caches
    /// with them) before the teardown collectors run, keeping residency and the anomaly
    /// accounting exact. Bounded by call-site count: a site that sees a second distinct callee
    /// is poisoned, never re-pinned.
    #[cfg(feature = "jit")]
    jit_cache_pins: Vec<Value>,
    /// The empty-`Frame` template the JIT's native frame push copies (stable address for the
    /// `Vm`'s lifetime; the `Jit` and its generated code are dropped with the same `Vm`).
    #[cfg(feature = "jit")]
    jit_frame_template: Option<Box<Frame>>,
}

/// Tier-1 promotion threshold: a prototype interprets until it has been entered this many times,
/// then the JIT compiles it (P-JIT). The `--jit-differential` oracle bypasses this via `force_jit`.
#[cfg(feature = "jit")]
const JIT_HOT_THRESHOLD: u32 = 50;

/// What a compiled prototype's tier-1 run tells the interpreter to do next (P-JIT, decoded from the
/// [`noeta_jit::CompiledFn`] `i64` return).
#[cfg(feature = "jit")]
enum JitOutcome {
    /// Resume interpreting this frame at the given bytecode pc (the native code bailed there).
    Bail(usize),
    /// A native `Call` pushed a callee frame; re-derive the top frame and run it (`continue 'reload`).
    Called,
    /// A native `Return` transferred its result to the caller and popped this frame; re-derive the
    /// caller frame and continue (`continue 'reload`).
    Returned,
    /// The bottom frame returned natively; the run is over — yield its value (on `vm.jit_ret`).
    Halted,
    /// The frame aborted (a diagnostic is recorded); propagate the unwind.
    Abort,
}

// Counts how many times a tier-1 bail stub has called `jit_observe` on this thread — the J0 proof
// that generated native code actually ran (and reached a runtime helper), used by the tests.
#[cfg(feature = "jit")]
thread_local! {
    static JIT_OBSERVE_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The J0 runtime-helper skeleton (P-JIT): the generated bail stub calls this once per frame entry,
/// proving a compiled prototype can reach a Rust helper with the live VM pointer under the tier-1
/// ABI. It only bumps a thread-local counter here; J1+ registers the real `retain`/`release`/`call`
/// helpers beside it and reconstitutes `&mut Vm` from `vm` to service them.
#[cfg(feature = "jit")]
extern "C" fn jit_observe(_vm: *mut core::ffi::c_void) {
    JIT_OBSERVE_COUNT.with(|c| c.set(c.get().wrapping_add(1)));
}

/// This thread's running total of tier-1 bail-stub entries (see [`jit_observe`]). Test-only: the
/// J0 proof that native code actually ran.
#[cfg(all(test, feature = "jit"))]
fn jit_observe_count() -> u64 {
    JIT_OBSERVE_COUNT.with(|c| c.get())
}

/// Runtime helper for native `StoreGlobal` (P-JIT globals): the compiled code has already written the
/// slot; this records `g` in `global_order` so program-end teardown destroys globals in reverse
/// binding order (the one part of a first-time `StoreGlobal` that can't be inlined — a `Vec` push may
/// reallocate). Called only on the unbound→bound transition, matching the interpreter's `None` arm.
///
/// # Safety
/// `vm` must be the live `*mut Vm` the tier-1 ABI passed; the call happens synchronously inside
/// `jit_enter`, where no other borrow of `*vm` is active.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_note_global_bound(vm: *mut core::ffi::c_void, g: u32) {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    vm.global_order.push(g);
}

/// Runtime helper: bump a value's refcount (heap-aware register moves, J3). No-op on an immediate.
#[cfg(feature = "jit")]
extern "C" fn jit_retain(v: u64) {
    retain(Value::from_bits(v));
}

/// Runtime helper: drop one reference to a value — the plain, non-destructor release the
/// interpreter's `set_reg` uses on an overwritten register (J3). No-op on an immediate.
#[cfg(feature = "jit")]
extern "C" fn jit_release(v: u64) {
    release(Value::from_bits(v));
}

/// Runtime helper: the destructor-aware release for an IR-relevant `Drop` (may run a `destruct`
/// block if this is the last reference), J3.
///
/// # Safety
/// `vm` must be the live `*mut Vm` the tier-1 ABI passed (see [`jit_note_global_bound`]).
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_release_value(vm: *mut core::ffi::c_void, v: u64) {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    vm.release_value(Value::from_bits(v));
}

/// The layout of [`Frame`] and the `Vec` header — the single source of truth the JIT bakes into its
/// native call-frame codegen (P-CALL). Filled from `offset_of!`/`size_of!` on *this build's* `Frame`
/// and a one-time `Vec`-header probe; because the JIT compiles in the same process/build, the numbers
/// it bakes always match the real layout (a lock test asserts each offset locates its field). See
/// [`noeta_jit::FrameLayout`].
#[cfg(feature = "jit")]
pub fn frame_layout() -> noeta_jit::FrameLayout {
    let (vec_ptr_word, vec_len_word, vec_cap_word) = vec_header_words();
    noeta_jit::FrameLayout {
        frame_size: size_of::<Frame>(),
        frame_align: align_of::<Frame>(),
        proto_offset: core::mem::offset_of!(Frame, proto),
        base_offset: core::mem::offset_of!(Frame, base),
        pc_offset: core::mem::offset_of!(Frame, pc),
        ret_dst_offset: core::mem::offset_of!(Frame, ret_dst),
        ret_transform_offset: core::mem::offset_of!(Frame, ret_transform),
        upvalues_offset: core::mem::offset_of!(Frame, upvalues),
        vec_ptr_word,
        vec_len_word,
        vec_cap_word,
    }
}

/// Identify which of a `Vec`'s three pointer-sized words hold its data pointer, length, and capacity,
/// by constructing a `Vec` with distinct, recognizable values and reading its raw words. `Vec<T>`'s
/// header layout is `T`-independent, so a `Vec<usize>` stands in for `Vec<Frame>`/`Vec<Value>`.
///
/// # Safety
/// `transmute_copy` reads the three header words of a live `Vec` by value; it neither moves nor frees
/// the `Vec`, and `size_of::<Vec<_>>() == size_of::<[usize; 3]>()`.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
fn vec_header_words() -> (usize, usize, usize) {
    let mut v: Vec<usize> = Vec::with_capacity(97);
    v.extend_from_slice(&[0usize; 5]); // len = 5
    let ptr = v.as_ptr() as usize;
    let len = v.len(); // 5
    let cap = v.capacity(); // >= 97
    // ptr (a heap address), len (5), and cap (>= 97) are pairwise distinct, so each word is uniquely
    // identifiable by value.
    let words: [usize; 3] = unsafe { core::mem::transmute_copy(&v) };
    let find = |target: usize| {
        words
            .iter()
            .position(|&w| w == target)
            .expect("Vec header word not found — layout probe failed")
    };
    (find(ptr), find(len), find(cap))
}

/// Runtime helper for a native `Op::Call` (J3): read the call back from `proto`/`pc` and run the
/// shared closure-call setup on the interpreter's frame/register stacks (pushing the callee frame or
/// completing a synchronous first-class-builtin call). Returns the [`noeta_jit`] outcome the compiled
/// function propagates: `OUTCOME_CALLED` (frame pushed), a resume pc (synchronous call done, continue
/// there), or `OUTCOME_ABORTED` (a diagnostic was recorded).
///
/// # Safety
/// `vm`/`frames`/`regs_vec` must be the live pointers the tier-1 ABI passed; the call runs
/// synchronously inside `jit_enter`, where no other borrow of them is active.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_call(
    vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    base: usize,
    proto: i32,
    pc: i32,
) -> i64 {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    let module = vm.module;
    // `emit_call` emits this helper for `Op::Call` *and* `Op::CallGlobal`; source the callee from a
    // register (Call) or straight from its global slot (CallGlobal — a known top-level `fn`, read
    // without a retain, exactly like the interpreter arm).
    let (dst, callee_val, args, span) = match &module.protos[proto as usize].code[pc as usize] {
        Op::Call {
            dst,
            callee,
            args,
            span,
        } => (*dst, regs[base + *callee as usize], args, *span),
        Op::CallGlobal {
            dst,
            global,
            args,
            span,
        } => {
            let cv = vm.globals[global.0 as usize];
            if cv.is_unbound() {
                let msg = format!(
                    "cannot find `{}` in this scope",
                    module.global_name(*global)
                );
                let _ = vm.error(DiagnosticCode::UnknownName, *span, msg);
                return noeta_jit::OUTCOME_ABORTED;
            }
            (*dst, cv, args, *span)
        }
        // `emit_call` only emits this helper for a call op, so this is unreachable; treat a
        // mismatch defensively as an abort rather than misbehave.
        _ => return noeta_jit::OUTCOME_ABORTED,
    };
    let caller_top = frames.len() - 1;
    match vm.setup_closure_call(
        frames,
        regs,
        caller_top,
        base,
        dst,
        callee_val,
        args,
        span,
        pc as usize + 1,
    ) {
        Ok(true) => noeta_jit::OUTCOME_CALLED,
        Ok(false) => pc as i64 + 1,
        Err(Abort) => noeta_jit::OUTCOME_ABORTED,
    }
}

/// Runtime helper for a native `Op::Return` (J3): run the shared return protocol (transfer the value
/// to the caller's destination, pop this frame). Returns `OUTCOME_RETURNED` when it transferred to a
/// caller, or `OUTCOME_HALTED` (parking the value on `vm.jit_ret`) when the bottom frame returned.
///
/// `release_mask` is the P-JSSA S4.0 fast teardown: bit `r` set means window slot `r` may hold a
/// heap value at this return site (the bare-store analysis row at the `Return`'s pc), so only
/// those slots need a release; `u64::MAX` means "release every slot" (an unanalyzed prototype, or
/// one with more than 64 registers). The mask is native-path-sound: this helper is reached only
/// by natively-executed `Op::Return`s, and native execution maintains the analysis's claims
/// (entries verify them, native defs preserve them) — a clear bit is a guarantee the slot holds
/// an immediate, whose release is a no-op.
///
/// # Safety
/// `vm`/`frames`/`regs_vec` must be the live pointers the tier-1 ABI passed.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_return(
    vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    raw: u64,
    release_mask: u64,
) -> i64 {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    match vm.do_return_masked(frames, regs, Value::from_bits(raw), release_mask) {
        Some(v) => {
            vm.jit_ret = v;
            noeta_jit::OUTCOME_HALTED
        }
        None => noeta_jit::OUTCOME_RETURNED,
    }
}

/// The two-word result of [`jit_prepare_call`], returned by value (rax:rdx under SysV; the JIT
/// declares the import with two `i64` returns, which lowers to the same registers — one helper
/// roundtrip instead of the former `prepare_call` + `callee_base` pair, P-JSSA S4.0).
#[cfg(feature = "jit")]
#[repr(C)]
struct PreparedCall {
    /// The callee's compiled entry pointer, or `0` (fall back to `jit_call`).
    fnptr: i64,
    /// The callee's reserved window base (meaningful only when `fnptr != 0`).
    base: usize,
}

/// Runtime helper for a native direct call (J3 native→native): decide whether the `Op::Call` at
/// `pc` can be called directly and, if so, set up the callee frame on the shared stacks and return
/// the callee's compiled entry pointer plus its window base; otherwise a zero `fnptr` (the caller
/// falls back to `jit_call`). Direct-able means: a closure callee, plain arity (no defaults), no
/// upvalues, an already-compiled callee, and stack capacity for the callee window without a
/// reallocation (so the caller's register pointer stays valid across the indirect call).
///
/// # Safety
/// `vm`/`frames`/`regs_vec` must be the live pointers the tier-1 ABI passed.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_prepare_call(
    vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    base: usize,
    proto: i32,
    pc: i32,
    site: *mut noeta_jit::CallSiteCache,
) -> PreparedCall {
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    let module = vm.module;
    // Direct-call setup for `Op::Call` or `Op::CallGlobal`; the callee comes from a register or its
    // global slot. An unbound `CallGlobal` slot falls back to `jit_call`, which raises the E0005.
    const FALLBACK: PreparedCall = PreparedCall { fnptr: 0, base: 0 };
    let (dst, callee_val, args) = match &module.protos[proto as usize].code[pc as usize] {
        Op::Call {
            dst, callee, args, ..
        } => (*dst, regs[base + *callee as usize], args),
        Op::CallGlobal {
            dst, global, args, ..
        } => {
            let cv = vm.globals[global.0 as usize];
            if cv.is_unbound() {
                return FALLBACK;
            }
            (*dst, cv, args)
        }
        _ => return FALLBACK,
    };
    let Some(callee_proto) = callee_val.as_closure() else {
        return FALLBACK; // a first-class builtin / non-callable → fall back
    };
    let cc = &module.protos[callee_proto as usize];
    // Plain arity (no default-filling) and no upvalues — else the general setup path handles it.
    if args.len() != cc.num_params as usize || callee_val.closure_upvalue_count() != 0 {
        return FALLBACK;
    }
    let num_regs = cc.num_registers as usize;
    // The callee's window must fit without reallocating the register stack (which would dangle
    // the caller's pointer).
    if regs.len() + num_regs > regs.capacity() {
        return FALLBACK;
    }
    let Some(jit) = vm.jit.as_ref() else {
        return FALLBACK;
    };
    // Fast convention (P-JSSA S4.1): the callee has a frameless-window body — reserve the window
    // WITHOUT initializing it (the fast body normalizes it before the interpreter can ever see
    // it) and skip the argument copy/retain (the arguments travel as machine arguments, borrowed
    // from the caller's still-live registers). Bit 0 of the returned pointer tags the convention.
    if let Some(ff) = jit.get_fast(callee_proto as usize) {
        // S4.2: fill this call site's inline cache so the next call with the same callee pushes
        // the frame natively, without this helper. The cached closure is **pinned** (retained +
        // held on `jit_cache_pins` until teardown) so its bits can never be reused by another
        // object while cached; a site that sees a second distinct callee is poisoned instead
        // (megamorphic — the pin stays until teardown, bounding pins by site count).
        if !site.is_null() {
            let slot = unsafe { &mut *site };
            if slot[0] == noeta_jit::SITE_EMPTY {
                retain(callee_val);
                vm.jit_cache_pins.push(callee_val);
                slot[1] = ff as u64;
                slot[2] = num_regs as u64;
                slot[3] = callee_proto as u64;
                slot[0] = callee_val.bits();
            } else if slot[0] != callee_val.bits() {
                slot[0] = noeta_jit::SITE_POISON;
            }
        }
        let new_base = regs.len();
        // SAFETY: capacity was checked above, and `reserve_window` keeps the register stack's
        // entire capacity initialized (its growth path fills to capacity), so every element in
        // `..new_base + num_regs` has been written at some point — `set_len`'s contract.
        #[allow(clippy::uninit_vec)]
        unsafe {
            regs.set_len(new_base + num_regs);
        }
        let caller_top = frames.len() - 1;
        frames[caller_top].pc = pc as usize + 1;
        frames.push(Frame {
            proto: callee_proto,
            base: new_base,
            pc: 0,
            ret_dst: dst,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        });
        return PreparedCall {
            fnptr: ff as i64 | 1,
            base: new_base,
        };
    }
    // The classic direct path needs the callee's normal body compiled.
    let Some(f) = jit.get(callee_proto as usize) else {
        return FALLBACK;
    };
    // Set up the callee frame (like `setup_closure_call`'s closure arm, minus defaults/upvalues).
    let new_base = reserve_window(regs, num_regs);
    for (i, &arg_reg) in args.iter().enumerate() {
        let v = regs[base + arg_reg as usize];
        retain(v);
        regs[new_base + i] = v;
    }
    let caller_top = frames.len() - 1;
    frames[caller_top].pc = pc as usize + 1;
    frames.push(Frame {
        proto: callee_proto,
        base: new_base,
        pc: 0,
        ret_dst: dst,
        ret_transform: RetTransform::None,
        upvalues: Vec::new(),
    });
    PreparedCall {
        fnptr: f as usize as i64,
        base: new_base,
    }
}

/// Runtime helper: interpret a direct callee's outcome for its native caller (J3). `RETURNED` → the
/// caller continues in place (`OUTCOME_CONTINUE`, result already in its destination). Otherwise the
/// callee did not complete natively — a bail sets the (still-live) callee frame's pc so the
/// interpreter resumes it there, and the caller propagates `CALLED`; `CALLED`/`ABORTED` pass through.
///
/// # Safety
/// `frames` must be the live pointer the tier-1 ABI passed.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_after_call(
    _vm: *mut core::ffi::c_void,
    frames: *mut core::ffi::c_void,
    callee_outcome: i64,
) -> i64 {
    let frames = unsafe { &mut *(frames as *mut Vec<Frame>) };
    match callee_outcome {
        noeta_jit::OUTCOME_RETURNED => noeta_jit::OUTCOME_CONTINUE,
        noeta_jit::OUTCOME_CALLED => noeta_jit::OUTCOME_CALLED,
        noeta_jit::OUTCOME_ABORTED => noeta_jit::OUTCOME_ABORTED,
        // A bail pc: the callee frame is still the top; point it at its resume pc so the interpreter
        // runs it there, and tell the caller a frame is pending (CALLED). (HALTED can't occur — a
        // direct callee always has a caller — so it also lands here defensively as a re-run.)
        bail_pc => {
            if let Some(top) = frames.last_mut() {
                top.pc = bail_pc.max(0) as usize;
            }
            noeta_jit::OUTCOME_CALLED
        }
    }
}

/// Runtime helper for a native leaf heap/collection op (J4): run the `Op` at `proto`/`pc` — the
/// interpreter's exact arm, refcounts and all — on the shared register stack, and return
/// `OUTCOME_CONTINUE` when it completed. It handles only the non-dispatching, non-erroring path of
/// each op; a receiver that would dispatch (a user `Iterable`/`Index`) or a case that would raise
/// returns the op's own pc, so the interpreter re-runs it. Every early return happens **before** any
/// register write, so a re-run in the interpreter starts from clean state.
///
/// # Safety
/// `vm`/`regs_vec` must be the live pointers the tier-1 ABI passed.
#[cfg(feature = "jit")]
#[allow(unsafe_code)]
extern "C" fn jit_run_leaf_op(
    vm: *mut core::ffi::c_void,
    regs_vec: *mut core::ffi::c_void,
    base: usize,
    proto: i32,
    pc: i32,
) -> i64 {
    // Reconstitute `&mut Vm` (some leaf ops, e.g. `SetField`, release displaced values through
    // `self`); `regs_vec` points at the dispatch loop's local register stack, disjoint from the VM.
    let vm = unsafe { &mut *(vm as *mut Vm) };
    let regs = unsafe { &mut *(regs_vec as *mut Vec<Value>) };
    let module = vm.module;
    let bail = pc as i64;
    match &module.protos[proto as usize].code[pc as usize] {
        Op::MakeRange {
            dst,
            start,
            end,
            inclusive,
            ..
        } => {
            let (Some(a), Some(b)) = (
                regs[base + *start as usize].as_int(),
                regs[base + *end as usize].as_int(),
            ) else {
                return bail; // non-int bounds → interpreter raises the error
            };
            let upper = if *inclusive { b.saturating_add(1) } else { b };
            let elements: Vec<Value> = (a..upper).map(Value::int).collect();
            set_reg(regs, base, *dst, Value::list(elements));
            noeta_jit::OUTCOME_CONTINUE
        }
        Op::IterSnapshot { dst, src, .. } => {
            let v = regs[base + *src as usize];
            if v.is_object() {
                return bail; // `Iterable::iter` dispatch → interpreter
            }
            if v.is_packed_list() {
                set_reg(regs, base, *dst, v.realize_list());
                return noeta_jit::OUTCOME_CONTINUE;
            }
            match v
                .list_items()
                .or_else(|| v.set_items())
                .or_else(|| v.map_values())
            {
                Some(elements) => {
                    for &e in &elements {
                        retain(e);
                    }
                    set_reg(regs, base, *dst, Value::list(elements));
                    noeta_jit::OUTCOME_CONTINUE
                }
                None => bail, // not iterable → interpreter raises the error
            }
        }
        Op::ListLen { dst, src, .. } => match regs[base + *src as usize].list_len() {
            Some(n) => {
                set_reg(regs, base, *dst, Value::int(n as i64));
                noeta_jit::OUTCOME_CONTINUE
            }
            None => bail,
        },
        Op::ListGet { dst, list, index } => {
            let element = regs[base + *index as usize]
                .as_int()
                .filter(|&i| i >= 0)
                .and_then(|i| regs[base + *list as usize].list_get(i as usize));
            match element {
                Some(element) => {
                    retain(element);
                    set_reg(regs, base, *dst, element);
                    noeta_jit::OUTCOME_CONTINUE
                }
                None => bail,
            }
        }
        Op::LoadField {
            dst, obj, field, ..
        } => {
            // The interpreter's inline-cache lookup (`caches` is loop-local) is skipped here; the
            // cache-miss resolution — `slot_of` then `slot_at` — is the same read and is bailed on
            // exactly where the interpreter would raise (unknown field / non-object receiver). A
            // tier-1 inline cache on this path was measured (J6 investigation) and does *not* help: a
            // shape-pointer guard costs about as much as the short field-name scan it would replace,
            // and the real floor is this helper call itself — only a call-free native read (which
            // needs a layout-stable object representation) beats the interpreter. See plans/jit.
            let field = module.name(*field);
            let v = regs[base + *obj as usize];
            match v
                .shape()
                .and_then(|sh| sh.slot_of(field))
                .and_then(|s| v.slot_at(s))
            {
                Some(value) => {
                    retain(value);
                    set_reg(regs, base, *dst, value);
                    noeta_jit::OUTCOME_CONTINUE
                }
                None => bail, // unknown field / non-object → interpreter raises the error
            }
        }
        Op::SetField {
            dst,
            obj,
            field,
            value,
            reuse,
            ..
        } => {
            let field = module.name(*field);
            if vm.set_field_fast(regs, base, *dst, *obj, field, *value, *reuse) {
                noeta_jit::OUTCOME_CONTINUE
            } else {
                bail // unknown field → interpreter raises the error
            }
        }
        Op::Index {
            dst, recv, index, ..
        } => {
            let v = regs[base + *recv as usize];
            let idx = regs[base + *index as usize];
            // An `Index` trait dispatch (`o[i]` on a user object → `get`) pushes a frame — bail. Every
            // error case (out-of-bounds, wrong index type, missing key, non-indexable) also bails so the
            // interpreter raises the exact diagnostic; each of these returns before any register write.
            if v.is_object() {
                return bail;
            }
            if let Some(len) = v.list_len() {
                let Some(i) = idx.as_int().filter(|&i| i >= 0 && (i as usize) < len) else {
                    return bail;
                };
                // A packed list materializes the one element (owned, refcount 1); a boxed list borrows
                // and retains it into `dst` — matching the interpreter exactly.
                let element = if v.is_packed_list() {
                    v.packed_get(i as usize)
                } else {
                    let element = v.list_get(i as usize).expect("bounds checked above");
                    retain(element);
                    element
                };
                set_reg(regs, base, *dst, element);
                noeta_jit::OUTCOME_CONTINUE
            } else if v.is_map() {
                let Some(element) = idx.with_str(|key| v.map_get(key)).flatten() else {
                    return bail; // non-string key or missing key → interpreter raises
                };
                retain(element);
                set_reg(regs, base, *dst, element);
                noeta_jit::OUTCOME_CONTINUE
            } else if let Some(s) = v.as_string() {
                let Some(i) = idx
                    .as_int()
                    .filter(|&i| i >= 0 && (i as usize) < s.chars().count())
                else {
                    return bail;
                };
                let ch = s.chars().nth(i as usize).unwrap().to_string();
                set_reg(regs, base, *dst, Value::string(&ch));
                noeta_jit::OUTCOME_CONTINUE
            } else {
                bail // non-indexable → interpreter raises
            }
        }
        Op::MakeTuple { dst, items } => {
            // Retain each element into a fresh tuple (no bail path — construction never fails). The
            // retains land in the local `elements`, then the tuple is stored, so nothing leaks.
            let mut elements = Vec::with_capacity(items.len());
            for &r in items.iter() {
                let v = regs[base + r as usize];
                retain(v);
                elements.push(v);
            }
            set_reg(regs, base, *dst, Value::tuple(elements));
            noeta_jit::OUTCOME_CONTINUE
        }
        Op::TupleIndex {
            dst,
            receiver,
            index,
            ..
        } => {
            // Positional projection `receiver.N`, retaining the element into `dst` — the companion to
            // the native `ListGet` for `for (i, x) in xs.enumerate()` loops. Out of range bails so the
            // interpreter raises (the checker makes this unreachable for well-typed code).
            let v = regs[base + *receiver as usize];
            match v.tuple_field(*index as usize) {
                Some(element) => {
                    retain(element);
                    set_reg(regs, base, *dst, element);
                    noeta_jit::OUTCOME_CONTINUE
                }
                None => bail,
            }
        }
        _ => bail,
    }
}

/// Builds a fresh host + async executor for a worker isolate (isolates I.4b). Injected by the CLI (its
/// `RealHost` + `RealExecutor`), so `noeta-vm` stays free of `noeta-runtime`/tokio. `Send + Sync` so the
/// worker closure can carry a clone across the thread boundary.
pub type IsolateFactory =
    Arc<dyn Fn() -> (Box<dyn noeta_stdlib::Host>, Box<dyn noeta_stdlib::Executor>) + Send + Sync>;

/// A spawned worker isolate (isolates I.4b): the channel its result (a marshalled [`isolate::Wire`], or
/// an abort message) arrives on, and the thread's join handle (taken to join at teardown).
struct IsolateSlot {
    result: std::sync::mpsc::Receiver<Result<isolate::Wire, String>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// An owned reference to a VM [`Value`], with RAII refcounting — the element type the reactive graph
/// (reactivity S1) stores. The VM's NaN-boxed `Value` is `Copy` and does **not** adjust its refcount
/// on a bitwise clone, so a generic container that clones/drops `Value` internally would leak or
/// double-free. Wrapping it makes `Clone` `retain` and `Drop` `release`, so every clone/drop the
/// `ReactiveGraph` performs (memoizing a value, replacing a signal's content, disposing a node) stays
/// refcount-exact for free — the leak oracle is the proof. Constructing a `GcVal` either takes over an
/// already-owned reference ([`GcVal::owned`]) or takes a fresh reference to a borrowed value
/// ([`GcVal::retained`]); [`GcVal::into_value`] hands the owned reference back out (e.g. into a
/// register) without releasing it.
struct GcVal(Value);

impl GcVal {
    /// Wrap a value we already own one reference to (a freshly built value, a call result). No retain.
    /// (Used from S2/S3 when a `computed`/`effect` body's call result enters the graph.)
    #[allow(dead_code)]
    #[inline]
    fn owned(v: Value) -> Self {
        GcVal(v)
    }

    /// Take a new reference to a value owned elsewhere (e.g. a borrowed builtin argument we want the
    /// graph to keep). Retains.
    #[inline]
    fn retained(v: Value) -> Self {
        retain(v);
        GcVal(v)
    }

    /// Hand the owned reference back out without releasing it (into a register / to `call_value`).
    #[inline]
    fn into_value(self) -> Value {
        let v = self.0;
        std::mem::forget(self);
        v
    }

    /// Peek the underlying value without affecting ownership (e.g. to feed it into the GC root set).
    #[inline]
    fn get(&self) -> Value {
        self.0
    }
}

impl Clone for GcVal {
    #[inline]
    fn clone(&self) -> Self {
        retain(self.0);
        GcVal(self.0)
    }
}

impl Drop for GcVal {
    #[inline]
    fn drop(&mut self) {
        release(self.0);
    }
}

/// A bounded channel's scheduler-owned state (isolates I.1): a FIFO queue of buffered messages, its
/// capacity, and whether it has been closed. Endpoints are indices into [`Vm::channels`]; the queue is
/// never shared heap memory. Mirrors the tree-walker's `Channel`, so the FIFO + block-on-full/empty
/// behaviour is identical and the differential holds by construction. The channel owns one reference to
/// each queued message (released if the channel is still holding it when the VM drops).
/// A channel's backing (isolates I.1 + I.4c). `Local` is the cooperative, in-VM FIFO the sandbox
/// (and any non-parallel VM) uses — a `VecDeque` of heap `Value`s, block-on-full/empty via the
/// cooperative scheduler; identical to the tree-walker's, so the differential holds. `Shared` is the
/// cross-thread channel a parallel VM (CLI real path, I.4c) uses: an `Arc<ChannelCore>` whose
/// `Mutex`-guarded queue of `Wire` messages is reachable from every isolate that holds an endpoint,
/// so shipping a `Sender`/`Receiver` into a worker shares one queue. Send/recv still *poll*
/// cooperatively (Pending on full/empty), never blocking a thread — a producer/consumer across
/// isolate threads makes progress by each thread's scheduler re-polling the shared queue.
enum Channel {
    Local {
        buffer: std::collections::VecDeque<Value>,
        capacity: usize,
        closed: bool,
    },
    Shared(Arc<isolate::ChannelCore>),
}

/// A spawned task in a structured-concurrency scope (Track A.3b): its future (an `async fn` state
/// machine) and its completion result once driven to ready (`None` while pending). The scope owns a
/// reference to `future`, and to `result` once set; both are released when the scope is joined+popped.
struct Task {
    future: Value,
    result: Option<Value>,
    /// Set when the task is **cancelled** (Track A.8) — e.g. a `race` loser. A cancelled task is
    /// never polled again and counts as done for the join; its future is reclaimed by `ScopeEnd`
    /// exactly like a completed task's, so cancellation frees no differently than a normal join (the
    /// leak oracle confirms residency 0). It stops cooperatively at its last suspension. (Running user
    /// `destruct` on an async task's captured locals is a separate, pre-existing gap — see
    /// `plans/deferred.md` — that affects completed and cancelled tasks alike.)
    cancelled: bool,
}

/// The outcome of polling a future once (Track A.3): ready with a value, or still pending.
enum Poll {
    Ready(Value),
    Pending,
}

/// How a value behaves under `?`/`??`: the unwrapped success payload, or the empty case.
enum TryOutcome {
    Success(Value),
    Empty,
}

/// Classify a value for `?`/`??`. Only the built-in `Result`/`Option` enums qualify; the
/// success payload is shared (not retained). Mirrors the M0 tree-walker's `try_branch`.
fn try_classify(v: Value) -> Option<TryOutcome> {
    if !v.is_enum() {
        return None;
    }
    let shape = v.shape()?;
    match (shape.name.as_str(), shape.variant.as_deref()) {
        ("Result", Some("Ok")) | ("Option", Some("some")) => {
            let inner = v
                .enum_data()
                .and_then(|d| d.into_iter().next())
                .unwrap_or_else(Value::unit);
            Some(TryOutcome::Success(inner))
        }
        ("Result", Some("Err")) | ("Option", Some("none")) => Some(TryOutcome::Empty),
        _ => None,
    }
}

/// Whether a value matches a narrowing target (`x.as<T>()`). Generics are erased, so only the
/// runtime **head constructor** is tested. The primitive/collection kinds compare against
/// [`Value::type_name`] — the same canonical strings the M0 tree-walker matches on, so both
/// backends decide a narrowing identically; `Named` (a user struct/class/enum, or the built-in
/// `Option`/`Result`) matches by shape name; `Dyn` always matches (no-op narrowing).
fn narrow_matches(v: Value, target: &NarrowTarget) -> bool {
    let kind = match target {
        NarrowTarget::Int => "int",
        NarrowTarget::Float => "float",
        NarrowTarget::Bool => "bool",
        NarrowTarget::String => "string",
        NarrowTarget::Bytes => "bytes",
        NarrowTarget::Unit => "unit",
        NarrowTarget::List => "list",
        NarrowTarget::Map => "map",
        NarrowTarget::Set => "set",
        NarrowTarget::Tuple => "tuple",
        NarrowTarget::Fn => "function",
        NarrowTarget::Dyn => return true,
        NarrowTarget::Named(name) => return v.shape().is_some_and(|s| &s.name == name),
        NarrowTarget::AnyOf(members) => return members.iter().any(|m| narrow_matches(v, m)),
        // Abstract kind-types match any value of that declaration kind, by the value's shape kind.
        NarrowTarget::AnyEnum => {
            return v.shape().is_some_and(|s| s.kind == ShapeKind::Enum);
        }
        NarrowTarget::AnyStruct => {
            return v.shape().is_some_and(|s| s.kind == ShapeKind::Struct);
        }
        NarrowTarget::AnyClass => {
            return v.shape().is_some_and(|s| s.kind == ShapeKind::Class);
        }
        // A parametrized target (R3): the head must match head-only (which handles the untagged and
        // widening cases), and — when the value carries a reflected tag — its type arguments must
        // match `args` (a `dyn` on either side is a wildcard). An untagged value classifies its args
        // to `dyn`, so `vm_type_repr` yields `dyn` arguments and the check passes head-only.
        NarrowTarget::Generic { head, args } => {
            return narrow_matches(v, head)
                && noeta_ast::reflect::narrow_args_match(args, &vm_type_repr(&v));
        }
    };
    v.type_name() == kind
}

/// Build the [`noeta_stdlib::IoRequest`] for an `fs.*_async` call (Track A.4c/A.10), or `None` if
/// `func` is not an async fs op. Marshals this backend's `Value`s to strings; the func→request
/// mapping is shared in [`noeta_stdlib::IoRequest::from_fs_async`].
fn vm_fs_async_request(func: &str, args: &[Value]) -> Option<noeta_stdlib::IoRequest> {
    let strings: Vec<Option<String>> = args.iter().map(|v| v.as_string()).collect();
    noeta_stdlib::IoRequest::from_fs_async(func, &strings)
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
        vm.init_jit();
    }
    #[cfg(not(feature = "jit"))]
    let _ = jit;
    run_and_teardown(&mut vm, mode)
}

impl<'m> Vm<'m> {
    /// Build a VM ready to run `module` — resolving every derived table (shapes, packed schemas,
    /// methods, destructors, defaults, derives) but **without running `main`** (isolates I.4b). The
    /// normal entry points run `main` right after; a worker isolate instead seeds its globals from the
    /// parent's marshalled snapshot and calls one function, so it must be able to load the module
    /// without triggering the top-level program's side effects.
    fn load(
        module: &'m Module,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
    ) -> Vm<'m> {
        let methods = module
            .methods
            .iter()
            .map(|m| ((m.type_name.clone(), m.method.clone()), m.proto))
            .collect();
        let destructors = module.destructors.iter().cloned().collect();
        let field_defaults = module
            .field_defaults
            .iter()
            .map(|(t, f, proto)| ((t.clone(), f.clone()), *proto))
            .collect();
        let destruct_reachable = module.destruct_reachable.iter().cloned().collect();
        let comparable_derives = module.comparable_derives.iter().cloned().collect();
        let tojson_derives = module.tojson_derives.iter().cloned().collect();
        // One shared `Rc<Shape>` per shape-table entry, then resolve each packed-list layout against it.
        // Schemas are interned inner-before-outer, so a nested struct's schema (a lower index) is always
        // built before the parent that references it.
        let shapes: Vec<Rc<Shape>> = module.shapes.iter().cloned().map(Rc::new).collect();
        let mut packed_schemas: Vec<Rc<noeta_object::PackedSchema>> =
            Vec::with_capacity(module.packed_schemas.len());
        for def in &module.packed_schemas {
            let fields = def
                .fields
                .iter()
                .map(|f| match f {
                    noeta_bytecode::PackedFieldDef::Int => noeta_object::PackedKind::Int,
                    noeta_bytecode::PackedFieldDef::Float => noeta_object::PackedKind::Float,
                    noeta_bytecode::PackedFieldDef::F32 => noeta_object::PackedKind::F32,
                    noeta_bytecode::PackedFieldDef::Bool => noeta_object::PackedKind::Bool,
                    noeta_bytecode::PackedFieldDef::Struct(idx) => {
                        noeta_object::PackedKind::Struct(Rc::clone(&packed_schemas[*idx as usize]))
                    }
                })
                .collect();
            packed_schemas.push(Rc::new(noeta_object::PackedSchema {
                shape: Rc::clone(&shapes[def.shape as usize]),
                fields,
                byte_size: def.byte_size as usize,
                column: def.column,
            }));
        }
        // Resolve each packed `map(...)` result site to its shared schema (P-PACK 2.6 category B).
        let map_packed: HashMap<Span, Rc<noeta_object::PackedSchema>> = module
            .map_packed_sites
            .iter()
            .map(|(span, idx)| (*span, Rc::clone(&packed_schemas[*idx as usize])))
            .collect();
        // Build one shared `Rc<TypeRepr>` per interned reflected element type (R1), so each tagged
        // `MakeList` is a cheap `Rc` clone rather than a fresh `TypeRepr` allocation per execution.
        let type_reprs: Vec<Rc<noeta_ast::reflect::TypeRepr>> =
            module.type_reprs.iter().cloned().map(Rc::new).collect();
        Vm {
            module,
            shapes,
            packed_schemas,
            type_reprs,
            map_packed,
            methods,
            destructors,
            field_defaults,
            destruct_reachable,
            comparable_derives,
            tojson_derives,
            globals: vec![Value::unbound(); module.global_names.len()],
            global_order: Vec::new(),
            next_id: 1,
            host,
            executor,
            scopes: Vec::new(),
            channels: Vec::new(),
            channel_progress: 0,
            reactive: Rc::new(ReactiveGraph::new()),
            parallel_isolates: false,
            isolate_module: None,
            isolate_factory: None,
            isolates: Vec::new(),
            inflight_isolates: 0,
            stdout: String::new(),
            diagnostics: Vec::new(),
            #[cfg(feature = "jit")]
            jit: None,
            #[cfg(feature = "jit")]
            force_jit: false,
            #[cfg(feature = "jit")]
            jit_counters: Vec::new(),
            #[cfg(feature = "jit")]
            jit_declined: Vec::new(),
            #[cfg(feature = "jit")]
            jit_ret: Value::unit(),
            #[cfg(feature = "jit")]
            jit_cache_pins: Vec::new(),
            #[cfg(feature = "jit")]
            jit_frame_template: None,
        }
    }

    /// Build the tier-1 JIT engine and, when `force_jit` is set, eagerly compile every prototype so
    /// the whole run goes through tier 1 (the oracle path). Registers the runtime-helper symbols the
    /// generated code links against. If the host ISA is unavailable the JIT stays `None` and the run
    /// interprets — behaviour is identical either way (J0 always bails to tier 0).
    #[cfg(feature = "jit")]
    fn init_jit(&mut self) {
        let helpers: &[(&str, *const u8)] = &[
            (noeta_jit::OBSERVE_HELPER, jit_observe as *const u8),
            (
                noeta_jit::NOTE_GLOBAL_BOUND_HELPER,
                jit_note_global_bound as *const u8,
            ),
            (noeta_jit::RETAIN_HELPER, jit_retain as *const u8),
            (noeta_jit::RELEASE_HELPER, jit_release as *const u8),
            (
                noeta_jit::RELEASE_VALUE_HELPER,
                jit_release_value as *const u8,
            ),
            (noeta_jit::CALL_HELPER, jit_call as *const u8),
            (noeta_jit::RETURN_HELPER, jit_return as *const u8),
            (
                noeta_jit::PREPARE_CALL_HELPER,
                jit_prepare_call as *const u8,
            ),
            (noeta_jit::AFTER_CALL_HELPER, jit_after_call as *const u8),
            (noeta_jit::LEAF_OP_HELPER, jit_run_leaf_op as *const u8),
        ];
        let template = self.jit_frame_template.get_or_insert_with(|| {
            Box::new(Frame {
                proto: 0,
                base: 0,
                pc: 0,
                ret_dst: 0,
                ret_transform: RetTransform::None,
                upvalues: Vec::new(),
            })
        });
        let template_ptr = template.as_ref() as *const Frame as *const u8;
        match noeta_jit::Jit::new(helpers, frame_layout(), template_ptr) {
            Ok(mut jit) => {
                if self.force_jit {
                    for p in 0..self.module.protos.len() {
                        let _ = jit.compile(self.module, p);
                    }
                }
                self.jit = Some(jit);
            }
            Err(_) => self.jit = None,
        }
    }

    /// Tier-0/tier-1 dispatch at a frame `'reload` (P-JIT). `entry_pc` is where native execution
    /// should resume — `0` for a fresh frame, or a post-call resume pc when re-entering a compiled
    /// frame after its callee returned (J3 resume-native). Returns what the interpreter should do next
    /// (the deopt contract). `None` when the prototype is not compiled and the interpreter should run
    /// it as usual. Hot-counter promotion happens only on a fresh entry (`entry_pc == 0`), so a resume
    /// never compiles — it only re-enters an already-native frame.
    #[cfg(feature = "jit")]
    #[allow(unsafe_code)]
    fn jit_enter(
        &mut self,
        proto: usize,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        base: usize,
        entry_pc: usize,
    ) -> Option<JitOutcome> {
        let f = match self.jit.as_ref().and_then(|j| j.get(proto)) {
            Some(f) => f,
            // Only a fresh entry drives compilation; a resume at a compiled-away frame just interprets.
            None if entry_pc == 0 => self.jit_maybe_compile(proto)?,
            None => return None,
        };
        let vm_ptr = self as *mut Vm as *mut core::ffi::c_void;
        let regs_ptr = regs.as_mut_ptr();
        let globals_ptr = self.globals.as_mut_ptr();
        let frames_ptr = frames as *mut Vec<Frame> as *mut core::ffi::c_void;
        let regs_vec_ptr = regs as *mut Vec<Value> as *mut core::ffi::c_void;
        // SAFETY: `f` is a finalized tier-1 entry point with the `CompiledFn` ABI. `regs_ptr` is the
        // frame data base (native adds `base * 8`); it is used only *before* any call, and a native
        // `Call` returns immediately (`CALLED`) without touching it again, so a `reserve_window`
        // realloc inside `jit_call` can't leave it dangling in use. `frames_ptr`/`regs_vec_ptr` let
        // `jit_call` push the callee frame and grow the shared stacks; `globals` never reallocates.
        // All pointers are live for the synchronous call.
        let raw = unsafe {
            f(
                vm_ptr,
                regs_ptr,
                base,
                globals_ptr,
                frames_ptr,
                regs_vec_ptr,
                entry_pc,
            )
        };
        Some(match raw {
            noeta_jit::OUTCOME_CALLED => JitOutcome::Called,
            noeta_jit::OUTCOME_ABORTED => JitOutcome::Abort,
            noeta_jit::OUTCOME_RETURNED => JitOutcome::Returned,
            noeta_jit::OUTCOME_HALTED => JitOutcome::Halted,
            pc => JitOutcome::Bail(pc as usize),
        })
    }

    /// Bump prototype `proto`'s entry counter and, once it is hot (or immediately under `force_jit`),
    /// compile it — returning the fresh entry point on the promoting call. `None` while still cold or
    /// when the JIT is unavailable.
    #[cfg(feature = "jit")]
    fn jit_maybe_compile(&mut self, proto: usize) -> Option<noeta_jit::CompiledFn> {
        self.jit.as_ref()?;
        // Already found not worth compiling (a prototype whose only loops bail) → keep interpreting.
        if self.jit_declined.get(proto).copied().unwrap_or(false) {
            return None;
        }
        if proto >= self.jit_counters.len() {
            self.jit_counters.resize(proto + 1, 0);
        }
        self.jit_counters[proto] = self.jit_counters[proto].saturating_add(1);
        let hot = self.force_jit || self.jit_counters[proto] >= JIT_HOT_THRESHOLD;
        if !hot {
            return None;
        }
        // A prototype dominated by a bailing loop bounces tier-0↔tier-1 every iteration, slower than
        // the interpreter — decline it once (the oracle's `force_jit` compiles everything anyway).
        if !self.force_jit && !noeta_jit::worth_compiling(&self.module.protos[proto]) {
            if proto >= self.jit_declined.len() {
                self.jit_declined.resize(proto + 1, false);
            }
            self.jit_declined[proto] = true;
            return None;
        }
        let module = self.module;
        let jit = self.jit.as_mut()?;
        jit.compile(module, proto).ok()
    }

    /// On-stack replacement trigger (P-JIT J5): a taken **backward branch** in prototype `proto` is a
    /// loop back-edge. Count it toward the hot threshold and, once the prototype crosses it, compile
    /// the prototype — returning `true` to signal the inner loop to re-enter native code at the loop
    /// header (the compiled body has an OSR entry block for every loop header). `false` = keep
    /// interpreting.
    ///
    /// This closes the hole where a long-running loop never gets hot: promotion otherwise counts only
    /// frame *entries*, so a top-level program that is one big loop (its `main` frame entered exactly
    /// once) would run entirely in tier 0. Counting back-edges makes such a loop promote and jump into
    /// native code mid-flight.
    ///
    /// **One OSR per prototype.** If the prototype is already compiled we do nothing: the frame goes
    /// native at its next `'reload` anyway, and re-OSRing from tier 0 (after a native op bailed back)
    /// would risk bouncing tier-0↔tier-1 every iteration for a loop whose body native can't sustain.
    #[cfg(feature = "jit")]
    fn jit_osr_backedge(&mut self, proto: usize) -> bool {
        if self.jit.as_ref().and_then(|j| j.get(proto)).is_some() {
            return false;
        }
        // Already found un-sustainable (all loops bail) → keep interpreting, no per-iteration re-scan.
        if self.jit_declined.get(proto).copied().unwrap_or(false) {
            return false;
        }
        // Bump the back-edge counter; only decide once the prototype is hot. `force_jit` (the oracle)
        // compiles everything for full coverage, so it skips the worthiness gate.
        if proto >= self.jit_counters.len() {
            self.jit_counters.resize(proto + 1, 0);
        }
        self.jit_counters[proto] = self.jit_counters[proto].saturating_add(1);
        if !(self.force_jit || self.jit_counters[proto] >= JIT_HOT_THRESHOLD) {
            return false;
        }
        if !self.force_jit && !noeta_jit::worth_osr(&self.module.protos[proto]) {
            // A heap-op-dominated loop: native would bounce tier-0↔tier-1 every iteration, slower than
            // the interpreter. Decline OSR for this prototype, once and for good.
            if proto >= self.jit_declined.len() {
                self.jit_declined.resize(proto + 1, false);
            }
            self.jit_declined[proto] = true;
            return false;
        }
        let module = self.module;
        let jit = match self.jit.as_mut() {
            Some(j) => j,
            None => return false,
        };
        jit.compile(module, proto).is_ok()
    }
}

/// Run `main` and tear the VM down (globals, cycle collection, channel drain), returning the program's
/// [`RunResult`]. Split from [`Vm::load`] so a worker isolate can load the module without running
/// `main` (isolates I.4b). The teardown is unchanged from the original single-function entry.
fn run_and_teardown(vm: &mut Vm, mode: noeta_value::CollectorMode) -> RunResult {
    let module = vm.module;
    let regs = vec![Value::unit(); module.main().num_registers as usize];
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
    if let Ok(v) = vm.run(vec![top], regs) {
        release(v);
    }
    // An abort (e.g. a detected deadlock, E0010) can leave open `concurrent` scopes whose tasks
    // were never joined — each scope still owns its tasks' futures (and any parked results).
    // Release them exactly as `ScopeEnd` would, so an aborted program's teardown stays
    // refcount-exact (the anomaly oracle checks) and destructors on captured locals still run.
    for scope in std::mem::take(&mut vm.scopes) {
        for task in scope {
            vm.release_value(task.future);
            if let Some(result) = task.result {
                vm.release_value(result);
            }
        }
    }
    // Release the JIT inline caches' closure pins (S4.2) before any collector accounting: a
    // pinned closure the program itself dropped must read as garbage now, not as an anomaly.
    // Native code can no longer run (the run above is over), so the caches are dead.
    #[cfg(feature = "jit")]
    for v in std::mem::take(&mut vm.jit_cache_pins) {
        release(v);
    }
    // Reap reference cycles the program may have tied through `mut` fields / cells / closures that
    // refcounting alone cannot reclaim (e.g. a self-recursive nested `fn`). The two collectors run at
    // different points: the **trace** marks from the live globals *before* teardown (the frame stack
    // is unwound, so the globals are the whole root set) and sweeps everything unreachable; the
    // **trial-deletion** path instead reaps its buffered candidates *after* teardown, once every frame
    // and global release has had a chance to buffer the cycle's roots.
    if mode == noeta_value::CollectorMode::Trace {
        let mut roots: Vec<Value> = vm
            .globals
            .iter()
            .copied()
            .filter(|v| !v.is_unbound())
            .collect();
        // The reactive graph (reactivity S1) holds a `+1` reference to every signal's content and
        // every computed/effect body, but a value reachable *only* through the graph is invisible to
        // this mark-from-roots sweep. Feed those held values in as roots so the collector does not
        // reclaim a still-referenced value out from under the graph (which `reactive.clear()` below
        // would then double-free). The reactive analogue of scanning the channel buffers.
        vm.reactive.for_each_value(|gv| roots.push(gv.get()));
        let garbage = collect_trace(&roots);
        vm.reclaim_cycle_garbage(garbage);
    }
    // Release any messages still buffered in channels at program end (isolates I.1) — undrained
    // `send`s. Draining here keeps residency at zero; `release_value` runs any message destructor. A
    // `Shared` channel (I.4c) holds `Wire` copies, not heap `Value`s, so dropping it frees cleanly.
    for chan in std::mem::take(&mut vm.channels) {
        if let Channel::Local { buffer, .. } = chan {
            for msg in buffer {
                vm.release_value(msg);
            }
        }
    }
    // Release every value held by the reactive graph (reactivity S1): signals never disposed by the
    // program still own their content until here. Dropping the nodes fires each `GcVal`'s `Drop`, so
    // residency returns to zero — the leak oracle's proof that the graph's refcounting is exact.
    vm.reactive.clear();
    // Destroy the globals at program end in reverse declaration order, running each
    // destructor on its last reference — the deterministic destruction the spec requires.
    for slot in vm.global_order.clone().into_iter().rev() {
        let v = std::mem::replace(&mut vm.globals[slot as usize], Value::unbound());
        if !v.is_unbound() {
            vm.release_value(v);
        }
    }
    // Backup collection (object-model slice 2c): a reference `class` cycle rooted in the globals
    // (`a.next = b; b.next = a`) survives the teardown above — each member still holds the other, so
    // refcounting never reaches zero. With the globals now gone there are **no roots left**, so every
    // still-live object is unreachable garbage; trace-collect from an empty root set to reclaim it,
    // running each member's `destruct` exactly once. (The pre-teardown trace above only catches
    // cycles already unreachable mid-run.)
    if mode == noeta_value::CollectorMode::Trace {
        let garbage = collect_trace(&[]);
        vm.reclaim_cycle_garbage(garbage);
    }
    if mode == noeta_value::CollectorMode::TrialDeletion {
        let garbage = noeta_gc::collect_trial_deletion();
        vm.reclaim_cycle_garbage(garbage);
    }

    // Join any isolate worker threads not already harvested (a structured scope harvests + joins its
    // isolates at `}`, so this is normally empty — defensive against an early exit).
    for slot in std::mem::take(&mut vm.isolates) {
        if let Some(h) = slot.handle {
            let _ = h.join();
        }
    }

    let exit_code = if vm.diagnostics.is_empty() { 0 } else { 1 };
    RunResult {
        stdout: std::mem::take(&mut vm.stdout),
        exit_code,
        diagnostics: std::mem::take(&mut vm.diagnostics),
    }
}

/// Run one real-thread isolate to completion (isolates I.4b), on its own thread. Builds a fresh VM with
/// its own heap (thread-local), host, and executor from `factory`, seeds globals from the parent's
/// marshalled snapshot, rebuilds the arguments, calls `callee(args)` and drives the resulting future to
/// completion, then marshals the result back to `Send` [`isolate::Wire`]. An abort inside the isolate
/// (a panic) comes back as `Err(message)`, which the parent re-raises at the `.await`. The worker tears
/// down its own globals/channels so its thread-local heap returns to zero residency.
fn run_isolate_worker(
    module: &Arc<Module>,
    factory: &IsolateFactory,
    proto: u32,
    wire_args: Vec<isolate::Wire>,
    wire_globals: Vec<(u32, isolate::Wire)>,
    span: Span,
) -> Result<isolate::Wire, String> {
    noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    let (host, executor) = factory();
    let mut wvm = Vm::load(module, host, executor);
    wvm.parallel_isolates = true;
    wvm.isolate_module = Some(Arc::clone(module));
    wvm.isolate_factory = Some(factory.clone());
    // Seed the worker's globals from the parent's snapshot so the isolate body can call other
    // top-level functions (and read value-type constants). Slots match: parent and worker share the
    // same `Arc<Module>`, so a global's `GlobalId` is identical on both sides (P-VMT-GSLOT).
    for (slot, wire) in &wire_globals {
        let value = isolate::rebuild(wire, &wvm.shapes, &mut wvm.channels);
        wvm.globals[*slot as usize] = value;
        wvm.global_order.push(*slot);
    }
    let arg_vals: Vec<Value> = wire_args
        .iter()
        .map(|w| isolate::rebuild(w, &wvm.shapes, &mut wvm.channels))
        .collect();
    let callee = Value::closure(proto, Vec::new());
    let outcome = match wvm.call_value(callee, arg_vals, span) {
        Ok(future) => {
            let result = wvm.drive_future(future, span);
            release(future);
            result
        }
        Err(abort) => Err(abort),
    };
    release(callee);
    let message = match outcome {
        Ok(result) => {
            let marshalled = isolate::marshal(result, &wvm.shapes, &wvm.channels)
                .map_err(|e| format!("isolate result is not shippable: {e}"));
            wvm.release_value(result);
            marshalled
        }
        Err(_abort) => Err(wvm
            .diagnostics
            .last()
            .map(|d| d.message.clone())
            .unwrap_or_else(|| "isolate aborted".to_string())),
    };
    // Tear the worker down so its thread-local heap returns to zero residency: release the JIT
    // inline caches' closure pins (S4.2), destroy globals in reverse declaration order, then
    // drain any channel buffers.
    #[cfg(feature = "jit")]
    for v in std::mem::take(&mut wvm.jit_cache_pins) {
        release(v);
    }
    for slot in wvm.global_order.clone().into_iter().rev() {
        let value = std::mem::replace(&mut wvm.globals[slot as usize], Value::unbound());
        if !value.is_unbound() {
            wvm.release_value(value);
        }
    }
    for chan in std::mem::take(&mut wvm.channels) {
        if let Channel::Local { buffer, .. } = chan {
            for msg in buffer {
                wvm.release_value(msg);
            }
        }
    }
    wvm.reactive.clear();
    message
}

impl<'m> Vm<'m> {
    /// Materialize the `#[type_name(...)]` attributes from the module manifest into a
    /// `List<Attributed<T>>` — each a real `T` struct (built from its stored args) paired with its
    /// target. Shapes are built fresh from the shared reflection info; because shape equality is
    /// structural (name + fields), they match the tree-walker's by construction.
    fn materialize_attributes(&self, type_name: &str) -> Value {
        let attributed_shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "Attributed",
            vec!["target".to_string(), "value".to_string()],
        ));
        let shape = noeta_ast::reflect::attribute_shape(type_name, &self.module.reflection);
        let fields = shape.fields;
        let kind = if shape.is_struct {
            ShapeKind::Struct
        } else {
            ShapeKind::Class
        };
        let items: Vec<Value> = self
            .module
            .reflection
            .manifest
            .iter()
            .filter(|a| a.name == type_name)
            .map(|a| {
                let values: Vec<Value> =
                    noeta_ast::reflect::materialize_args(a, &fields, &shape.defaults)
                        .iter()
                        .map(|v| attr_value_to_vm(v, &self.module.reflection))
                        .collect();
                let t_shape = Rc::new(Shape::object(kind, type_name, fields.clone()));
                let t_value = Value::object(t_shape, values);
                Value::object(
                    attributed_shape.clone(),
                    vec![Value::string(&a.target), t_value],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Materialize the `(declaration, Role)` index from the module's reflection info into a
    /// `List<RoleBinding>` — each `{ target: string, role: Role }`. Shapes are built fresh; because
    /// shape equality is structural (name + variant + fields), the `Role` enum and `RoleBinding`
    /// struct match the tree-walker's by construction. (P2.7.)
    fn materialize_roles(&self) -> Value {
        let binding_shape = Rc::new(Shape::object(
            ShapeKind::Struct,
            "RoleBinding",
            vec!["target".to_string(), "role".to_string()],
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .roles
            .iter()
            .map(|r| {
                Value::object(
                    binding_shape.clone(),
                    vec![
                        Value::string(&r.target),
                        make_role(&r.enum_name, &r.variant),
                    ],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Record a runtime diagnostic and produce the unwind token.
    fn error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Abort {
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Abort
    }

    /// Release a value that may be the *last* reference to a destructor-carrying object. If so,
    /// the `destruct` block runs synchronously (with the instance's fields in scope) before the
    /// object is freed — the deterministic destruction the spec requires. Used at every
    /// destructor-relevant drop point: reassignment, program end, and (Phase 4) a destructor-
    /// relevant `Op::Drop` at a local's last use. A non-relevant release uses the plain `release`.
    /// Reclaim the garbage a cycle collector identified (Phase 6 destructor-on-collect + the trace's
    /// external-reference fix). Runs each fresh member's `__destruct` while the whole dead subgraph is
    /// still allocated (container-before-contained — a destructor may read a sibling's fields), then —
    /// for the trace, whose shallow free does not release children — drops every reference a freed
    /// member holds to a still-**live** value so that value is not left over-counted, and finally frees
    /// every member. Members are pinned across the destructor + release phases so a side effect there
    /// cannot free one early; `gc_free_shallow` reclaims the box regardless of that pinned count. Object
    /// cycles cannot form under value semantics (a shared mutation copies), so a *member* never carries
    /// a destructor — but a destructor-bearing value **captured** by a cycle's closure is itself dead,
    /// and this is where its `__destruct` fires. Intra-cycle order is best-effort (spec §6); the eval
    /// reaper mirrors the behavior so the differential agrees on order-independent programs.
    fn reclaim_cycle_garbage(&mut self, garbage: noeta_gc::Garbage) {
        let noeta_gc::Garbage {
            fresh,
            already_destructed,
            release_external,
        } = garbage;
        // Reclaim in `Trace` mode regardless of the active collector: the members are pinned below, so
        // in `TrialDeletion` mode a destructor's internal `release` of a (refcount-inflated) member
        // would see it survive and **re-buffer it as a candidate** — a stale entry pointing at memory
        // this reclaim then frees. Trace buffers nothing; the frees here are explicit `free_shallow`
        // either way. Restored after (a no-op at clean exit, but correct if a safepoint ever calls this).
        let saved_mode = noeta_value::collector_mode();
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
        for &g in fresh.iter().chain(&already_destructed) {
            retain(g);
        }
        // Finalize cycle members in **reverse-creation order** (newest-first, object-model slice 2c)
        // so cyclic `destruct` order is deterministic and agrees with the tree-walker — the live
        // registry is a `HashSet`, so `fresh`'s own order is otherwise arbitrary.
        let mut to_destruct = fresh.clone();
        to_destruct.sort_by_key(|g| std::cmp::Reverse(g.gc_seq()));
        for &g in &to_destruct {
            if let Some(proto) = g
                .shape()
                .and_then(|s| self.destructors.get(&s.name).copied())
            {
                self.run_destructor(proto, g);
            }
        }
        if release_external {
            let dead: HashSet<u64> = fresh
                .iter()
                .chain(&already_destructed)
                .map(|v| v.bits())
                .collect();
            for &g in &fresh {
                for child in g.gc_children() {
                    if !dead.contains(&child.bits()) {
                        self.release_value(child);
                    }
                }
            }
        }
        for g in fresh.into_iter().chain(already_destructed) {
            g.gc_free_shallow();
        }
        noeta_value::set_collector_mode(saved_mode);
    }

    fn release_value(&mut self, value: Value) {
        // Immediates, and any reference that is not the last, never run a destructor here: an
        // immediate has none, and an alias survives (spec §2 — destruction defers to the final
        // reference). Both take the plain release (a decrement; a free only at the true last ref).
        if !value.is_pointer() || value.refcount() > 1 {
            release(value);
            return;
        }
        // The last reference. Take the slow container-before-contained path only if this subtree
        // owns a destructor — its own (an object/enum whose `destruct` is in the table) or a
        // contained one (`subtree_owns_destructor`). Otherwise the plain recursive free reclaims it
        // with no per-node destructor lookups — the Phase-4.3 fast path for non-RAII data.
        let own = value
            .shape()
            .and_then(|s| self.destructors.get(&s.name).copied());
        if own.is_none() && !self.subtree_owns_destructor(value) {
            release(value);
            return;
        }
        // Container before contained (spec §4): the container's own `destruct` runs first, while
        // its fields are still live; then each child is released in declared/iteration order — a
        // child reaching zero runs its own `destruct`, recursively — and finally the container's
        // own box is freed (children already released, so a shallow free that does not touch them).
        if let Some(proto) = own {
            self.run_destructor(proto, value);
        }
        // Children are released container-before-contained in the spec's destruction order: an
        // object's/list's fields in declared/iteration order, but a **closure's captured upvalues in
        // reverse capture order** — the reverse-declaration order the tree-walker uses at scope exit
        // (`Scope::order` reversed), so a multi-capture closure destroys its captures identically on
        // both backends.
        let children = value.gc_children();
        if value.is_closure() {
            for child in children.into_iter().rev() {
                self.release_value(child);
            }
        } else {
            for child in children {
                self.release_value(child);
            }
        }
        value.gc_free_shallow();
    }

    /// Perform `Op::SetField`'s store into `regs` — the reference-`class` in-place mutation, the value-
    /// `struct` copy-on-write, and the `reuse` fast path — returning `true` when the field exists and
    /// the store happened, or `false` when `obj` has no such field (the caller raises the E0022-family
    /// error or, from the tier-1 leaf helper, bails so the interpreter re-runs and raises it). Factored
    /// so the interpreter arm and the JIT leaf helper (P-JIT J4) share one implementation and are
    /// refcount-identical by construction. The `false` path performs **no** mutation, so a leaf-helper
    /// bail re-runs from clean state (the bail-before-mutate rule).
    // The operands mirror `Op::SetField`'s fields one-to-one; both call sites already hold them
    // destructured, so an explicit parameter list is clearer here than a wrapper struct.
    #[allow(clippy::too_many_arguments)]
    fn set_field_fast(
        &mut self,
        regs: &mut [Value],
        base: usize,
        dst: u16,
        obj: u16,
        field: &str,
        value: u16,
        reuse: bool,
    ) -> bool {
        let v = regs[base + obj as usize];
        let val = regs[base + value as usize];
        let Some(slot) = v.shape().and_then(|sh| sh.slot_of(field)) else {
            return false;
        };
        // A reference `class` mutates the shared instance **in place**, regardless of refcount or the
        // reuse flag — the change must be visible through every alias (object-model slice 2b). A value
        // `struct` keeps copy-on-write below.
        let is_class = v.shape().is_some_and(|s| s.kind == ShapeKind::Class);
        if is_class {
            let old = v.replace_slot(slot, val);
            self.release_value(old);
            if reuse {
                // The receiver's reference moves into `dst` (its register cleared, as in the struct
                // path); the instance is unchanged-but-mutated.
                regs[base + obj as usize] = Value::unit();
                set_reg(regs, base, dst, v);
            } else {
                // The receiver register is untouched (a temp is dropped later by the compiler-emitted
                // `Drop`); `dst` takes its own counted reference.
                retain(v);
                set_reg(regs, base, dst, v);
            }
        } else if reuse {
            // The receiver's sole reference moves into this op (its register cleared, like the
            // map/struct in-place paths), so the `refcount == 1` check below sees the accumulator's
            // reference and a `dst == obj` store is safe.
            regs[base + obj as usize] = Value::unit();
            if v.refcount() == 1 {
                // Unique: overwrite the slot in place (`replace_slot` retains the new value); the
                // displaced old value's `destruct` fires now (spec §5).
                let old = v.replace_slot(slot, val);
                self.release_value(old);
                set_reg(regs, base, dst, v);
            } else {
                // Aliased: copy with the field replaced, preserving the alias's view, then release the
                // consumed receiver reference.
                let new = object_copy_with_slot(v, slot, val);
                release(v);
                set_reg(regs, base, dst, new);
            }
        } else {
            // Unmarked: a functional update — copy with the field replaced, the receiver register
            // untouched (a temp receiver is dropped by the compiler-emitted Drop).
            let new = object_copy_with_slot(v, slot, val);
            set_reg(regs, base, dst, new);
        }
        true
    }

    /// Whether `value`'s subtree may contain a destructor — the container-before-contained
    /// field-walk gate (spec §4, Phase 4.3). An object/enum is decided by its type name against the
    /// checker's destruct-reachability set; a list/map/set is always walked because its element
    /// types are erased at runtime (a non-relevant element then takes the fast path on its own);
    /// any other value kind (string, closure, cell, handle, boxed int) is a leaf with no
    /// destructor-bearing children, so it frees plainly.
    fn subtree_owns_destructor(&self, value: Value) -> bool {
        match value.shape() {
            Some(shape) => self.destruct_reachable.contains(&shape.name),
            // A list/map/set is always walked (its element types are erased). A closure/cell/future/
            // generator may hold a destructor-bearing value it captured — an async fn's hoisted locals,
            // or a value captured by a closure that outlives its defining scope — so walk it too, but
            // only when the program defines any destructor at all (destructor-free code keeps the
            // plain-free fast path). `release_value`'s refcount guard defers a still-aliased capture's
            // destructor to its true last reference.
            None => {
                value.is_list()
                    || value.is_map()
                    || value.is_set()
                    || (!self.destruct_reachable.is_empty()
                        && (value.is_closure()
                            || value.is_cell()
                            || value.is_future()
                            || value.is_iter()))
            }
        }
    }

    /// Run an instance's `destruct` block on a fresh frame stack, with the instance in
    /// register 0 (so its fields resolve like a method's). The instance is retained for the
    /// duration, so the block sees a live object and the net reference count is unchanged —
    /// the caller's subsequent `release` performs the actual free.
    fn run_destructor(&mut self, proto: u32, instance: Value) {
        let chunk = &self.module.protos[proto as usize];
        let mut regs = vec![Value::unit(); chunk.num_registers as usize];
        retain(instance);
        regs[0] = instance;
        let frame = Frame {
            proto,
            base: 0,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        };
        // A destructor returns unit (its body is run for its effects); discard it. An abort
        // inside a destructor has already recorded its diagnostic.
        if let Ok(v) = self.run(vec![frame], regs) {
            release(v);
        }
    }

    /// Run a frame stack until its bottom frame returns (`Return`) or the program/function
    /// halts (an implicit unit return). Returns the produced value, which the caller owns.
    /// On abort, every register still owned by a frame left on the stack is released here.
    fn run(&mut self, mut frames: Vec<Frame>, mut regs: Vec<Value>) -> Result<Value, Abort> {
        // Give the register stack generous headroom up front (P-JIT J3): a native direct call only
        // fires when the callee window fits without reallocating (so the caller's register pointer
        // stays valid), so a pre-reserved buffer keeps common recursion on the fast path. A deeper
        // stack simply reallocates once and the direct-call check re-passes at the new capacity, so
        // this only affects performance, never correctness — and is a no-op without the `jit` feature.
        #[cfg(feature = "jit")]
        regs.reserve(8192usize.saturating_sub(regs.len()));
        let result = self.dispatch(&mut frames, &mut regs);
        if result.is_err() {
            // Phase 4.2c-ii: a panic unwinds the live frames. Before reclaiming their memory, fire
            // the `destruct` of every live destructor-bearing frame local — innermost frame first,
            // reverse-construction within each (the `frame_locals` list reversed) — so an aborting
            // program destroys its abandoned values deterministically (spec §6). This matches the
            // tree-walker, which fires each aborted scope's `drain_reverse` as the abort climbs the
            // call stack. Each fired register is cleared to `unit`, so the plain release below (which
            // also reclaims temporaries, never destructor-fired in either backend) never double-frees.
            for fi in (0..frames.len()).rev() {
                let f_base = frames[fi].base;
                let proto = frames[fi].proto as usize;
                let count = self.module.protos[proto].frame_locals.len();
                for idx in (0..count).rev() {
                    let reg = self.module.protos[proto].frame_locals[idx] as usize;
                    let v = std::mem::replace(&mut regs[f_base + reg], Value::unit());
                    self.release_value(v);
                }
            }
            // Release each live frame's register window from the shared stack (P-VMT-FRAME). A frame
            // owns `regs[base .. base + num_registers]`; the windows partition the stack, so this
            // releases every register exactly once.
            for frame in &frames {
                let n = self.module.protos[frame.proto as usize].num_registers as usize;
                for i in 0..n {
                    release(regs[frame.base + i]);
                }
                for u in &frame.upvalues {
                    release(*u);
                }
            }
        }
        result
    }

    /// The dispatch loop. Returns `Ok(value)` once the bottom frame returns (the stack is
    /// then empty), or `Err(Abort)` with the stack left intact for [`Vm::run`] to release.
    fn dispatch(&mut self, frames: &mut Vec<Frame>, regs: &mut Vec<Value>) -> Result<Value, Abort> {
        // Copy the shared module reference out so the loop can index prototypes without
        // borrowing `self` — leaving `self.stdout`/`globals`/`diagnostics` free to mutate.
        let module = self.module;
        // Per-run inline caches, one slot per cacheable call site (`LoadField`/`CallMethod`),
        // indexed by the op's `cache` field. Each entry memoizes the last receiver shape and the
        // resolved field-slot / method prototype; a hit is a pointer compare against the cached
        // shape, skipping the field-name scan / `(type, method)` hashmap lookup. A local (not a
        // `self` field) so it neither borrows `self` in the loop nor leaks across runs; holding the
        // `Rc<Shape>` keeps the cached shape alive, so the pointer key can never alias a freed shape.
        let mut caches: Vec<Option<(Rc<Shape>, u32)>> = vec![None; module.cache_slots as usize];
        // S3 dispatch window (P-VMT-DISP). The interpreter is two nested loops. The OUTER `'reload`
        // loop re-derives the active frame's register window — its base, prototype (`chunk`), and
        // starting `pc` — and is re-entered ONLY when control transfers to a *different* frame: a
        // call pushes one, a return / short-circuiting `?` pops one, each ending its arm with
        // `continue 'reload`. Within a frame the INNER loop runs straight-line: an op advances the
        // local `pc` and loops; a jump assigns it; neither re-indexes `frames` nor re-bounds-checks
        // the prototype table, which is what pinned the empty-loop floor at ~80 ns/iter before this
        // slice. `fbase`/`chunk` are immutable for the frame's lifetime — the only way to get a new
        // window is a new outer iteration, so a transfer *cannot* silently forget to reload. The
        // current frame's window is `regs[fbase .. fbase + chunk.num_registers]` (P-VMT-FRAME) and
        // every operand access below is `regs[fbase + i]` (`fbase`, not `base`, to avoid colliding
        // with ops that carry their own `base` field). `chunk` borrows `*module` (an `&'m Module`
        // copied out of `self`), so it is independent of the `&mut self` the arms use.
        'reload: loop {
            let top = frames.len() - 1;
            let fbase = frames[top].base;
            let proto = frames[top].proto as usize;
            let chunk = &module.protos[proto];
            let mut pc = frames[top].pc;
            // Tier-0/tier-1 dispatch (P-JIT). Only at a fresh frame entry (`pc == 0`): a return-pop
            // re-enters `'reload` with the caller's saved `pc > 0`, and an in-frame jump never leaves
            // the inner loop, so `pc == 0` is exactly "this frame is starting". A compiled prototype
            // may run the whole frame in native code; J0 always bails, so control falls straight
            // through to the interpreter below (byte-identical).
            // Fire at every frame `'reload`, not only fresh entries: after a native `Call`'s callee
            // returns, the interpreter re-enters the caller at its resume pc and native execution
            // picks up there (J3 resume-native). `entry_pc = pc` is 0 for a fresh frame or the saved
            // resume pc otherwise; the compiled code jumps to that block (or bails if it has no entry
            // for it).
            #[cfg(feature = "jit")]
            if self.jit.is_some() {
                match self.jit_enter(proto, frames, regs, fbase, pc) {
                    // Not compiled → interpret as usual.
                    None => {}
                    // Native code ran the frame to a bail point and left the register window in the
                    // state the interpreter expects at `resume`; continue interpreting there.
                    Some(JitOutcome::Bail(resume)) => pc = resume,
                    // A native `Call` pushed the callee frame — run it.
                    Some(JitOutcome::Called) => continue 'reload,
                    // A native `Return` transferred to the caller and popped this frame — re-derive
                    // the caller and continue.
                    Some(JitOutcome::Returned) => continue 'reload,
                    // The bottom frame returned natively — yield its value.
                    Some(JitOutcome::Halted) => {
                        return Ok(std::mem::replace(&mut self.jit_ret, Value::unit()));
                    }
                    // The frame aborted inside native code (a diagnostic is recorded).
                    Some(JitOutcome::Abort) => return Err(Abort),
                }
            }
            // OSR back-edge trigger (P-JIT J5): a taken backward branch to `target` is a loop
            // back-edge. When the JIT is armed (real-host path only — `self.jit` is `None` on the
            // sandbox/differential path, so this is a single predicted branch there) and the branch
            // goes backward, count it; once the prototype is hot, compile it and re-enter native at the
            // loop header by saving `pc` and reloading. `$target` is evaluated against the current `pc`
            // (the branch's own location) *before* `pc` is reassigned to it.
            macro_rules! osr_backedge {
                ($target:expr) => {
                    #[cfg(feature = "jit")]
                    {
                        let _osr_t = $target as usize;
                        if _osr_t <= pc && self.jit.is_some() && self.jit_osr_backedge(proto) {
                            frames[top].pc = _osr_t;
                            continue 'reload;
                        }
                    }
                };
            }
            loop {
                // Every prototype ends with `Halt`, so `pc` never runs off the end — index directly
                // instead of the `.get()` guard the pre-S3 loop used. A call keeps `fbase` on the
                // *caller* until `continue 'reload`, so a call op reads its arguments first.
                let op = &chunk.code[pc];
                match op {
                    Op::LoadConst { dst, k } => {
                        let v = materialize(&chunk.consts[*k as usize]);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::Move { dst, src } => {
                        let v = regs[fbase + *src as usize];
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::LoadGlobal { dst, global, span } => {
                        // Direct slot index — no name hashing (P-VMT-GSLOT). An unbound slot holds the
                        // `Value::unbound` sentinel (P-JIT globals); every other value is a real binding.
                        let v = self.globals[global.0 as usize];
                        if v.is_unbound() {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "cannot find `{}` in this scope",
                                    module.global_name(*global)
                                ),
                            ));
                        }
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::StoreGlobal { global, src } => {
                        // Transfer ownership from the (dead) source temporary into the global,
                        // rather than retaining a duplicate. This keeps the reference count equal
                        // to the tree-walker's direct-binding model — a lingering temporary would
                        // otherwise inflate the count and hide a reassigned value's last reference,
                        // suppressing its destructor.
                        let v = std::mem::replace(&mut regs[fbase + *src as usize], Value::unit());
                        let old = std::mem::replace(&mut self.globals[global.0 as usize], v);
                        if old.is_unbound() {
                            // First binding of this slot: record it for reverse-order destruction.
                            self.global_order.push(global.0);
                        } else {
                            // Reassigning: the previous value is dropped here, running its destructor
                            // if this was its last reference.
                            self.release_value(old);
                        }
                        pc += 1;
                    }
                    Op::TakeGlobal { dst, global, span } => {
                        // Move the global's value into `dst`, leaving `unit` — no retain, so the single
                        // owning reference transfers and a following `ConcatInPlace` can see uniqueness.
                        // An unbound slot raises E0005 (and is left unbound); a bound slot stays bound
                        // (to `unit`), matching the pre-refactor `Option` semantics.
                        if self.globals[global.0 as usize].is_unbound() {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "cannot find `{}` in this scope",
                                    module.global_name(*global)
                                ),
                            ));
                        }
                        let v =
                            std::mem::replace(&mut self.globals[global.0 as usize], Value::unit());
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::Drop { reg, relevant } => {
                        // Release a dead binding/temporary at its last use and clear it to `unit` (so
                        // `set_reg`/teardown later release `unit`, never double-freeing). This frees the
                        // value promptly, restoring an accumulator's unique ownership. When the IR marked
                        // the drop destructor-relevant (Phase 4), route it through `release_value` so a
                        // `destruct` block fires here if this is the final owning reference; otherwise the
                        // value provably reaches no destructor and the plain `release` is used.
                        let v = std::mem::replace(&mut regs[fbase + *reg as usize], Value::unit());
                        if *relevant {
                            self.release_value(v);
                        } else {
                            release(v);
                        }
                        pc += 1;
                    }
                    Op::ConcatInPlace { dst, lhs, rhs, .. } => {
                        let l = regs[fbase + *lhs as usize];
                        let r = regs[fbase + *rhs as usize];
                        // `lhs` is consumed: clear its register *without* releasing (a direct overwrite,
                        // not `set_reg`), so the refcount below still counts the accumulator's reference
                        // and the single owner is transferred into the result. This also makes a
                        // `dst == lhs` store safe (the old occupant is now `unit`, not the live list).
                        regs[fbase + *lhs as usize] = Value::unit();
                        let result = if l.is_list() && r.is_list() {
                            if l.is_packed_list()
                                && r.is_packed_list()
                                && l.refcount() == 1
                                && l.packed_extend_in_place(r)
                            {
                                // Sole owner, both flat, same layout: append `rhs`'s words to `lhs`'s
                                // buffer in place (P-PACK 2.6). The single reference moves into the result.
                                l
                            } else if !l.is_packed_list()
                                && !r.is_packed_list()
                                && l.refcount() == 1
                            {
                                // Sole owner, both boxed: extend the backing buffer in place (O(1)
                                // amortized). The single reference moves from `lhs` into the result.
                                l.list_extend(r);
                                l
                            } else if let Some(flat) = l.packed_concat(r) {
                                // Aliased but both flat (same layout): copy the word buffers, then drop the
                                // consumed accumulator reference — stays flat without mutating the alias.
                                release(l);
                                flat
                            } else {
                                // A mixed packed/boxed pairing (or differing layouts): copy, preserving
                                // immutable semantics. Demote each operand to an owned boxed list, retain
                                // each element into the new list, release the demotions, then drop the
                                // accumulator's consumed reference.
                                let lb = l.realize_list();
                                let rb = r.realize_list();
                                let mut items = lb.list_items().unwrap();
                                items.extend(rb.list_items().unwrap());
                                for &item in &items {
                                    item.inc_ref();
                                }
                                lb.release();
                                rb.release();
                                release(l);
                                Value::list(items)
                            }
                        } else if l.is_string() && l.refcount() == 1 {
                            // Sole owner of a string accumulator: append `rhs`'s display form to its
                            // buffer in place (amortized O(1)), mirroring the list path — the single
                            // reference moves into the result. This is what makes `s = s ~ x` in a loop
                            // O(n) instead of O(n²) (the `format!` below copies all of `l` each time).
                            l.str_push_in_place(&r.display());
                            l
                        } else {
                            // Aliased accumulator or non-string lhs: display concatenation into a fresh
                            // string (preserves immutable semantics), identical to `Op::Binary`'s `~`.
                            let s = Value::string(&format!("{}{}", l.display(), r.display()));
                            release(l);
                            s
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::MakeClosure {
                        dst,
                        proto,
                        captures,
                    } => {
                        // Gather one cell per capture (from a celled local register, or one of this
                        // frame's own upvalues — forwarding a capture down a level), retaining each
                        // into the new closure, which owns its upvalue cells.
                        let mut upvalues = Vec::with_capacity(captures.len());
                        for capture in captures.iter() {
                            let cell = match capture {
                                CaptureFrom::Local(reg) => regs[fbase + *reg as usize],
                                CaptureFrom::Upvalue(index) => {
                                    frames[top].upvalues[*index as usize]
                                }
                            };
                            retain(cell);
                            upvalues.push(cell);
                        }
                        let v = Value::closure(*proto, upvalues);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::MakeCell { dst, src } => {
                        // Box the value into a fresh cell, which owns one reference to it.
                        let v = regs[fbase + *src as usize];
                        retain(v);
                        set_reg(regs, fbase, *dst, Value::cell(v));
                        pc += 1;
                    }
                    Op::CellGet { dst, cell } => {
                        let v = regs[fbase + *cell as usize].cell_get();
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::CellSet { cell, src } => {
                        // `cell_set` retains the new occupant and releases the old internally.
                        let v = regs[fbase + *src as usize];
                        regs[fbase + *cell as usize].cell_set(v);
                        pc += 1;
                    }
                    Op::UpvalueGet { dst, index } => {
                        let v = frames[top].upvalues[*index as usize].cell_get();
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::UpvalueSet { index, src } => {
                        let v = regs[fbase + *src as usize];
                        frames[top].upvalues[*index as usize].cell_set(v);
                        pc += 1;
                    }
                    Op::LoadNativeFn { dst, func } => {
                        set_reg(regs, fbase, *dst, Value::native_fn(*func));
                        pc += 1;
                    }
                    Op::MakeList {
                        dst,
                        items,
                        reflect,
                    } => {
                        let mut elements = Vec::with_capacity(items.len());
                        for &r in items.iter() {
                            let v = regs[fbase + r as usize];
                            retain(v);
                            elements.push(v);
                        }
                        let list = Value::list(elements);
                        // Stamp the checker-resolved element type onto the list (R1) so `type_of` recovers
                        // it after a `dyn` launder. A cheap `Rc` clone of the shared load-time entry; the
                        // tag lives beside the payload, invisible to value semantics.
                        if let Some(idx) = reflect {
                            list.set_reflect(Some(Rc::clone(&self.type_reprs[*idx as usize])));
                        }
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                    }
                    // A `List<packed>` literal (P-PACK 2.4): pack each element into a flat raw-primitive
                    // buffer (no boxed objects, no retains — the words are copied), then the element
                    // temporaries are released by the following compiler-emitted drops, exactly as for
                    // `MakeList`'s consumed operands. If any element fails to pack (a shape the schema
                    // does not expect — not reachable for a well-typed marked site), fall back to a boxed
                    // list that retains each element, staying consistent with those drops.
                    Op::PackedListNew { dst, schema } => {
                        // Allocate the empty flat buffer the following `PackedListPush` chain fills
                        // (P-PACK 2.5 streaming construction).
                        let schema = Rc::clone(&self.packed_schemas[*schema as usize]);
                        let list = Value::packed_list(schema, Vec::new());
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                    }
                    Op::FromBytes {
                        dst,
                        src,
                        schema,
                        span,
                    } => {
                        // Deserialize a `bytes` buffer into a flat `List<T>` (P-PACK 4.4): wrap the raw
                        // bytes as a packed list of the interned schema — the inverse of `to_bytes`.
                        let blob = regs[fbase + *src as usize];
                        let Some(bytes) = blob.bytes_data() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`from_bytes` expects a `bytes` value, found {}",
                                    blob.type_name()
                                ),
                            ));
                        };
                        let schema = Rc::clone(&self.packed_schemas[*schema as usize]);
                        if schema.byte_size == 0 || bytes.len() % schema.byte_size != 0 {
                            return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`from_bytes` buffer of {} bytes is not a whole number of {}-byte elements",
                                bytes.len(),
                                schema.byte_size
                            ),
                        ));
                        }
                        let list = Value::packed_list(schema, bytes);
                        set_reg(regs, fbase, *dst, list);
                        pc += 1;
                    }
                    Op::ExtCall {
                        dst,
                        module: mod_id,
                        func: func_id,
                        args,
                        recipe,
                        span,
                    } => {
                        // Resolve the interned module/func names (`module` is the outer loop-local
                        // `&Module`, so bind the op's ids under different names to avoid shadowing it).
                        let mod_name = module.name(*mod_id);
                        let func = module.name(*func_id);
                        // A call-site-typed native module call (`json.parse::<T>(s)`). The recipe is
                        // required; its absence was already reported by the checker.
                        let Some(recipe) = recipe else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`{mod_name}.{func}::<T>(...)` has no resolved result type"
                                ),
                            ));
                        };
                        // The only call-site-typed native function today is `json.parse::<T>(text)`.
                        if mod_name == "json" && func == "parse" {
                            let text = args
                                .first()
                                .map(|r| regs[fbase + *r as usize])
                                .and_then(|v| v.as_string());
                            let Some(text) = text else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    "`json.parse` expects a `string` argument".to_string(),
                                ));
                            };
                            match noeta_stdlib::json::parse_typed(&text, recipe) {
                                Ok(out) => {
                                    let value = materialize_recipe(out);
                                    set_reg(regs, fbase, *dst, value);
                                }
                                Err(error) => {
                                    return Err(self.error(
                                        stdlib_error_code(error.kind),
                                        *span,
                                        error.message,
                                    ));
                                }
                            }
                        } else {
                            return Err(self.error(
                            DiagnosticCode::UnknownName,
                            *span,
                            format!(
                                "`{mod_name}.{func}::<T>(...)` is not a call-site-typed native function"
                            ),
                        ));
                        }
                        pc += 1;
                    }
                    Op::PackedListPush {
                        dst, list, value, ..
                    } => {
                        let acc = regs[fbase + *list as usize];
                        let element = regs[fbase + *value as usize];
                        // `list` is the streaming accumulator — a uniquely-owned temp. Clear its register
                        // to `unit` *without* releasing (a direct overwrite, like `ConcatInPlace`), so the
                        // single owning reference transfers into `result` and a `dst == list` store is
                        // safe. `value` is left in its register for the compiler-emitted `Drop` to free.
                        regs[fbase + *list as usize] = Value::unit();
                        let result = if acc.is_packed_list() {
                            if acc.packed_push(element) {
                                // Element primitives copied into the buffer (not retained) — the buffer
                                // extended in place; the `Drop` of `value` frees the element object.
                                acc
                            } else {
                                // Defensive demote (a checked `@packed` type never mismatches): materialize
                                // the packed buffer to an owned boxed list, release the packed accumulator,
                                // then push the (retained) element so the boxed list owns one reference.
                                let boxed = acc.realize_list();
                                release(acc);
                                retain(element);
                                boxed.list_push(element);
                                boxed
                            }
                        } else {
                            // Already boxed (a prior demote): push the retained element in place.
                            retain(element);
                            acc.list_push(element);
                            acc
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    // A tuple builds exactly like a list (object-model slice 4): retain each element into
                    // the aggregate, which owns one reference to each.
                    Op::MakeTuple { dst, items } => {
                        let mut elements = Vec::with_capacity(items.len());
                        for &r in items.iter() {
                            let v = regs[fbase + r as usize];
                            retain(v);
                            elements.push(v);
                        }
                        set_reg(regs, fbase, *dst, Value::tuple(elements));
                        pc += 1;
                    }
                    // Positional projection `receiver.N`: read the Nth element of the tuple, retaining it
                    // into `dst`. The index is in range by construction (the checker verified it).
                    Op::TupleIndex {
                        dst,
                        receiver,
                        index,
                        span,
                    } => {
                        let v = regs[fbase + *receiver as usize];
                        let Some(element) = v.tuple_field(*index as usize) else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "tuple index `{index}` is out of range for {}",
                                    v.type_name()
                                ),
                            ));
                        };
                        retain(element);
                        set_reg(regs, fbase, *dst, element);
                        pc += 1;
                    }
                    Op::MakeRange {
                        dst,
                        start,
                        end,
                        inclusive,
                        span,
                    } => {
                        let lo = regs[fbase + *start as usize];
                        let hi = regs[fbase + *end as usize];
                        match (lo.as_int(), hi.as_int()) {
                            (Some(a), Some(b)) => {
                                // `..=` shifts the exclusive upper to `b + 1`; `saturating_add` keeps
                                // the unmaterializable `i64::MAX` edge from panicking. The elements are
                                // fresh int immediates (no refcount), so no retain is needed.
                                let upper = if *inclusive { b.saturating_add(1) } else { b };
                                let elements: Vec<Value> = (a..upper).map(Value::int).collect();
                                set_reg(regs, fbase, *dst, Value::list(elements));
                                pc += 1;
                            }
                            _ => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "range bounds must be ints, found {} and {}",
                                        lo.type_name(),
                                        hi.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    Op::MakeMap {
                        dst,
                        entries,
                        reflect,
                    } => {
                        let mut map = BTreeMap::new();
                        for (key_reg, value_reg) in entries.iter() {
                            let key = regs[fbase + *key_reg as usize]
                                .as_string()
                                .expect("map keys are validated by RequireMapKey");
                            let value = regs[fbase + *value_reg as usize];
                            retain(value);
                            // A duplicate key keeps the later value (M0 `BTreeMap` semantics); the
                            // displaced value loses its owner, so release it.
                            if let Some(old) = map.insert(key, value) {
                                release(old);
                            }
                        }
                        let map = Value::map(map);
                        // Stamp the checker-resolved `Map(K, V)` type onto the map (R1) so `type_of`
                        // recovers it after a `dyn` launder — the same node-tag path `MakeList` uses.
                        if let Some(idx) = reflect {
                            map.set_reflect(Some(Rc::clone(&self.type_reprs[*idx as usize])));
                        }
                        set_reg(regs, fbase, *dst, map);
                        pc += 1;
                    }
                    Op::RequireMapKey { reg, span } => {
                        let v = regs[fbase + *reg as usize];
                        if v.as_string().is_none() {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("map keys must be strings, found {}", v.type_name()),
                            ));
                        }
                        pc += 1;
                    }
                    Op::IterSnapshot { dst, src, span } => {
                        let v = regs[fbase + *src as usize];
                        // A user object lights up the `Iterable` trait: `for x in o` iterates the list
                        // its `iter` method returns. The method runs bytecode, so it is pushed as a
                        // call frame; its returned value becomes the snapshot (the following `ListLen`
                        // raises E0007 if it was not a list). Matches the tree-walker's `exec_for`.
                        if v.is_object() {
                            let type_name = v.shape().unwrap().name.clone();
                            if let Some(&proto) =
                                self.methods.get(&(type_name.clone(), "iter".to_string()))
                            {
                                let callee_chunk = &module.protos[proto as usize];
                                if callee_chunk.num_params != 1 {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "this method takes {} argument(s) but 0 were supplied",
                                            callee_chunk.num_params - 1
                                        ),
                                    ));
                                }
                                let new_base =
                                    reserve_window(regs, callee_chunk.num_registers as usize);
                                retain(v);
                                regs[new_base] = v;
                                frames[top].pc = pc + 1;
                                frames.push(Frame {
                                    proto,
                                    base: new_base,
                                    pc: 0,
                                    ret_dst: *dst,
                                    ret_transform: RetTransform::None,
                                    upvalues: Vec::new(),
                                });
                                continue 'reload;
                            }
                        }
                        // A packed list (P-PACK 2.4) materializes directly into an owned boxed snapshot
                        // (a fresh list owning each element) — the loop then indexes that boxed snapshot,
                        // so `ListLen`/`ListGet` never see the flat form.
                        if v.is_packed_list() {
                            let snapshot = v.realize_list();
                            set_reg(regs, fbase, *dst, snapshot);
                            pc += 1;
                            continue;
                        }
                        // Snapshot the elements to iterate (a list's elements, a set's canonical
                        // elements, or a map's values in sorted-key order), each retained so the loop
                        // owns them independently.
                        let snapshot = match v
                            .list_items()
                            .or_else(|| v.set_items())
                            .or_else(|| v.map_values())
                        {
                            Some(elements) => {
                                for &e in &elements {
                                    retain(e);
                                }
                                Value::list(elements)
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("cannot iterate over {}", v.type_name()),
                                ));
                            }
                        };
                        set_reg(regs, fbase, *dst, snapshot);
                        pc += 1;
                    }
                    Op::ListLen { dst, src, span } => {
                        // After `IterSnapshot`, `src` is a list for the list/map paths; the only way it
                        // is not is an `Iterable::iter` that returned a non-list, reported here (E0007),
                        // matching the tree-walker's `exec_for`.
                        let v = regs[fbase + *src as usize];
                        match v.list_len() {
                            Some(n) => {
                                set_reg(regs, fbase, *dst, Value::int(n as i64));
                                pc += 1;
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("`iter` must return a list, found {}", v.type_name()),
                                ));
                            }
                        }
                    }
                    Op::ListGet { dst, list, index } => {
                        let idx = regs[fbase + *index as usize]
                            .as_int()
                            .expect("a loop index is an int")
                            as usize;
                        let element = regs[fbase + *list as usize]
                            .list_get(idx)
                            .expect("the loop keeps the index in bounds");
                        retain(element);
                        set_reg(regs, fbase, *dst, element);
                        pc += 1;
                    }
                    // Streaming `for` step (Track I.2): advance the iterator, binding the element + a bool
                    // continue flag. A `map`/`filter` closure runs here (via `iter_for_next`), so it can
                    // abort. `set_reg` releases the previous element / flag each iteration.
                    Op::IterForNext {
                        iter,
                        elem,
                        has,
                        span,
                    } => {
                        let it = regs[fbase + *iter as usize];
                        match self.iter_for_next(it, *span)? {
                            Some(element) => {
                                set_reg(regs, fbase, *elem, element);
                                set_reg(regs, fbase, *has, Value::bool(true));
                            }
                            None => {
                                set_reg(regs, fbase, *elem, Value::unit());
                                set_reg(regs, fbase, *has, Value::bool(false));
                            }
                        }
                        pc += 1;
                    }
                    Op::CallBuiltin {
                        dst,
                        builtin,
                        args,
                        span,
                    } => {
                        // A user object lights up the `Length` trait: `len(o)` dispatches to its `len`
                        // method, which runs bytecode, so it is pushed as a call frame rather than
                        // handled by the synchronous `call_builtin`. (Matches the tree-walker's
                        // `Builtin::Len` object case.)
                        if *builtin == Builtin::Len && args.len() == 1 {
                            let recv = regs[fbase + args[0] as usize];
                            if recv.is_object() {
                                let type_name = recv.shape().unwrap().name.clone();
                                if let Some(&proto) =
                                    self.methods.get(&(type_name.clone(), "len".to_string()))
                                {
                                    let callee_chunk = &module.protos[proto as usize];
                                    if callee_chunk.num_params != 1 {
                                        return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "this method takes {} argument(s) but 0 were supplied",
                                            callee_chunk.num_params - 1
                                        ),
                                    ));
                                    }
                                    let new_base =
                                        reserve_window(regs, callee_chunk.num_registers as usize);
                                    retain(recv);
                                    regs[new_base] = recv;
                                    frames[top].pc = pc + 1;
                                    frames.push(Frame {
                                        proto,
                                        base: new_base,
                                        pc: 0,
                                        ret_dst: *dst,
                                        ret_transform: RetTransform::None,
                                        upvalues: Vec::new(),
                                    });
                                    continue 'reload;
                                }
                            }
                        }
                        // Builtins borrow their arguments (the registers keep ownership); the
                        // result is a fresh owned value.
                        let arg_vals: Vec<Value> =
                            args.iter().map(|r| regs[fbase + *r as usize]).collect();
                        let (dst, builtin, span) = (*dst, *builtin, *span);
                        let v = self.call_builtin(builtin, &arg_vals, span)?;
                        set_reg(regs, fbase, dst, v);
                        pc += 1;
                    }
                    Op::CallMethod {
                        dst,
                        recv,
                        method,
                        args,
                        span,
                        cache,
                        reuse,
                        consume_key,
                    } => {
                        // Resolve the interned method name once; every path below wants the `&str`.
                        let method = module.name(*method);
                        let v = regs[fbase + *recv as usize];
                        // In-place map self-update (Phase 5.1c): a reuse-marked `m = m.set(k,v)` /
                        // `m = m.remove(k)` whose runtime receiver is actually a map consumes the receiver
                        // register and mutates the sole-owned backing buffer in place (an alias copies). A
                        // non-map receiver — a user method that happens to be named `set` — falls through to
                        // the ordinary dispatch below with the receiver intact.
                        if *reuse
                            && v.is_map()
                            && let Some(map_method) = noeta_stdlib::MapMethod::from_name(method)
                            && matches!(
                                map_method,
                                noeta_stdlib::MapMethod::Set | noeta_stdlib::MapMethod::Remove
                            )
                        {
                            let arg_values: Vec<Value> =
                                args.iter().map(|r| regs[fbase + *r as usize]).collect();
                            // Consume the receiver: take its single reference out of the register without
                            // releasing (a direct overwrite, like `ConcatInPlace`), so the refcount below
                            // still counts the accumulator's reference and a `dst == recv` store is safe.
                            regs[fbase + *recv as usize] = Value::unit();
                            let result = self.map_update_in_place(
                                v,
                                map_method,
                                method,
                                &arg_values,
                                *consume_key,
                                *span,
                            )?;
                            set_reg(regs, fbase, *dst, result);
                            pc += 1;
                            continue;
                        }
                        // In-place list self-update (`xs[i] = v` ⟶ `xs = xs.set(i, v)`): a uniquely-owned
                        // list overwrites slot `i` in place (O(1)) instead of copying the whole list.
                        if *reuse && v.is_list() && method == "set" {
                            let arg_values: Vec<Value> =
                                args.iter().map(|r| regs[fbase + *r as usize]).collect();
                            regs[fbase + *recv as usize] = Value::unit();
                            let result = self.list_set_in_place(v, &arg_values, *span)?;
                            set_reg(regs, fbase, *dst, result);
                            pc += 1;
                            continue;
                        }
                        // In-place set self-update (`s = s.add(x)` / `s = s.remove(x)`): a uniquely-owned,
                        // canonically-ordered set binary-search-inserts/removes one element in its existing
                        // buffer instead of cloning + re-sorting the whole set.
                        if *reuse
                            && v.is_set()
                            && let Some(set_method) = noeta_stdlib::SetMethod::from_name(method)
                            && matches!(
                                set_method,
                                noeta_stdlib::SetMethod::Add | noeta_stdlib::SetMethod::Remove
                            )
                        {
                            let arg_values: Vec<Value> =
                                args.iter().map(|r| regs[fbase + *r as usize]).collect();
                            regs[fbase + *recv as usize] = Value::unit();
                            let result = self.set_update_in_place(
                                v,
                                set_method,
                                method,
                                &arg_values,
                                *span,
                            )?;
                            set_reg(regs, fbase, *dst, result);
                            pc += 1;
                            continue;
                        }
                        // `json.parse(...)` — a Ring 2 native module function call, dispatched before
                        // the object/collection paths.
                        if let Some(module_name) = v.native_module_name() {
                            let arg_values: Vec<Value> =
                                args.iter().map(|r| regs[fbase + *r as usize]).collect();
                            let value =
                                self.call_native_module(&module_name, method, &arg_values, *span)?;
                            set_reg(regs, fbase, *dst, value);
                            pc += 1;
                            continue;
                        }
                        // An object dispatches to a user method through the type's method table;
                        // anything else falls to the built-in `count`/`enumerate` methods.
                        if v.is_object() {
                            // `o.to_json()` on a type that `@derive(Serialize<Json>)` (so has no hand-written
                            // `to_json`) synthesizes a structural JSON string — a pure value
                            // computation, so it is produced inline rather than via a call frame. Only a
                            // literal `to_json` site reaches here, so the shape clone stays off the common
                            // method-call path.
                            if method == "to_json" && args.is_empty() {
                                let type_name = v.shape().unwrap().name.clone();
                                if self.tojson_derives.contains(&type_name) {
                                    let json = Value::string(&v.to_json());
                                    set_reg(regs, fbase, *dst, json);
                                    pc += 1;
                                    continue;
                                }
                            }
                            // Inline cache: a hit (the receiver's shape pointer matches the cached one)
                            // gives the resolved prototype directly, skipping the `(type, method)` hashmap
                            // lookup and its two `String` clones. The hit check avoids bumping the shape
                            // refcount (raw pointer compare); only a miss clones the shape into the cache.
                            let ci = *cache as usize;
                            let shape_ptr = v.object_shape_ptr();
                            let hit = match &caches[ci] {
                                Some((cs, p)) if Some(Rc::as_ptr(cs)) == shape_ptr => Some(*p),
                                _ => None,
                            };
                            let proto = match hit {
                                Some(proto) => proto,
                                None => {
                                    let shape = v.shape().unwrap();
                                    let Some(&proto) =
                                        self.methods.get(&(shape.name.clone(), method.to_string()))
                                    else {
                                        return Err(self.error(
                                            DiagnosticCode::UnknownName,
                                            *span,
                                            format!(
                                                "type `{}` has no method `{method}`",
                                                shape.name
                                            ),
                                        ));
                                    };
                                    caches[ci] = Some((shape, proto));
                                    proto
                                }
                            };
                            let callee_chunk = &module.protos[proto as usize];
                            // The prototype takes the receiver in register 0 and the user arguments
                            // after it, so its declared arity is one more than the supplied args. A
                            // method may have trailing defaulted parameters, so the supplied count is a
                            // range `[total - defaults, total]` (all less the receiver).
                            let total = callee_chunk.num_params as usize - 1;
                            let required = total - callee_chunk.defaults.len();
                            if args.len() < required || args.len() > total {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    arity_message("method", required, total, args.len()),
                                ));
                            }
                            let num_registers = callee_chunk.num_registers as usize;
                            let defaults = callee_chunk.defaults.clone();
                            let new_base = reserve_window(regs, num_registers);
                            retain(v);
                            regs[new_base] = v;
                            for (i, &arg_reg) in args.iter().enumerate() {
                                let a = regs[fbase + arg_reg as usize];
                                retain(a);
                                regs[new_base + i + 1] = a;
                            }
                            // Fill any omitted trailing parameters from their default thunks. The
                            // receiver and supplied args occupy registers `0..=args.len()`, so a default
                            // register at or beyond that was not supplied.
                            // A method frame carries no upvalues (it is defined at module scope), so its
                            // default thunks resolve globals only.
                            let filled = args.len() + 1;
                            for (reg, proto) in &defaults {
                                if *reg as usize >= filled {
                                    let value = self.run_thunk(*proto, &[])?;
                                    regs[new_base + *reg as usize] = value;
                                }
                            }
                            frames[top].pc = pc + 1;
                            frames.push(Frame {
                                proto,
                                base: new_base,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::None,
                                upvalues: Vec::new(),
                            });
                            continue 'reload;
                        }
                        // An enum value dispatches to a user method (the unified body, object-model
                        // slice 3) through the same `(type, method)` table as an object. Enums carry no
                        // inline-cache shape pointer, so this is a direct table lookup. An unknown method
                        // falls through to the built-in paths below.
                        if v.is_enum() {
                            let type_name = v.shape().unwrap().name.clone();
                            if let Some(&proto) = self.methods.get(&(type_name, method.to_string()))
                            {
                                let callee_chunk = &module.protos[proto as usize];
                                let total = callee_chunk.num_params as usize - 1;
                                let required = total - callee_chunk.defaults.len();
                                if args.len() < required || args.len() > total {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        arity_message("method", required, total, args.len()),
                                    ));
                                }
                                let num_registers = callee_chunk.num_registers as usize;
                                let defaults = callee_chunk.defaults.clone();
                                let new_base = reserve_window(regs, num_registers);
                                retain(v);
                                regs[new_base] = v;
                                for (i, &arg_reg) in args.iter().enumerate() {
                                    let a = regs[fbase + arg_reg as usize];
                                    retain(a);
                                    regs[new_base + i + 1] = a;
                                }
                                let filled = args.len() + 1;
                                for (reg, proto) in &defaults {
                                    if *reg as usize >= filled {
                                        let value = self.run_thunk(*proto, &[])?;
                                        regs[new_base + *reg as usize] = value;
                                    }
                                }
                                frames[top].pc = pc + 1;
                                frames.push(Frame {
                                    proto,
                                    base: new_base,
                                    pc: 0,
                                    ret_dst: *dst,
                                    ret_transform: RetTransform::None,
                                    upvalues: Vec::new(),
                                });
                                continue 'reload;
                            }
                        }
                        // Everything below the object/enum dispatch is a built-in method on a
                        // non-object receiver — value-in/value-out, factored into
                        // `call_builtin_method` (prelude-redesign MH.2) so an unbound method handle
                        // (`list.len` as a value) dispatches through the SAME branches by
                        // construction. Arguments are borrowed from the registers (which keep
                        // ownership); the result is freshly owned. Up to 4 args stage through a
                        // stack buffer — no method-call path allocates that did not before the
                        // extraction (A/B-benched: a per-call 1-element `Vec` cost ~20ns on
                        // `signal.set`-class branches that previously read registers directly).
                        let mut arg_buf = [Value::unit(); 4];
                        let arg_vec: Vec<Value>;
                        let arg_values: &[Value] = if args.len() <= 4 {
                            for (i, r) in args.iter().enumerate() {
                                arg_buf[i] = regs[fbase + *r as usize];
                            }
                            &arg_buf[..args.len()]
                        } else {
                            arg_vec = args.iter().map(|r| regs[fbase + *r as usize]).collect();
                            &arg_vec
                        };
                        let (dst, span) = (*dst, *span);
                        let value = self.call_builtin_method(v, method, arg_values, span)?;
                        set_reg(regs, fbase, dst, value);
                        pc += 1;
                    }
                    Op::Index {
                        dst,
                        recv,
                        index,
                        span,
                    } => {
                        let v = regs[fbase + *recv as usize];
                        let idx = regs[fbase + *index as usize];
                        // `o[i]` on a user object lights up the `Index` trait: dispatch to `get`,
                        // pushing a call frame `[recv, index]` exactly like a method call. An object
                        // without an `Index` impl has no `get` method, so this reports the missing
                        // method — matching the tree-walker's `eval_index`.
                        if v.is_object() {
                            let type_name = v.shape().unwrap().name.clone();
                            let Some(&proto) =
                                self.methods.get(&(type_name.clone(), "get".to_string()))
                            else {
                                return Err(self.error(
                                    DiagnosticCode::UnknownName,
                                    *span,
                                    format!("type `{type_name}` has no method `get`"),
                                ));
                            };
                            let callee_chunk = &module.protos[proto as usize];
                            if callee_chunk.num_params as usize != 2 {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "this method takes {} argument(s) but 1 were supplied",
                                        callee_chunk.num_params - 1
                                    ),
                                ));
                            }
                            let new_base =
                                reserve_window(regs, callee_chunk.num_registers as usize);
                            retain(v);
                            regs[new_base] = v;
                            retain(idx);
                            regs[new_base + 1] = idx;
                            frames[top].pc = pc + 1;
                            frames.push(Frame {
                                proto,
                                base: new_base,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: RetTransform::None,
                                upvalues: Vec::new(),
                            });
                            continue 'reload;
                        }
                        // A built-in list addresses an element by integer position (bounds-checked).
                        if let Some(len) = v.list_len() {
                            let Some(i) = idx.as_int() else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!("list index must be an int, found {}", idx.type_name()),
                                ));
                            };
                            if i < 0 || i as usize >= len {
                                return Err(self.error(
                                    DiagnosticCode::IndexOutOfBounds,
                                    *span,
                                    format!("index {i} out of bounds for list of length {len}"),
                                ));
                            }
                            // A packed list (P-PACK 2.4) materializes the one indexed element (owned,
                            // refcount 1) — no full-list materialization, no extra retain. A boxed list
                            // borrows the element and retains it into `dst`.
                            let element = if v.is_packed_list() {
                                v.packed_get(i as usize)
                            } else {
                                let element = v.list_get(i as usize).expect("bounds checked above");
                                retain(element);
                                element
                            };
                            set_reg(regs, fbase, *dst, element);
                            pc += 1;
                            continue;
                        }
                        // A map looks the value up by its string key; a missing key is `E0018`.
                        if v.is_map() {
                            // Borrow the key's `&str` for the lookup — no clone on the hot found path;
                            // the cold error paths clone only for their message.
                            match idx.with_str(|key| v.map_get(key)) {
                                Some(Some(element)) => {
                                    retain(element);
                                    set_reg(regs, fbase, *dst, element);
                                    pc += 1;
                                    continue;
                                }
                                Some(None) => {
                                    let key = idx.as_string().unwrap_or_default();
                                    return Err(self.error(
                                        DiagnosticCode::KeyNotFound,
                                        *span,
                                        format!("map has no key {key:?}"),
                                    ));
                                }
                                None => {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "map index must be a string, found {}",
                                            idx.type_name()
                                        ),
                                    ));
                                }
                            }
                        }
                        // A string addresses a single character by position (bounds-checked),
                        // counting by Unicode scalar values to match `len`.
                        if let Some(s) = v.as_string() {
                            let Some(i) = idx.as_int() else {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "string index must be an int, found {}",
                                        idx.type_name()
                                    ),
                                ));
                            };
                            let count = s.chars().count();
                            if i < 0 || i as usize >= count {
                                return Err(self.error(
                                    DiagnosticCode::IndexOutOfBounds,
                                    *span,
                                    format!("index {i} out of bounds for string of length {count}"),
                                ));
                            }
                            let ch = s.chars().nth(i as usize).unwrap().to_string();
                            set_reg(regs, fbase, *dst, Value::string(&ch));
                            pc += 1;
                            continue;
                        }
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("cannot index a value of type {}", v.type_name()),
                        ));
                    }
                    Op::IndexField {
                        dst,
                        recv,
                        index,
                        field,
                        span,
                    } => {
                        let field = module.name(*field);
                        let v = regs[fbase + *recv as usize];
                        let idx = regs[fbase + *index as usize];
                        // Fast path: a packed list decodes the one field's word(s) directly — no element
                        // materialization (the P-PACK 2.5+ scalar-access win). Any miss (non-int index,
                        // out of range, or unknown field) falls through to the boxed index-then-load,
                        // which reproduces the exact diagnostics of the unfused `Index` + `LoadField`.
                        if v.is_packed_list()
                            && let Some(i) = idx.as_int()
                            && i >= 0
                            && let Some(value) = v.packed_field(i as usize, field)
                        {
                            set_reg(regs, fbase, *dst, value);
                            pc += 1;
                            continue;
                        }
                        // Fallback. The static type guarantees a `List`; bounds-check the index exactly as
                        // `Op::Index`'s list branch, then read the element's field exactly as
                        // `Op::LoadField`. A boxed element is borrowed (only its loaded field is retained
                        // into `dst`); a packed element reached here (unknown field — unreachable for a
                        // checker-fused site) is materialized owned and released after.
                        let Some(len) = v.list_len() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot index a value of type {}", v.type_name()),
                            ));
                        };
                        let Some(i) = idx.as_int() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("list index must be an int, found {}", idx.type_name()),
                            ));
                        };
                        if i < 0 || i as usize >= len {
                            return Err(self.error(
                                DiagnosticCode::IndexOutOfBounds,
                                *span,
                                format!("index {i} out of bounds for list of length {len}"),
                            ));
                        }
                        let packed = v.is_packed_list();
                        let element = if packed {
                            v.packed_get(i as usize) // owned (rc 1)
                        } else {
                            v.list_get(i as usize).expect("bounds checked above") // borrowed
                        };
                        let slot = element.shape().and_then(|sh| sh.slot_of(field));
                        match slot.and_then(|s| element.slot_at(s)) {
                            Some(value) => {
                                retain(value);
                                if packed {
                                    release(element);
                                }
                                set_reg(regs, fbase, *dst, value);
                                pc += 1;
                            }
                            None => {
                                let err = if element.is_object() {
                                    self.error(
                                        DiagnosticCode::UnknownName,
                                        *span,
                                        format!(
                                            "type `{}` has no field `{field}`",
                                            element.shape().unwrap().name
                                        ),
                                    )
                                } else {
                                    self.error(
                                        DiagnosticCode::UnknownName,
                                        *span,
                                        format!("no field `{field}` on {}", element.type_name()),
                                    )
                                };
                                if packed {
                                    release(element);
                                }
                                return Err(err);
                            }
                        }
                    }
                    Op::MakeStruct {
                        dst,
                        shape,
                        named,
                        spread,
                        reflect,
                        span,
                    } => {
                        let shape = self.shapes[*shape as usize].clone();
                        let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
                        // `...base` fills declared slots the base provides; named initializers then
                        // override. A slot left unset by both is a missing-field error (E0009).
                        if let Some(base_reg) = spread {
                            let base = regs[fbase + *base_reg as usize];
                            for (i, field) in shape.fields.iter().enumerate() {
                                if let Some(value) = base.field(field) {
                                    retain(value);
                                    slots[i] = Some(value);
                                }
                            }
                        }
                        for (slot, reg) in named.iter() {
                            let value = regs[fbase + *reg as usize];
                            retain(value);
                            if let Some(old) = slots[*slot as usize].replace(value) {
                                release(old);
                            }
                        }
                        // A slot still unset after spread + named is filled from its field default
                        // (slice 5), run in global scope (empty upvalues — a default resolves globals
                        // only). A slot with neither a value nor a default violates the
                        // full-initialization guarantee (E0009).
                        let mut missing: Vec<String> = Vec::new();
                        for i in 0..shape.fields.len() {
                            if slots[i].is_some() {
                                continue;
                            }
                            let field = shape.fields[i].clone();
                            if let Some(&proto) = self
                                .field_defaults
                                .get(&(shape.name.clone(), field.clone()))
                            {
                                match self.run_thunk(proto, &[]) {
                                    Ok(value) => slots[i] = Some(value),
                                    Err(abort) => {
                                        for slot in slots.into_iter().flatten() {
                                            release(slot);
                                        }
                                        return Err(abort);
                                    }
                                }
                            } else {
                                missing.push(field);
                            }
                        }
                        if !missing.is_empty() {
                            for slot in slots.into_iter().flatten() {
                                release(slot);
                            }
                            let list = missing
                                .iter()
                                .map(|name| format!("`{name}`"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            return Err(self.error(
                            DiagnosticCode::MissingField,
                            *span,
                            format!(
                                "missing field(s) {list} in `{}` literal — every field must be set",
                                shape.name
                            ),
                        ));
                        }
                        let slots = slots.into_iter().map(Option::unwrap).collect();
                        let object = Value::object(shape, slots);
                        // Stamp the reflected type onto a generic instantiation (R2) so `type_of` recovers
                        // its type arguments after a `dyn` launder. The object's type is invariant under
                        // field mutation, so — unlike the collection tags — it is never cleared.
                        if let Some(idx) = reflect {
                            object.set_reflect(Some(Rc::clone(&self.type_reprs[*idx as usize])));
                        }
                        set_reg(regs, fbase, *dst, object);
                        pc += 1;
                    }
                    Op::MakeStructInPlace {
                        dst,
                        shape,
                        named,
                        base,
                        check,
                        reflect,
                        span,
                    } => {
                        let shape = self.shapes[*shape as usize].clone();
                        // The base is consumed: take its single reference out of the register without
                        // releasing (a direct overwrite, mirroring `ConcatInPlace`), so the refcount
                        // below still counts the accumulator's reference and a `dst == base` store is
                        // safe (the old occupant is now `unit`).
                        let base_val = regs[fbase + *base as usize];
                        regs[fbase + *base as usize] = Value::unit();
                        let same_shape = base_val.object_shape_ptr() == Some(Rc::as_ptr(&shape));
                        let reuse = match check {
                            ReuseCheck::Static => {
                                // The linearity analysis proved sole ownership, so the **refcount** check
                                // is elided — this is the compile-time-hoisted uniqueness path. The debug
                                // assertion documents (and, in debug builds, guards) that invariant; a
                                // failure means the analysis is wrong. The shape is still guarded (a
                                // well-typed self-update always matches, but a mismatch must fall back to
                                // copy rather than corrupt the object at the wrong slot layout).
                                debug_assert!(
                                    base_val.refcount() == 1,
                                    "static record reuse requires a uniquely-owned base"
                                );
                                same_shape
                            }
                            ReuseCheck::Runtime => same_shape && base_val.refcount() == 1,
                        };
                        if reuse {
                            // Reuse the allocation: overwrite only the changed slots. Every unchanged
                            // field keeps base's reference, which transfers into the result — base *is*
                            // the result. The displaced old field value is routed through `release_value`
                            // (not a plain free) so its `destruct` fires at the right time — matching the
                            // copy-and-destroy baseline, which would destroy the old base and its fields
                            // (spec §4/§5). The reuse pass guarantees `base`'s own type has no destructor,
                            // so reuse never skips a container destructor.
                            for (slot, reg) in named.iter() {
                                let v = regs[fbase + *reg as usize];
                                let old = base_val.replace_slot(*slot as usize, v);
                                self.release_value(old);
                            }
                            // Reuse keeps the base node's existing reflected type (R2): a self-update
                            // rebuilds a value of the same (generic) type, so the base's tag already carries
                            // it — matching the tree-walker's reuse path, which keeps the accumulator's tag.
                            set_reg(regs, fbase, *dst, base_val);
                            pc += 1;
                        } else {
                            // Aliased or a different shape: build a fresh object exactly like
                            // `MakeStruct` (spreading base's fields), then release the consumed base.
                            let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
                            for (i, field) in shape.fields.iter().enumerate() {
                                if let Some(value) = base_val.field(field) {
                                    retain(value);
                                    slots[i] = Some(value);
                                }
                            }
                            for (slot, reg) in named.iter() {
                                let value = regs[fbase + *reg as usize];
                                retain(value);
                                if let Some(old) = slots[*slot as usize].replace(value) {
                                    release(old);
                                }
                            }
                            let missing: Vec<&str> = shape
                                .fields
                                .iter()
                                .zip(&slots)
                                .filter(|(_, slot)| slot.is_none())
                                .map(|(name, _)| name.as_str())
                                .collect();
                            if !missing.is_empty() {
                                for slot in slots.into_iter().flatten() {
                                    release(slot);
                                }
                                release(base_val);
                                let list = missing
                                    .iter()
                                    .map(|name| format!("`{name}`"))
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                return Err(self.error(
                                DiagnosticCode::MissingField,
                                *span,
                                format!(
                                    "missing field(s) {list} in `{}` literal — every field must be set",
                                    shape.name
                                ),
                            ));
                            }
                            let slots = slots.into_iter().map(Option::unwrap).collect();
                            release(base_val);
                            let object = Value::object(shape, slots);
                            if let Some(idx) = reflect {
                                object
                                    .set_reflect(Some(Rc::clone(&self.type_reprs[*idx as usize])));
                            }
                            set_reg(regs, fbase, *dst, object);
                            pc += 1;
                        }
                    }
                    Op::MakeOpaque {
                        dst,
                        type_name,
                        keys,
                        spread,
                    } => {
                        // An opaque object's shape is built from its (spread ∪ named) keys in sorted
                        // order, so its display matches the tree-walker's `BTreeMap` field bag.
                        let mut bag: BTreeMap<String, Value> = BTreeMap::new();
                        if let Some(base_reg) = spread
                            && let Some(base) = regs[fbase + *base_reg as usize].shape()
                        {
                            let base_val = regs[fbase + *base_reg as usize];
                            for (i, field) in base.fields.iter().enumerate() {
                                let value = base_val.slots().unwrap()[i];
                                retain(value);
                                if let Some(old) = bag.insert(field.clone(), value) {
                                    release(old);
                                }
                            }
                        }
                        for (key, reg) in keys.iter() {
                            let value = regs[fbase + *reg as usize];
                            retain(value);
                            if let Some(old) = bag.insert(module.name(*key).to_string(), value) {
                                release(old);
                            }
                        }
                        let fields: Vec<String> = bag.keys().cloned().collect();
                        let slots: Vec<Value> = bag.into_values().collect();
                        let shape = Rc::new(Shape::object(
                            ShapeKind::Opaque,
                            module.name(*type_name).to_string(),
                            fields,
                        ));
                        set_reg(regs, fbase, *dst, Value::object(shape, slots));
                        pc += 1;
                    }
                    Op::MakeEnum {
                        dst,
                        shape,
                        args,
                        reflect,
                    } => {
                        let shape = self.shapes[*shape as usize].clone();
                        let mut data = Vec::with_capacity(args.len());
                        for &r in args.iter() {
                            let v = regs[fbase + r as usize];
                            retain(v);
                            data.push(v);
                        }
                        let value = Value::enum_value(shape, data);
                        // Stamp the reflected type onto a generic enum-variant construction (R2b.2) so
                        // `type_of` recovers its type arguments after a `dyn` launder. Like an object's tag,
                        // an enum value's type is invariant, so it is never cleared.
                        if let Some(idx) = reflect {
                            value.set_reflect(Some(Rc::clone(&self.type_reprs[*idx as usize])));
                        }
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
                    }
                    Op::EnumFromStr {
                        dst,
                        arg,
                        enum_name,
                        cases,
                        some_shape,
                        none_shape,
                        panic,
                        span,
                    } => {
                        let enum_name = module.name(*enum_name);
                        let key = match regs[fbase + *arg as usize].as_string() {
                            Some(s) => s,
                            None => {
                                let kind = if *panic { "from" } else { "try_from" };
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`{enum_name}.{kind}` expects a string, found {}",
                                        regs[fbase + *arg as usize].type_name()
                                    ),
                                ));
                            }
                        };
                        let matched = cases.iter().find(|(name, _)| module.name(*name) == key);
                        let result = match matched {
                            Some((_, shape_idx)) => {
                                // Build the payload-free case; its single reference transfers onward.
                                let shape = self.shapes[*shape_idx as usize].clone();
                                let case = Value::enum_value(shape, Vec::new());
                                if *panic {
                                    case
                                } else {
                                    let some = self.shapes[*some_shape as usize].clone();
                                    Value::enum_value(some, vec![case])
                                }
                            }
                            None if *panic => {
                                return Err(self.error(
                                    DiagnosticCode::Panic,
                                    *span,
                                    format!("panic: `{enum_name}` has no case `{key}`"),
                                ));
                            }
                            None => {
                                let none = self.shapes[*none_shape as usize].clone();
                                Value::enum_value(none, Vec::new())
                            }
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::LoadField {
                        dst,
                        obj,
                        field,
                        span,
                        cache,
                    } => {
                        let field = module.name(*field);
                        let v = regs[fbase + *obj as usize];
                        // Inline cache: a hit (the receiver's shape pointer matches the cached one) reads
                        // the memoized slot directly; a miss resolves `slot_of` and refreshes the cache.
                        // The hit check returns an owned slot so the `&caches[ci]` borrow ends before the
                        // miss path mutates the same entry.
                        let ci = *cache as usize;
                        let hit = match &caches[ci] {
                            Some((cs, slot)) if v.object_shape_ptr() == Some(Rc::as_ptr(cs)) => {
                                Some(*slot as usize)
                            }
                            _ => None,
                        };
                        let cached_slot = match hit {
                            Some(slot) => Some(slot),
                            None => match v.shape() {
                                Some(sh) => sh.slot_of(field).inspect(|&s| {
                                    caches[ci] = Some((sh.clone(), s as u32));
                                }),
                                None => None,
                            },
                        };
                        match cached_slot.and_then(|s| v.slot_at(s)) {
                            Some(value) => {
                                retain(value);
                                set_reg(regs, fbase, *dst, value);
                                pc += 1;
                            }
                            None if v.is_object() => {
                                return Err(self.error(
                                    DiagnosticCode::UnknownName,
                                    *span,
                                    format!(
                                        "type `{}` has no field `{field}`",
                                        v.shape().unwrap().name
                                    ),
                                ));
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::UnknownName,
                                    *span,
                                    format!("no field `{field}` on {}", v.type_name()),
                                ));
                            }
                        }
                    }
                    Op::SetField {
                        dst,
                        obj,
                        field,
                        value,
                        reuse,
                        span,
                    } => {
                        let field = module.name(*field);
                        // The store (class in-place / struct COW / reuse) is shared with the tier-1 JIT
                        // leaf helper (P-JIT J4); a `false` return is the field-not-found error path.
                        if !self.set_field_fast(regs, fbase, *dst, *obj, field, *value, *reuse) {
                            let v = regs[fbase + *obj as usize];
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                if v.is_object() {
                                    format!(
                                        "type `{}` has no field `{field}`",
                                        v.shape().unwrap().name
                                    )
                                } else {
                                    format!("cannot assign field `{field}` on {}", v.type_name())
                                },
                            ));
                        }
                        pc += 1;
                    }
                    Op::NextId { dst } => {
                        let id = self.next_id;
                        self.next_id += 1;
                        set_reg(regs, fbase, *dst, Value::int(id as i64));
                        pc += 1;
                    }
                    Op::Panic { msg, span } => {
                        let message = regs[fbase + *msg as usize].display();
                        return Err(self.error(
                            DiagnosticCode::Panic,
                            *span,
                            format!("panic: {message}"),
                        ));
                    }
                    Op::TryUnwrap {
                        dst,
                        src,
                        on_error,
                        span,
                    } => {
                        let v = regs[fbase + *src as usize];
                        match try_classify(v) {
                            Some(TryOutcome::Success(inner)) => {
                                retain(inner);
                                set_reg(regs, fbase, *dst, inner);
                                pc += 1;
                            }
                            // `Err(_)`/`none`: early-return the whole value from this frame, exactly
                            // as `Op::Return` does (the M0 `Unwind::Return`).
                            Some(TryOutcome::Empty) => {
                                retain(v);
                                // Drop the frame locals this `?` abandons before unwinding (Phase 4.2c) —
                                // destructor-relevant ones fire `destruct`, in the drop pass's order. Each
                                // is cleared to `unit`, so the teardown release below never double-frees.
                                for (reg, relevant) in on_error.iter() {
                                    let dv = std::mem::replace(
                                        &mut regs[fbase + *reg as usize],
                                        Value::unit(),
                                    );
                                    if *relevant {
                                        self.release_value(dv);
                                    } else {
                                        release(dv);
                                    }
                                }
                                let finished = frames.pop().unwrap();
                                let n =
                                    module.protos[finished.proto as usize].num_registers as usize;
                                for i in 0..n {
                                    release(regs[finished.base + i]);
                                }
                                for u in &finished.upvalues {
                                    release(*u);
                                }
                                regs.truncate(finished.base);
                                // Apply the frame's return transform on every exit path, for the same
                                // reason `Op::Return` does (a short-circuiting `?` is an early return);
                                // release the original if the transform replaced it.
                                let (out, replaced) = finished.ret_transform.apply(v);
                                if replaced {
                                    release(v);
                                }
                                match frames.last() {
                                    Some(caller) => {
                                        let idx = caller.base + finished.ret_dst as usize;
                                        let old = regs[idx];
                                        regs[idx] = out;
                                        release(old);
                                    }
                                    None => return Ok(out),
                                }
                                // `?` short-circuits like an early return — re-derive the caller's window.
                                continue 'reload;
                            }
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`?` expects a `Result` or `Option`, found {}",
                                        v.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    Op::Coalesce {
                        dst,
                        src,
                        fallback,
                        span,
                    } => {
                        let v = regs[fbase + *src as usize];
                        match try_classify(v) {
                            Some(TryOutcome::Success(inner)) => {
                                retain(inner);
                                set_reg(regs, fbase, *dst, inner);
                                pc += 1;
                            }
                            // Empty: jump to the fallback expression (which writes `dst`).
                            Some(TryOutcome::Empty) => pc = *fallback as usize,
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`??` expects a `Result` or `Option` on the left, found {}",
                                        v.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    Op::Narrow {
                        dst,
                        src,
                        target,
                        some_shape,
                        none_shape,
                    } => {
                        let v = regs[fbase + *src as usize];
                        let result = if narrow_matches(v, target) {
                            retain(v);
                            let shape = self.shapes[*some_shape as usize].clone();
                            Value::enum_value(shape, vec![v])
                        } else {
                            let shape = self.shapes[*none_shape as usize].clone();
                            Value::enum_value(shape, Vec::new())
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::IsType { dst, src, target } => {
                        let v = regs[fbase + *src as usize];
                        let result = Value::bool(narrow_matches(v, target));
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::MakeGen { dst, src } => {
                        // Wrap the step closure into a generator iterator (Track G.1b). `iter_gen` retains
                        // its own reference to the closure; the source register's reference is released by
                        // the register's normal end-of-life (exactly as `Op::Narrow` retains its payload).
                        let step = regs[fbase + *src as usize];
                        let result = Value::iter_gen(step);
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::MakeFuture { dst, src } => {
                        // Wrap the lazy thunk closure into a future (Track A.1). `make_future` retains its
                        // own reference to the closure; the source register's reference is released by the
                        // register's normal end-of-life (like `Op::MakeGen`).
                        let thunk = regs[fbase + *src as usize];
                        let result = Value::make_future(thunk);
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::RunFuture { dst, src, span } => {
                        // Drive an awaited future to completion (Track A.2/A.3 top-level). See
                        // `drive_future`: poll; on pending advance the clock and re-poll; it borrows the
                        // future and returns an owned result. `.await` **consumes** the future (a spent
                        // future cannot be awaited again — a second await already deadlocks), so take it
                        // out of the source register and release it destructor-aware here, at its last
                        // reference: a destructor-bearing local captured in the async fn's state (held in
                        // the future's step-closure cells) runs now rather than being lost.
                        let future =
                            std::mem::replace(&mut regs[fbase + *src as usize], Value::unit());
                        // Release before propagating an abort: the register was already emptied, so
                        // the frame teardown can no longer see the future — skipping this on the
                        // error path (e.g. a detected async deadlock) orphans it (the refcount
                        // anomaly the strengthened leak oracle catches). `drive_future` borrows.
                        let value = self.drive_future(future, *span);
                        self.release_value(future);
                        let value = value?;
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
                    }
                    Op::PollFuture { dst, src, span } => {
                        // Poll a future once (Track A.3 state machine): `some(v)` if ready, `none` if
                        // pending. The source register keeps owning the future.
                        let future = regs[fbase + *src as usize];
                        let result = match self.poll_once(future, *span)? {
                            Poll::Ready(value) => make_some(value),
                            Poll::Pending => make_none(),
                        };
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::LoadPending { dst } => {
                        // The async pending sentinel (Track A.3) — what a step returns when it suspends.
                        set_reg(regs, fbase, *dst, Value::pending());
                        pc += 1;
                    }
                    Op::ScopeBegin => {
                        // Open a structured-concurrency scope (Track A.3b): a fresh, empty task list.
                        self.scopes.push(Vec::new());
                        pc += 1;
                    }
                    Op::Spawn { dst, src, .. } => {
                        // Register the future as a task in the current scope (retaining the scope's own
                        // reference), yielding a handle that references it by `(scope, task)`. A `spawn`
                        // outside any scope is E0041 at check, so `self.scopes` is non-empty here.
                        let future = regs[fbase + *src as usize];
                        let handle = if self.scopes.is_empty() {
                            retain(future);
                            future
                        } else {
                            retain(future);
                            let scope_idx = self.scopes.len() - 1;
                            let task_idx = self.scopes[scope_idx].len();
                            self.scopes[scope_idx].push(Task {
                                future,
                                result: None,
                                cancelled: false,
                            });
                            Value::make_handle(
                                ScopeId::from_index(scope_idx),
                                TaskId::from_index(task_idx),
                            )
                        };
                        set_reg(regs, fbase, *dst, handle);
                        pc += 1;
                    }
                    Op::SpawnIsolate {
                        dst,
                        callee,
                        args,
                        span,
                    } => {
                        // `isolate f(args)` (I.4b). Only the CLI's real (VM) path emits this op; the
                        // differential/salsa sandbox lowers `isolate` to `Call`+`Spawn`, so it is never
                        // reached in-oracle. Runs on a real OS thread when the VM is parallel and no
                        // argument ships a channel; otherwise falls back to a cooperative task (so a
                        // non-parallel VM — `@test`/`bench` — and channel-shipping isolates never regress).
                        let callee_val = regs[fbase + *callee as usize];
                        let arg_vals: Vec<Value> =
                            args.iter().map(|r| regs[fbase + *r as usize]).collect();
                        let handle = self.spawn_isolate(callee_val, &arg_vals, *span)?;
                        set_reg(regs, fbase, *dst, handle);
                        pc += 1;
                    }
                    Op::ScopeEnd { span } => {
                        // Join the scope (drive every task to completion), then pop it and release the
                        // tasks' owned futures and results.
                        self.join_scope(*span)?;
                        if let Some(scope) = self.scopes.pop() {
                            for task in scope {
                                // Destructor-aware: a task's future holds the async body's captured
                                // locals in its state-machine cells. A completed task's cells are spent,
                                // but a **cancelled** task (a `race` loser) abandoned its future mid-body
                                // with a live captured value — release it here so its destructor runs.
                                self.release_value(task.future);
                                if let Some(result) = task.result {
                                    self.release_value(result);
                                }
                            }
                        }
                        pc += 1;
                    }
                    Op::MakeChannel {
                        dst,
                        capacity,
                        span,
                    } => {
                        // Create a bounded channel and yield its `(Sender, Receiver)` endpoint tuple
                        // (isolates I.1). The message type is checker-only; only the capacity reaches here.
                        let cap = regs[fbase + *capacity as usize];
                        let Some(cap) = cap.as_int() else {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`channel` expects an int capacity, found {}",
                                    cap.type_name()
                                ),
                            ));
                        };
                        if cap < 0 {
                            return Err(self.error(
                                DiagnosticCode::Panic,
                                *span,
                                format!("`channel` capacity must be non-negative, found {cap}"),
                            ));
                        }
                        let id = ChannelId::from_index(self.channels.len());
                        // In a parallel VM (real isolates, I.4c) a channel is a *shared* cross-thread queue
                        // from birth, so shipping an endpoint into a worker shares one queue; the sandbox
                        // (and any non-parallel VM) uses the cooperative in-VM `Local` FIFO, unchanged.
                        let channel = if self.parallel_isolates {
                            Channel::Shared(isolate::ChannelCore::new(cap as usize))
                        } else {
                            Channel::Local {
                                buffer: std::collections::VecDeque::new(),
                                capacity: cap as usize,
                                closed: false,
                            }
                        };
                        self.channels.push(channel);
                        // The two endpoints are fresh (refcount 1); `Value::tuple` takes ownership of
                        // exactly those references, so no extra retain is needed.
                        let tuple =
                            Value::tuple(vec![Value::make_sender(id), Value::make_receiver(id)]);
                        set_reg(regs, fbase, *dst, tuple);
                        pc += 1;
                    }
                    Op::AttributesOf { dst, type_name } => {
                        let result = self.materialize_attributes(module.name(*type_name));
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::RolesOf { dst } => {
                        let result = self.materialize_roles();
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::TypeOf { dst, src } => {
                        let repr = vm_type_repr(&regs[fbase + *src as usize]);
                        let result = build_type_value(&repr);
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::TypeOfStatic { dst, repr } => {
                        let result = build_type_value(repr);
                        set_reg(regs, fbase, *dst, result);
                        pc += 1;
                    }
                    Op::TypeValue { dst, name } => {
                        // A bare type name used as a value (an `invoke` receiver) materializes as the
                        // reflection `Type` ADT — the one representation of "a type as a value", shared
                        // with `type_of` and stored type-refs. `Op::Invoke` resolves it back to the
                        // named type via `reflection_type_name`.
                        let value =
                            build_type_value(&module.reflection.type_ref_repr(module.name(*name)));
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
                    }
                    Op::Invoke {
                        dst,
                        recv,
                        name,
                        args,
                        ok_shape,
                        err_shape,
                        ..
                    } => {
                        let recv_val = regs[fbase + *recv as usize];
                        let name_val = regs[fbase + *name as usize];
                        let args_val = regs[fbase + *args as usize];
                        // A packed args list (P-PACK 2.4) is materialized to a temporary boxed list for
                        // the duration of the dispatch, then released after the call frame is built (its
                        // elements retained into it). `arg_items` below borrows from this temporary.
                        let mut args_to_release: Option<Value> = None;
                        // Resolve the dispatch by name: either a prototype to call (`Ok`) or a reason it
                        // failed (`Err(msg)` → `Result.Err`). Every resolution failure — non-string name,
                        // non-list args, non-invokable receiver, unknown name, arity mismatch — is a
                        // runtime `Err`, never an abort (only a panic *inside* the called body aborts).
                        let outcome: Result<(u32, bool, Vec<Value>), String> = 'resolve: {
                            let Some(method) = name_val.as_string() else {
                                break 'resolve Err(format!(
                                    "invoke name must be a string, found {}",
                                    name_val.type_name()
                                ));
                            };
                            if !args_val.is_list() {
                                break 'resolve Err(format!(
                                    "invoke args must be a list, found {}",
                                    args_val.type_name()
                                ));
                            }
                            let args_list = args_val.realize_list();
                            args_to_release = Some(args_list);
                            let arg_items = args_list.list_items().expect("checked is_list");
                            // A type handle dispatches an associated function (no receiver); an object
                            // dispatches an instance method (receiver in register 0). A reflection `Type`
                            // value (a stored type-ref) names the type for an associated call too.
                            let (type_name, is_assoc) = if recv_val.is_object() {
                                (recv_val.shape().unwrap().name.clone(), false)
                            } else if let Some(tn) = reflection_type_name(recv_val) {
                                (tn, true)
                            } else {
                                break 'resolve Err(format!(
                                    "cannot invoke on a value of type `{}`",
                                    recv_val.type_name()
                                ));
                            };
                            let kind = if is_assoc {
                                "associated function"
                            } else {
                                "method"
                            };
                            let Some(&proto) =
                                self.methods.get(&(type_name.clone(), method.clone()))
                            else {
                                break 'resolve Err(format!(
                                    "type `{type_name}` has no {kind} `{method}`"
                                ));
                            };
                            // The prototype reserves register 0 for `self` (unit for an associated
                            // call), so its declared arity is one more than the supplied args; trailing
                            // defaults widen the accepted range, exactly as `Op::CallMethod`.
                            let callee_chunk = &module.protos[proto as usize];
                            let total = callee_chunk.num_params as usize - 1;
                            let required = total - callee_chunk.defaults.len();
                            if arg_items.len() < required || arg_items.len() > total {
                                break 'resolve Err(arity_message(
                                    kind,
                                    required,
                                    total,
                                    arg_items.len(),
                                ));
                            }
                            Ok((proto, is_assoc, arg_items))
                        };
                        match outcome {
                            Err(message) => {
                                let shape = self.shapes[*err_shape as usize].clone();
                                let err = Value::enum_value(shape, vec![Value::string(&message)]);
                                set_reg(regs, fbase, *dst, err);
                                pc += 1;
                            }
                            Ok((proto, is_assoc, arg_items)) => {
                                let callee_chunk = &module.protos[proto as usize];
                                let num_registers = callee_chunk.num_registers as usize;
                                let defaults = callee_chunk.defaults.clone();
                                let new_base = reserve_window(regs, num_registers);
                                // An associated call leaves register 0 as unit (no receiver); an instance
                                // call places the retained receiver there.
                                if !is_assoc {
                                    retain(recv_val);
                                    regs[new_base] = recv_val;
                                }
                                for (i, &arg) in arg_items.iter().enumerate() {
                                    retain(arg);
                                    regs[new_base + i + 1] = arg;
                                }
                                // Fill any omitted trailing parameters from their default thunks (module
                                // scope only, like a method frame).
                                let filled = arg_items.len() + 1;
                                for (reg, proto) in &defaults {
                                    if *reg as usize >= filled {
                                        let value = self.run_thunk(*proto, &[])?;
                                        regs[new_base + *reg as usize] = value;
                                    }
                                }
                                // The result is wrapped in `Result.Ok` as it lands in the caller, so the
                                // invocation yields a `Result` whichever way the body returns.
                                let ok = self.shapes[*ok_shape as usize].clone();
                                frames[top].pc = pc + 1;
                                frames.push(Frame {
                                    proto,
                                    base: new_base,
                                    pc: 0,
                                    ret_dst: *dst,
                                    ret_transform: RetTransform::WrapOk(ok),
                                    upvalues: Vec::new(),
                                });
                                // Release the temporary boxed args list before transferring (its
                                // elements were already retained into the call frame above); `take`
                                // leaves the after-match release for the non-transferring `Err` path.
                                if let Some(list) = args_to_release.take() {
                                    list.release();
                                }
                                continue 'reload;
                            }
                        }
                        // Release the temporary boxed args list (if the args were materialized from a
                        // packed list); its elements were retained into the call frame above.
                        if let Some(list) = args_to_release {
                            list.release();
                        }
                    }
                    Op::MatchInt { src, value, fail } => {
                        if regs[fbase + *src as usize].as_int() == Some(*value) {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::MatchStr { src, value, fail } => {
                        if regs[fbase + *src as usize].as_string().as_deref()
                            == Some(module.name(*value))
                        {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::MatchBool { src, value, fail } => {
                        if regs[fbase + *src as usize].as_bool() == Some(*value) {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::MatchVariant {
                        src,
                        type_name,
                        variant,
                        arity,
                        fail,
                    } => {
                        let v = regs[fbase + *src as usize];
                        let matches = v.is_enum()
                            && v.shape().is_some_and(|shape| {
                                shape.variant.as_deref() == Some(module.name(*variant))
                                    && type_name
                                        .is_none_or(|t| module.name(t) == shape.name.as_str())
                            })
                            && v.enum_data().is_some_and(|d| d.len() == *arity as usize);
                        if matches {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    // A tuple pattern test (object-model slice 4b.2): `src` must be a tuple of exactly
                    // `arity` elements. The elements are then read with `TupleIndex` for sub-patterns.
                    Op::MatchTuple { src, arity, fail } => {
                        let v = regs[fbase + *src as usize];
                        let matches = v
                            .tuple_items()
                            .is_some_and(|items| items.len() == *arity as usize);
                        if matches {
                            pc += 1;
                        } else {
                            pc = *fail as usize;
                        }
                    }
                    Op::ExtractField { dst, src, index } => {
                        let element =
                            regs[fbase + *src as usize].enum_data().unwrap()[*index as usize];
                        retain(element);
                        set_reg(regs, fbase, *dst, element);
                        pc += 1;
                    }
                    Op::MatchFail { src, span } => {
                        let shown = regs[fbase + *src as usize].display();
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("no match arm matched the value {shown}"),
                        ));
                    }
                    Op::Unary { op, dst, src, span } => {
                        match apply_unary(*op, regs[fbase + *src as usize]) {
                            Ok(v) => {
                                // `..xs` (spread) returns the source value unchanged, so the result
                                // aliases a live heap reference — retain it before `set_reg` releases
                                // the old occupant of `dst` (which is `src`). A no-op for the fresh
                                // primitives `Neg`/`Not` produce; mirrors `Op::Move`.
                                retain(v);
                                set_reg(regs, fbase, *dst, v);
                                pc += 1;
                            }
                            Err(e) => return Err(self.error(e.code, *span, e.text)),
                        }
                    }
                    Op::MaskWidth {
                        dst,
                        src,
                        signed,
                        bits,
                    } => {
                        // Reduce an erased fixed-width integer (an `int` value) into its declared width
                        // (Tier W). Total — the shared helper runs identically in the tree-walker. A
                        // non-int (only if the checker's IntN guarantee broke) passes through unchanged.
                        //
                        // Ownership: a masked result is a *fresh* value from `Value::int` — already
                        // owning its one reference if it heap-boxes (a `u64` past the immediate range),
                        // so it must NOT be retained again (the refcount-anomaly oracle catches the
                        // over-count as a leak). Only the pass-through borrows from the src register
                        // and needs the retain for its new owner.
                        let v = regs[fbase + *src as usize];
                        let masked = match v.as_int() {
                            Some(n) => Value::int(noeta_stdlib::mask_to_width(n, *signed, *bits)),
                            None => {
                                retain(v);
                                v
                            }
                        };
                        set_reg(regs, fbase, *dst, masked);
                        pc += 1;
                    }
                    Op::Binary {
                        op,
                        dst,
                        a,
                        b,
                        span,
                    } => {
                        let left = regs[fbase + *a as usize];
                        let right = regs[fbase + *b as usize];
                        // Operator-trait dispatch on a user object or enum value (the unified body's
                        // in-body `impl` blocks are uniform across kinds — object-model slice 3): an
                        // arithmetic/concat operator routes to its trait method and uses the result
                        // directly; `==`/`!=` route to `Equatable::eq` (`!=` negating via the frame's
                        // return transform); `< <= > >=` route to `Comparable::compare`. The method table
                        // is keyed by the value's shape name, identical for objects and enums. Built-in
                        // semantics apply otherwise; the checker guarantees a dispatched method's arity.
                        let dispatch = if left.is_object() || left.is_enum() {
                            let type_name = left.shape().unwrap().name.clone();
                            if let Some(method_name) = op.overload_method() {
                                self.methods
                                    .get(&(type_name, method_name.to_string()))
                                    .map(|&proto| (proto, RetTransform::None))
                            } else if let Some(negate) = op.equatable_negation() {
                                let transform = if negate {
                                    RetTransform::Negate
                                } else {
                                    RetTransform::None
                                };
                                self.methods
                                    .get(&(type_name, "eq".to_string()))
                                    .map(|&proto| (proto, transform))
                            } else if let Some(method_name) = op.comparable_method() {
                                self.methods
                                    .get(&(type_name, method_name.to_string()))
                                    .map(|&proto| (proto, RetTransform::Ordering(*op)))
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        if let Some((proto, transform)) = dispatch
                            && module.protos[proto as usize].num_params == 2
                        {
                            let callee_chunk = &module.protos[proto as usize];
                            let new_base =
                                reserve_window(regs, callee_chunk.num_registers as usize);
                            retain(left);
                            regs[new_base] = left;
                            retain(right);
                            regs[new_base + 1] = right;
                            frames[top].pc = pc + 1;
                            frames.push(Frame {
                                proto,
                                base: new_base,
                                pc: 0,
                                ret_dst: *dst,
                                ret_transform: transform,
                                upvalues: Vec::new(),
                            });
                            continue 'reload;
                        }
                        // Derived structural comparison: `< <= > >=` on an object whose type
                        // `@derive(Comparable)`s (and has no hand-written `compare`) — field-wise
                        // ordering, computed synchronously (no method to call).
                        if left.is_object()
                            && op.comparable_method().is_some()
                            && self
                                .comparable_derives
                                .contains(&left.shape().unwrap().name)
                        {
                            match structural_compare(left, right) {
                                Some(ordering) => {
                                    let satisfied = op
                                        .ordering_satisfies(noeta_ast::ordering_variant(ordering));
                                    set_reg(regs, fbase, *dst, Value::bool(satisfied));
                                    pc += 1;
                                }
                                None => {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "cannot compare {} and {}",
                                            left.type_name(),
                                            right.type_name()
                                        ),
                                    ));
                                }
                            }
                            continue;
                        }
                        match apply_binary(*op, left, right) {
                            Ok(v) => {
                                set_reg(regs, fbase, *dst, v);
                                pc += 1;
                            }
                            Err(e) => return Err(self.error(e.code, *span, e.text)),
                        }
                    }
                    Op::WideInt {
                        op,
                        dst,
                        a,
                        b,
                        signed,
                        bits,
                        span,
                    } => {
                        // Sign-dependent fixed-width op (Tier W3): `/ % < <= > >=` on erased-int operands,
                        // read as `signed`/unsigned `bits`-wide. No trait dispatch (ints only).
                        let left = regs[fbase + *a as usize];
                        let right = regs[fbase + *b as usize];
                        match apply_binary_wide(*op, left, right, *signed, *bits) {
                            Ok(v) => {
                                set_reg(regs, fbase, *dst, v);
                                pc += 1;
                            }
                            Err(e) => return Err(self.error(e.code, *span, e.text)),
                        }
                    }
                    Op::WidthIntMethod {
                        dst,
                        recv,
                        method,
                        arg,
                        bits,
                        ..
                    } => {
                        // Width-exact bit intrinsic (Tier W5): compute within `bits`, not the erased i64.
                        // The checker guarantees an integer receiver and (for `rotate_*`) an integer arg.
                        let recv_int = regs[fbase + *recv as usize].as_int().unwrap_or(0);
                        let amount = match arg {
                            Some(r) => regs[fbase + *r as usize].as_int().unwrap_or(0),
                            None => 0,
                        };
                        let value = Value::int(noeta_stdlib::int_method_width(
                            recv_int, *method, amount, *bits,
                        ));
                        set_reg(regs, fbase, *dst, value);
                        pc += 1;
                    }
                    Op::RequireBool {
                        reg,
                        side,
                        op,
                        span,
                    } => {
                        let v = regs[fbase + *reg as usize];
                        if v.as_bool().is_none() {
                            let where_ = match side {
                                BoolSide::Left => "left",
                                BoolSide::Right => "right",
                            };
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "`{}` expects a bool on the {where_}, found {}",
                                    op.symbol(),
                                    v.type_name()
                                ),
                            ));
                        }
                        pc += 1;
                    }
                    Op::RequireCondBool { reg, span } => {
                        let v = regs[fbase + *reg as usize];
                        if v.as_bool().is_none() {
                            return Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("`if` condition must be a bool, found {}", v.type_name()),
                            ));
                        }
                        pc += 1;
                    }
                    Op::Jump { target } => {
                        osr_backedge!(*target);
                        pc = *target as usize;
                    }
                    Op::JumpIfTrue { reg, target } => {
                        if regs[fbase + *reg as usize].as_bool() == Some(true) {
                            osr_backedge!(*target);
                            pc = *target as usize;
                        } else {
                            pc += 1;
                        }
                    }
                    Op::JumpIfFalse { reg, target } => {
                        if regs[fbase + *reg as usize].as_bool() == Some(false) {
                            osr_backedge!(*target);
                            pc = *target as usize;
                        } else {
                            pc += 1;
                        }
                    }
                    Op::CondBranch { reg, target, span } => {
                        // Fused bool-check + false-branch (P-VMT-CBR): identical to the
                        // `RequireCondBool` + `JumpIfFalse` pair it replaces.
                        let v = regs[fbase + *reg as usize];
                        match v.as_bool() {
                            Some(false) => {
                                osr_backedge!(*target);
                                pc = *target as usize;
                            }
                            Some(true) => pc += 1,
                            None => {
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    *span,
                                    format!(
                                        "`if` condition must be a bool, found {}",
                                        v.type_name()
                                    ),
                                ));
                            }
                        }
                    }
                    Op::Echo { reg } => {
                        let text = regs[fbase + *reg as usize].display();
                        self.stdout.push_str(&text);
                        self.stdout.push('\n');
                        pc += 1;
                    }
                    Op::Stringify { dst, src, span } => {
                        let v = regs[fbase + *src as usize];
                        // A user object or enum value lights up the `Display` trait: render it via its
                        // `to_string` method (which runs bytecode, so it is pushed as a call frame). The
                        // method table is keyed by the value's shape name, identical for both kinds
                        // (object-model slice 3). Matches the tree-walker's `display_value`.
                        if v.is_object() || v.is_enum() {
                            let type_name = v.shape().unwrap().name.clone();
                            if let Some(&proto) = self
                                .methods
                                .get(&(type_name.clone(), "to_string".to_string()))
                            {
                                let callee_chunk = &module.protos[proto as usize];
                                if callee_chunk.num_params != 1 {
                                    return Err(self.error(
                                        DiagnosticCode::TypeMismatch,
                                        *span,
                                        format!(
                                            "this method takes {} argument(s) but 0 were supplied",
                                            callee_chunk.num_params - 1
                                        ),
                                    ));
                                }
                                let new_base =
                                    reserve_window(regs, callee_chunk.num_registers as usize);
                                retain(v);
                                regs[new_base] = v;
                                frames[top].pc = pc + 1;
                                frames.push(Frame {
                                    proto,
                                    base: new_base,
                                    pc: 0,
                                    ret_dst: *dst,
                                    ret_transform: RetTransform::None,
                                    upvalues: Vec::new(),
                                });
                                continue 'reload;
                            }
                        }
                        // Identity for every other value: the consuming `Echo`/`Concat` stringifies
                        // it via `display`.
                        retain(v);
                        set_reg(regs, fbase, *dst, v);
                        pc += 1;
                    }
                    Op::BuildString { dst, parts } => {
                        // One pass, one output allocation (P-VMT-STR). Size the buffer from the
                        // literal segments (known up front); holes grow it as they render. Each hole
                        // register holds an already-`Stringify`-ed value (a `Display` object was
                        // dispatched to `to_string` by the preceding `Stringify`), so `display` here
                        // never pushes a frame — the whole build stays within this one op. Holes are
                        // read by value (`Value` is `Copy`); their registers keep ownership and are
                        // released at frame teardown, exactly as the old fold's temporaries were.
                        let cap: usize = parts
                            .iter()
                            .map(|p| match p {
                                StrPart::Literal(k) => match &chunk.consts[*k as usize] {
                                    Const::Str(s) => s.len(),
                                    _ => 0,
                                },
                                StrPart::Hole(_) => 0,
                            })
                            .sum();
                        let mut out = String::with_capacity(cap);
                        for part in parts.iter() {
                            match part {
                                StrPart::Literal(k) => {
                                    if let Const::Str(s) = &chunk.consts[*k as usize] {
                                        out.push_str(s);
                                    }
                                }
                                StrPart::Hole(r) => {
                                    // Render directly into the buffer — no per-hole `display()` clone.
                                    regs[fbase + *r as usize].display_into(&mut out);
                                }
                            }
                        }
                        // Move the finished buffer into the heap string — no second copy.
                        set_reg(regs, fbase, *dst, Value::from_string(out));
                        pc += 1;
                    }
                    Op::Raise { idx } => {
                        self.diagnostics
                            .push(chunk.diagnostics[*idx as usize].clone());
                        return Err(Abort);
                    }
                    Op::Call {
                        dst,
                        callee,
                        args,
                        span,
                    } => {
                        let callee_val = regs[fbase + *callee as usize];
                        // Shared closure-call setup (also used by the JIT's `jit_call` helper): pushes
                        // the callee frame (→ `continue 'reload`) or completes a first-class-builtin
                        // call synchronously (→ advance to `pc + 1`).
                        if self.setup_closure_call(
                            frames,
                            regs,
                            top,
                            fbase,
                            *dst,
                            callee_val,
                            args,
                            *span,
                            pc + 1,
                        )? {
                            continue 'reload;
                        }
                        pc += 1;
                    }
                    Op::CallGlobal {
                        dst,
                        global,
                        args,
                        span,
                    } => {
                        // A statically-known top-level `fn`: read the callee straight from its
                        // global slot. No retain — the slot owns the reference for the whole call,
                        // so there is no matching release either; net refcount-neutral, exactly as
                        // `LoadGlobal` (retain) balanced by the register overwrite (release) would be.
                        let callee_val = self.globals[global.0 as usize];
                        if callee_val.is_unbound() {
                            return Err(self.error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!(
                                    "cannot find `{}` in this scope",
                                    module.global_name(*global)
                                ),
                            ));
                        }
                        if self.setup_closure_call(
                            frames,
                            regs,
                            top,
                            fbase,
                            *dst,
                            callee_val,
                            args,
                            *span,
                            pc + 1,
                        )? {
                            continue 'reload;
                        }
                        pc += 1;
                    }
                    Op::Return { src } => {
                        let raw = regs[fbase + *src as usize];
                        match self.do_return(frames, regs, raw) {
                            // The bottom frame returned: hand the value to `run`'s caller.
                            Some(v) => return Ok(v),
                            // Transferred to a caller — re-derive its window.
                            None => continue 'reload,
                        }
                    }
                    Op::Halt => {
                        let finished = frames.pop().unwrap();
                        let n = module.protos[finished.proto as usize].num_registers as usize;
                        for i in 0..n {
                            release(regs[finished.base + i]);
                        }
                        for u in &finished.upvalues {
                            release(*u);
                        }
                        regs.truncate(finished.base);
                        match frames.last() {
                            // A non-bottom frame falling off the end implicitly returns unit.
                            Some(caller) => {
                                set_reg(regs, caller.base, finished.ret_dst, Value::unit())
                            }
                            // The bottom frame halted: the program (or re-entrant call) ends.
                            None => return Ok(Value::unit()),
                        }
                        // Control returns to the caller (or a re-entry frame) — re-derive its window.
                        continue 'reload;
                    }
                }
            }
        }
    }

    /// Call a value with already-owned arguments (each carrying one reference transferred to
    /// the callee), re-entering the VM on a fresh frame stack. Only closures are callable in
    /// this slice — builtins are never first-class values. Used by `map`/`filter`.
    fn call_value(&mut self, callee: Value, args: Vec<Value>, span: Span) -> Result<Value, Abort> {
        match callee.as_closure() {
            Some(proto) => {
                let chunk = &self.module.protos[proto as usize];
                let num_params = chunk.num_params as usize;
                let num_registers = chunk.num_registers as usize;
                let required = num_params - chunk.defaults.len();
                let defaults = chunk.defaults.clone();
                if args.len() < required || args.len() > num_params {
                    let supplied = args.len();
                    for a in args {
                        release(a);
                    }
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        arity_message("function", required, num_params, supplied),
                    ));
                }
                let filled = args.len();
                let mut regs = vec![Value::unit(); num_registers];
                for (i, v) in args.into_iter().enumerate() {
                    regs[i] = v;
                }
                // A first-class closure may capture upvalues; carry its cells into the re-entrant
                // frame (one owned reference each) and hand them to each default thunk, which shares
                // the closure's upvalue layout so a capture-referencing default reads the right cell.
                let count = callee.closure_upvalue_count();
                let cells: Vec<Value> = (0..count).map(|i| callee.closure_upvalue(i)).collect();
                // Fill any omitted trailing parameters from their default thunks.
                for (reg, dproto) in &defaults {
                    if *reg as usize >= filled {
                        let value = self.run_thunk(*dproto, &cells)?;
                        regs[*reg as usize] = value;
                    }
                }
                let mut upvalues = Vec::with_capacity(count);
                for &cell in &cells {
                    retain(cell);
                    upvalues.push(cell);
                }
                self.run(
                    vec![Frame {
                        proto,
                        base: 0,
                        pc: 0,
                        ret_dst: 0,
                        ret_transform: RetTransform::None,
                        upvalues,
                    }],
                    regs,
                )
            }
            None => match callee.as_native_fn() {
                // A first-class builtin passed as the callee (e.g. `map(xs, len)`). The args are
                // owned here, so release them after the borrowing helper returns.
                Some(func) => {
                    let result = self.call_native_fn(func, &args, span);
                    for a in &args {
                        release(*a);
                    }
                    result
                }
                // A selectively-imported native-module function (`use std.math.sqrt`) called by its
                // bare name — dispatched through the same `call_native_module` as `math.sqrt(...)`.
                None => match callee.module_fn_parts() {
                    Some((module, func)) => {
                        let result = self.call_native_module(&module, &func, &args, span);
                        for a in &args {
                            release(*a);
                        }
                        result
                    }
                    // An unbound method handle (`Type.method`) applied to its arguments — the first is
                    // the receiver (prelude-redesign MH). Runs the resolved method on a fresh frame
                    // stack, consuming the owned arguments into the callee window.
                    None => match callee.method_handle_parts() {
                        Some((ty, method, associated)) => {
                            self.run_method_handle(&ty, &method, associated, args, span)
                        }
                        None => {
                            let type_name = callee.type_name();
                            for a in args {
                                release(a);
                            }
                            Err(self.error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!("{type_name} is not callable"),
                            ))
                        }
                    },
                },
            },
        }
    }

    /// Dispatch a **built-in** method on a non-object receiver — every method-call path that is
    /// value-in/value-out: `compare`, string/int/numeric-conversion methods, the Ring 1
    /// list/set/map/iterator/file-handle methods, channel endpoints, reactive handles, `to_bytes`,
    /// `iter()`, the eager `map`/`filter`/`sum`, and `count`/`len`/`enumerate` — ending in the
    /// canonical takes-no-arguments / no-method errors. Factored out of the `Op::CallMethod` arm
    /// (prelude-redesign MH.2) so the opcode and an unbound method handle (`list.len` passed as a
    /// value) dispatch through the SAME branches by construction. The op-field-dependent fast paths
    /// (`reuse` in-place updates) and the frame-pushing object/enum dispatches stay in the opcode —
    /// receivers here never resolve through the user method table.
    ///
    /// The receiver and arguments are **borrowed** (the caller keeps its references, exactly as the
    /// opcode's registers did); the result is a freshly-owned value. Branch ORDER is semantic
    /// (string before int, `IntMethod` before `NumConvert`, …) — do not reorder.
    ///
    /// `#[inline]` so the `Op::CallMethod` arm — the hot call site — folds this back into the
    /// dispatch loop exactly as the pre-extraction inline branches were (A/B-benched: the bare
    /// out-of-line call cost ~+15-25ns per built-in method call); the cold handle path may call it
    /// out-of-line.
    #[inline]
    fn call_builtin_method(
        &mut self,
        v: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // `x.compare(y)` — the `Ordering` of two primitives (the value a `Comparable`
        // impl returns). One argument, on any non-object receiver.
        if method == "compare" {
            if args.len() != 1 {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!(
                        "method `compare` takes 1 argument but {} were supplied",
                        args.len()
                    ),
                ));
            }
            let other = args[0];
            return match compare_primitive(v, other) {
                Some(ordering) => Ok(make_ordering(noeta_ast::ordering_variant(ordering))),
                None => Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("cannot compare {} and {}", v.type_name(), other.type_name()),
                )),
            };
        }
        // Ring 1 string methods (`upper`/`split`/`replace`/...) — dispatched through
        // the shared `noeta-stdlib` surface so the tree-walker and the VM cannot drift.
        // `Unknown` falls through to the collection methods below. `as_string` clones
        // out of the heap, so the projected args own their strings for the call.
        if let Some(recv_str) = v.as_string() {
            let arg_strings: Vec<Option<String>> = args.iter().map(|a| a.as_string()).collect();
            let projected: Vec<noeta_stdlib::Arg> = args
                .iter()
                .zip(&arg_strings)
                .map(|(a, s)| {
                    if let Some(s) = s {
                        noeta_stdlib::Arg::Str(s)
                    } else if let Some(i) = a.as_int() {
                        noeta_stdlib::Arg::Int(i)
                    } else if let Some(f) = a.as_float() {
                        noeta_stdlib::Arg::Float(f)
                    } else if let Some(b) = a.as_bool() {
                        noeta_stdlib::Arg::Bool(b)
                    } else {
                        noeta_stdlib::Arg::Other
                    }
                })
                .collect();
            match noeta_stdlib::string_method(&recv_str, method, &projected) {
                noeta_stdlib::Dispatch::Done(output) => {
                    return Ok(stdlib_output_to_value(output));
                }
                noeta_stdlib::Dispatch::Err(error) => {
                    return Err(self.error(stdlib_error_code(error.kind), span, error.message));
                }
                noeta_stdlib::Dispatch::Unknown => {}
            }
        }
        // Bit-manipulation methods on `int` (P-BITS Tier B4) — the popcount-class
        // intrinsics, delegating to the shared `int_method` so the backends agree. The
        // checker already arity/type-checked the call; `rotate_*` take one `int` amount.
        if let Some(recv_int) = v.as_int()
            && let Some(int_method) = noeta_stdlib::IntMethod::from_name(method)
        {
            let arg = match args.first() {
                Some(a) => match a.as_int() {
                    Some(n) => n,
                    None => {
                        return Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!(
                                "`int.{method}` expects an integer argument, found {}",
                                a.type_name()
                            ),
                        ));
                    }
                },
                None => 0,
            };
            return Ok(Value::int(noeta_stdlib::int_method(recv_int, int_method, arg)));
        }
        // Cross-domain numeric conversions (S0): `int→float/f32`, `float/f32→int`,
        // `float↔f32`. The `IntMethod` branch above handled `int→int` and returned; an
        // integer receiver reaches here only for a float destination (`to_float`/`to_f32`),
        // a `float`/`f32` receiver for any. Shared `num_convert` keeps the backends in step.
        if let Some(src) = v
            .as_f32()
            .map(noeta_stdlib::NumScalar::F32)
            .or_else(|| v.as_float().map(noeta_stdlib::NumScalar::F64))
            .or_else(|| v.as_int().map(noeta_stdlib::NumScalar::Int))
            && let Some(dest) = noeta_stdlib::NumConvert::from_name(method)
        {
            return Ok(match noeta_stdlib::num_convert(src, dest) {
                noeta_stdlib::NumScalar::Int(i) => Value::int(i),
                noeta_stdlib::NumScalar::F64(f) => Value::float(f),
                noeta_stdlib::NumScalar::F32(f) => Value::f32(f),
            });
        }
        // Ring 1 list methods (reverse/contains/join) — the shared `ListMethod` enum
        // makes the helper's `match` exhaustive, so the tree-walker cannot offer a
        // method this backend lacks.
        if v.list_len().is_some()
            && let Some(list_method) = noeta_stdlib::ListMethod::from_name(method)
        {
            return self.call_list_method(v, list_method, method, args, span);
        }
        // Ring 1 set methods (contains/union/intersection).
        if v.is_set()
            && let Some(set_method) = noeta_stdlib::SetMethod::from_name(method)
        {
            return self.call_set_method(v, set_method, method, args, span);
        }
        // File-handle methods (read_line/read/write/close) — the shared
        // `FileHandleMethod` enum keeps the two backends in lockstep.
        if v.is_file_handle()
            && let Some(handle_method) = noeta_stdlib::FileHandleMethod::from_name(method)
        {
            return self.call_file_handle_method(v, handle_method, method, args, span);
        }
        // Channel endpoint methods (isolates I.1): `tx.send(v)`/`tx.close()` on a sender,
        // `rx.recv()` on a receiver. `send`/`recv` yield leaf futures (enqueue/dequeue when
        // polled); `close` is synchronous. Endpoint validity was checked statically.
        if let Some(id) = v.sender_id() {
            match method {
                "send" => {
                    // The future retains its own reference to the message; the caller's
                    // reference is released by its normal end-of-life.
                    return Ok(Value::make_channel_send(id, args[0]));
                }
                "close" => {
                    match &mut self.channels[id.index()] {
                        Channel::Local { closed, .. } => *closed = true,
                        Channel::Shared(core) => core.close(),
                    }
                    self.channel_progress += 1;
                    return Ok(Value::unit());
                }
                _ => {}
            }
        }
        if let Some(id) = v.receiver_id()
            && method == "recv"
        {
            return Ok(Value::make_channel_recv(id));
        }
        // Reactive handle methods (reactivity S1/S2/S3): `signal.get()`/`.set(v)`/
        // `.update(fn)`, `computed.get()`, and `effect.dispose()`. Each method is guarded
        // by the node's `kind` (a `signal` is not disposable, a `computed` is read-only,
        // an `effect` is not readable); an invalid pair falls through to the generic
        // no-method runtime error below, exactly as any other unknown method on a
        // built-in type. `get` on a `computed` recomputes a dirty body via
        // `read_reactive`; on a `signal` it is a plain read (the callback never fires).
        if let Some((kind, node)) = v.reactive_parts() {
            match (kind, method) {
                (NodeKind::Signal | NodeKind::Computed, "get") => {
                    return self.read_reactive(node, span);
                }
                (NodeKind::Signal, "set") => {
                    // The graph retains its own reference to the new content and
                    // releases the old; the caller's reference is released by its normal
                    // end-of-life. Then flush so subscribed effects rerun — but coalesce:
                    // a set inside a running effect body only enqueues, the ongoing flush
                    // picks it up (no nested flush). (reactivity S4)
                    self.reactive.set(node, GcVal::retained(args[0]));
                    if !self.reactive.is_flushing() {
                        self.drive_flush(span)?;
                    }
                    return Ok(Value::unit());
                }
                (NodeKind::Signal, "update") => {
                    // Read-modify-write: read the current value (owned), call the
                    // updater with it (which consumes that reference), store the
                    // result, then flush (coalescing inside a running flush, like set).
                    let f = args[0];
                    let current = self.read_reactive(node, span)?;
                    let updated = self.call_value(f, vec![current], span)?;
                    self.reactive.set(node, GcVal::owned(updated));
                    if !self.reactive.is_flushing() {
                        self.drive_flush(span)?;
                    }
                    return Ok(Value::unit());
                }
                (NodeKind::Effect, "dispose") => {
                    // Unsubscribe and free the node; its stored body/content drop.
                    self.reactive.dispose(node);
                    return Ok(Value::unit());
                }
                _ => {}
            }
        }
        // Iterator methods (next/collect) — the shared `IterMethod` enum, like the file
        // handle above.
        if v.is_iter()
            && let Some(iter_method) = noeta_stdlib::IterMethod::from_name(method)
        {
            return self.call_iter_method(v, iter_method, method, args, span);
        }
        // Ring 1 map methods (keys/values/has).
        if v.is_map()
            && let Some(map_method) = noeta_stdlib::MapMethod::from_name(method)
        {
            return self.call_map_method(v, map_method, method, args, span);
        }
        // `list.to_bytes()` — serialize a `List<@packed>` to its flat buffer (P-PACK 4.4);
        // a boxed list has no canonical form, so it's a type error (surfaced, not silent).
        if method == "to_bytes" && v.is_list() {
            if !args.is_empty() {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "method `to_bytes` takes no arguments".to_string(),
                ));
            }
            return match v.packed_bytes() {
                Some(buf) => Ok(Value::bytes(buf)),
                None => Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "`to_bytes` expects a packed list (a `List` of `@packed` structs)".to_string(),
                )),
            };
        }
        // `iter()` on a built-in collection (Track I.1a) → a lazy iterator. A list shares
        // its backing (the iterator retains one reference); a set/map first becomes a list
        // of its elements / values (the iteration order `for` uses).
        if method == "iter" && (v.is_list() || v.is_set() || v.is_map()) {
            if !args.is_empty() {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    "method `iter` takes no arguments".to_string(),
                ));
            }
            let value = if v.is_list() {
                Value::iter(v)
            } else {
                let items = if v.is_set() {
                    v.set_items()
                } else {
                    v.map_values()
                }
                .expect("set/map receiver");
                for item in &items {
                    item.inc_ref();
                }
                let list = Value::list(items);
                let iter = Value::iter(list);
                // `Value::iter` retained the list; drop this local reference so the
                // iterator is its sole owner.
                list.release();
                iter
            };
            return Ok(value);
        }
        // Eager collection methods reusing the prelude builtin impls (prelude-redesign
        // P1): `xs.map(f)` / `xs.filter(f)` / `xs.sum()` on a list, routed through
        // `call_builtin` with the receiver as the first argument so the method and
        // (legacy) free-function forms share one impl. A user object's own method wins
        // (dispatched earlier); a list receiver is never an object.
        if v.list_len().is_some()
            && let Some(builtin) = match method {
                "map" if args.len() == 1 => Some(Builtin::Map),
                "filter" if args.len() == 1 => Some(Builtin::Filter),
                "sum" if args.is_empty() => Some(Builtin::Sum),
                _ => None,
            }
        {
            let mut arg_vals = Vec::with_capacity(args.len() + 1);
            arg_vals.push(v);
            arg_vals.extend_from_slice(args);
            return self.call_builtin(builtin, &arg_vals, span);
        }
        // Built-in zero-argument methods on lists/maps/strings. `len()` is the collection
        // length (P1.3 — `count` is iterator-only, a consuming terminal).
        let result = if !args.is_empty() {
            None
        } else if method == "len" {
            v.list_len()
                .or_else(|| v.set_len())
                .or_else(|| v.map_len())
                .or_else(|| v.as_string().map(|s| s.chars().count()))
                .or_else(|| v.bytes_len())
                .map(|n| Value::int(n as i64))
        } else if method == "enumerate" && v.is_list() {
            // A list of `(index, value)` **tuples** (object-model slice 4b), matching the
            // tree-walker's `Value::Tuple` pairs. A packed list is materialized to a
            // temporary boxed list first (then released).
            let boxed = v.realize_list();
            let items = boxed.list_items().expect("list receiver");
            let pairs = items
                .iter()
                .enumerate()
                .map(|(i, &element)| {
                    retain(element);
                    Value::tuple(vec![Value::int(i as i64), element])
                })
                .collect();
            boxed.release();
            Some(Value::list(pairs))
        } else {
            None
        };
        match result {
            Some(value) => Ok(value),
            None if !args.is_empty() && (method == "len" || method == "enumerate") =>
            {
                Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("method `{method}` takes no arguments"),
                ))
            }
            None => Err(self.error(
                DiagnosticCode::UnknownName,
                span,
                format!("no method `{method}` on {}", v.type_name()),
            )),
        }
    }

    /// Run an unbound method handle (`Type.method`) applied to `args` on a fresh frame stack,
    /// consuming the owned arguments into the callee window (prelude-redesign MH). For an **instance**
    /// handle the first argument is the receiver (register 0 = `self`), the rest are the method's
    /// parameters — identical to a closure call whose prototype is resolved from the method table
    /// rather than a first-class closure. Associated handles are not yet produced (MH.1 is
    /// instance-only); they return a clean error rather than mis-dispatching.
    fn run_method_handle(
        &mut self,
        ty: &str,
        method: &str,
        associated: bool,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, Abort> {
        // An ASSOCIATED handle (`ctor = Stack.new`, prelude-redesign EX.2) calls the function
        // directly — no receiver; the prototype's register 0 (`self`) stays unit, exactly as the
        // opcode's associated dispatch leaves it.
        if associated {
            let Some(&proto) = self.methods.get(&(ty.to_string(), method.to_string())) else {
                for a in args {
                    release(a);
                }
                return Err(self.error(
                    DiagnosticCode::UnknownName,
                    span,
                    format!("type `{ty}` has no associated function `{method}`"),
                ));
            };
            let chunk = &self.module.protos[proto as usize];
            // Register 0 is the (unit) receiver slot, so declared arity is one more than the args.
            let total = chunk.num_params as usize - 1;
            let required = total - chunk.defaults.len();
            if args.len() < required || args.len() > total {
                let supplied = args.len();
                for a in args {
                    release(a);
                }
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    arity_message("associated function", required, total, supplied),
                ));
            }
            let filled = args.len() + 1;
            let num_registers = chunk.num_registers as usize;
            let defaults = chunk.defaults.clone();
            let mut regs = vec![Value::unit(); num_registers];
            for (i, v) in args.into_iter().enumerate() {
                regs[i + 1] = v;
            }
            for (reg, dproto) in &defaults {
                if *reg as usize >= filled {
                    let value = self.run_thunk(*dproto, &[])?;
                    regs[*reg as usize] = value;
                }
            }
            return self.run(
                vec![Frame {
                    proto,
                    base: 0,
                    pc: 0,
                    ret_dst: 0,
                    ret_transform: RetTransform::None,
                    upvalues: Vec::new(),
                }],
                regs,
            );
        }
        // The receiver's runtime type names the method table entry, so a subtype dispatches to its
        // own method; fall back to the handle's declared type if the receiver has no shape.
        let type_name = match args.first() {
            Some(recv) => recv
                .shape()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| ty.to_string()),
            None => {
                return Err(self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("method handle `{ty}.{method}` needs a receiver argument"),
                ));
            }
        };
        let Some(&proto) = self.methods.get(&(type_name.clone(), method.to_string())) else {
            // Not a user method — a **built-in** receiver (`list.len`, `string.upper`, MH.2):
            // dispatch through the same `call_builtin_method` the `Op::CallMethod` opcode uses, so a
            // handle call and a direct call agree by construction (this mirrors the tree-walker,
            // whose handle arm reuses its ordinary `call_method`). The helper borrows; the owned
            // arguments are released after (the result is a fresh value, so this is safe even when
            // it aliases an argument's content).
            let recv = args[0];
            let result = self.call_builtin_method(recv, method, &args[1..], span);
            for a in args {
                release(a);
            }
            return result;
        };
        let chunk = &self.module.protos[proto as usize];
        let num_params = chunk.num_params as usize; // includes register 0 = self (the receiver)
        let num_registers = chunk.num_registers as usize;
        let required = num_params - chunk.defaults.len();
        if args.len() < required || args.len() > num_params {
            let supplied = args.len();
            for a in args {
                release(a);
            }
            return Err(self.error(
                DiagnosticCode::TypeMismatch,
                span,
                arity_message("method", required, num_params, supplied),
            ));
        }
        let filled = args.len();
        let defaults = chunk.defaults.clone();
        let mut regs = vec![Value::unit(); num_registers];
        for (i, v) in args.into_iter().enumerate() {
            regs[i] = v;
        }
        // A method never captures upvalues; fill any omitted trailing defaults from module scope.
        for (reg, dproto) in &defaults {
            if *reg as usize >= filled {
                let value = self.run_thunk(*dproto, &[])?;
                regs[*reg as usize] = value;
            }
        }
        self.run(
            vec![Frame {
                proto,
                base: 0,
                pc: 0,
                ret_dst: 0,
                ret_transform: RetTransform::None,
                upvalues: Vec::new(),
            }],
            regs,
        )
    }

    /// Read a reactive node, driving a `computed`'s recompute through [`call_value`](Self::call_value)
    /// if it is dirty (reactivity S3). A `signal` read never enters the callback; a dirty `computed`
    /// runs its body (and, transitively, any dirty computeds it reads), memoizing as it goes. The graph
    /// is cloned out as an `Rc` first so its `&self` `read` can borrow it while the callback borrows
    /// `self` for `call_value` (different objects, no aliasing). The returned [`GcVal`] owns one
    /// reference (a signal's cloned content or a computed's fresh/memoized value); the caller transfers
    /// it into a register via [`into_value`](GcVal::into_value). A body that aborts is captured
    /// deterministically — the first abort stops further recomputes and propagates.
    fn read_reactive(&mut self, node: noeta_reactive::NodeId, span: Span) -> Result<Value, Abort> {
        let graph = Rc::clone(&self.reactive);
        let mut aborted = false;
        let result = graph.read(node, &mut |body: GcVal| -> GcVal {
            if aborted {
                return GcVal::owned(Value::unit());
            }
            // `body` owns the reference the graph cloned for this recompute; `call_value` only borrows
            // the callee, so peek with `.get()` and let `body` drop (releasing it) at closure end.
            match self.call_value(body.get(), Vec::new(), span) {
                Ok(value) => GcVal::owned(value),
                Err(Abort) => {
                    aborted = true;
                    GcVal::owned(Value::unit())
                }
            }
        });
        if aborted {
            // `result` owns the placeholder unit reference from the aborted run; drop releases it.
            drop(result);
            Err(Abort)
        } else {
            Ok(result.into_value())
        }
    }

    /// Run the reactive graph's pending effects to a fixpoint (reactivity S2), invoking each effect
    /// body through [`call_value`](Self::call_value). Called after any `signal.set`/`.update` and on
    /// `effect(...)` creation. The graph is cloned out as an `Rc` first so its `&self` `flush` can
    /// borrow it while the run callback borrows `self` for `call_value` (different objects, no
    /// aliasing). An effect body that aborts (a panic or `?`) is captured deterministically: the first
    /// abort by flush order stops further bodies (subsequent runs no-op) and propagates — identically
    /// on both backends, since the flush order is a pure function of the graph.
    fn drive_flush(&mut self, span: Span) -> Result<(), Abort> {
        let graph = Rc::clone(&self.reactive);
        let mut aborted = false;
        let overflowed = graph
            .flush(&mut |body: GcVal| -> GcVal {
                if aborted {
                    return GcVal::owned(Value::unit());
                }
                // `body` owns the reference the graph cloned for this run. `call_value` only *borrows*
                // the callee (it does not release it), so peek with `.get()` and let `body` drop —
                // releasing that reference — when this closure returns. The stored body keeps its own.
                match self.call_value(body.get(), Vec::new(), span) {
                    Ok(result) => GcVal::owned(result),
                    Err(Abort) => {
                        aborted = true;
                        GcVal::owned(Value::unit())
                    }
                }
            })
            .is_err();
        // A body-driven abort (panic / `?`) takes priority — propagate it as itself. Otherwise, a
        // non-converging flush (a self-reinforcing effect) becomes the reactive-cycle runtime error.
        if aborted {
            return Err(Abort);
        }
        if overflowed {
            return Err(self.error(
                DiagnosticCode::ReactiveCycle,
                span,
                format!(
                    "reactive update did not converge after {} steps — an effect keeps changing a \
                     signal it depends on",
                    noeta_reactive::MAX_FLUSH_STEPS
                ),
            ));
        }
        Ok(())
    }

    /// Run a defaulted parameter's zero-argument thunk prototype to its value, on a fresh frame
    /// stack (the same re-entry `map`/`filter` callbacks use). `upvalues` are the calling closure's
    /// captured cells — the thunk is compiled with that same upvalue layout, so a default that
    /// references a captured variable reads the right cell; for a top-level function or method this
    /// is empty and the thunk resolves globals only. Each cell is retained for the thunk frame (and
    /// released at its teardown). The returned value owns one reference, transferred to its register.
    fn run_thunk(&mut self, proto: u32, upvalues: &[Value]) -> Result<Value, Abort> {
        let num_registers = self.module.protos[proto as usize].num_registers as usize;
        let mut ups = Vec::with_capacity(upvalues.len());
        for &cell in upvalues {
            retain(cell);
            ups.push(cell);
        }
        let regs = vec![Value::unit(); num_registers];
        self.run(
            vec![Frame {
                proto,
                base: 0,
                pc: 0,
                ret_dst: 0,
                ret_transform: RetTransform::None,
                upvalues: ups,
            }],
            regs,
        )
    }

    /// Set up a call to `callee_val` on the shared frame/register stacks — the closure-call machinery
    /// shared by the `Op::Call` interpreter arm and the JIT's `jit_call` helper (so it lives in one
    /// place). Reads the arguments from `regs[caller_base + arg_regs[i]]`, moves them into a fresh
    /// callee window, fills defaults, carries upvalues, saves `resume_pc` on the caller frame, and
    /// pushes the callee frame. Returns `Ok(true)` when a frame was pushed (the caller should re-derive
    /// its window — `continue 'reload`), or `Ok(false)` when the call completed synchronously (a
    /// first-class builtin, result already in `regs[caller_base + dst]`; the caller advances to
    /// `resume_pc`).
    #[allow(clippy::too_many_arguments)]
    fn setup_closure_call(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        caller_top: usize,
        caller_base: usize,
        dst: u16,
        callee_val: Value,
        arg_regs: &[u16],
        span: Span,
        resume_pc: usize,
    ) -> Result<bool, Abort> {
        match callee_val.as_closure() {
            Some(proto_idx) => {
                let callee_chunk = &self.module.protos[proto_idx as usize];
                let num_params = callee_chunk.num_params as usize;
                let required = num_params - callee_chunk.defaults.len();
                if arg_regs.len() < required || arg_regs.len() > num_params {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        arity_message("function", required, num_params, arg_regs.len()),
                    ));
                }
                let num_registers = callee_chunk.num_registers as usize;
                let new_base = reserve_window(regs, num_registers);
                for (i, &arg_reg) in arg_regs.iter().enumerate() {
                    let v = regs[caller_base + arg_reg as usize];
                    retain(v);
                    regs[new_base + i] = v;
                }
                let count = callee_val.closure_upvalue_count();
                // Fast path (B): a plain function — no defaults to fill and no upvalues to carry —
                // skips the defaults clone, the cell collection, the default-thunk loop, and the
                // upvalue vector entirely. This is the shape of every top-level `fn` call.
                let upvalues = if callee_chunk.defaults.is_empty() && count == 0 {
                    Vec::new()
                } else {
                    let defaults = callee_chunk.defaults.clone();
                    let cells: Vec<Value> =
                        (0..count).map(|i| callee_val.closure_upvalue(i)).collect();
                    let filled = arg_regs.len();
                    for (reg, proto) in &defaults {
                        if *reg as usize >= filled {
                            let value = self.run_thunk(*proto, &cells)?;
                            regs[new_base + *reg as usize] = value;
                        }
                    }
                    let mut upvalues = Vec::with_capacity(count);
                    for &cell in &cells {
                        retain(cell);
                        upvalues.push(cell);
                    }
                    upvalues
                };
                frames[caller_top].pc = resume_pc;
                frames.push(Frame {
                    proto: proto_idx,
                    base: new_base,
                    pc: 0,
                    ret_dst: dst,
                    ret_transform: RetTransform::None,
                    upvalues,
                });
                Ok(true)
            }
            None => match callee_val.as_native_fn() {
                Some(func) => {
                    let arg_vals: Vec<Value> = arg_regs
                        .iter()
                        .map(|&r| regs[caller_base + r as usize])
                        .collect();
                    let result = self.call_native_fn(func, &arg_vals, span)?;
                    set_reg(regs, caller_base, dst, result);
                    Ok(false)
                }
                // A selectively-imported native-module function called by its bare name.
                None => match callee_val.module_fn_parts() {
                    Some((module, func)) => {
                        let arg_vals: Vec<Value> = arg_regs
                            .iter()
                            .map(|&r| regs[caller_base + r as usize])
                            .collect();
                        let result = self.call_native_module(&module, &func, &arg_vals, span)?;
                        set_reg(regs, caller_base, dst, result);
                        Ok(false)
                    }
                    // An unbound method handle (`Type.method`) stored and called directly. Run it
                    // synchronously (its method body re-enters the VM) and land the result — the
                    // arguments are retained since `run_method_handle` consumes owned references.
                    None => match callee_val.method_handle_parts() {
                        Some((ty, method, associated)) => {
                            let arg_vals: Vec<Value> = arg_regs
                                .iter()
                                .map(|&r| {
                                    let v = regs[caller_base + r as usize];
                                    retain(v);
                                    v
                                })
                                .collect();
                            let result =
                                self.run_method_handle(&ty, &method, associated, arg_vals, span)?;
                            set_reg(regs, caller_base, dst, result);
                            Ok(false)
                        }
                        None => Err(self.error(
                            DiagnosticCode::TypeMismatch,
                            span,
                            format!("{} is not callable", callee_val.type_name()),
                        )),
                    },
                },
            },
        }
    }

    /// The `Op::Return` protocol, factored so both the interpreter arm and the JIT's `jit_return`
    /// helper share it (J3 native calls). `raw` is the value being returned (already read from the
    /// returning frame). Retains it across teardown, pops the finished frame, releases its register
    /// window and upvalues, truncates the register stack, applies any `ret_transform`, and transfers
    /// the result into the caller's destination register. Returns `Some(v)` when the **bottom** frame
    /// returned (there is no caller — `run` should yield `v`), or `None` when it transferred to a
    /// caller (control resumes in that caller frame).
    fn do_return(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        raw: Value,
    ) -> Option<Value> {
        self.do_return_masked(frames, regs, raw, u64::MAX)
    }

    /// [`Vm::do_return`] with a window-release mask (P-JSSA S4.0, see [`jit_return`]):
    /// `u64::MAX` releases every slot (the interpreter's path — it has no per-site analysis);
    /// any other value releases only the set bits, a guarantee from the JIT that the clear
    /// slots hold immediates at this (natively-executed) return site.
    fn do_return_masked(
        &mut self,
        frames: &mut Vec<Frame>,
        regs: &mut Vec<Value>,
        raw: Value,
        release_mask: u64,
    ) -> Option<Value> {
        retain(raw); // keep alive across this frame's teardown
        let finished = frames.pop().unwrap();
        if release_mask == u64::MAX {
            let n = self.module.protos[finished.proto as usize].num_registers as usize;
            for i in 0..n {
                release(regs[finished.base + i]);
            }
        } else {
            let mut m = release_mask;
            while m != 0 {
                let i = m.trailing_zeros() as usize;
                m &= m - 1;
                release(regs[finished.base + i]);
            }
        }
        for u in &finished.upvalues {
            release(*u);
        }
        regs.truncate(finished.base);
        // An operator-dispatch frame may post-process its result (`!=` negates `eq`'s bool; `< <= > >=`
        // map `compare`'s `Ordering`). When the transform replaces a heap value (an `Ordering`) with a
        // fresh `bool`, release the original's keep-alive reference so it is not leaked.
        let (v, replaced) = finished.ret_transform.apply(raw);
        if replaced {
            release(raw);
        }
        match frames.last() {
            Some(caller) => {
                // Transfer the retained reference into the caller's destination.
                let idx = caller.base + finished.ret_dst as usize;
                let old = regs[idx];
                regs[idx] = v;
                release(old);
                None
            }
            None => Some(v),
        }
    }

    /// Dispatch a first-class prelude builtin called indirectly. Reuses `call_builtin` (so the
    /// arity/error text matches the direct `CallBuiltin` path exactly), except `len` on a user
    /// object, which re-enters that object's `Length` (`len`) method — mirroring the `CallBuiltin`
    /// object case. Arguments are borrowed; the result is freshly owned.
    fn call_native_fn(
        &mut self,
        func: Builtin,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        if func == Builtin::Len && args.len() == 1 && args[0].is_object() {
            let recv = args[0];
            let type_name = recv.shape().unwrap().name.clone();
            if let Some(&proto) = self.methods.get(&(type_name, "len".to_string())) {
                let chunk = &self.module.protos[proto as usize];
                if chunk.num_params != 1 {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "this method takes {} argument(s) but 0 were supplied",
                            chunk.num_params - 1
                        ),
                    ));
                }
                let mut regs = vec![Value::unit(); chunk.num_registers as usize];
                retain(recv);
                regs[0] = recv;
                return self.run(
                    vec![Frame {
                        proto,
                        base: 0,
                        pc: 0,
                        ret_dst: 0,
                        ret_transform: RetTransform::None,
                        upvalues: Vec::new(),
                    }],
                    regs,
                );
            }
        }
        self.call_builtin(func, args, span)
    }

    /// Dispatch a prelude collection builtin. Arguments are borrowed (their registers retain
    /// ownership); the returned value is freshly owned.
    fn call_builtin(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        match builtin {
            // `next_id()` as a first-class value (`use std.id.{next_id}`, P2c) — direct calls
            // compile to the dedicated `Op::NextId`; an indirect call reads the same counter.
            Builtin::NextId => {
                self.check_arity(builtin, args, 0, span)?;
                let id = self.next_id;
                self.next_id += 1;
                Ok(Value::int(id as i64))
            }
            Builtin::Len => {
                self.check_arity(builtin, args, 1, span)?;
                let v = args[0];
                match v
                    .list_len()
                    .or_else(|| v.set_len())
                    .or_else(|| v.map_len())
                    .or_else(|| v.as_string().map(|s| s.chars().count()))
                {
                    Some(n) => Ok(Value::int(n as i64)),
                    None => Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`len` expects a list, map, or string, found {}",
                            v.type_name()
                        ),
                    )),
                }
            }
            Builtin::Map => {
                self.check_arity(builtin, args, 2, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`map` expects a list, found {}", args[0].type_name()),
                    ));
                }
                // A `map(...)` whose result element type is packed (the checker marked this call span)
                // builds a flat result directly (P-PACK 2.6 category B): each mapped element is packed
                // into the buffer and its boxed object freed, so the result keeps the dense layout (and
                // downstream `[i].field` fusion) instead of materializing N boxed objects. A packed
                // input is read one element at a time (`packed_get`), so only one input element is live
                // at once too.
                if let Some(schema) = self.map_packed.get(&span).cloned() {
                    let input = args[0];
                    let n = input.list_len().expect("list");
                    let packed_input = input.is_packed_list();
                    let func = args[1];
                    let flat = Value::packed_list(schema, Vec::new()); // owned, refcount 1
                    let mut boxed: Option<Vec<Value>> = None;
                    for i in 0..n {
                        let element = if packed_input {
                            input.packed_get(i)
                        } else {
                            let e = input.list_get(i).expect("in bounds");
                            retain(e);
                            e
                        };
                        let out = match self.call_value(func, vec![element], span) {
                            Ok(v) => v,
                            Err(abort) => {
                                flat.release();
                                if let Some(b) = boxed {
                                    for v in b {
                                        release(v);
                                    }
                                }
                                return Err(abort);
                            }
                        };
                        if let Some(b) = &mut boxed {
                            b.push(out); // already in boxed mode
                        } else if flat.packed_push(out) {
                            release(out); // primitives copied into the buffer
                        } else {
                            // The mapped element did not pack (unreachable for a checker-marked site):
                            // demote the accumulated flat elements to a boxed vec, then continue boxed.
                            let count = flat.list_len().expect("packed");
                            let mut b = Vec::with_capacity(n);
                            for j in 0..count {
                                b.push(flat.packed_get(j));
                            }
                            flat.release();
                            b.push(out); // owned (not copied) — transferred into the vec
                            boxed = Some(b);
                        }
                    }
                    return Ok(match boxed {
                        Some(b) => Value::list(b),
                        None => flat,
                    });
                }
                // Demote a packed list to a temporary boxed one (P-PACK 2.4); its elements are
                // borrowed for the per-element calls and the temporary is released afterward.
                let list = args[0].realize_list();
                let items = list.list_items().expect("list receiver");
                let func = args[1];
                let mut result = Vec::with_capacity(items.len());
                let mut failed = None;
                for element in items {
                    retain(element); // transferred into the call
                    match self.call_value(func, vec![element], span) {
                        Ok(v) => result.push(v),
                        Err(abort) => {
                            failed = Some(abort);
                            break;
                        }
                    }
                }
                list.release();
                if let Some(abort) = failed {
                    for r in &result {
                        release(*r);
                    }
                    return Err(abort);
                }
                Ok(Value::list(result))
            }
            Builtin::Filter => {
                self.check_arity(builtin, args, 2, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`filter` expects a list, found {}", args[0].type_name()),
                    ));
                }
                // A packed list stays *flat* (P-PACK 2.6): test each element (materialized only for the
                // predicate call, then consumed by it), record the indices that pass, and rebuild a new
                // packed buffer from those word-blocks — never demoting the whole list to boxed.
                if args[0].is_packed_list() {
                    let list = args[0];
                    let func = args[1];
                    let n = list.list_len().expect("packed list");
                    let mut kept: Vec<usize> = Vec::new();
                    for i in 0..n {
                        let element = list.packed_get(i); // owned (rc 1), consumed by the call
                        let verdict = self.call_value(func, vec![element], span)?;
                        match verdict.as_bool() {
                            Some(true) => kept.push(i),
                            Some(false) => {}
                            None => {
                                let type_name = verdict.type_name();
                                release(verdict);
                                return Err(self.error(
                                    DiagnosticCode::TypeMismatch,
                                    span,
                                    format!(
                                        "`filter` predicate must return a bool, found {type_name}"
                                    ),
                                ));
                            }
                        }
                        release(verdict); // the bool verdict (an immediate) is no longer needed
                    }
                    return Ok(list.packed_select(&kept));
                }
                // Demote a packed list (P-PACK 2.4); elements are borrowed from the temporary, which
                // is released after the loop (a kept element is retained into the result first).
                let list = args[0].realize_list();
                let items = list.list_items().expect("list receiver");
                let func = args[1];
                let mut result = Vec::new();
                let mut failed = None;
                for element in items {
                    retain(element); // transferred into the call
                    let verdict = match self.call_value(func, vec![element], span) {
                        Ok(v) => v,
                        Err(abort) => {
                            failed = Some(abort);
                            break;
                        }
                    };
                    match verdict.as_bool() {
                        Some(true) => {
                            retain(element); // the result list now owns it too
                            result.push(element);
                        }
                        Some(false) => {}
                        None => {
                            let type_name = verdict.type_name();
                            release(verdict);
                            failed = Some(self.error(
                                DiagnosticCode::TypeMismatch,
                                span,
                                format!("`filter` predicate must return a bool, found {type_name}"),
                            ));
                            break;
                        }
                    }
                    release(verdict); // the bool verdict (an immediate) is no longer needed
                }
                list.release();
                if let Some(abort) = failed {
                    for r in &result {
                        release(*r);
                    }
                    return Err(abort);
                }
                Ok(Value::list(result))
            }
            Builtin::Sum => {
                self.check_arity(builtin, args, 1, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects a list, found {}", args[0].type_name()),
                    ));
                }
                // Demote a packed list (P-PACK 2.4) to a temporary boxed one, sum its (numeric)
                // elements, then release the temporary. (A `List<packed struct>` would not type-check
                // for `sum`, but the materialize keeps the path uniform.)
                let list = args[0].realize_list();
                let items = list.list_items().expect("list receiver");
                let mut int_total: i64 = 0;
                let mut float_total: f64 = 0.0;
                let mut any_float = false;
                let mut bad: Option<&'static str> = None;
                for element in &items {
                    // Floats take the float path; every other numeric is an int (matching the
                    // M0 tree-walker, which distinguishes `3` from `3.0`).
                    if let Some(f) = element.as_float() {
                        any_float = true;
                        float_total += f;
                    } else if let Some(i) = element.as_int() {
                        int_total = int_total.wrapping_add(i);
                    } else {
                        bad = Some(element.type_name());
                        break;
                    }
                }
                list.release();
                if let Some(type_name) = bad {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sum` expects numeric elements, found {type_name}"),
                    ));
                }
                Ok(if any_float {
                    Value::float(float_total + int_total as f64)
                } else {
                    Value::int(int_total)
                })
            }
            // `assert(cond)` / `assert(cond, msg)` — mirrors the tree-walker (`Builtin::Assert`): a
            // false condition aborts with the same `Panic` diagnostic `panic` raises, a true one
            // yields unit. The condition must be `bool`; a non-bool is a `TypeMismatch`. Messages use
            // `display()` (as `Op::Panic` does), so the failure text is byte-identical across the
            // differential.
            Builtin::Assert => {
                if args.len() != 1 && args.len() != 2 {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`assert` expects 1 or 2 arguments, found {}", args.len()),
                    ));
                }
                let Some(cond) = args[0].as_bool() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`assert` expects a bool, found {}", args[0].display()),
                    ));
                };
                if cond {
                    Ok(Value::unit())
                } else {
                    let message = match args.get(1) {
                        Some(msg) => format!("assertion failed: {}", msg.display()),
                        None => "assertion failed".to_string(),
                    };
                    Err(self.error(DiagnosticCode::Panic, span, message))
                }
            }
            // `sleep(ms)` — a leaf timer future (Track A.2), ready once the executor clock reaches
            // `now + ms`. Mirrors the tree-walker's `Builtin::Sleep`: the deadline is fixed at
            // creation from the current logical clock. A non-int or negative `ms` is a `TypeMismatch`.
            Builtin::Sleep => {
                self.check_arity(builtin, args, 1, span)?;
                let Some(ms) = args[0].as_int() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sleep` expects an int (ms), found {}", args[0].display()),
                    ));
                };
                if ms < 0 {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`sleep` expects a non-negative duration, found {ms}"),
                    ));
                }
                Ok(Value::make_timer(self.executor.now() + ms as u64))
            }
            // `all(list)` — await every future concurrently, returning a `List<T>` of results in order
            // (Track A.9). Drives the scheduler until each element is ready, collecting its (retained)
            // result; the collected list is a fresh owned value.
            Builtin::All => {
                self.check_arity(builtin, args, 1, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`all` expects a list of futures, found {}",
                            args[0].type_name()
                        ),
                    ));
                }
                let list = args[0];
                let n = list.list_len().expect("a list has a length");
                let mut results: Vec<Option<Value>> = vec![None; n];
                loop {
                    let mut i = 0;
                    while i < n {
                        if results[i].is_none() {
                            let h = list.list_get(i).expect("in bounds");
                            if let Poll::Ready(v) = self.poll_once(h, span)? {
                                results[i] = Some(v);
                            }
                        }
                        i += 1;
                    }
                    if results.iter().all(Option::is_some) {
                        let items = results.into_iter().map(|r| r.expect("all ready")).collect();
                        return Ok(Value::list(items));
                    }
                    let progressed = self.poll_all_scopes_round(span)?;
                    if !progressed
                        && self.executor.advance().is_none()
                        && !self.isolate_in_flight_wait()
                    {
                        for v in results.into_iter().flatten() {
                            release(v);
                        }
                        return Err(self.error(
                            DiagnosticCode::Panic,
                            span,
                            "async deadlock: `all` awaited futures with no pending timers"
                                .to_string(),
                        ));
                    }
                }
            }
            // `race(list)` — await concurrently, returning the first ready result and cancelling the
            // losing tasks (Track A.9 + A.8). Ties break deterministically by list order, so both
            // backends agree.
            Builtin::Race => {
                self.check_arity(builtin, args, 1, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`race` expects a list of futures, found {}",
                            args[0].type_name()
                        ),
                    ));
                }
                let list = args[0];
                let n = list.list_len().expect("a list has a length");
                if n == 0 {
                    return Err(self.error(
                        DiagnosticCode::Panic,
                        span,
                        "`race` requires at least one future".to_string(),
                    ));
                }
                loop {
                    for i in 0..n {
                        let h = list.list_get(i).expect("in bounds");
                        if let Poll::Ready(v) = self.poll_once(h, span)? {
                            for j in 0..n {
                                if j != i {
                                    let hj = list.list_get(j).expect("in bounds");
                                    self.cancel_task(hj);
                                }
                            }
                            return Ok(v);
                        }
                    }
                    let progressed = self.poll_all_scopes_round(span)?;
                    if !progressed
                        && self.executor.advance().is_none()
                        && !self.isolate_in_flight_wait()
                    {
                        return Err(self.error(
                            DiagnosticCode::Panic,
                            span,
                            "async deadlock: `race` awaited futures with no pending timers"
                                .to_string(),
                        ));
                    }
                }
            }
            // `map_bounded(items, n, f)` — apply async `f` to each item with at most `n` futures in
            // flight, collecting results in item order (Track A.9). Keeps a sliding window: fill up to
            // `n`, poll them (each first poll starts f's body), collect completions and top the window
            // back up. Also drives any open `concurrent` scope so it composes with A.7.
            Builtin::MapBounded => {
                self.check_arity(builtin, args, 3, span)?;
                if !args[0].is_list() {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`map_bounded` expects a list, found {}",
                            args[0].type_name()
                        ),
                    ));
                }
                let Some(limit) = args[1].as_int() else {
                    return Err(self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!(
                            "`map_bounded` expects an int concurrency limit, found {}",
                            args[1].type_name()
                        ),
                    ));
                };
                let items = args[0];
                let f = args[2];
                let window = limit.max(1) as usize;
                let count = items.list_len().expect("a list has a length");
                let mut results: Vec<Option<Value>> = vec![None; count];
                let mut in_flight: Vec<(usize, Value)> = Vec::new();
                let mut next = 0usize;
                let mut done = 0usize;
                // Release everything owned so far, for the early-exit paths.
                macro_rules! cleanup {
                    ($in_flight:expr, $results:expr) => {{
                        for (_, fut) in $in_flight {
                            release(fut);
                        }
                        for v in $results.into_iter().flatten() {
                            release(v);
                        }
                    }};
                }
                loop {
                    while in_flight.len() < window && next < count {
                        let item = items.list_get(next).expect("in bounds");
                        retain(item);
                        match self.call_value(f, vec![item], span) {
                            Ok(fut) => in_flight.push((next, fut)),
                            Err(abort) => {
                                cleanup!(in_flight, results);
                                return Err(abort);
                            }
                        }
                        next += 1;
                    }
                    if done == count {
                        let out = results.into_iter().map(|r| r.expect("all done")).collect();
                        return Ok(Value::list(out));
                    }
                    let mut progressed = false;
                    let mut k = 0;
                    while k < in_flight.len() {
                        let (idx, fut) = in_flight[k];
                        match self.poll_once(fut, span) {
                            Ok(Poll::Ready(v)) => {
                                results[idx] = Some(v);
                                release(fut);
                                in_flight.remove(k);
                                done += 1;
                                progressed = true;
                            }
                            Ok(Poll::Pending) => k += 1,
                            Err(abort) => {
                                cleanup!(in_flight, results);
                                return Err(abort);
                            }
                        }
                    }
                    if !self.scopes.is_empty() {
                        progressed |= self.poll_all_scopes_round(span)?;
                    }
                    if !progressed
                        && self.executor.advance().is_none()
                        && !self.isolate_in_flight_wait()
                    {
                        cleanup!(in_flight, results);
                        return Err(self.error(
                            DiagnosticCode::Panic,
                            span,
                            "async deadlock: `map_bounded` stalled with no pending timers"
                                .to_string(),
                        ));
                    }
                }
            }
            Builtin::Signal => {
                self.check_arity(builtin, args, 1, span)?;
                // `signal(v)` — allocate a reactive cell holding `v`. The graph keeps its own
                // (retained) reference; the register's reference is released by the caller as usual.
                let id = self.reactive.signal(GcVal::retained(args[0]));
                Ok(Value::make_reactive(NodeKind::Signal, id))
            }
            Builtin::Computed => {
                self.check_arity(builtin, args, 1, span)?;
                // `computed(fn)` — register a lazy derivation, retaining the body closure. Created dirty;
                // it computes on first `.get()`, so nothing runs now (no flush).
                let id = self.reactive.computed(GcVal::retained(args[0]));
                Ok(Value::make_reactive(NodeKind::Computed, id))
            }
            Builtin::Effect => {
                self.check_arity(builtin, args, 1, span)?;
                // `effect(fn)` — register the effect (created queued), retaining the body closure, then
                // flush to run it once now (subscribing it to the signals it reads). If we are already
                // inside a flush (an effect created within another effect's body), the ongoing flush
                // drains it — do not nest. (reactivity S4)
                let id = self.reactive.effect(GcVal::retained(args[0]));
                if !self.reactive.is_flushing() {
                    self.drive_flush(span)?;
                }
                Ok(Value::make_reactive(NodeKind::Effect, id))
            }
        }
    }

    fn check_arity(
        &mut self,
        builtin: Builtin,
        args: &[Value],
        expected: usize,
        span: Span,
    ) -> Result<(), Abort> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` takes {expected} argument(s) but {} were supplied",
                    builtin.name(),
                    args.len()
                ),
            ))
        }
    }
}

/// Overwrite a register, releasing the value it held.
fn set_reg(regs: &mut [Value], base: usize, dst: u16, value: Value) {
    let idx = base + dst as usize;
    let old = regs[idx];
    regs[idx] = value;
    release(old);
}

/// Reserve a fresh `n`-slot register window at the top of the dispatch register stack for a callee
/// frame and return its base (P-VMT-FRAME). Slots are `unit`-initialized; the caller writes the
/// receiver/arguments into `regs[base..]` and pushes a `Frame { base, .. }`. Growing the stack may
/// reallocate the backing buffer, so no borrow into `regs` may be held across this call — access is
/// always by `(base, index)`.
fn reserve_window(regs: &mut Vec<Value>, n: usize) -> usize {
    let base = regs.len();
    if base + n > regs.capacity() {
        // Growing: initialize the whole new capacity (fill to capacity, then shrink back), so
        // the JIT's fast call convention may later extend `len` over it with `set_len` — every
        // element within capacity has then been written at least once (`set_len`'s contract).
        // Runs only on the rare growth, not per call.
        regs.reserve(base + n - regs.len());
        let cap = regs.capacity();
        regs.resize(cap, Value::unit());
        regs.truncate(base);
    }
    regs.resize(base + n, Value::unit());
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn run(src: &str) -> RunResult {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        VmBackend::new()
            .try_run(&parsed.program)
            .expect("program should be in the M1.0 subset")
    }

    /// Compile a source program to a [`Module`] (or panic if it's outside the VM subset), for the
    /// tests that need to drive `run_module`/`run_module_jit` directly.
    #[cfg(feature = "jit")]
    fn compile_module(src: &str) -> Module {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        compile(&parsed.program).expect("program should be in the M1.0 subset")
    }

    /// P-CALL S1 lock test: every offset [`frame_layout`] reports must locate the real `Frame` field,
    /// and the probed `Vec`-header word indices must read back a live `Vec`'s ptr/len/cap. Because the
    /// JIT bakes these numbers into native code generated in the same build, a silent `Frame`-layout
    /// or `Vec`-header change would corrupt memory under the JIT; this test fails the build first.
    #[cfg(feature = "jit")]
    #[test]
    #[allow(unsafe_code)]
    fn frame_layout_locks_the_real_layout() {
        let l = frame_layout();
        assert_eq!(l.frame_size, size_of::<Frame>());
        assert_eq!(l.frame_align, align_of::<Frame>());

        // A sentinel frame: read each scalar field back through its reported offset.
        let f = Frame {
            proto: 0x0BAD_F00D,
            base: 0x1111_2222,
            pc: 0x3333_4444,
            ret_dst: 0x5566,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        };
        let fp = (&f as *const Frame) as usize;
        unsafe {
            assert_eq!(*((fp + l.proto_offset) as *const u32), 0x0BAD_F00D);
            assert_eq!(*((fp + l.base_offset) as *const usize), 0x1111_2222);
            assert_eq!(*((fp + l.pc_offset) as *const usize), 0x3333_4444);
            assert_eq!(*((fp + l.ret_dst_offset) as *const u16), 0x5566);
        }
        // The two empty-initialized fields must sit within the struct.
        assert!(l.ret_transform_offset < l.frame_size);
        assert!(l.upvalues_offset + size_of::<Vec<Value>>() <= l.frame_size);

        // Vec-header words: read a live Vec's ptr/len/cap back through the probed indices.
        let mut v: Vec<Value> = Vec::with_capacity(64);
        v.push(Value::unit());
        v.push(Value::unit());
        let words: [usize; 3] = unsafe { core::mem::transmute_copy(&v) };
        assert_eq!(words[l.vec_ptr_word], v.as_ptr() as usize);
        assert_eq!(words[l.vec_len_word], v.len());
        assert_eq!(words[l.vec_cap_word], v.capacity());
        // The three indices are a permutation of {0, 1, 2}.
        let mut idx = [l.vec_ptr_word, l.vec_len_word, l.vec_cap_word];
        idx.sort_unstable();
        assert_eq!(idx, [0, 1, 2]);
    }

    /// P-JIT foundation: a prototype with no compilable op runs its bail stub — reaching the
    /// `noeta_jit_observe` helper — and control falls cleanly back to tier 0 with a byte-identical
    /// result. `echo "hi"` is exactly such a program (its only prototype is `LoadConst`(str) /
    /// `Stringify` / `Echo` / `Halt`, none of them fast). Proves the seam (Cranelift build + finalize,
    /// tier-0/1 dispatch, the helper ABI, the deopt handoff) end to end.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_foundation_bails_to_identical_result_and_runs_native_stubs() {
        let module = compile_module("echo \"hi\";\n");
        let interp = VmBackend::new().run_module(&module);
        let before = jit_observe_count();
        let jit = VmBackend::new().run_module_jit(&module);
        let entered = jit_observe_count() - before;

        assert_eq!(interp, jit, "tier-1 result must match the interpreter");
        assert_eq!(jit.stdout, "hi\n");
        assert!(entered >= 1, "expected the bail stub to run, got {entered}");
    }

    /// J1 (integer fast path): a pure-integer `while`-loop function compiles to native code and, run
    /// through the forced JIT, produces exactly the interpreter's result. This exercises the whole
    /// integer op set — `LoadConst`, `Binary` (`+`/`%`/`<`), `CondBranch`, `Move`, `Drop`, `Jump` —
    /// natively, with the `Return` bailing to tier 0.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_integer_while_loop_is_native_and_correct() {
        // sum of (i % 7) for i in 0..n — arithmetic, remainder, comparison, and a back-edge, all in
        // registers (no globals, no calls) → J1-eligible.
        let src = "fn run(n: int): int {\n  mut total = 0;\n  mut i = 0;\n  while i < n {\n    total = total + (i % 7);\n    i = i + 1;\n  }\n  return total;\n}\necho run(1000);\n";
        let module = compile_module(src);

        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);

        // The `run` prototype (and only it) is J1-eligible.
        assert!(
            stats.native >= 1,
            "the while-loop fn must go native, got {stats:?}"
        );
        assert_eq!(interp, jit, "tier-1 result must match the interpreter");
        // Independently confirm the value: sum_{i=0}^{999} (i % 7).
        let expected: i64 = (0..1000).map(|i| i % 7).sum();
        assert_eq!(jit.stdout, format!("{expected}\n"));
    }

    /// J1 deopt: a would-be big-int result (overflowing the 48-bit immediate range) bails from native
    /// code to the interpreter, which heap-boxes it — so the JIT and interpreter still agree.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_integer_overflow_bails_and_matches() {
        // 2^40 * 2^40 = 2^80 wraps in i64 and, at each doubling, eventually exceeds the 48-bit
        // immediate range, forcing the overflow-bail path; the interpreter's wrapping result must match.
        let src = "fn run(n: int): int {\n  mut x = 1;\n  mut i = 0;\n  while i < n {\n    x = x * 3;\n    i = i + 1;\n  }\n  return x;\n}\necho run(60);\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, _) = VmBackend::new().run_module_jit_with_stats(&module);
        assert_eq!(
            interp, jit,
            "overflow-bail result must match the interpreter"
        );
    }

    /// J2 (float fast path): a mixed int/float `while` loop — a float accumulator (`+`) with an
    /// integer counter (`<`, `+`) — compiles to native code (each homogeneous `Binary` takes its
    /// int or float branch) and matches the interpreter exactly.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_float_while_loop_is_native_and_correct() {
        let src = "fn run(n: int): float {\n  mut x = 0.0;\n  mut i = 0;\n  while i < n {\n    x = x + 1.5;\n    i = i + 1;\n  }\n  return x;\n}\necho run(1000);\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the float loop fn must go native, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "tier-1 float result must match the interpreter"
        );
        assert_eq!(jit.stdout, "1500.0\n");
    }

    /// J2 float division, comparison, and NaN: `6.0 / 4.0` divides natively, `0.0 / 0.0` produces a
    /// canonicalized NaN, and an ordered float `<` (false on NaN) drives a `CondBranch` — the paths
    /// most likely to diverge from the interpreter.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_float_division_and_nan_match() {
        let src = "fn run(): float {\n  mut a = 6.0 / 4.0;\n  mut z = 0.0;\n  mut q = z / z;\n  if q < a { return 0.0; }\n  return a;\n}\necho run();\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the float fn must go native, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "NaN/division float result must match the interpreter"
        );
        assert_eq!(jit.stdout, "1.5\n");
    }

    /// J4 (heap/collections): a `for i in 0..n` loop — the idiomatic range loop, whose `MakeRange` /
    /// `IterSnapshot` / `ListLen` / `ListGet` internals now run natively (through the leaf-op helper),
    /// so the whole loop body is native. Refcount-exact (the snapshot list is a heap value) and
    /// byte-identical to the interpreter.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_for_range_loop_is_native_and_correct() {
        let src = "fn run(n: int): int {\n  mut acc = 0;\n  for i in 0..n {\n    acc = acc + i;\n  }\n  return acc;\n}\necho run(1000);\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the for-range loop fn must go native, got {stats:?}"
        );
        assert_eq!(interp, jit, "for-range result must match the interpreter");
        let expected: i64 = (0..1000).sum();
        assert_eq!(jit.stdout, format!("{expected}\n"));
    }

    /// Field access (P-JIT J4 slice 2): a hot loop that reads (`LoadField`) and writes (`SetField`,
    /// the struct copy-on-write / reuse path) object fields runs natively through the leaf-op helper
    /// and matches the interpreter — the store logic is the shared `set_field_fast`, so refcounts are
    /// identical across the tier boundary (the `--jit-differential` leak check gates that).
    #[cfg(feature = "jit")]
    #[test]
    fn jit_field_access_loop_is_native_and_correct() {
        let src = "struct Point {\n  mut x: int\n  mut y: int\n}\nfn run(n: int): int {\n  mut p = Point { x: 0, y: 0 };\n  mut i = 0;\n  while i < n {\n    p.x = p.x + i;\n    p.y = p.y + p.x;\n    i = i + 1;\n  }\n  return p.x + p.y;\n}\necho run(100);\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the field-access loop fn must go native, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "field-access result must match the interpreter"
        );
        assert_eq!(jit.stdout, "171600\n");
    }

    /// Subscript indexing (P-JIT J4 slice 3): a hot loop that indexes a list (`xs[i]`), a map
    /// (`m[key]`), and a nested list-of-keys runs natively through the leaf-op helper's `Op::Index`
    /// arm (the non-dispatching list/map/string paths; a user `Index` impl and every error case bail)
    /// and matches the interpreter, including the borrow/retain of each looked-up element.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_indexing_loop_is_native_and_correct() {
        let src = "fn run(n: int): int {\n  xs = [10, 20, 30, 40, 50];\n  m = { \"a\": 1, \"b\": 2, \"c\": 3 };\n  keys = [\"a\", \"b\", \"c\"];\n  mut total = 0;\n  mut i = 0;\n  while i < n {\n    total = total + xs[i % 5];\n    total = total + m[keys[i % 3]];\n    i = i + 1;\n  }\n  return total;\n}\necho run(30);\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the indexing loop fn must go native, got {stats:?}"
        );
        assert_eq!(interp, jit, "indexing result must match the interpreter");
        assert_eq!(jit.stdout, "960\n");
    }

    /// Tuple construction + projection (P-JIT J4 slice 4): a `for (i, x) in xs.enumerate()` loop —
    /// `enumerate` yields `(int, T)` tuples (native `ListGet`) that the destructuring reads with
    /// `TupleIndex` — runs natively through the leaf-op helper and matches the interpreter, including
    /// the retain of each projected element.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_tuple_enumerate_loop_is_native_and_correct() {
        let src = "fn run(): int {\n  xs = [10, 20, 30, 40];\n  mut total = 0;\n  for (i, x) in xs.enumerate() {\n    total = total + i * x;\n  }\n  return total;\n}\necho run();\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the enumerate loop fn must go native, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "tuple-enumerate result must match the interpreter"
        );
        // 0*10 + 1*20 + 2*30 + 3*40 = 200.
        assert_eq!(jit.stdout, "200\n");
    }

    /// OSR (P-JIT J5): a **top-level** loop — the whole program is one `while` loop in `main`, which
    /// is entered exactly *once*, so entry-count promotion would never make it hot. Under ordinary
    /// hot-counter promotion (not `force_jit`), it must still go native by counting the loop's
    /// **back-edges** and entering tier 1 mid-frame at the loop header (on-stack replacement). This is
    /// the production hole J5 closes.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_osr_top_level_loop_goes_native() {
        // 200 iterations > JIT_HOT_THRESHOLD (50): the back-edge counter promotes `main` (proto 0)
        // and OSRs into its loop, even though `main` is entered only once.
        let src =
            "mut acc = 0\nmut i = 0\nwhile i < 200 {\n  acc = acc + i\n  i = i + 1\n}\necho acc\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_hot_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the top-level loop must go native via OSR under hot-counter promotion, got {stats:?}"
        );
        assert_eq!(interp, jit, "OSR result must match the interpreter");
        let expected: i64 = (0..200).sum();
        assert_eq!(jit.stdout, format!("{expected}\n"));
    }

    /// OSR refcount-exactness (P-JIT J5): a top-level loop whose body moves **heap** values — a
    /// top-level struct `b` read (`LoadField`) and written (`SetField`, the struct copy-on-write path)
    /// each iteration, with the global `b` loaded into a register (a heap value) every pass. It
    /// promotes and OSRs into native code mid-frame with that heap value live. Forcing `heap_aware`
    /// for OSR-capable prototypes keeps the register stores refcount-correct; the result must match
    /// the interpreter (the `--jit-differential` leak check gates residency).
    #[cfg(feature = "jit")]
    #[test]
    fn jit_osr_heap_body_matches_interpreter() {
        let src = "struct Box { mut v: int }\nmut b = Box { v: 0 }\nmut i = 0\nwhile i < 100 {\n  b.v = b.v + i\n  i = i + 1\n}\necho b.v\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_hot_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the heap-body top-level loop must go native via OSR, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "OSR heap-body result must match the interpreter"
        );
        let expected: i64 = (0..100).sum();
        assert_eq!(jit.stdout, format!("{expected}\n"));
    }

    /// Native calls (P-JIT J3): recursive `fib` — the callee closure loaded via a heap-aware
    /// `LoadGlobal` (retain), the recursive `Call` handled by the shared setup on the contiguous
    /// stack, refcounts exact across the tier-0/tier-1 boundary — produces exactly the interpreter's
    /// result. The `fib` prototype (and the top-level) go native.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_recursive_call_is_native_and_correct() {
        let src = "fn fib(n: int): int {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(20);\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        assert!(
            stats.native >= 1,
            "the recursive fn must go native, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "recursive-call result must match the interpreter"
        );
        // fib(20) = 6765.
        assert_eq!(jit.stdout, "6765\n");
    }

    /// Native globals (P-JIT): a **top-level** loop with global `mut` accumulators — the natural
    /// scripting shape — compiles natively (LoadGlobal/StoreGlobal inlined; first-bind via the
    /// `note_global_bound` helper; `echo` at the end bails) and matches the interpreter. This exercises
    /// per-op bail (the top-level prototype has `Echo`/`Stringify` it can't compile) plus the
    /// unbound→bound global transition.
    #[cfg(feature = "jit")]
    #[test]
    fn jit_global_top_level_loop_is_native_and_correct() {
        let src = "mut total = 0;\nmut i = 0;\nwhile i < 1000 {\n  total = total + (i % 7);\n  i = i + 1;\n}\necho total;\n";
        let module = compile_module(src);
        let interp = VmBackend::new().run_module(&module);
        let (jit, stats) = VmBackend::new().run_module_jit_with_stats(&module);
        // The top-level prototype (proto 0) itself goes native here.
        assert!(
            stats.native >= 1,
            "the top-level global loop must go native, got {stats:?}"
        );
        assert_eq!(
            interp, jit,
            "native-globals result must match the interpreter"
        );
        let expected: i64 = (0..1000).map(|i| i % 7).sum();
        assert_eq!(jit.stdout, format!("{expected}\n"));
    }

    /// Peak heap residency for one program (architecture §0.3) — `reset_peak` before, `live_peak`
    /// after, so the high-water mark is measured in isolation.
    fn peak_residency(src: &str) -> usize {
        noeta_value::reset_peak();
        let _ = run(src);
        noeta_value::live_peak()
    }

    #[test]
    fn destructor_runs_on_collected_cycle_capture() {
        // Phase-6 destructor-on-collect: a self-recursive nested `fn` (the closure↔cell cycle) also
        // captures a destructor-bearing `Res`. After the call the whole subgraph — cycle + captured
        // `Res` — is unreachable garbage that only the collector reclaims; reclaiming it must run the
        // captured `Res`'s `destruct` (its last reference died with the cycle). So `drop 7` prints at
        // program-exit collection, after `make()`'s own `7`.
        let r = run(
            "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}\nfn make(): int {\n  r = Res.new(7);\n  fn rec(n: int): int { if n <= 0 { return r.id; } return rec(n - 1); }\n  return rec(2);\n}\necho make();\n",
        );
        assert_eq!(r.stdout, "7\ndrop 7\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn cycle_is_reclaimed_by_backup_trace() {
        // A self-recursive nested `fn` ties a closure↔cell cycle that outlives the enclosing call;
        // refcounting alone cannot reclaim it (each member is kept alive by the other), so without
        // the Phase-6 backup mark-sweep it would leak. After the run, live residency must return to
        // its pre-run baseline — the collector reaped the cycle. Run under miri to validate the
        // collector + live-object registry (no use-after-free / double-free / leak).
        let before = noeta_value::live_count();
        let r = run(
            "fn compute(): int {\n  fn fact(n: int): int {\n    if n <= 1 { return 1; }\n    return n * fact(n - 1);\n  }\n  return fact(5);\n}\necho compute();\n",
        );
        assert_eq!(r.stdout, "120\n");
        assert_eq!(r.exit_code, 0);
        assert_eq!(
            noeta_value::live_count(),
            before,
            "the closure↔cell cycle must be reclaimed by the backup trace"
        );
    }

    fn run_with_collector(src: &str, mode: noeta_value::CollectorMode) -> RunResult {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).expect("program should be in the M1.0 subset");
        VmBackend::new().run_module_with_collector(&module, mode)
    }

    #[test]
    fn trial_deletion_reclaims_cycles_and_acyclic_garbage() {
        // The Phase-6.4 trial-deletion collector, exercised on its release path: a self-recursive
        // nested `fn` (the closure↔cell cycle, buffered as a candidate when the frame unwinds) plus
        // ordinary acyclic, heap-bearing programs (strings, objects, lists — none should be wrongly
        // buffered/freed). Each must finish with residency back at its pre-run baseline. Run under
        // miri to validate the deferred-dealloc release path + candidate buffering (no UAF / double
        // free / leak).
        let cyclic = "fn compute(): int {\n  fn fact(n: int): int {\n    if n <= 1 { return 1; }\n    return n * fact(n - 1);\n  }\n  return fact(5);\n}\necho compute();\n";
        let acyclic = "class P { mut x: int  tag: string\n  fn new(): P { return P { x: 0, tag: \"t\" }; } }\nmut p = P.new();\nfor i in 0..3 { p.x = p.x + i; }\nmut xs = [\"a\", \"b\"];\nxs[0] = \"z\";\necho \"${p.x} ${xs.join(\",\")}\";\n";
        // A reassigned destructor-bearing object exercises the VM's `release_value` last-reference
        // free — the path that must defer a *buffered* object rather than free it shallowly (the bug
        // that segfaulted before `free_shallow` became the universal deferral point).
        let destructed = "class Res { id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { x = self.id + 1; }\n}\nmut r = Res.new(0);\nfor i in 0..3 { r = Res.new(i); }\necho r.id;\n";
        for src in [cyclic, acyclic, destructed] {
            let before = noeta_value::live_count();
            let r = run_with_collector(src, noeta_value::CollectorMode::TrialDeletion);
            assert_eq!(r.exit_code, 0, "program aborted: {:?}", r.diagnostics);
            assert_eq!(
                noeta_value::live_count(),
                before,
                "trial-deletion must reclaim all heap (cycles + acyclic) by clean exit"
            );
        }
        // Reset the thread-local mode so later tests on this thread see the default again.
        noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    }

    #[test]
    fn mm_peak_residency_baseline() {
        // The pre-migration peak-residency snapshot for `plans/memory-management/phase-0-benchmarks`.
        // Prints under `--nocapture`; asserts the meter reflects each program's footprint shape.

        // Allocation churn: each short-lived struct dies before the next is built ⇒ a small,
        // n-independent peak (the reclaim-at-last-use shape we already have on a local temp).
        let churn = "class Pair { a: int b: int }\nmut total = 0;\nfor i in 0..4000 { p = Pair { a: i, b: i }; total = total + p.a; }\necho total;\n";
        let churn_peak = peak_residency(churn);

        // A monotonically-growing accumulator of **heap** elements (records — ints would be immediate
        // and never counted). Peak ≈ n live objects at the end: the genuinely-live structure prompt
        // reclamation cannot shrink, but whose transient cost reuse/COW keeps O(n) not O(n²).
        let accumulate = "class Pair { a: int b: int }\nmut acc = [];\nfor i in 0..4000 { acc ~= [Pair { a: i, b: i }]; }\necho acc.len();\n";
        let accumulate_peak = peak_residency(accumulate);

        // (Deep-nested teardown is benched separately on the optimized bench profile — its recursive
        // `free` overflows this 2 MiB debug test thread at shallow depth, the MM limitation recorded
        // in `phase-0-benchmarks.md`; it is not measured here.)

        eprintln!(
            "MM peak residency (objects): alloc_churn(n=4000)={churn_peak}  accumulate_records(n=4000)={accumulate_peak}"
        );

        // Shape assertions (not exact counts — those are the recorded baseline): churn stays small and
        // n-independent; the struct accumulator's peak scales with n.
        assert!(churn_peak < 100, "alloc churn peak should be n-independent");
        assert!(
            accumulate_peak >= 4000,
            "record-accumulator peak should scale with n"
        );
    }

    /// A function with `n` **single-assignment** intermediate records chained `aᵢ = f(aᵢ₋₁)`, each
    /// dead once the next is built. Returns a scalar so nothing heap stays live past the chain.
    fn sequential_intermediates_src(n: usize) -> String {
        let mut body = String::from("  a0 = Pair { a: 1, b: 1 };\n");
        for i in 1..n {
            body.push_str(&format!(
                "  a{i} = Pair {{ a: a{prev}.a + 1, b: a{prev}.b }};\n",
                prev = i - 1
            ));
        }
        format!(
            "class Pair {{ a: int b: int }}\nfn chain(): int {{\n{body}  return a{last}.a;\n}}\necho chain();\n",
            last = n - 1
        )
    }

    #[test]
    fn mm_peak_residency_prompt_reclamation_is_n_independent() {
        // The headline Phase-3 metric (memory-management `phase-3-rc-passes` gate): precise last-use
        // drops reclaim a function-local the moment it dies, so a straight-line chain of n transient
        // intermediates holds only ~the current+previous struct live at once — an O(1), n-INDEPENDENT
        // peak. Under the pre-migration reclaim-at-teardown model every aᵢ stayed live until `chain`
        // returned, an O(n) peak. We prove the win by its shape: the peak must not grow with n.
        let small = peak_residency(&sequential_intermediates_src(50));
        let large = peak_residency(&sequential_intermediates_src(400));
        eprintln!(
            "MM peak residency (objects): sequential_intermediates n=50={small}  n=400={large}"
        );
        // n-independence is the proof of prompt reclamation: 8× the chain length leaves the peak flat
        // (a tiny constant — the live window — not 8× larger). A generous bound absorbs allocator slack
        // while still failing hard if drops regressed to teardown reclamation (which would be ≈ n).
        assert!(
            small < 20 && large < 20,
            "prompt last-use reclamation should keep the intermediate-chain peak O(1); got n=50→{small}, n=400→{large}"
        );
    }

    #[test]
    fn invoke_by_name_wraps_ok_and_err() {
        // `invoke` dispatches by runtime name: a hit wraps the return in `Result.Ok` (via the
        // `WrapOk` frame transform); an unknown name / arity mismatch builds `Result.Err`. Exercises
        // the new type-handle value, the `Op::Invoke` dispatch, and the refcount handoff on return.
        let r = run(
            "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return self.v * 2; }\n}\nhit = match invoke(Box.new(21), \"doubled\", []) { Ok(v) => \"${v}\", Err(e) => \"err ${e}\" };\necho hit;\nmade = match invoke(Box, \"new\", [7]) { Ok(b) => match invoke(b, \"doubled\", []) { Ok(d) => \"${d}\", Err(_) => \"x\" }, Err(_) => \"x\" };\necho made;\nmiss = match invoke(Box.new(1), \"nope\", []) { Ok(_) => \"ok\", Err(_) => \"miss\" };\necho miss;\n",
        );
        assert_eq!(r.stdout, "42\n14\nmiss\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn type_of_distinguishes_nominal_kinds() {
        // `type_of` classifies a value's shape kind into `Type.Enum`/`Type.Struct`/`Type.Class`
        // (not a collapsed `Named`). Exercises `vm_type_repr` + `build_type_value`'s kind arms and
        // their refcount handoff.
        let r = run(
            "enum E { A; }\nstruct R { x: int }\nclass C {\n  v: int\n  fn new(): C { return C { v: 1 }; }\n}\nfn k(t: Type): string { return match t { Type.Enum(n, _) => \"e:${n}\", Type.Struct(n, _) => \"r:${n}\", Type.Class(n, _) => \"c:${n}\", _ => \"?\" }; }\necho k(type_of(E.A));\necho k(type_of(R { x: 1 }));\necho k(type_of(C.new()));\n",
        );
        assert_eq!(r.stdout, "e:E\nr:R\nc:C\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn abstract_kind_is_tests() {
        // `is Enum`/`Struct`/`Class` are runtime kind tests over a `dyn` value, keyed on the
        // value's shape kind. Exercises the new `narrow_matches` arms in the VM.
        let r = run(
            "enum E { A; }\nstruct R { x: int }\nclass C {\n  v: int\n  fn new(): C { return C { v: 1 }; }\n}\ne: dyn = E.A;\nrec: dyn = R { x: 1 };\nc: dyn = C.new();\necho e is Enum;\necho rec is Struct;\necho c is Class;\necho e is Struct;\n",
        );
        assert_eq!(r.stdout, "true\ntrue\ntrue\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn roles_of_materializes_the_index() {
        // `roles_of()` materializes the `(declaration, role)` index into a `List<RoleBinding>`,
        // each carrying a fresh `string` target and the named enum value. Exercises `materialize_roles`
        // and `make_role` plus the refcount handoff of the freshly-built list/struct/enum values.
        let r = run(
            "@attribute(Function)\n@role(Semantic.EntryPoint)\nstruct Route { path: string }\n#[Route(\"/x\")]\nfn handle(): int { return 1; }\nfor b in roles_of() {\n  echo match b.role { Semantic.EntryPoint => \"${b.target}=entry\", _ => \"other\" };\n}\n",
        );
        assert_eq!(r.stdout, "handle=entry\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn arithmetic_and_concat() {
        let r = run("echo 1 + 2 * 3;\necho \"users/\" ~ 42 ~ \"/profile\";\n");
        assert_eq!(r.stdout, "7\nusers/42/profile\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn cow_in_place_append_paths() {
        // VM-side copy-on-write self-append (`~=`). Covers: a GLOBAL accumulator (TakeGlobal +
        // ConcatInPlace) on the unique path (`g ~= ["b"]`) and the aliased path (`h = g; g ~= ["c"]`
        // — the alias must keep `h` at the pre-append value, so COW copies); and a LOCAL accumulator
        // inside a function (the register path, int elements). Heap elements (strings) exercise the
        // element-retain accounting; run under miri to validate refcounts (no UAF / double free).
        let r = run(
            "mut g = [\"a\"];\ng ~= [\"b\"];\nh = g;\ng ~= [\"c\"];\necho g;\necho h;\nfn build(): List<int> {\n    mut acc = [];\n    for i in 0..3 {\n        acc ~= [i];\n    }\n    return acc;\n}\necho build();\n",
        );
        assert_eq!(
            r.stdout,
            "[\"a\", \"b\", \"c\"]\n[\"a\", \"b\"]\n[0, 1, 2]\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn record_update_reuse_paths() {
        // VM-side record-update reuse (`acc = T { ...acc, … }`). Covers the RUNTIME-checked
        // `Op::MakeStructInPlace` paths reached via a GLOBAL accumulator (`TakeGlobal` exposes the
        // taken-out value's uniqueness; Phase 5.1b): (1) the in-place hit — a global whose update
        // overwrites a field, with a HEAP field (`tag`) whose reference must transfer untouched across
        // the reuse; (2) the copy fallback — an aliased accumulator (`snap = acc`) must keep `snap` at
        // the pre-update value (the runtime refcount > 1 forces the copy). Heap fields exercise the
        // slot retain/release accounting; run under miri to validate refcounts (no UAF/double free).
        let r = run(
            "class Point {\n  x: int\n  tag: string\n  fn show(): string { return \"${self.x} ${self.tag}\"; }\n}\nmut acc = Point { x: -1, tag: \"k\" };\nfor i in 0..4 {\n  acc = Point { ...acc, x: i };\n}\necho acc.show();\nmut p = Point { x: 1, tag: \"a\" };\nsnap = p;\np = Point { ...p, x: 9 };\necho p.show();\necho snap.show();\n",
        );
        assert_eq!(r.stdout, "3 k\n9 a\n1 a\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn map_update_reuse_paths() {
        // VM-side in-place map update (`m[k] = v` ⟶ `m = m.set(k, v)`; Phase 5.1c). Covers the two
        // runtime paths of a reuse-marked local map self-update: (1) the in-place hit — a uniquely-owned
        // accumulator mutated in place, including overwriting a key (its displaced HEAP value released)
        // and removing one; (2) the copy fallback — an aliased accumulator (`snap = m`) must keep `snap`
        // at the pre-update value. String values exercise the slot retain/release accounting; run under
        // miri to validate refcounts (no UAF / double free).
        let r = run(
            "fn build(): string {\n  mut m = {};\n  for i in 0..3 { m[\"k${i}\"] = \"v${i}\"; }\n  m[\"k0\"] = \"x\";\n  m = m.remove(\"k1\");\n  return \"${m.values()} ${m.len()}\";\n}\necho build();\nmut acc = { \"a\": \"1\" };\nsnap = acc;\nacc[\"a\"] = \"9\";\nacc[\"b\"] = \"2\";\necho acc.values();\necho snap.values();\n",
        );
        assert_eq!(r.stdout, "[\"x\", \"v2\"] 2\n[\"9\", \"2\"]\n[\"1\"]\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn list_set_reuse_paths() {
        // VM-side in-place list `set` (`xs[i] = v` ⟶ `xs = xs.set(i, v)`). Covers the in-place hit — a
        // function-local accumulator overwrites each slot in place, its displaced HEAP element released
        // each step — and the copy fallback — an aliased accumulator (`snap = ys`) keeps its value.
        // String elements exercise the slot retain/release accounting; run under miri (no UAF / double
        // free).
        let r = run(
            "fn build(): string {\n  mut xs = [\"a\", \"b\", \"c\"];\n  for i in 0..3 { xs[i] = \"v${i}\"; }\n  return xs.join(\",\");\n}\necho build();\nmut ys = [\"x\", \"y\"];\nsnap = ys;\nys[0] = \"z\";\necho ys.join(\",\");\necho snap.join(\",\");\n",
        );
        assert_eq!(r.stdout, "v0,v1,v2\nz,y\nx,y\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn set_update_reuse_paths() {
        // VM-side in-place set update (`s = s.add(x)` / `s = s.remove(x)`). Covers the in-place hit —
        // a function-local accumulator binary-search-inserts/removes one element in its existing
        // canonical buffer, including a duplicate `add` (a no-op) and a `remove` — and the copy
        // fallback — an aliased accumulator (`snap = t`) keeps its value. String elements exercise the
        // element retain/release accounting; run under miri (no UAF / double free).
        let r = run(
            "fn build(): string {\n  mut s = #{};\n  for i in 0..3 { s = s.add(\"v${i}\"); }\n  s = s.add(\"v0\");\n  s = s.remove(\"v1\");\n  return \"${s.len()}\";\n}\necho build();\nmut t = #{\"a\", \"b\"};\nsnap = t;\nt = t.add(\"c\");\nt = t.remove(\"a\");\necho t;\necho snap;\n",
        );
        assert_eq!(r.stdout, "2\n{\"b\", \"c\"}\n{\"a\", \"b\"}\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn mut_field_set_reuse_paths() {
        // VM-side in-place `mut` field assignment on a value `struct` (`x.f = v`, copy-on-write).
        // Covers the in-place hit — a function-local accumulator overwrites its `mut` fields each
        // iteration (its displaced HEAP field, a string, released each step) — and the copy fallback
        // — an aliased snapshot (`snap = p`) keeps its value because the shared struct is copied
        // before the write. The string field exercises the slot retain/release accounting; run under
        // miri (no UAF / double free).
        let r = run(
            "struct Box {\n  mut tag: string\n  mut n: int\n  fn new(): Box { return Box { tag: \"init\", n: 0 }; }\n}\nfn build(): string {\n  mut b = Box.new();\n  for i in 0..3 { b.n = b.n + i; b.tag = \"t${i}\"; }\n  return \"${b.tag} ${b.n}\";\n}\necho build();\nmut p = Box.new();\nsnap = p;\np.tag = \"changed\";\necho p.tag;\necho snap.tag;\n",
        );
        assert_eq!(r.stdout, "t2 3\nchanged\ninit\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn class_field_set_is_reference_semantic() {
        // VM-side in-place `mut` field assignment on a reference `class` (object-model slice 2b):
        // the instance is mutated in place even when **aliased**, so a snapshot taken beforehand
        // (`snap = p`) observes the change (`snap.tag` → "changed", unlike the struct copy fallback).
        // The displaced HEAP string is released on each overwrite; run under miri (no UAF / double
        // free) to validate the in-place-while-shared retain/release accounting.
        let r = run(
            "class Box {\n  mut tag: string\n  mut n: int\n  fn new(): Box { return Box { tag: \"init\", n: 0 }; }\n}\nfn build(): string {\n  mut b = Box.new();\n  for i in 0..3 { b.n = b.n + i; b.tag = \"t${i}\"; }\n  return \"${b.tag} ${b.n}\";\n}\necho build();\nmut p = Box.new();\nsnap = p;\np.tag = \"changed\";\necho p.tag;\necho snap.tag;\n",
        );
        assert_eq!(r.stdout, "t2 3\nchanged\nchanged\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn reference_cycle_is_collected_at_exit() {
        // VM-side reference-`class` cycle (object-model slice 2c): `a.next = b; b.next = a` ties a
        // cycle precise refcounting cannot reclaim. The exit-time backup `collect_trace(&[])` reclaims
        // both members and runs each `destruct` in reverse-creation order (newest-first). Run under
        // miri to validate the cycle's `gc_free_shallow` reclamation (no UAF / double free) and the
        // leak oracle to confirm residency 0.
        let before = noeta_value::live_count();
        let r = run(
            "class Node {\n  mut next: ?Node\n  id: int\n  fn new(id: int): Node { return Node { next: none, id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}\na = Node.new(1);\nb = Node.new(2);\na.next = some(b);\nb.next = some(a);\necho \"linked\";\n",
        );
        assert_eq!(r.stdout, "linked\ndrop 2\ndrop 1\n");
        assert_eq!(r.exit_code, 0);
        assert_eq!(
            noeta_value::live_count(),
            before,
            "cycle must leave no residency"
        );
    }

    #[test]
    fn record_reassign_reuse_paths() {
        // VM-side whole-value struct reassignment reuse (`p = P { … }`, no spread; Phase 5 general
        // reassignment). The reuse pass injects a `...p` spread (a struct literal sets every field, so
        // it is value-identical), so this lowers to `MakeStructInPlace` overwriting *all* slots — the
        // in-place hit reuses `p`'s cell across the loop (its displaced HEAP field `tag` released each
        // step), while an aliased reassignment (`snap = q`) copies to preserve `snap`. Run under miri to
        // validate the all-slot overwrite's retain/release accounting (no UAF / double free).
        let r = run(
            "class P {\n  n: int\n  tag: string\n  fn show(): string { return \"${self.n} ${self.tag}\"; }\n}\nfn build(): string {\n  mut p = P { n: 0, tag: \"a\" };\n  for i in 0..3 { p = P { n: i, tag: \"t${i}\" }; }\n  return p.show();\n}\necho build();\nmut q = P { n: 1, tag: \"x\" };\nsnap = q;\nq = P { n: 9, tag: \"y\" };\necho q.show();\necho snap.show();\n",
        );
        assert_eq!(r.stdout, "2 t2\n9 y\n1 x\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn record_update_reuse_with_self_read() {
        // Drop insertion (Step B): a self-update that *reads* the accumulator
        // (`acc = Point { ...acc, x: acc.x + 1 }`) reuses in place — the `Drop` after the `acc.x`
        // `LoadField` frees the receiver temporary, restoring unique ownership before the construct.
        // Covers a LOCAL accumulator (Step A: no declaration `Move`) inside a function with a HEAP
        // field carried across each in-place update. Run under miri to validate the `Drop` does not
        // double-free the receiver and the carried heap field's refcount stays balanced.
        let r = run(
            "class Point {\n  x: int\n  label: string\n  fn show(): string { return \"${self.x} ${self.label}\"; }\n}\nfn run(n: int): string {\n  mut acc = Point { x: 0, label: \"p\" };\n  for i in 0..n {\n    acc = Point { ...acc, x: acc.x + 2 };\n  }\n  return acc.show();\n}\necho run(5);\n",
        );
        assert_eq!(r.stdout, "10 p\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn in_place_reuse_fires_replaced_field_destructor() {
        // Phase 5.1a: a function-local self-update of a destructor-free `Box` reuses in place, but the
        // *replaced* field `r` (a destructor-bearing `Res`) must run its `destruct` at the update via
        // the in-place path's `replace_slot` + `release_value`. Run under miri to validate the
        // displaced field is released exactly once (no UAF / double-free) and the carried field `n`
        // stays balanced.
        let r = run(
            "class Res {\n  id: int\n  fn new(id: int): Res { return Res { id: id }; }\n  destruct { echo \"drop ${self.id}\"; }\n}\nclass Box {\n  r: Res\n  n: int\n}\nfn run(): void {\n  mut acc = Box { r: Res.new(0), n: 7 };\n  acc = Box { ...acc, r: Res.new(1) };\n  echo \"n=${acc.n}\";\n}\nrun();\n",
        );
        assert_eq!(r.stdout, "drop 0\nn=7\ndrop 1\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn heap_element_list_concat_refcounts() {
        // Probe: concatenating lists of HEAP elements (strings) must keep element refcounts
        // balanced (no UAF / double free at teardown). Run under miri to validate.
        let r = run(
            "mut acc = [\"a\", \"b\"];\nacc = acc ~ [\"c\"];\nacc ~= [\"d\"];\nb = acc;\nacc ~= [\"e\"];\necho acc;\necho b;\n",
        );
        assert_eq!(
            r.stdout,
            "[\"a\", \"b\", \"c\", \"d\", \"e\"]\n[\"a\", \"b\", \"c\", \"d\"]\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn integer_wrapping_matches_i64() {
        let r = run("echo 9223372036854775807 + 1;\necho 9223372036854775807 * 2;\n");
        assert_eq!(r.stdout, "-9223372036854775808\n-2\n");
    }

    #[test]
    fn mutable_reassignment() {
        let r = run("mut total = 0;\ntotal = total + 5;\necho total;\n");
        assert_eq!(r.stdout, "5\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn immutable_reassignment_is_e0006() {
        let r = run("name = \"a\";\nname = \"b\";\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::ImmutableAssignment
        );
    }

    #[test]
    fn functions_calls_and_nested_calls() {
        let r = run(
            "fn add(a, b) { return a + b; }\nfn dbl(n) { return n * 2; }\nfn quad(n) { return dbl(dbl(n)); }\necho add(2, 3);\necho quad(3);\n",
        );
        assert_eq!(r.stdout, "5\n12\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn recursion_through_globals() {
        let r = run(
            "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(10);\n",
        );
        assert_eq!(r.stdout, "55\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn closure_captures_global() {
        let r = run("base = 100;\nadd_base = fn(x) => x + base;\necho add_base(5);\n");
        assert_eq!(r.stdout, "105\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn pipeline_threads_first_argument() {
        let r = run(
            "fn inc(n) { return n + 1; }\nfn add(a, b) { return a + b; }\necho 5 |> inc |> inc;\necho 5 |> add(10);\n",
        );
        assert_eq!(r.stdout, "7\n15\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn parameter_shadows_global() {
        let r = run("base = 100;\nfn f(base) { return base; }\necho f(5);\necho base;\n");
        assert_eq!(r.stdout, "5\n100\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn arity_mismatch_is_type_error() {
        let r = run("fn add(a, b) { return a + b; }\necho add(1);\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn implicit_unit_return_displays_empty() {
        // A function with no `return` yields unit, which echoes as an empty line (M0 parity).
        let r = run("fn noop(x) { x + 1; }\necho noop(5);\n");
        assert_eq!(r.stdout, "\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn short_circuit_logic() {
        // `false && <error>` short-circuits to false without evaluating the right side.
        assert_eq!(run("echo false && 1 < 2;\n").stdout, "false\n");
        assert_eq!(run("echo true || 1 < 2;\n").stdout, "true\n");
        assert_eq!(run("echo 1 < 2 && 3 >= 3;\n").stdout, "true\n");
    }

    #[test]
    fn division_by_zero_is_e0008() {
        let r = run("echo 1 / 0;\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::DivisionByZero
        );
    }

    #[test]
    fn unknown_name_is_e0005() {
        let r = run("echo missing;\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::UnknownName
        );
    }

    #[test]
    fn destructors_run_at_program_end_in_reverse_declaration_order() {
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\na = R.new(\"a\");\nb = R.new(\"b\");\necho \"body\";\n",
        );
        // Globals destroyed in reverse declaration order: b before a.
        assert_eq!(r.stdout, "body\nclose b\nclose a\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn destructor_fires_at_a_locals_last_use_not_at_program_end() {
        // Phase 4: a destructor-bearing function **local** runs its `destruct` at its last use —
        // here the `r.announce()` call — before the function returns, not deferred to program end.
        // The bare `compile` path marks every drop conservatively relevant, so the local's
        // `Op::Drop` routes through `release_value` and fires the destructor.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn announce(): void { echo \"here ${self.name}\"; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn scope(): void {\n  r = R.new(\"x\");\n  r.announce();\n  echo \"after\";\n}\necho \"start\";\nscope();\necho \"end\";\n",
        );
        // `r`'s last use is `r.announce()`; the destructor fires right after it returns, before
        // "after" — and definitely before program end ("end").
        assert_eq!(r.stdout, "start\nhere x\nclose x\nafter\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn reassigning_a_binding_destroys_the_displaced_value() {
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\nmut x = R.new(\"first\");\nx = R.new(\"second\");\necho \"mid\";\n",
        );
        // "first" is destroyed at the reassignment; "second" at program end.
        assert_eq!(r.stdout, "close first\nmid\nclose second\n");
    }

    #[test]
    fn reassigning_a_local_destroys_displaced_then_survivor_at_scope_exit() {
        // Phase 4.2a: a reassigned **local** (not a global) destroys its displaced value at the
        // assignment via the `Op::Drop` the compiler emits before the overwriting `Op::Move`
        // (`set_reg`'s plain release would not fire the destructor), and its surviving value via the
        // function-body scope-exit drop. "first" closes between the two reads; "second" before return.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn use_it(): void { echo \"use ${self.name}\"; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn go(): void {\n  mut r = R.new(\"first\");\n  r.use_it();\n  r = R.new(\"second\");\n  r.use_it();\n}\necho \"start\";\ngo();\necho \"end\";\n",
        );
        assert_eq!(
            r.stdout,
            "start\nuse first\nclose first\nuse second\nclose second\nend\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn question_mark_propagation_destroys_abandoned_locals() {
        // Phase 4.2c: a `?` that early-returns an `Err` destroys the frame locals it abandons before
        // unwinding (the `on_error` drops the compiler attaches to `Op::TryUnwrap`). `r` is live past
        // the `?`, so `close r` fires on the error path, before the caller prints the propagated Err.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn check(c: bool): Result<int, string> {\n  if c { return Ok(1); }\n  return Err(\"bad\");\n}\nfn go(c: bool): Result<int, string> {\n  r = R.new(\"r\");\n  x = check(c)?;\n  return Ok(x);\n}\necho \"start\";\necho go(false);\necho \"end\";\n",
        );
        assert_eq!(r.stdout, "start\nclose r\nErr(bad)\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn panic_destroys_live_frame_locals_in_reverse_construction_order() {
        // Phase 4.2c-ii: as a panic aborts, the VM's per-frame teardown fires the `destruct` of each
        // live destructor-bearing frame local (the `frame_locals` list reversed), so `a` and `b` are
        // destroyed — `b` before `a` — before the program exits 1. They are never read, so they live
        // undropped to the panic; the panic-aware `coalesce` pinning keeps them in distinct registers.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  destruct { echo \"close ${self.name}\"; }\n}\nfn go(): void {\n  a = R.new(\"a\");\n  b = R.new(\"b\");\n  echo \"made\";\n  panic(\"boom\");\n}\necho \"start\";\ngo();\n",
        );
        assert_eq!(r.stdout, "start\nmade\nclose b\nclose a\n");
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn destroying_a_container_runs_its_destructor_then_its_fields_in_declared_order() {
        // Phase 4.3 (spec §4): destroying an object runs the container's own `destruct` first (its
        // fields still live), then releases its fields depth-first in declared order, each firing its
        // own `destruct`. `Outer`'s two destructor-bearing `Leaf` fields are built inline (so the
        // struct holds the sole reference — the construction-temp release makes refcount 1 here), and
        // `o` is a dead-store dropped at scope exit: `outer`, then `a`, then `b` (declared order).
        let r = run(
            "class Leaf {\n  tag: string\n  fn new(tag: string): Leaf { return Leaf { tag: tag }; }\n  destruct { echo \"drop ${self.tag}\"; }\n}\nclass Outer {\n  label: string\n  a: Leaf\n  b: Leaf\n  fn new(): Outer { return Outer { label: \"o\", a: Leaf.new(\"a\"), b: Leaf.new(\"b\") }; }\n  destruct { echo \"drop outer ${self.label}\"; }\n}\nfn go(): void {\n  o = Outer.new();\n  echo \"built\";\n}\necho \"start\";\ngo();\necho \"end\";\n",
        );
        assert_eq!(
            r.stdout,
            "start\nbuilt\ndrop outer o\ndrop a\ndrop b\nend\n"
        );
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn destroying_a_list_runs_its_elements_destructors_in_order() {
        // Phase 4.3 (spec §4): a collection releases its elements in iteration order. The list has no
        // `destruct`; its contained `Leaf`s do, and fire a, b, c (index order) when the list dies. The
        // construction-temp releases make the list the sole owner, so each element is at refcount 1.
        let r = run(
            "class Leaf {\n  tag: string\n  fn new(tag: string): Leaf { return Leaf { tag: tag }; }\n  destruct { echo \"drop ${self.tag}\"; }\n}\nfn go(): void {\n  items = [Leaf.new(\"a\"), Leaf.new(\"b\"), Leaf.new(\"c\")];\n  echo \"built\";\n}\necho \"start\";\ngo();\necho \"end\";\n",
        );
        assert_eq!(r.stdout, "start\nbuilt\ndrop a\ndrop b\ndrop c\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn a_temp_used_only_as_a_receiver_fires_its_destructor() {
        // Phase 4.4 (spec §2): a destructor-bearing value used only as a method receiver, or
        // discarded as a bare statement, still fires at last use — a temp is an owner. `R.new("a")`
        // is consumed by `.use_it()` (fires after the call); `R.new("b");` is discarded (fires at the
        // statement). The compiler emits a destructor-aware `Op::Drop` of the receiver / discarded
        // register where there was none before.
        let r = run(
            "class R {\n  name: string\n  fn new(name: string): R { return R { name: name }; }\n  fn use_it(): void { echo \"use ${self.name}\"; }\n  destruct { echo \"close ${self.name}\"; }\n}\necho \"start\";\nR.new(\"a\").use_it();\nR.new(\"b\");\necho \"end\";\n",
        );
        assert_eq!(r.stdout, "start\nuse a\nclose a\nclose b\nend\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn a_class_without_a_destructor_runs_nothing() {
        let r = run(
            "class R {\n  v: int\n  fn new(v: int): R { return R { v: v }; }\n}\nx = R.new(1);\necho \"done\";\n",
        );
        assert_eq!(r.stdout, "done\n");
    }

    #[test]
    fn record_literal_field_access_and_structural_equality() {
        let r = run(
            "struct Item { price: float qty: int }\na = Item { price: 2.5, qty: 4 };\necho a.price;\necho a.price * a.qty;\nb = Item { price: 2.5, qty: 4 };\necho a == b;\n",
        );
        assert_eq!(r.stdout, "2.5\n10.0\ntrue\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn object_displays_as_a_literal() {
        let r = run("struct Pt { x: int y: int }\necho Pt { x: 1, y: 2 };\n");
        assert_eq!(r.stdout, "Pt {x: 1, y: 2}\n");
    }

    #[test]
    fn missing_field_is_e0009() {
        let r = run("struct P { x: int y: int }\np = P { x: 1 };\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::MissingField
        );
    }

    #[test]
    fn class_constructor_method_and_field_access() {
        let r = run(
            "class Box {\n  v: int\n  fn new(v: int): Box { return Box { v: v }; }\n  fn doubled(): int { return self.v * 2; }\n}\nb = Box.new(21);\necho b.doubled();\necho b.v;\n",
        );
        assert_eq!(r.stdout, "42\n21\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn method_takes_arguments_alongside_fields() {
        let r = run(
            "class Counter {\n  base: int\n  fn new(base: int): Counter { return Counter { base: base }; }\n  fn plus(n: int): int { return self.base + n; }\n}\nc = Counter.new(10);\necho c.plus(5);\n",
        );
        assert_eq!(r.stdout, "15\n");
    }

    #[test]
    fn structural_update_overrides_one_field() {
        let r = run(
            "class M {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): M { return M { amount: a, currency: c }; }\n}\na = M.new(500, \"USD\");\nb = M { amount: 300, ...a };\necho b.amount;\necho b.currency;\necho a.amount;\n",
        );
        assert_eq!(r.stdout, "300\nUSD\n500\n");
    }

    #[test]
    fn operator_trait_overloads_plus() {
        // `a + b` on a class implementing `Add` dispatches to its `add` method (M1.8).
        let r = run(
            "class Money {\n  amount: int\n  currency: string\n  fn new(a: int, c: string): Money { return Money { amount: a, currency: c }; }\n  impl Add {\n    fn add(other: Money): Money { return Money { amount: self.amount + other.amount, currency: self.currency }; }\n  }\n}\na = Money.new(5, \"USD\");\nb = Money.new(3, \"USD\");\nt = a + b;\necho t.amount;\necho t.currency;\n",
        );
        assert_eq!(r.stdout, "8\nUSD\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn operators_on_builtins_are_unaffected_by_overloads() {
        // A class without the relevant trait method leaves built-in `+` semantics untouched.
        let r = run("echo 2 + 3;\necho \"a\" ~ \"b\";\n");
        assert_eq!(r.stdout, "5\nab\n");
    }

    #[test]
    fn equatable_overrides_equality_and_negates_for_ne() {
        // `impl Equatable` routes `==`/`!=` to `eq`; `eq` here ignores `tag`, and `!=` negates the
        // returned bool through the frame's return transform.
        let r = run(
            "class M {\n  amount: int\n  tag: int\n  fn new(a: int, t: int): M { return M { amount: a, tag: t }; }\n  impl Equatable {\n    fn eq(other: M): bool { return self.amount == other.amount; }\n  }\n}\na = M.new(5, 1);\nb = M.new(5, 2);\necho a == b;\necho a != b;\necho a == M.new(9, 1);\n",
        );
        assert_eq!(r.stdout, "true\nfalse\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn comparable_overloads_ordering_operators() {
        // `impl Comparable` routes `< <= > >=` to `compare`; the returned `Ordering` is mapped to
        // each operator's bool via the frame's return transform.
        let r = run(
            "class M {\n  amount: int\n  fn new(a: int): M { return M { amount: a }; }\n  impl Comparable {\n    fn compare(other: M): Ordering { return self.amount.compare(other.amount); }\n  }\n}\na = M.new(5);\nb = M.new(8);\necho a < b;\necho a > b;\necho a <= b;\necho a >= b;\n",
        );
        assert_eq!(r.stdout, "true\nfalse\ntrue\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn primitive_compare_yields_ordering() {
        let r = run("echo 1.compare(2);\necho 5.compare(5);\necho 9.compare(2);\n");
        assert_eq!(
            r.stdout,
            "Ordering.Less\nOrdering.Equal\nOrdering.Greater\n"
        );
    }

    #[test]
    fn derive_comparable_orders_fields_lexicographically() {
        // `@derive(Comparable)` gives structural ordering via the Module's comparable set + the
        // VM's `structural_compare`; no method is called.
        let r = run(
            "@derive(Comparable)\nclass P {\n  x: int\n  y: int\n  fn new(x: int, y: int): P { return P { x: x, y: y }; }\n}\na = P.new(1, 2);\nb = P.new(1, 5);\nc = P.new(1, 2);\necho a < b;\necho a > b;\necho a <= c;\necho a >= c;\n",
        );
        assert_eq!(r.stdout, "true\nfalse\ntrue\ntrue\n");
    }

    #[test]
    fn comparison_on_non_comparable_object_errors() {
        let r = run(
            "class P {\n  x: int\n  fn new(x: int): P { return P { x: x }; }\n}\necho P.new(1) < P.new(2);\n",
        );
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn index_list_by_position() {
        // List element access retains the element (refcount discipline checked under miri).
        let r = run("xs = [\"a\", \"b\", \"c\"];\necho xs[1];\necho [10, 20][0];\n");
        assert_eq!(r.stdout, "b\n10\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn index_out_of_bounds_is_e0016() {
        let r = run("xs = [1, 2];\necho xs[5];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::IndexOutOfBounds
        );
    }

    #[test]
    fn index_dispatches_to_index_trait() {
        // `inv[i]` routes to the class's `Index::get`, pushing a call frame `[recv, index]`.
        let r = run(
            "class Inv {\n  items: list\n  fn new(items: list): Inv { return Inv { items: items }; }\n  impl Index {\n    fn get(i: int): int { return self.items[i]; }\n  }\n}\necho Inv.new([7, 8, 9])[2];\n",
        );
        assert_eq!(r.stdout, "9\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn indexing_a_non_indexable_is_type_error() {
        let r = run("echo 42[0];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn index_map_by_key() {
        // Map element access by string key retains the value (refcount discipline under miri).
        let r = run("m = {\"a\": \"x\", \"b\": \"y\"};\necho m[\"b\"];\n");
        assert_eq!(r.stdout, "y\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn index_map_missing_key_is_e0018() {
        let r = run("m = {\"a\": 1};\necho m[\"z\"];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::KeyNotFound
        );
    }

    #[test]
    fn index_string_by_position() {
        let r = run("s = \"hello\";\necho s[0];\necho s[4];\n");
        assert_eq!(r.stdout, "h\no\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn index_string_out_of_bounds_is_e0016() {
        let r = run("s = \"hi\";\necho s[5];\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::IndexOutOfBounds
        );
    }

    #[test]
    fn len_dispatches_to_length_trait() {
        // `len(o)` routes to the class's `Length::len`, pushing a receiver-only call frame.
        let r = run(
            "class Stack {\n  items: list\n  fn new(items: list): Stack { return Stack { items: items }; }\n  impl Length {\n    fn len(): int { return self.items.len(); }\n  }\n}\necho Stack.new([1, 2, 3]).len();\n",
        );
        assert_eq!(r.stdout, "3\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn echo_dispatches_to_display_trait() {
        // `echo o` and `"{o}"` route to the class's `Display::to_string` (the `Stringify` op).
        let r = run(
            "class P {\n  n: int\n  fn new(n: int): P { return P { n: n }; }\n  impl Display {\n    fn to_string(): string { return \"P#${self.n}\"; }\n  }\n}\np = P.new(7);\necho p;\necho \"it is ${p}\";\n",
        );
        assert_eq!(r.stdout, "P#7\nit is P#7\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn tuple_construct_project_and_equality() {
        // Object-model slice 4: build a tuple (`MakeTuple`), project positions (`TupleIndex`,
        // including a nested `.0.1`), and compare structurally. Mirrors the tree-walker (the
        // differential oracle guards the agreement).
        let r = run(
            "p = (1, \"two\", 3.0);\necho p;\necho p.1;\nn = ((1, 2), (3, 4));\necho n.1.0;\necho p == (1, \"two\", 3.0);\necho p == (1, \"two\", 4.0);\n",
        );
        assert_eq!(r.stdout, "(1, \"two\", 3.0)\ntwo\n3\ntrue\nfalse\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn match_tuple_patterns() {
        // Object-model slice 4b.2: refutable tuple patterns in `match` — literal, binding, wildcard,
        // and nested tuple positions all compose (the `MatchTuple` test + `TupleIndex` extraction).
        let r = run(
            "fn f(p: (int, int)): string { return match p { (0, 0) => \"o\", (0, y) => \"y${y}\", (x, _) => \"x${x}\" }; }\necho f((0, 0));\necho f((0, 7));\necho f((3, 9));\necho match (1, (\"a\", true)) { (n, (s, b)) => \"${n}/${s}/${b}\" };\n",
        );
        assert_eq!(r.stdout, "o\ny7\nx3\n1/a/true\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn enum_method_and_impl_dispatch() {
        // Object-model slice 3: an enum's unified body. An instance method (`label`) takes the whole
        // value as `self`; `echo`/`${}` route to an `impl Display { to_string }`; and `==` routes to
        // an `impl Equatable { eq }` — all through the same `(type, method)` table an object uses.
        let r = run(
            "enum Color {\n  Red;\n  Green;\n  fn label(): string { return match self { Color.Red => \"r\", Color.Green => \"g\" }; }\n  impl Display { fn to_string(): string { return \"<${self.label()}>\"; } }\n  impl Equatable { fn eq(other: Color): bool { return true; } }\n}\necho Color.Red.label();\necho Color.Red;\necho Color.Red == Color.Green;\n",
        );
        assert_eq!(r.stdout, "r\n<r>\ntrue\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn derived_to_json_serializes_structurally() {
        // `@derive(Serialize<Json>)` synthesizes `to_json`: fields in declared order, strings
        // escaped, nested objects recursed — computed inline (no call frame).
        let r = run(
            "@derive(Serialize<Json>)\nclass U {\n  name: string\n  id: int\n  fn new(name: string, id: int): U { return U { name: name, id: id }; }\n}\necho U.new(\"Ada\", 7).to_json();\n",
        );
        assert_eq!(r.stdout, "{\"name\":\"Ada\",\"id\":7}\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn for_dispatches_to_iterable_trait() {
        // `for x in o` routes to the class's `Iterable::iter`, iterating its returned list.
        let r = run(
            "class Bag {\n  items: list\n  fn new(items: list): Bag { return Bag { items: items }; }\n  impl Iterable {\n    fn iter(): list { return self.items; }\n  }\n}\nmut total = 0;\nfor x in Bag.new([1, 2, 3]) { total = total + x; }\necho total;\n",
        );
        assert_eq!(r.stdout, "6\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn iterable_returning_non_list_is_e0007() {
        let r = run(
            "class B {\n  x: int\n  fn new(): B { return B { x: 1 }; }\n  impl Iterable { fn iter(): int { return 5; } }\n}\nfor v in B.new() { echo v; }\n",
        );
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn object_without_display_uses_structural_render() {
        // No `Display` impl ⇒ the `Stringify` op is identity and the structural form prints.
        let r = run(
            "class P {\n  n: int\n  fn new(n: int): P { return P { n: n }; }\n}\necho P.new(7);\n",
        );
        assert_eq!(r.stdout, "P {n: 7}\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn plain_enum_construction_and_equality() {
        let r = run("enum S { A; B; }\necho S.A == S.A;\necho S.A == S.B;\n");
        assert_eq!(r.stdout, "true\nfalse\n");
    }

    #[test]
    fn opaque_use_stub_constructs_and_reads_fields() {
        let r = run(
            "use App.Models.User;\nu = User { name: \"Ada\", id: 7 };\necho u.name;\necho u.id;\necho u;\n",
        );
        // Opaque objects display their fields in sorted-key order (M0 `BTreeMap` parity).
        assert_eq!(r.stdout, "Ada\n7\nUser {id: 7, name: \"Ada\"}\n");
    }

    #[test]
    fn match_over_enums_binds_variant_data() {
        let r = run(
            "enum E { Empty; Code(n: int); }\nx = E.Code(42);\necho match x { E.Empty => \"empty\", E.Code(n) => \"code ${n}\" };\n",
        );
        assert_eq!(r.stdout, "code 42\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn match_literals_and_wildcard() {
        let r = run(
            "fn name(n) { return match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" }; }\necho name(0);\necho name(5);\n",
        );
        assert_eq!(r.stdout, "zero\nmany\n");
    }

    #[test]
    fn unmatched_value_is_a_runtime_error() {
        let r = run("enum E { A; B; C; }\necho match E.C { E.A => 1, E.B => 2 };\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn result_constructors_display_bare() {
        let r = run("echo Ok(5);\necho Err(\"boom\");\necho some(3);\necho none;\necho Ok();\n");
        assert_eq!(r.stdout, "Ok(5)\nErr(boom)\nsome(3)\nnone\nOk\n");
    }

    #[test]
    fn question_propagates_err_and_unwraps_ok() {
        assert_eq!(
            run("fn validate(): int { return Err(\"empty\"); }\nfn run_it(): int { validate()?; return Ok(\"done\"); }\necho run_it();\n").stdout,
            "Err(empty)\n"
        );
        assert_eq!(
            run("fn ok_val(): int { return Ok(41); }\nfn use_it(): int { return Ok(ok_val()? + 1); }\necho use_it();\n").stdout,
            "Ok(42)\n"
        );
    }

    #[test]
    fn coalesce_supplies_a_default() {
        let r =
            run("echo none ?? 99;\necho some(7) ?? 99;\necho Err(\"x\") ?? 0;\necho Ok(5) ?? 0;\n");
        assert_eq!(r.stdout, "99\n7\n0\n5\n");
    }

    #[test]
    fn panic_aborts_with_e0010_keeping_prior_output() {
        let r = run("echo \"before\";\npanic(\"boom\");\necho \"after\";\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.stdout, "before\n");
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::Panic
        );
    }

    #[test]
    fn next_id_is_a_deterministic_counter() {
        let r = run("use std.id.{next_id}\necho next_id();\necho next_id();\necho next_id();\n");
        assert_eq!(r.stdout, "1\n2\n3\n");
    }

    #[test]
    fn capture_free_closure_inside_a_method_is_supported() {
        // The `fn(it) => it.price * it.qty` closure captures nothing enclosing, so it compiles
        // even though it is defined inside a method (true upvalue capture stays unsupported).
        let r = run(
            "struct Item { price: float qty: int }\nclass Cart {\n  items: List<Item>\n  fn new(items: List<Item>): Cart { return Cart { items: items }; }\n  fn total(): float { return self.items.map(fn(it) => it.price * it.qty).sum(); }\n}\nc = Cart.new([Item { price: 2.5, qty: 4 }, Item { price: 1.0, qty: 3 }]);\necho c.total();\n",
        );
        assert_eq!(r.stdout, "13.0\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn string_interpolation_concatenates_display_forms() {
        let r = run("name = \"Niro\";\necho \"Hello ${name}\";\necho \"sum is ${1 + 2 * 3}\";\n");
        assert_eq!(r.stdout, "Hello Niro\nsum is 7\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn list_literals_display_with_repr() {
        let r = run("echo [1, 2, 3];\necho [\"a\", \"b\"];\necho [];\n");
        assert_eq!(r.stdout, "[1, 2, 3]\n[\"a\", \"b\"]\n[]\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn maps_display_in_sorted_key_order() {
        let r = run("echo {\"b\": 2, \"a\": 1};\necho {\"a\": 1, \"b\": 2}.len();\n");
        assert_eq!(r.stdout, "{\"a\": 1, \"b\": 2}\n2\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn len_over_list_map_and_string() {
        let r = run(
            "echo [1, 2, 3].len();\necho {\"a\": 1}.len();\necho \"héllo\".len();\necho [].len();\n",
        );
        assert_eq!(r.stdout, "3\n1\n5\n0\n");
    }

    #[test]
    fn filter_map_sum_pipeline() {
        let r = run(
            "echo [1, 2, 3, 4].filter(fn(n) => n % 2 == 0).map(fn(n) => n * 10).sum();\n",
        );
        assert_eq!(r.stdout, "60\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn sum_promotes_to_float_when_any_element_is_float() {
        assert_eq!(run("echo [1, 2, 3].sum();\n").stdout, "6\n");
        assert_eq!(run("echo [1, 2.5, 3].sum();\n").stdout, "6.5\n");
        assert_eq!(run("echo [].sum();\n").stdout, "0\n");
    }

    #[test]
    fn for_over_list_accumulates_into_a_global() {
        let r =
            run("mut total = 0;\nfor n in [1, 2, 3, 4] {\n  total = total + n;\n}\necho total;\n");
        assert_eq!(r.stdout, "10\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn for_over_empty_list_runs_no_iterations() {
        let r = run("for x in [] { echo \"never\"; }\necho \"done\";\n");
        assert_eq!(r.stdout, "done\n");
    }

    #[test]
    fn for_pair_destructures_enumerate() {
        let r = run("for (i, x) in [\"a\", \"b\"].enumerate() {\n  echo i ~ \":\" ~ x;\n}\n");
        assert_eq!(r.stdout, "0:a\n1:b\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn for_over_map_iterates_values_in_key_order() {
        let r = run(
            "mut total = 0;\nfor v in {\"b\": 20, \"a\": 1} {\n  total = total + v;\n}\necho total;\n",
        );
        assert_eq!(r.stdout, "21\n");
    }

    #[test]
    fn iterating_a_non_collection_is_a_type_error() {
        let r = run("for x in 42 { echo x; }\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn len_of_an_int_is_an_unknown_method() {
        // `len` is a collection method (P1.2), so on an int it is an unknown method (E0005) — the
        // same error every other unknown method raises (the old free `len(42)` was a TypeMismatch).
        let r = run("echo (42).len();\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::UnknownName
        );
    }

    #[test]
    fn map_closure_error_propagates_and_frees() {
        // The closure divides by zero on the second element: the error must surface and the
        // partially-built result list must be freed (miri verifies no leak).
        let r = run("echo [1, 0, 2].map(fn(n) => 10 / n);\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            noeta_diagnostics::DiagnosticCode::DivisionByZero
        );
    }

    #[test]
    fn nested_list_of_lists_round_trips() {
        // Exercises recursive collection freeing through the register/global machinery.
        let r = run("xs = [[1, 2], [3, 4]];\necho xs;\necho xs.len();\n");
        assert_eq!(r.stdout, "[[1, 2], [3, 4]]\n2\n");
    }

    #[test]
    fn disassembly_is_stable() {
        let source = Source::new(SourceId::FIRST, "t.noe", "mut x = 1;\necho x + 2;\n");
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn attribute_manifest_records_decorations() {
        // `#[...]` data attributes (with literal args) are collected into the queryable
        // build manifest, in source order, keyed by the decorated type.
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "#[Entity]\n#[Route(login, post)]\nclass Account {\n  id: int\n  fn new(id: int): Account { return Account { id: id }; }\n}\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let attrs: Vec<_> = module.attributes_for("Account").collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].name, "Entity");
        assert!(attrs[0].args.is_empty());
        assert_eq!(attrs[1].name, "Route");
        let arg_values: Vec<_> = attrs[1].args.iter().map(|a| a.value.clone()).collect();
        assert_eq!(
            arg_values,
            vec![
                noeta_ast::AttrValue::TypeRef("login".to_string()),
                noeta_ast::AttrValue::TypeRef("post".to_string()),
            ]
        );
        // A type with no attributes has no manifest entries.
        assert_eq!(module.attributes_for("Missing").count(), 0);
    }

    #[test]
    fn disassembly_of_a_recursive_function_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(6);\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_for_loop_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "mut total = 0;\nfor n in [1, 2, 3] {\n  total = total + n;\n}\necho total;\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_the_object_model_is_stable() {
        // A struct literal, a class with a constructor + an instance method (showing the
        // shape and method tables, field loads, and enum construction).
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "enum Status { Pending; Paid; }\nclass Order {\n  id: int\n  mut status: Status\n  fn new(id: int): Order { return Order { id: id, status: Status.Pending }; }\n  fn tag(): int { return self.id; }\n}\no = Order.new(7);\necho o.tag();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn local_self_update_lowers_to_in_place_record_reuse() {
        // Phase 5.1a: a self-update of a destructor-free type whose accumulator is a directly-held
        // **function-local** must lower to the in-place `MakeStructInPlace` (the reuse pass marks it,
        // the compiler emits it) rather than a copying `MakeStruct` — the proof the reuse token reaches
        // the VM. (A top-level global accumulator is the `TakeGlobal` case — see
        // `global_self_update_lowers_to_take_global_plus_in_place_reuse`.)
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "class P { x: int }\nfn run(): int {\n  mut acc = P { x: 0 };\n  acc = P { ...acc, x: acc.x + 1 };\n  return acc.x;\n}\necho run();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert!(
            disasm.contains("MakeRecIP"),
            "expected an in-place record-reuse op, got:\n{disasm}"
        );
    }

    #[test]
    fn global_self_update_lowers_to_take_global_plus_in_place_reuse() {
        // Phase 5.1b: a top-level (global) struct accumulator's self-update must move the global out
        // with `TakeGlobal` and reuse it in place with `MakeStructInPlace` — not the copying
        // `MakeStruct` the local-only 5.1a path fell back to for a global. Both ops together are the
        // proof the global path is wired.
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "class P { x: int }\nmut acc = P { x: 0 };\nacc = P { ...acc, x: 5 };\necho acc.x;\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert!(
            disasm.contains("TakeGlobal") && disasm.contains("MakeRecIP"),
            "expected TakeGlobal + in-place record reuse for a global accumulator, got:\n{disasm}"
        );
    }

    #[test]
    fn local_map_self_update_lowers_to_reuse_method_call() {
        // Phase 5.1c: a function-local map accumulator updated with `m[k] = v` (desugaring to
        // `m = m.set(k, v)`) must carry the in-place-reuse token to the VM — `CallMethod ... [reuse]` —
        // so the dispatch mutates the uniquely-owned backing map in place rather than copying it. A
        // top-level (global) map accumulator is the `TakeGlobal` case (a later slice; the IR
        // interpreter already reuses it, and reuse is invisible, so the backends still agree).
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "fn build(): Map<string, int> {\n  mut m = {};\n  for i in 0..3 { m[\"k${i}\"] = i; }\n  return m;\n}\necho build().len();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert!(
            disasm.contains("[reuse"),
            "expected a reuse-marked method call for a local map self-update, got:\n{disasm}"
        );
    }

    #[test]
    fn self_append_lowers_to_in_place_concat() {
        // Phase 5.1b: a list self-append `acc ~= rhs` must lower to `ConcatInPlace` — for a global
        // accumulator preceded by `TakeGlobal` (to expose unique ownership), and for a function-local
        // accumulator directly on its register. The proof the concat reuse token reaches the VM rather
        // than the copying `Op::Binary` (`~`).
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "mut g = [\"a\"];\ng ~= [\"b\"];\nfn build(): List<int> {\n  mut acc = [];\n  for i in 0..3 { acc ~= [i]; }\n  return acc;\n}\necho g;\necho build();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        let disasm = module.disassemble();
        assert_eq!(
            disasm.matches("ConcatIP").count(),
            2,
            "expected two in-place concats (global + local), got:\n{disasm}"
        );
        assert!(
            disasm.contains("TakeGlobal"),
            "expected the global self-append to be preceded by TakeGlobal, got:\n{disasm}"
        );
    }

    #[test]
    fn disassembly_of_a_match_decision_tree_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "enum E { Empty; Code(n: int); }\nfn describe(e): string {\n  return match e {\n    E.Empty => \"empty\",\n    E.Code(n) => \"code ${n}\",\n  };\n}\necho describe(E.Code(7));\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_question_propagating_function_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "fn validate(): int { return Err(\"bad\"); }\nfn place(): int { validate()?; return Ok(\"ok\"); }\necho place();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_map_filter_chain_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "echo [1, 2, 3, 4].filter(fn(n) => n % 2 == 0).map(fn(n) => n * 10).sum();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_local_bindings_consumes_temporaries() {
        // Each local declaration's value is a single-use temporary, so the local *adopts* the
        // temporary's register (a consuming move, Phase 3.3b) instead of a retaining `Op::Move` into
        // a fresh slot: the body holds no `Move` between the producing `Add` and the binding, and
        // `registers` stays small. A borrowed source (`y = x`, an aliased live local) still copies.
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "fn build(): int {\n  a = 1 + 2;\n  b = a + 3;\n  return b;\n}\necho build();\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn closure_default_reads_a_captured_cell() {
        // A closure default that references a captured variable the body never otherwise names: the
        // default thunk shares the closure's upvalue layout and reads the captured cell. Exercises
        // the run_thunk upvalue-retain path (miri verifies no leak / double-free).
        let r = run(
            "fn make(tag: string): dyn {\n  return fn(s: string, label: string = tag) => label ~ \":\" ~ s;\n}\nt = make(\"X\");\necho t(\"a\");\necho t(\"a\", \"Y\");\n",
        );
        assert_eq!(r.stdout, "X:a\nY:a\n");
        assert_eq!(r.exit_code, 0);
    }
}
