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
use std::sync::mpsc::Sender;

use noeta_ast::{BinaryOp, ClosureBody, Expr, Param, Program, Stmt};
// `RunResult` is re-exported below (`pub use noeta_backend::{RunResult, …}`), so it is not imported
// privately here (that would be a duplicate binding).
use noeta_bytecode::{
    BoolSide, Builtin, CaptureFrom, Chunk, Const, Module, NarrowTarget, Op, Reg, ReuseCheck,
    StrPart,
};
#[cfg(feature = "compile")]
use noeta_compiler::{Unsupported, compile};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_gc::{collect_trace, release, retain};
use noeta_object::{Shape, ShapeKind};
use noeta_span::Span;

use crate::scheduler::SchedState;
use noeta_value::{
    ChannelId, HeapKind, ScopeId, TaskId, Value, apply_binary, apply_binary_wide, apply_unary,
    compare_primitive, structural_compare,
};

/// Protocol-neutral debug-session support shared by `noeta dap` and `noeta mcp` (breakpoint
/// resolution, line-granular stepping, owned stack capture).
pub mod debug;
mod isolate;
#[cfg(feature = "jit")]
mod jit_service;
mod values;
pub(crate) use values::*;
mod methods;
mod native_ctx;
mod scheduler;
#[cfg(feature = "compile")]
mod session;
#[cfg(feature = "compile")]
pub use session::{CallError, EmbedArg, EmbedHandle, HostFactory, SessionOutput, VmSession};
/// The crate's entry points: [`VmBackend`], the `execute*` drivers, and the JIT report types.
mod backend;
pub use backend::*;
/// The re-entrant call layer (`call_value`, `run_thunk`, closure setup, returns).
mod calls;
/// The tier-0 dispatch loop (`Vm::run` / `Vm::dispatch`) and its register helpers.
mod dispatch;
/// In-run safepoint cycle collection: trigger polling, root enumeration, mid-run reclaim.
mod gc;
pub(crate) use dispatch::*;
/// The observation hooks: [`Debugger`] / [`ProfileHook`] and the debug request vocabulary.
mod hooks;
pub use hooks::*;
/// Hot-swap / console-fragment installation and its [`FragmentCompiler`] seam.
mod hotswap;
pub use hotswap::*;
/// VM lifecycle: load / teardown / destructors / isolate workers.
mod lifecycle;
pub use lifecycle::*;
/// Tier-1 (JIT) runtime glue: helper symbols, trampolines, engine management.
/// (The re-export is feature-gated: without tier-1 the module has no items and
/// a glob over an empty module trips `unused_imports`.)
mod tier1;
#[cfg(feature = "jit")]
pub use tier1::*;
#[cfg(all(feature = "jit-rt", not(feature = "jit")))]
pub(crate) use tier1::*;

/// One activation record: a prototype index, its register file, the program counter, the caller
/// register the return value flows into (irrelevant for the bottom/top-level frame), and an
/// optional transform applied to the return value as it lands in the caller.
#[derive(Debug)]
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
    WrapOk(&'static Shape),
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

/// The debug console's session machinery (tooling-unification T4), present only on a debug run
/// launched with an adopted session: the live incremental compiler (seeded from the launch's
/// *checked* compile, so fragment ids append onto the program's own id-spaces) and the arena that
/// keeps every extended module snapshot alive for the rest of the run. A fragment install
/// ([`Vm::install_fragment`]) compiles through the session and swaps [`Vm::module`] to the arena'd
/// snapshot — old frames keep executing their (prefix-identical) code, new frames resolve
/// fragment protos/names through the newest module, and an escaped fragment closure stays callable
/// after the program resumes.
struct DebugSession<'m> {
    /// The live incremental compiler, behind the [`FragmentCompiler`] seam so this struct — and the
    /// install/eval paths that use it — stay compiler-free (native-size slice 2). The concrete
    /// `noeta_compiler::SessionCompiler` is boxed in here by the (feature-gated) session entry points.
    compiler: Box<dyn FragmentCompiler>,
    arena: &'m typed_arena::Arena<Module>,
    /// Compiled-wrapper memo (tooling-unification U3): `(fragment text, in-scope local names)` →
    /// the installed entry proto. A watch panel re-evaluates its expressions on **every step**;
    /// without this each re-eval would append a fresh proto + global slot to the session for the
    /// rest of the run. A hit skips compile + install entirely and re-runs the existing entry with
    /// fresh values (indices stay valid forever — the module only grows). Only successful compiles
    /// are memoized, and the param names are part of the key, so a hit is exactly a replay.
    memo: HashMap<(String, Vec<String>), u32>,
    /// Watch-result memo (watch-memoization): `(fragment text, frame index)` → the stop generation
    /// it was rendered at and its rendered `(value, type)`. The compiled-wrapper memo above still
    /// re-*runs* the fragment on every render; this one lets an **observational** watch (all
    /// top-level statements are expressions) skip execution entirely when it is re-rendered at the
    /// same stop. A hit requires the stored generation to equal [`DebugSession::stop_generation`],
    /// so any resume/step or console mutation (each of which bumps the generation) forces a fresh
    /// evaluation; stale entries never match and are overwritten lazily. Frame index is part of the
    /// key so the same expression watched against different paused frames does not collide.
    result_memo: HashMap<(String, usize), (u64, String, String)>,
    /// The **stop generation** — a monotonically-increasing state version for the paused program.
    /// It bumps whenever the observed state may have changed: the program resumes/steps (the
    /// dispatch loop bumps it on `DebugAction::Continue`), a console entry runs, a mutating watch
    /// runs, or a Variables-panel `setVariable` writes a register. A memoized watch result is valid
    /// only while its stored generation equals this one.
    stop_generation: u64,
}

/// The unforgeable global a wrapped console fragment binds its closure to (see
/// [`Vm::debug_eval_fragment`]) — a NUL-prefixed name no user identifier can collide with, taken
/// back out of its slot immediately after the entry runs (the same trick as the REPL's
/// trailing-expression sentinel).
const FRAGMENT_SENTINEL: &str = "\0debug-fragment";

/// Whether `expr` belongs to the hover-safe **read-only surface** (T6): names, `.field` chains,
/// `[index]`, arithmetic / comparison / logical operators, and plain literals. Everything else —
/// a call, a construction, a closure, an interpolated string (its holes hide expressions), a
/// `match`/`if` form — is refused: a hover fires on mouse-over and must never run code. This is
/// the static gate; the receiver-dependent dispatches it cannot decide (an object's `Index` impl,
/// a user ordering method — both frame pushes) are backstopped at run time by `Vm::pure_eval`.
fn is_pure_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Ident { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Bool { .. }
        | Expr::Str { .. } => true,
        Expr::Member { receiver, .. } => is_pure_expr(receiver),
        Expr::Index {
            receiver, index, ..
        } => is_pure_expr(receiver) && is_pure_expr(index),
        Expr::Binary { lhs, rhs, .. } => is_pure_expr(lhs) && is_pure_expr(rhs),
        Expr::Unary { operand, .. } => is_pure_expr(operand),
        _ => false,
    }
}

impl std::fmt::Debug for DebugSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugSession")
            .field("compiler", &self.compiler)
            .finish_non_exhaustive()
    }
}

/// The run's captured output (audit-1 finding 3): stdout, diagnostics, a deliberate
/// `os.exit`, and the abort traceback. Drained into the [`RunResult`] at teardown.
struct RunOutput {
    stdout: String,
    /// The program's standard-error accumulator — `std.io`'s `err`/`errln` push here through
    /// [`noeta_ext_abi::NativeCtx::write_stderr`], the stderr twin of `stdout`. Observable output,
    /// drained into the [`RunResult`] at teardown and compared by the differential oracle.
    stderr: String,
    /// Whether this run streams output as it is produced ([`noeta_stdlib::Console::streams_output`],
    /// cached at load — fixed per host, and the write path must not re-ask per `echo`). `false` (the
    /// sandbox, the `@test` runner) keeps every write in the buffers above; the drain is
    /// `lifecycle.rs`'s `Vm::emit_stdout` / `flush_live`.
    live: bool,
    diagnostics: Vec<Diagnostic>,
    /// A deliberate `os.exit(code)` (stdlib-gaps): the requested exit code, set when the
    /// distinguished `ErrorKind::Exit` unwinds. Not a diagnostic — the run halts cleanly
    /// (stdout kept, nothing reported) and the run's exit code is this value.
    requested_exit: Option<i32>,
    /// The **abort traceback**: the call stack captured as a fatal abort unwinds, innermost frame
    /// first. Appended by [`Vm::run`]'s error path — each (possibly re-entrant) run contributes its
    /// own frame stack as the abort climbs — and handed out by the host-facing entry points for the
    /// CLI / debug adapter to render. Written **only after** an abort, so it costs the hot path
    /// nothing; empty for a run that completes.
    abort_trace: Vec<TraceFrame>,
}

/// The tier-1 (JIT) engine state (P-JIT; audit-1 finding 3): engines, mirror tables,
/// promotion counters, and stats. One `#[cfg(feature = "jit-rt")]` field on [`Vm`] instead
/// of ~19 per-field gates; jit-only (Cranelift-needing) fields keep their finer gate inside.
#[cfg(feature = "jit-rt")]
struct Tier1State {
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
    /// When set, the engine built by [`Vm::init_jit`] emits **AOT-form** bodies (inline caches off,
    /// null call sites, no cancellation poll) — the codegen `noeta build --native` links, run
    /// in-process. Armed from [`RunOptions::aot_bodies`]; see [`noeta_jit::Jit::set_aot_bodies`].
    ///
    /// [`RunOptions::aot_bodies`]: crate::RunOptions::aot_bodies
    #[cfg(feature = "jit")]
    aot_bodies: bool,
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
    jit_ret: Value,
    /// Closures pinned by the JIT's per-call-site inline caches (P-JSSA S4.2): `jit_prepare_call`
    /// retains a closure when it caches it, so bits-equality at the site stays a proof of
    /// identity (no free/reuse while cached). Only 0-upvalue closures are cacheable — they hold
    /// nothing, so delaying their free to teardown is observably inert. Released (and the caches
    /// with them) before the teardown collectors run, keeping residency and the anomaly
    /// accounting exact. Bounded by call-site count: a site that sees a second distinct callee
    /// is poisoned, never re-pinned.
    jit_cache_pins: Vec<Value>,
    /// The empty-`Frame` template the JIT's native frame push copies (stable address for the
    /// `Vm`'s lifetime; the `Jit` and its generated code are dropped with the same `Vm`).
    #[cfg(feature = "jit")]
    jit_frame_template: Option<Box<Frame>>,
    /// The off-thread compile service (P-PAR S4) — the production hot-counter path. Mutually
    /// exclusive with the synchronous `jit` engine (which the `force_jit` oracle keeps).
    #[cfg(feature = "jit")]
    jit_service: Option<jit_service::JitService>,
    /// Tier-1 engines **retired by a hot swap** (server-hmr H3). Their executable pages must
    /// outlive any in-flight native frame (a frame beneath the long-running serve dispatch can
    /// be native code), so a swap never drops an engine — it parks it here, clears the mirror
    /// tables (no NEW dispatch can enter retired code), and re-arms fresh against the swapped
    /// module. Dropped with the `Vm`, after the run's machine stack has fully unwound. Bounded
    /// per-swap growth, like the arena modules — the documented retention model.
    #[cfg(feature = "jit")]
    jit_graveyard: Vec<noeta_jit::Jit>,
    /// The service twin of [`Vm::jit_graveyard`]: a retired service handle keeps its compile
    /// thread parked (blocked on an empty request channel) and its pages alive; the `Drop` at
    /// `Vm` teardown stops and joins it.
    #[cfg(feature = "jit")]
    jit_service_graveyard: Vec<jit_service::JitService>,
    /// P-AOT L3.2b: native entries were **bound ahead of time** (from a linked dispatch table),
    /// not JIT-compiled — so `self.jit`/`jit_service` are both `None` yet the mirror tables carry
    /// real native entry points. This makes the frame-entry dispatch consult those pre-installed
    /// entries even with the compiler absent; an uncompiled (ineligible) prototype still falls
    /// through to the interpreter.
    aot: bool,
    /// The **mirror tables** — the single tier-1 lookup source for the dispatch loop and the
    /// native call helpers, in both modes: the sync engine fills them right after compiling,
    /// the service via the mailbox drain. The engine's own tables are never read by the
    /// mutator in service mode (they live on the compile thread).
    jit_entries: Vec<Option<noeta_jit_abi::CompiledFn>>,
    jit_fast: Vec<Option<usize>>,
    /// The **region-scoped OSR bodies** (P-OSRW), keyed by prototype: a second native body whose
    /// compiled region is one hot loop's pc window. A native re-entry whose pc falls inside that
    /// window goes here; every other entry uses `jit_entries`. Deliberately *not* the table the
    /// native call helpers read — a region body has no fresh-frame entry, so nothing may call it
    /// as a callee. Left empty on programs that never OSR, which is what makes the routing check
    /// in `jit_enter` one length test there.
    #[cfg(feature = "jit")]
    jit_osr_entries: Vec<Option<noeta_jit::OsrBody>>,
    /// Per-prototype "main-body request sent" flag (service mode) — a hot prototype is queued
    /// exactly once.
    #[cfg(feature = "jit")]
    jit_requested: Vec<bool>,
    /// Per-prototype "OSR-body request sent" flag (service mode). Separate from `jit_requested`
    /// because the two answer different questions: a back-edge asks for the loop's window, a hot
    /// call entry asks for the whole prototype, and a prototype that got hot one way may later
    /// need the other. One request each, at most.
    #[cfg(feature = "jit")]
    jit_osr_requested: Vec<bool>,
    /// Prototypes whose compile request was born at a **loop back-edge** (service mode): when the
    /// entry lands, the next back-edge OSR-enters mid-loop instead of waiting for a frame entry
    /// that a single long-running loop may never make.
    #[cfg(feature = "jit")]
    jit_osr_pending: Vec<bool>,
    /// Requests in flight to the service (sends minus drained responses): the mailbox mutex is
    /// only ever locked while this is non-zero, so a program that never promotes pays nothing.
    #[cfg(feature = "jit")]
    jit_pending: usize,
    /// The service's final compile accounting, captured at teardown shutdown (the engine — and
    /// its counters — live on the compile thread until then).
    #[cfg(feature = "jit")]
    jit_final_stats: Option<JitStats>,
    /// Whether teardown's service shutdown **drains** (compiles) the outstanding queue rather
    /// than abandoning it. Off in production (a process should not linger at exit for entries
    /// nothing will run); on for the stats entry points, whose tests/benches assert
    /// deterministic promotion counts.
    #[cfg(feature = "jit")]
    jit_drain_at_exit: bool,
    /// The **bail histogram** (`--jit-stats`): how many times native code bailed back to the
    /// interpreter, per `(proto, resume pc)`. `None` (the default) records nothing — the seam pays
    /// one `Option` check *per bail event* (already a tier transition), never per op. Keyed by the
    /// resume pc, which is the bailing op's own pc (bail-before-mutate), so the report resolves it
    /// to an exact op + source line. Counts are per **native entry** (frame entries + one OSR), not
    /// per loop iteration: once a compiled prototype bails, its frame stays tier-0 until the next
    /// `'reload` (a declined loop produces no bails at all — the report lists those separately).
    jit_bail_counts: Option<std::collections::HashMap<(u32, u32), u64>>,
}

/// The persistent runtime state carried between entries of a [`VmSession`] (REPL / embed /
/// hot-reload), embedded in the [`Vm`] as its `persist` field (audit-1 finding 4): everything a
/// later entry inherits so an earlier entry's effects survive. A plain single-shot run simply
/// builds a fresh one in [`Vm::load`] and tears it down at exit. Embedding it whole means session
/// entry/exit are single moves ([`Vm::load_seeded`] / [`Vm::into_state`]) — previously 16 fields
/// were hand-copied at four sites, and a field forgotten in one silently reset session state.
///
/// The `Rc`-wrapped derived tables (`shapes` / `packed_schemas` / `type_reprs`) grow by **append**
/// (never rebuild), so an entry-1 aggregate and an entry-2 aggregate of the same type share
/// `&'static Shape` identity — the invariant the reuse gate, packed-value ops, and inline caches
/// assume within a single run. `SessionState::sync_to` (in `session.rs`) extends them to a grown
/// module; the existing prefix keeps its identity.
pub(crate) struct SessionState {
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
    /// The channel table (isolates I.1): every `channel::<T>(cap)` appends a [`Channel`]; endpoint
    /// values (`Sender`/`Receiver`) reference one by index. A queued message is owned by the channel
    /// (retained on enqueue, transferred out on dequeue). `channel_progress` counts successful queue
    /// operations (a `send` push, a `recv` pop, a `close`) so the scheduler treats a channel op that
    /// unblocks a sibling as progress even when no task completes. Mirrors the tree-walker's fields.
    channels: Vec<Channel>,
    channel_progress: u64,
    /// The extensions' **retained-value arena** (higher-order-abi H4, Class 3): every `Some`
    /// entry owns one reference to a language value an extension holds *across* dispatches
    /// (`NativeCtx::retain`/`retained_get`/`retained_set`/`release_retained`); freed indices are
    /// reused via `ext_arena_free`. The arena is a first-class **root set**: teardown feeds it
    /// into the trace collector's roots and then releases every remaining entry (exactly the
    /// reactive graph's treatment), so residency returns to 0 whatever the program forgot.
    ext_arena: Vec<Option<Value>>,
    ext_arena_free: Vec<u32>,
    /// **Embed handles** (server-hmr F3): language values an embedding HOST holds across
    /// [`VmSession::call_by_name`] calls without marshalling them out — a game engine keeping an
    /// entity/object reference between frames. Each live slot owns one reference; freed slots are
    /// reused via `embed_handles_free`. Rooted and released exactly like `ext_arena`, so a handle
    /// the host forgets to release still reclaims at teardown (residency 0).
    embed_handles: Vec<Option<Value>>,
    embed_handles_free: Vec<u32>,
    /// Per-run extension Rust state (`NativeCtx::state`, H4): plain data keyed by the
    /// extension's own `'static` key, created on first access, dropped at VM drop. Language
    /// values never live here — they go through the arena above.
    ext_state: Vec<(&'static str, noeta_stdlib::ExtState)>,
    /// Extern types whose **read gate** is currently closed (H5 perf): while a type is listed,
    /// its declared `arena_getter` method takes the full ctx dispatch instead of the inlined
    /// arena read. Almost always empty (the hot check is `is_empty()`); toggled by extensions
    /// via `NativeCtx::set_read_gate` around tracking/dirty windows.
    ext_closed_gates: Vec<&'static str>,
    /// One shared `&'static Shape` per shape-table entry — cloned into every value of that shape,
    /// so equal-built aggregates point at one shape (identity is a pointer comparison).
    shapes: Vec<&'static Shape>,
    /// One shared `Rc<PackedSchema>` per compiled packed-list layout (P-PACK 2.4), resolved at load
    /// from [`Module::packed_schemas`] against `shapes` — so `Op::MakePackedList` packs/materializes
    /// elements that share shape identity with directly-constructed instances.
    packed_schemas: Vec<&'static noeta_object::PackedSchema>,
    /// One shared `Rc<TypeRepr>` per interned reflected element type (runtime type-argument
    /// reflection, R1), built once at load from [`Module::type_reprs`]. `Op::MakeList` stamps a cheap
    /// `Rc` clone of its indexed entry onto the built list, so `type_of` recovers the element type
    /// after a `dyn` launder. Empty for a program with no tagged list literal.
    type_reprs: Vec<Rc<noeta_ast::reflect::TypeRepr>>,
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
    /// The extension **registry** this VM resolves native names against (instance-registry IR3):
    /// module functions, extern types, method bundles, and their dispatch all consult it through
    /// [`Vm::reg`]. `None` — the default on every ordinary run — falls back to the process-global
    /// default registry (`noeta_stdlib::registry::default_seeded`), so a plain run is byte-for-byte
    /// unchanged. An embedding host that assembled its own extension set threads its `Registry` in
    /// (the embed API / server-hmr F2), and a worker isolate inherits its parent's (a `&'static`
    /// registry is `Send`). The std-concrete `static_dispatch_ctx*` fast paths deliberately stay on
    /// the global — std is in every assembled registry, so monomorphizing them costs no correctness.
    registry: Option<&'static noeta_stdlib::registry::Registry>,
}

/// One dispatch-loop inline-cache slot (`LoadField`/`CallMethod`, indexed by the op's `cache`
/// field): the last receiver shape seen at the site and its resolved field-slot / method
/// prototype. See [`Vm::dispatch`].
pub(crate) type MethodCacheEntry = Option<(&'static Shape, u32)>;

/// One extern-method route-cache slot (H5 perf): the extern type-name pointer (a registry
/// `&'static str`, a stable identity) and the site's resolved routing. See [`Vm::dispatch`].
pub(crate) type ExternCacheEntry = Option<(*const u8, crate::methods::ExternRoute)>;

/// One program's worth of execution state, shared across every (possibly re-entrant) frame
/// stack: the compiled module, the shared shape handles and instance-method table, the by-name
/// global environment, captured stdout, and the diagnostics recorded so far.
struct Vm<'m> {
    /// The compiled program. On a plain run this is the caller's module for the whole run; on a
    /// **debug run with a console session** it is swapped (through [`Vm::install_fragment`]) to
    /// each successive extended snapshot — always a stable-prefix superset, so an index minted
    /// under any earlier module resolves identically under every later one.
    module: &'m Module,
    /// See [`DebugSession`]; `None` on every non-debug run.
    debug_session: Option<DebugSession<'m>>,
    /// This VM's seat on the hot-reload mailbox — see [`HotConsumer`] (server-hmr W1); `None` on
    /// every run but `noeta serve --watch`'s in-process hot mode. A watcher thread deposits
    /// ready-to-apply [`SwapPlan`]s; the VM applies its pending ones at the scheduler tick
    /// ([`Vm::apply_pending_hotswap`] via `advance_tasks`), a safepoint every ctx-driven loop passes.
    ///
    /// [`SwapPlan`]: noeta_compiler::hotswap::SwapPlan
    hot_mailbox: Option<HotConsumer>,
    /// How many swap plans this VM has applied from its [`HotChannel`] queue (server-hmr F5): the
    /// generation index it drains from, and — via [`NativeCtx::hot_swap_count`] — how the serve
    /// loop detects its own swaps to push `reload` to *its* clients. Per-VM, so N workers each
    /// track their own progress against the shared broadcast queue.
    applied_swaps: usize,
    /// Set while a **hover** fragment runs (tooling-unification T6): a hover must stay
    /// side-effect-free, so the dispatch loop refuses any frame push beyond the fragment wrapper's
    /// own frame — the one chokepoint every way of running user code (a call, an object's `Index`
    /// impl, a user ordering method) passes through. The fragment's AST is pre-gated to the
    /// read-only surface (names / members / indexing / operators / literals); this flag is the
    /// runtime backstop for the receiver-dependent dispatches the gate cannot decide.
    pure_eval: bool,
    /// Set when this parallel scheduler is **registered in the stall registry** (isolates I.4c
    /// real-path deadlock detection); `false` for the sandbox and any parallel VM not driven through a
    /// registering entry point (which keeps the pre-existing keep-waiting behavior — no false deadlock).
    stall_active: bool,
    /// Isolate-worker stall slots registered by the parent at spawn but not yet dropped (isolates
    /// I.4c) — so `active` never lags a starting worker; counted to balance harvest vs teardown drops.
    registered_workers: usize,
    /// The session-persistent runtime — see [`SessionState`]. Everything else on `Vm` is
    /// per-entry scratch or module-derived tables.
    persist: SessionState,
    /// `map(...)` call span → the result element's `Rc<PackedSchema>` (P-PACK 2.6 category B), resolved
    /// at load from [`Module::map_packed_sites`]. The `map` builtin looks up its call span here to build
    /// a flat result instead of N boxed objects.
    map_packed: HashMap<Span, &'static noeta_object::PackedSchema>,
    /// Instance-method dispatch: type name → (method name → prototype index). Two-level
    /// (audit-1 finding 7) so every lookup probes with **borrowed** `&str` keys via
    /// [`Vm::method_proto`] — the previous flat `(String, String)` key forced two heap
    /// allocations per uncached dynamic dispatch (enum methods, operator overloads,
    /// `Op::Invoke`).
    methods: HashMap<String, HashMap<String, u32>>,
    /// The reverse of [`Module::global_names`]: global name → slot. Built once at load, because the
    /// forward table is slot-ordered and the only consumer that starts from a *name* is the
    /// free-function `Op::Invoke` — which would otherwise scan every global name on each dispatch.
    /// The tree-walker twin is its `globals` scope map.
    global_slots: HashMap<String, u32>,
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
    /// `@derive(Deserialize<Json>)` decode recipes (L2.2 DI), keyed by type name — lifted from
    /// [`noeta_bytecode::Module::deserialize_recipes`]. `Op::DecodeTyped` (`json.decode_typed(name,
    /// text)`) looks up the runtime type name here to decode a JSON body into that type.
    deserialize_recipes: HashMap<String, noeta_stdlib::TypeRecipe>,
    /// Async scheduler state — see [`SchedState`].
    sched: SchedState,
    /// Spare ctx slot tables (H5 perf): a ctx dispatch pops one instead of allocating, and its
    /// drop clears + returns it — a hot `set` loop then runs alloc-free. A stack, so ctx
    /// re-entrancy (a called closure re-entering a dispatch) simply pops the next one.
    ctx_table_pool: Vec<Vec<Option<Value>>>,
    /// Spare re-entrant run contexts (audit-1 finding 5, the `ctx_table_pool` pattern): a
    /// re-entrant [`Vm::run`] entry — a closure applied per element by `map`/`filter`/the
    /// iterator drains, a default thunk, a method handle, a destructor — pops a spare
    /// frame + register stack instead of allocating both, and [`Vm::run`] clears + returns
    /// them on exit. A stack, because re-entrancy nests.
    reentry_pool: Vec<(Vec<Frame>, Vec<Value>)>,
    /// Spare per-entry inline-cache tables for the dispatch loop (same finding): `dispatch`
    /// pops a spare pair instead of allocating two vectors sized to the whole module's
    /// `cache_slots` per entry. Cleared before returning to the pool — a resolution must
    /// never carry across runs (a hot-swap between entries rebinds methods), exactly the
    /// fresh-per-run semantics the previous locals had.
    cache_pool: Vec<(Vec<MethodCacheEntry>, Vec<ExternCacheEntry>)>,
    /// The current [`Vm::run`] nesting depth, maintained across re-entrant entries so the
    /// JIT's generous register-stack reserve fires only on the outermost run (finding 5's
    /// per-element 64 KB reserve gate).
    #[cfg_attr(not(feature = "jit"), allow(dead_code))]
    run_depth: usize,
    /// Extra safepoint-GC roots no register window covers (a depth-0 drive loop's Rust-local
    /// values — worker isolate, consumed await). Borrowed; see `gc.rs`.
    transient_roots: Vec<Value>,
    /// Teardown/reclaim in progress (destructors over a heap mid-surgery): polls must not collect.
    gc_suspended: bool,
    /// Real-thread isolate state — see [`IsolateState`].
    isolates: IsolateState,
    /// Captured run output — see [`RunOutput`].
    out: RunOutput,
    /// Tier-1 JIT state — see [`Tier1State`]. The one runtime-support gate on `Vm`.
    #[cfg(feature = "jit-rt")]
    tier1: Tier1State,
    /// The attached debugger (`noeta dap`), consulted before every instruction. `None` on every
    /// non-debug run (production, differential, salsa), where it costs one predicted branch per op.
    debugger: Option<Box<dyn Debugger>>,
    /// The attached profiler (`noeta profile`), consulted before every instruction on the same seam
    /// as `debugger` but without pausing. `None` on every non-profile run, where it costs one
    /// predicted branch per op. Never armed together with the JIT (a profile run pins tier-0).
    profiler: Option<Box<dyn ProfileHook>>,
}

/// The traceback vocabulary is shared with the tree-walker oracle through the backend contract
/// crate, so both backends produce the same `TraceFrame` shape (and can eventually be compared).
pub use noeta_backend::{RunResult, TraceFrame, render_trace};

/// Tier-1 promotion threshold: a prototype interprets until it has been entered this many times,
/// then the JIT compiles it (P-JIT). The `--jit-differential` oracle bypasses this via `force_jit`.
#[cfg(feature = "jit")]
const JIT_HOT_THRESHOLD: u32 = 50;

#[cfg(test)]
mod tests;
