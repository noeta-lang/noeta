//! VM **lifecycle**: [`Vm::load`] (derived-table resolution), [`Vm::teardown`]
//! (globals destruction, cycle reaping, channel drain), the value-release /
//! destructor cluster, the session heap-owner accounting, and the real-thread
//! isolate machinery ([`IsolateFactory`], `run_isolate_worker`, the
//! scheduler-owned `Channel`/`Task` state). Every item is moved verbatim from
//! the crate root purely to shrink `lib.rs` — no behavior change.

use crate::*;

use crate::scheduler::SchedState;

/// What a `construct` field-source (a `List` or a `Map`) resolves to before the object is built: the
/// `field -> value` pairs, the owned realized list to release afterward (only for the positional list
/// form; `None` for the map form), and the shared planner's validation outcome.
type ConstructResolve = (Vec<(String, Value)>, Option<Value>, Result<(), String>);

/// Validate a `construct("Enum.Variant", fields)` payload against the variant's declared schema and
/// order it into the positional slots an enum value carries, each **retained** for its new home.
///
/// The mirror of the tree-walker's function of the same name (`plans/backend-mirror.md`): only the
/// value extraction and the refcount protocol are backend-local, and every accept/reject decision
/// runs through the same shared planners a struct's fields go through — so a payload mismatch is
/// worded exactly like a field mismatch, in both backends.
fn plan_variant_payload(
    case_name: &str,
    payload: &[noeta_ast::reflect::FieldSpecData<'_>],
    fields_val: &Value,
) -> Result<Vec<Value>, String> {
    if fields_val.is_list() {
        // `realize_list` hands back values sharing the container's references (not retained), so each
        // value kept for the built case is retained and the realized list released afterward — the
        // `Op::Invoke` protocol the fielded path uses.
        let realized = fields_val.realize_list();
        let values = realized.list_items().expect("checked is_list");
        let reprs: Vec<noeta_ast::reflect::TypeRepr> = values.iter().map(vm_type_repr).collect();
        let plan = noeta_ast::reflect::plan_construct(case_name, payload, &reprs);
        let out = match plan {
            Err(msg) => {
                realized.release();
                return Err(msg);
            }
            Ok(_) => {
                values.iter().for_each(|v| retain(*v));
                values
            }
        };
        realized.release();
        Ok(out)
    } else if fields_val.is_map() {
        let keys = fields_val.map_keys().expect("checked is_map");
        let vals = fields_val.map_values().expect("checked is_map");
        let provided: Vec<(String, Value)> = keys
            .iter()
            .zip(vals)
            .filter_map(|(k, v)| match k {
                noeta_stdlib::MapKey::Str(s) => Some((s.as_str().to_owned(), v)),
                _ => None,
            })
            .collect();
        let reprs: Vec<(String, noeta_ast::reflect::TypeRepr)> = provided
            .iter()
            .map(|(n, v)| (n.clone(), vm_type_repr(v)))
            .collect();
        noeta_ast::reflect::plan_construct_named(case_name, payload, &reprs)?;
        let names: Vec<String> = provided.iter().map(|(n, _)| n.clone()).collect();
        Ok(
            noeta_ast::reflect::plan_variant_payload_order(payload, &names)
                .into_iter()
                .map(|i| {
                    let value = provided[i].1;
                    retain(value);
                    value
                })
                .collect(),
        )
    } else {
        Err(format!(
            "construct fields must be a list or a map, found {}",
            fields_val.type_name()
        ))
    }
}

/// Builds a fresh host + async executor for a worker isolate (isolates I.4b). Injected by the CLI (its
/// `RealHost` + `RealExecutor`), so `noeta-vm` stays free of `noeta-host-real`/tokio. `Send + Sync` so the
/// worker closure can carry a clone across the thread boundary.
pub type IsolateFactory =
    Arc<dyn Fn() -> (Box<dyn noeta_stdlib::Host>, Box<dyn noeta_stdlib::Executor>) + Send + Sync>;

/// Builds a fresh [`ProfileHook`] for a worker isolate (per-isolate profiles): called with the
/// isolate's display name at spawn, installed on the worker's VM, and — once the worker finishes —
/// the hook is handed to the [`ProfileSink`] for the profiler to resolve. Injected by `noeta
/// profile`; absent on ordinary runs.
pub type ProfileHookFactory = Arc<dyn Fn(&str) -> Box<dyn ProfileHook> + Send + Sync>;

/// Where finished worker isolates deposit their (display name, hook) pairs — one entry per isolate
/// run, harvested by the profiler after the main run completes. The `Mutex` is uncontended (one
/// lock at each isolate's start and end).
pub type ProfileSink = Arc<std::sync::Mutex<Vec<(String, Box<dyn ProfileHook>)>>>;

/// The real-OS-thread isolate state (isolates I.4b; audit-1 finding 3): spawn plumbing,
/// in-flight worker slots, and the borrow-share promotion region. Inert in the sandbox.
pub(crate) struct IsolateState {
    /// Real OS-thread isolates (isolates I.4b), CLI-only / out-of-oracle. `parallel_isolates` selects
    /// the real path in the `Op::SpawnIsolate` handler; `isolate_module` is an `Arc` clone of the
    /// compiled module (`Send + Sync`) the entry point holds *alongside* the `&Module` borrow, so a
    /// worker thread can own the module for its lifetime; `isolate_factory` builds a fresh host +
    /// executor per worker (injected by the CLI so `noeta-vm` needs no `noeta-host-real`/tokio dependency);
    /// `isolates` holds each spawned worker's result channel + join handle; `inflight_isolates` counts
    /// workers whose result has not yet been harvested (so the scheduler treats a pending isolate as
    /// progress, not a deadlock). All inert in the sandbox (`parallel_isolates` false).
    pub(crate) parallel_isolates: bool,
    pub(crate) isolate_module: Option<Arc<Module>>,
    pub(crate) isolate_factory: Option<IsolateFactory>,
    /// Per-isolate profiling (injected by `noeta profile`): build a hook for each spawned worker,
    /// deposit it (named) in the sink when the worker finishes. `None` on ordinary runs.
    pub(crate) profile_seam: Option<(ProfileHookFactory, ProfileSink)>,
    pub(crate) isolates: Vec<IsolateSlot>,
    pub(crate) inflight_isolates: usize,
    /// The borrow-share region for real-isolate arguments (P-PAR S2): promotable argument graphs
    /// are deep-copied into it **once** and every worker borrows zero-copy. `promote_memo` maps a
    /// source object's bits → its promoted root across spawns (the fan-out promote-once memo);
    /// each memoized source is retained into `promote_sources` so its address stays valid for the
    /// memo's lifetime. All three are freed/cleared together when the last in-flight isolate is
    /// joined (`finish_isolate`) and defensively at teardown. Always empty in the sandbox.
    pub(crate) shared_region: noeta_value::SharedRegion,
    pub(crate) promote_memo: HashMap<u64, Value>,
    pub(crate) promote_sources: Vec<Value>,
    /// Worker-side map of globals the parent could **not** ship into this isolate (isolates I.4b):
    /// global slot → the unshippable value's type name (e.g. a `class`, which has reference identity
    /// and cannot cross into a fresh heap). The slot is left unbound; if the worker body actually
    /// *reads* it, `Op::LoadGlobal` raises a precise E0042 naming the global + its type + the fix,
    /// instead of the confusing "cannot find `x`" an ordinary unbound slot yields. Empty on the
    /// parent VM and whenever every global shipped.
    pub(crate) unshippable_globals: HashMap<u32, String>,
    /// **This VM's cancellation flag** (isolate-cancel): the `Arc<AtomicBool>` whoever owns this
    /// run sets to ask it to stop. The VM polls it at its **safepoints** — the dispatch loop's
    /// frame transfers and taken loop back-edges, plus each scheduler round — and unwinds when it
    /// is set.
    ///
    /// Two owners, one mechanism. A **worker isolate** installs the flag its parent's `h.cancel()`
    /// stores through. A **top-level run** installs the one its embedder passed in
    /// ([`RunOptions::cancel`](crate::RunOptions::cancel)) — that is the test-timeout rail asking
    /// an overrunning `@test` case to stop. `None` otherwise, including on every cooperative task's
    /// VM (a cooperative task is cancelled by the scheduler's own `Task::cancelled` flag, since it
    /// is already parked), so the poll is a never-taken, perfectly-predicted branch on an ordinary
    /// run.
    pub(crate) cancel_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The whole token [`cancel_flag`](Self::cancel_flag) is the poll half of, kept so honoring a
    /// request can **clear** it. That matters because the unwind an honored cancellation starts
    /// runs user code — destructors, and the teardown behind them — which performs real IO: a
    /// request left standing would tell every host leaf on the way out to stop before it did
    /// anything. `None` exactly when `cancel_flag` is.
    pub(crate) cancel_signal: Option<Arc<noeta_stdlib::CancelSignal>>,
    /// Set when this worker observed its [`cancel_flag`](Self::cancel_flag) at a safepoint and
    /// unwound. Distinguishes the resulting `Abort` from a genuine runtime error, so
    /// [`run_isolate_worker`](crate::lifecycle::run_isolate_worker) ships
    /// [`IsolateOutcome::Cancelled`] rather than a failure.
    pub(crate) cancel_observed: bool,
}

/// A spawned worker isolate (isolates I.4b): the channel its outcome arrives on, the thread's join
/// handle (taken to join at teardown), and the **cancellation flag** the parent sets from
/// `h.cancel()` (isolate-cancel) and the worker polls at its safepoints.
pub(crate) struct IsolateSlot {
    pub(crate) result: std::sync::mpsc::Receiver<IsolateReport>,
    pub(crate) handle: Option<std::thread::JoinHandle<()>>,
    /// This worker's cancellation token, requested by the parent's `cancel_task` (isolate-cancel).
    /// Its flag is what the worker reads at every safepoint; its wake is what rouses the worker
    /// when the safepoints are exactly what it has left — parked in its executor's real-time sleep
    /// for one long `sleep(ms)`, or blocked in a host read of a child that has stopped talking. A
    /// wake with no hooks registered is inert, so this costs nothing on a worker that cannot block
    /// outside the interpreter.
    pub(crate) signal: Arc<noeta_stdlib::CancelSignal>,
}

/// What a worker isolate ships home when its thread finishes (isolate-cancel). Three terminal
/// states, kept distinct because the parent renders each differently: a completed body's marshalled
/// result, a **cancellation the worker actually honored** at one of its safepoints, and a failure
/// (a panic, or a result that would not marshal).
///
/// The `Cancelled` arm is what makes `h.join()` honest: before this existed the parent latched
/// "cancelled" the instant it *asked*, so a worker that ran to completion anyway was still reported
/// as cancelled. Now the parent only reports `Err(Cancelled)` once this arrives.
pub(crate) enum IsolateOutcome {
    Done(isolate::Wire),
    Cancelled,
    Failed(IsolateFailure),
}

/// Everything a worker isolate ships home: its terminal [`IsolateOutcome`] **and the program output
/// it produced** ([`IsolateOutput`]). One message, because the two are inseparable — a worker's
/// `echo` is as much a part of what it did as its return value, and there is no second channel a
/// shared-nothing isolate could have used.
pub(crate) struct IsolateReport {
    pub(crate) output: IsolateOutput,
    pub(crate) outcome: IsolateOutcome,
}

/// A worker isolate's captured program output, marshalled home with its outcome.
///
/// # Why it has to travel
///
/// A worker builds its **own** [`Vm`], with its own `Host`, its own executor and its own
/// thread-local object registry — shared-nothing by construction, which is exactly what makes
/// abandoning one a leak rather than a use-after-free. Nothing about the parent's `RunOutput` is
/// reachable from the worker, so the buffers cannot be shared; they have to be handed back
/// explicitly. They are plain `String`s, so they cross the thread boundary like every
/// [`isolate::Wire`] value does.
///
/// # The ordering contract
///
/// **A worker's output arrives as one contiguous block, appended to the parent's buffers at the
/// point the parent harvests that worker's outcome** — its `.await` / the structured scope's join.
/// Two guarantees follow, and one deliberate non-guarantee:
///
/// - *Within* a block, the worker's own writes are in the worker's program order. That is the only
///   order that is real, and it is preserved exactly.
/// - A block lands at a point in the **parent's** program order (the harvest), so a parent that
///   awaits its isolates in sequence gets a fully determined transcript.
/// - Across isolates running *concurrently*, blocks appear in harvest order, which is completion
///   order — and completion order is thread scheduling, so it is not reproducible.
///
/// Interleaving the two streams line-by-line was the alternative and is rejected: it needs a shared
/// wall clock the deterministic sandbox does not have, it would make ordering depend on thread
/// scheduling at *line* granularity rather than at block granularity, and it would shred a worker's
/// own transcript for a total order that never existed. Grouping keeps the one true order and is
/// honest about the one that is not.
///
/// # Live runs
///
/// On a run whose host streams output ([`noeta_stdlib::Console::streams_output`] — `noeta run`,
/// `noeta serve`), a worker's completed lines have already left its buffer for the terminal, so
/// only the unterminated tail travels. Nothing is printed twice, and nothing that was streamed is
/// re-captured.
#[derive(Default)]
pub(crate) struct IsolateOutput {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl IsolateOutput {
    /// Whether there is anything to merge — the common case is `true` for a worker that echoed and
    /// `false` for one that did not, and the parent skips the merge entirely when it is empty.
    pub(crate) fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty()
    }
}

/// A worker isolate's failure, shipped back across the thread boundary: the abort's message and the
/// worker's own stack trace (empty for a non-abort failure, e.g. an unshippable result). Plain data
/// (`String`s + `Span`s), so it crosses threads like the [`isolate::Wire`] values do. The parent
/// installs the trace before re-raising at the `.await`, so the rendered traceback tells the whole
/// story — the worker's frames innermost, the awaiting parent's frames beneath them.
pub(crate) struct IsolateFailure {
    pub(crate) message: String,
    pub(crate) trace: Vec<TraceFrame>,
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
/// Each queued message also carries the **sender's trace context** (native-otel T5d, `None` when
/// telemetry is off or the sender had no active span) — the automatic-propagation envelope: recv
/// seeds the receiving strand's context from it, so message *types* stay tracing-free.
pub(crate) enum Channel {
    Local {
        buffer: std::collections::VecDeque<(Value, Option<noeta_stdlib::TraceContext>)>,
        capacity: usize,
        closed: bool,
        /// Live **producer holds** of this channel (isolates I.4c auto-close): the count of spawned
        /// tasks/isolates that captured a `Sender<T>` for it. Incremented when such a producer is
        /// spawned, decremented when it completes; when the count returns to 0 (having been positive)
        /// the channel **auto-closes**, so a blocked receiver drains then observes `none` instead of
        /// deadlocking. Keyed on producer-task lifecycle rather than raw sender-value RC because the
        /// enclosing async/top-level scope retains a structural sender (a cell or global) until it
        /// ends — which is too late to signal "no more sends". A channel whose sender is only ever
        /// used structurally (never handed to a producer task) has 0 holds and relies on explicit
        /// `close()`, exactly as before.
        producers: u32,
    },
    Shared(Arc<isolate::ChannelCore>),
}

/// A spawned task in a structured-concurrency scope (Track A.3b): its future (an `async fn` state
/// machine) and its completion result once driven to ready (`None` while pending). The scope owns a
/// reference to `future`, and to `result` once set; both are released when the scope is joined+popped.
pub(crate) struct Task {
    pub(crate) future: Value,
    pub(crate) result: Option<Value>,
    /// Set when the task is **cancelled** (Track A.8) — e.g. a `race` loser. A cancelled task is
    /// never polled again and counts as done for the join; its future is reclaimed by `ScopeEnd`
    /// exactly like a completed task's, so cancellation frees no differently than a normal join (the
    /// leak oracle confirms residency 0). It stops cooperatively at its last suspension. (Running user
    /// `destruct` on an async task's captured locals is a separate, pre-existing gap — see
    /// `plans/deferred.md` — that affects completed and cancelled tasks alike.)
    pub(crate) cancelled: bool,
    /// Set while this task's future is **being polled** (its step is executing). A nested
    /// `poll_all_scopes_round` — a `concurrent` join *inside* this task's own body — must skip it:
    /// re-entering a mid-execution state machine re-runs its current segment (infinite recursion /
    /// duplicated effects). The task is already progressing; "skip while polling" is the correct
    /// scheduling, and it is also what keeps per-task context swaps balanced.
    pub(crate) polling: bool,
    /// The task's **saved task-local context** (native-otel T5a): a snapshot of the spawner's
    /// `ctx_current` taken at `spawn`, swapped into `Vm::ctx_current` around each poll of this
    /// task's step and back out after — so telemetry scope follows the task across suspensions
    /// instead of leaking between interleaved tasks. Plain `u64`s (span ids), not values — no
    /// refcount traffic, invisible to the GC and the leak oracle.
    pub(crate) context: Vec<u64>,
    /// The channels this task holds a **producer hold** on (isolates I.4c auto-close): the channel
    /// indices of every `Sender<T>` it captured at spawn. Each is decremented when the task's future
    /// is released (on completion or at `ScopeEnd`), auto-closing the channel when its last producer
    /// is gone. Emptied once decremented, so a completed task's early release and the scope's
    /// end-of-life sweep never double-count.
    pub(crate) holds: Vec<usize>,
    /// The **strand** this task belongs to (DAP worker debugging): a plain `spawn`/`concurrent`
    /// task inherits its spawner's strand (they are cooperative concurrency *within* one logical
    /// thread); a worker-isolate root task gets a fresh id. The scheduler swaps this into
    /// `SchedState::current_strand` around each poll, so a breakpoint inside the task reports the
    /// right DAP thread. `1` (the main strand) on ordinary runs.
    pub(crate) strand: u32,
    /// `Some(id)` iff this is a worker-isolate **root** task (the cooperative `isolate f(args)`
    /// spawn, DAP worker debugging): its `id` equals `strand`, and its completion fires the
    /// debugger's `on_strand_exited` (a `thread` exited event). `None` for every other task —
    /// including sub-tasks a worker spawns, which merely inherit the strand.
    pub(crate) isolate_strand: Option<u32>,
}

/// One traced future (native-otel T5c): the future-completion hook's entry. `future` is a
/// **retained** reference (identity = its NaN-box bits, stable while the reference is held);
/// `context` is the stack its polls run under (the registering strand's context + `span`), swapped
/// in/out around each poll exactly like a task's; `span` is ended when the future completes.
pub(crate) struct TracedFuture {
    pub(crate) future: Value,
    pub(crate) context: Vec<u64>,
    pub(crate) span: u64,
}

/// The outcome of polling a future once (Track A.3): ready with a value, still pending, or —
/// for a real-isolate future whose worker honored a cancellation request (isolate-cancel) —
/// terminally cancelled, which is neither a value nor a state further polling can leave.
pub(crate) enum Poll {
    Ready(Value),
    Pending,
    /// The polled future is a real-isolate future whose worker stopped at a safepoint because it
    /// was cancelled. Terminal: the task will never produce a value, and the scheduler marks it
    /// `cancelled` so the join reports `Err(Cancelled)`.
    Cancelled,
}

/// How a value behaves under `?`/`??`: the unwrapped success payload, or the empty case.
pub(crate) enum TryOutcome {
    Success(Value),
    Empty,
}

/// Classify a value for `?`/`??`. Only the built-in `Result`/`Option` enums qualify; the
/// success payload is shared (not retained). Mirrors the M0 tree-walker's `try_branch`.
pub(crate) fn try_classify(v: Value) -> Option<TryOutcome> {
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

/// Whether a variant pattern asking for one binding may match a **payload-less** built-in `Result`
/// value by supplying `unit`.
///
/// `Ok()` on a `Result<void, E>` — which is what every `impl Validate` returns — builds an enum with
/// no payload element, while the pattern `Ok(_)` (and `Ok(v)`) asks for one. Without this, the two
/// spellings of the same value disagree: `x?` unwraps it happily ([`try_classify`] already
/// substitutes `unit` on exactly this shape), and `match x { Ok(_) => …, Err(e) => … }` falls off
/// the end of an exhaustive-looking match with E0007 at run time — a failure the checker cannot see
/// and the reader cannot explain. Restricted to the two built-in carriers because for a *declared*
/// enum the arity is the author's own distinction: `Part.Text` and `Part.Text(t)` are different
/// cases, and a lenient match there would be a real bug.
pub(crate) fn unit_payload_match(builtin_carrier: bool, data_len: usize, arity: usize) -> bool {
    builtin_carrier && data_len == 0 && arity == 1
}

/// The **nominal runtime tag** a value carries, if any: a shape's name (user struct/class/enum)
/// or an extern value's qualified identity — the key the trait-membership table
/// (`ReflectionInfo::trait_impls`) and `traits_of` use. `None` for every non-nominal value
/// (scalars, collections, functions), which therefore implements no declared trait. Mirrors the
/// tree-walker's `value_nominal_name`.
pub(crate) fn vm_nominal_name(v: &Value) -> Option<String> {
    if v.is_extern() {
        return Some(v.with_extern(|e| e.type_identity().to_string()));
    }
    v.shape().map(|s| s.name.clone())
}

/// Whether a value matches a narrowing target (`x.as<T>()`). Generics are erased, so only the
/// runtime **head constructor** is tested. The primitive/collection kinds compare against
/// [`Value::type_name`] — the same canonical strings the M0 tree-walker matches on, so both
/// backends decide a narrowing identically; `Named` (a user struct/class/enum, or the built-in
/// `Option`/`Result`) matches by shape name; `Dyn` always matches (no-op narrowing); `DynTrait`
/// tests the value's nominal type against `reflection`'s trait-membership table (the same shared
/// table the tree-walker consults, so the two backends agree by construction).
pub(crate) fn narrow_matches(
    v: Value,
    target: &NarrowTarget,
    reflection: &noeta_ast::reflect::ReflectionInfo,
) -> bool {
    let kind = match target {
        NarrowTarget::Int => "int",
        // Subtype edge `F32 <: float`: a plain `float` OR a reified `f32` value matches `float`.
        NarrowTarget::Float => return v.type_name() == "float" || v.is_f32(),
        // The `f32` head matches only a reified `f32` value (a plain `float` is the base, not a
        // subtype — `(float) is f32` is false).
        NarrowTarget::F32 => return v.is_f32(),
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
        NarrowTarget::Named(name) => {
            // An extern-type value matches by its **qualified identity** (`std.id.Uuid`) — the
            // narrowing target the lowering produced for an imported native type, compared
            // directly against the identity the value itself carries (no registry walk) — so it
            // never matches a same-short-named *user* type (whose shape name is bare) nor a
            // same-short-named extern type from another namespace. User objects/enums match
            // their shape name.
            if v.is_extern() {
                return v.with_extern(|e| e.type_identity() == name);
            }
            return v.shape().is_some_and(|s| &s.name == name);
        }
        // A trait object matches iff the value's nominal type has a REGISTERED impl of the trait —
        // the module reflection's membership table, built from the same `impl`/`@derive`/ABI
        // declarations trait-method dispatch resolves through. A non-nominal value implements no
        // declared trait and never matches. Mirrors the tree-walker's `TypeRef::DynTrait` arm.
        NarrowTarget::DynTrait(trait_name) => {
            return vm_nominal_name(&v).is_some_and(|n| reflection.type_implements(&n, trait_name));
        }
        NarrowTarget::AnyOf(members) => {
            return members.iter().any(|m| narrow_matches(v, m, reflection));
        }
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
            return narrow_matches(v, head, reflection)
                && noeta_ast::reflect::narrow_args_match(args, &vm_type_repr(&v));
        }
    };
    v.type_name() == kind
}

/// How much unterminated output may accumulate before a live run flushes it anyway.
///
/// Live output is **line**-oriented (one write syscall per line, not per `echo`), which is both the
/// cheap shape and the one a terminal already expects. This bound is the escape hatch for a program
/// that writes a great deal without ever emitting a newline: it still appears, in chunks, instead of
/// growing a buffer nobody sees.
const LIVE_OUTPUT_CHUNK: usize = 8 * 1024;

impl<'m> Vm<'m> {
    /// Append to the program's stdout buffer, then stream it if this run is live.
    pub(crate) fn emit_stdout(&mut self, text: &str) {
        self.out.stdout.push_str(text);
        if self.out.live {
            self.flush_live(noeta_stdlib::Stream::Stdout);
        }
    }

    /// Append `text` and a newline — `Op::Echo`'s shape, kept as its own entry point so the hot path
    /// still appends straight into the buffer rather than growing the rendered string first.
    pub(crate) fn emit_stdout_line(&mut self, text: &str) {
        self.out.stdout.push_str(text);
        self.out.stdout.push('\n');
        if self.out.live {
            self.flush_live(noeta_stdlib::Stream::Stdout);
        }
    }

    /// Append to the program's stderr buffer, then stream it if this run is live.
    pub(crate) fn emit_stderr(&mut self, text: &str) {
        self.out.stderr.push_str(text);
        if self.out.live {
            self.flush_live(noeta_stdlib::Stream::Stderr);
        }
    }

    /// Merge a harvested worker isolate's program output into this run's buffers — the hand-back a
    /// shared-nothing isolate needs, since the worker's `RunOutput` lives on its own thread and is
    /// unreachable from here (see [`IsolateOutput`] for the ordering contract and why the block is
    /// appended at *this* point rather than interleaved).
    ///
    /// Routed through `emit_*` rather than a raw `push_str` so a live parent streams the block just
    /// like its own writes; on a batch run (`noeta test`, an embedder) the two entry points are the
    /// same append.
    pub(crate) fn merge_isolate_output(&mut self, output: IsolateOutput) {
        if output.is_empty() {
            return;
        }
        if !output.stdout.is_empty() {
            self.emit_stdout(&output.stdout);
        }
        if !output.stderr.is_empty() {
            self.emit_stderr(&output.stderr);
        }
    }

    /// Hand every **completed line** in `stream`'s batch buffer to the host's live-output door,
    /// removing exactly what was written.
    ///
    /// Only whole lines leave, so a `io.out("Total: ")` followed by `io.outln(n)` still reaches the
    /// terminal as one line rather than two writes — with [`LIVE_OUTPUT_CHUNK`] as the bound for a
    /// program that never terminates a line at all. A host that unexpectedly declines the write
    /// (`stream_output` → `false`) gets its text put back and the run reverts to batch capture, so
    /// no output can be lost between the two policies.
    fn flush_live(&mut self, stream: noeta_stdlib::Stream) {
        let buffer = match stream {
            noeta_stdlib::Stream::Stderr => &mut self.out.stderr,
            _ => &mut self.out.stdout,
        };
        let cut = match buffer.rfind('\n') {
            Some(at) => at + 1,
            None if buffer.len() >= LIVE_OUTPUT_CHUNK => buffer.len(),
            None => return,
        };
        let text: String = buffer.drain(..cut).collect();
        if !self.persist.host.stream_output(stream, &text) {
            let buffer = match stream {
                noeta_stdlib::Stream::Stderr => &mut self.out.stderr,
                _ => &mut self.out.stdout,
            };
            buffer.insert_str(0, &text);
            self.out.live = false;
        }
    }

    /// Build a VM ready to run `module` — resolving every derived table (shapes, packed schemas,
    /// methods, destructors, defaults, derives) but **without running `main`** (isolates I.4b). The
    /// normal entry points run `main` right after; a worker isolate instead seeds its globals from the
    /// parent's marshalled snapshot and calls one function, so it must be able to load the module
    /// without triggering the top-level program's side effects.
    pub(crate) fn load(
        module: &'m Module,
        host: Box<dyn noeta_stdlib::Host>,
        executor: Box<dyn noeta_stdlib::Executor>,
    ) -> Vm<'m> {
        // One shared `&'static Shape` per shape-table entry, then resolve each packed-list layout against it.
        // Schemas are interned inner-before-outer, so a nested struct's schema (a lower index) is always
        // built before the parent that references it.
        let shapes: Vec<&'static Shape> = module
            .shapes
            .iter()
            .cloned()
            .map(noeta_object::intern_shape)
            .collect();
        let mut packed_schemas: Vec<&'static noeta_object::PackedSchema> =
            Vec::with_capacity(module.packed_schemas.len());
        for def in &module.packed_schemas {
            let fields = def
                .fields
                .iter()
                .map(|f| match f {
                    noeta_bytecode::PackedFieldDef::Int => noeta_object::PackedKind::Int,
                    noeta_bytecode::PackedFieldDef::Float => noeta_object::PackedKind::Float,
                    noeta_bytecode::PackedFieldDef::F32 => noeta_object::PackedKind::F32,
                    noeta_bytecode::PackedFieldDef::F64 => noeta_object::PackedKind::F64,
                    noeta_bytecode::PackedFieldDef::IntN { bits, signed } => {
                        noeta_object::PackedKind::IntN {
                            bits: *bits,
                            signed: *signed,
                        }
                    }
                    noeta_bytecode::PackedFieldDef::Bool => noeta_object::PackedKind::Bool,
                    noeta_bytecode::PackedFieldDef::Struct(idx) => {
                        noeta_object::PackedKind::Struct(packed_schemas[*idx as usize])
                    }
                })
                .collect();
            packed_schemas.push(noeta_object::intern_schema(noeta_object::PackedSchema {
                // A bare-scalar element carries no shape (`None`) — it materializes to a bare `int`/`f32`.
                shape: def.shape.map(|i| shapes[i as usize]),
                fields,
                byte_size: def.byte_size as usize,
                column: def.column,
            }));
        }
        // Build one shared `Rc<TypeRepr>` per interned reflected element type (R1), so each tagged
        // `MakeList` is a cheap `Rc` clone rather than a fresh `TypeRepr` allocation per execution.
        let type_reprs: Vec<Rc<noeta_ast::reflect::TypeRepr>> =
            module.type_reprs.iter().cloned().map(Rc::new).collect();
        Self::load_with(
            module,
            SessionState {
                globals: vec![Value::unbound(); module.global_names.len()],
                global_order: Vec::new(),
                channels: Vec::new(),
                channel_progress: 0,
                ext_arena: Vec::new(),
                ext_arena_free: Vec::new(),
                embed_handles: Vec::new(),
                embed_handles_free: Vec::new(),
                ext_state: Vec::new(),
                ext_closed_gates: Vec::new(),
                shapes,
                packed_schemas,
                type_reprs,
                host,
                executor,
                registry: None,
            },
        )
    }

    /// Build a VM over a **ready persistent state** (audit-1 finding 4): [`Vm::load`] hands a fresh
    /// one, a session entry hands the session's own back in ([`Vm::load_seeded`]) — one move, so no
    /// persistent field can be silently dropped between entries. Everything built here is per-entry
    /// scratch or a module-derived *name* table; `persist`'s identity-carrying derived tables
    /// (shapes / packed schemas / type reprs / globals sizing) must already cover `module`
    /// (`SessionState::sync_to` on the seeded path, the fresh build in `load` otherwise).
    pub(crate) fn load_with(module: &'m Module, persist: SessionState) -> Vm<'m> {
        // Cached: fixed per host (see the `tel_on` field).
        let tel_on = persist.host.tel_enabled();
        // Likewise fixed per host: whether program output streams to the terminal as it is produced
        // (`noeta run`) or batches until teardown (the `@test` runner, the differential's sandbox).
        let host_streams_output = persist.host.streams_output();
        let mut methods: HashMap<String, HashMap<String, u32>> = HashMap::new();
        for m in &module.methods {
            methods
                .entry(m.type_name.clone())
                .or_default()
                .insert(m.method.clone(), m.proto);
        }
        // Name → global slot, for the free-function `Op::Invoke`. A later slot wins, matching the
        // compiler's own `global_slots` map (which a rebinding overwrites in place), so the VM
        // resolves a name to the same slot the statically-compiled `Op::CallGlobal` would.
        let global_slots: HashMap<String, u32> = module
            .global_names
            .iter()
            .enumerate()
            .map(|(slot, name)| (name.clone(), slot as u32))
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
        // Lift the `@derive(Deserialize<Json>)` decode recipes into a name→recipe map (L2.2 DI) so
        // `Op::DecodeTyped` can look up a runtime type name in O(1).
        let deserialize_recipes = module
            .deserialize_recipes
            .iter()
            .cloned()
            .collect::<HashMap<_, _>>();
        // P-PKEY: register each key-capable type's field names once, so a packed map key —
        // which carries only (type name, field values) — can derive its display on demand.
        // Over `persist.shapes` (idempotent, process-global), so a session entry that introduced
        // a new key-capable type registers it exactly like a fresh load does.
        for shape in &persist.shapes {
            if shape.key_capable {
                noeta_stdlib::map_key::packed_names::register(&shape.name, shape.fields.iter());
            }
        }
        // Resolve each packed `map(...)` result site to its shared schema (P-PACK 2.6 category B).
        // Against the persistent (identity-carrying) schemas, so on a seeded entry an old span
        // still resolves to the same shared schema.
        let map_packed: HashMap<Span, &'static noeta_object::PackedSchema> = module
            .map_packed_sites
            .iter()
            .map(|(span, idx)| (*span, persist.packed_schemas[*idx as usize]))
            .collect();
        Vm {
            module,
            debug_session: None,
            hot_mailbox: None,
            applied_swaps: 0,
            pure_eval: false,
            stall_active: false,
            registered_workers: 0,
            persist,
            map_packed,
            methods,
            global_slots,
            destructors,
            field_defaults,
            destruct_reachable,
            comparable_derives,
            tojson_derives,
            deserialize_recipes,
            sched: SchedState {
                scopes: Vec::new(),
                scope_closed: Vec::new(),
                ctx_current: Vec::new(),
                current_strand: 1,
                next_strand: 2,
                tel_on,
                traced_futures: Vec::new(),
            },
            ctx_table_pool: Vec::new(),
            reentry_pool: Vec::new(),
            cache_pool: Vec::new(),
            run_depth: 0,
            transient_roots: Vec::new(),
            gc_suspended: false,
            isolates: IsolateState {
                parallel_isolates: false,
                isolate_module: None,
                isolate_factory: None,
                profile_seam: None,
                isolates: Vec::new(),
                inflight_isolates: 0,
                shared_region: noeta_value::SharedRegion::new(),
                promote_memo: HashMap::new(),
                promote_sources: Vec::new(),
                unshippable_globals: HashMap::new(),
                cancel_flag: None,
                cancel_signal: None,
                cancel_observed: false,
            },
            out: RunOutput {
                stdout: String::new(),
                stderr: String::new(),
                // Asked of the host once, here: whether it streams cannot change mid-run, and the
                // write path must not pay a virtual call per `echo` to re-ask.
                live: host_streams_output,
                diagnostics: Vec::new(),
                requested_exit: None,
                abort_trace: Vec::new(),
            },
            #[cfg(feature = "jit-rt")]
            tier1: Tier1State {
                #[cfg(feature = "jit")]
                jit: None,
                #[cfg(feature = "jit")]
                force_jit: false,
                #[cfg(feature = "jit")]
                aot_bodies: false,
                #[cfg(feature = "jit")]
                jit_counters: Vec::new(),
                #[cfg(feature = "jit")]
                jit_declined: Vec::new(),
                jit_ret: Value::unit(),
                jit_cache_pins: Vec::new(),
                #[cfg(feature = "jit")]
                jit_frame_template: None,
                #[cfg(feature = "jit")]
                jit_service: None,
                #[cfg(feature = "jit")]
                jit_graveyard: Vec::new(),
                #[cfg(feature = "jit")]
                jit_service_graveyard: Vec::new(),
                aot: false,
                jit_entries: Vec::new(),
                jit_fast: Vec::new(),
                #[cfg(feature = "jit")]
                jit_osr_entries: Vec::new(),
                #[cfg(feature = "jit")]
                jit_requested: Vec::new(),
                #[cfg(feature = "jit")]
                jit_osr_inflight: Vec::new(),
                #[cfg(feature = "jit")]
                jit_osr_pending: Vec::new(),
                #[cfg(feature = "jit")]
                jit_pending: 0,
                #[cfg(feature = "jit")]
                jit_final_stats: None,
                #[cfg(feature = "jit")]
                jit_drain_at_exit: false,
                jit_bail_counts: None,
            },
            debugger: None,
            profiler: None,
        }
    }

    /// The extension registry this VM resolves native names against (instance-registry IR3) — the
    /// VM twin of `Checker::reg`. Every native dispatch and lookup goes through here so that an
    /// instance-scoped registry (an embed session's own extension set) takes effect uniformly; an
    /// unset field falls back to the process-global default, keeping every ordinary run unchanged.
    /// Returns `&'static` (the registry only ever hands out static extension data), so a caller may
    /// bind it once and use it past a later `&mut self` borrow.
    pub(crate) fn reg(&self) -> &'static noeta_stdlib::registry::Registry {
        self.persist
            .registry
            .unwrap_or_else(noeta_stdlib::registry::default_seeded)
    }
}

thread_local! {
    /// The count of **live session heap-owners** on this thread. A [`VmSession`]'s persistent
    /// `SessionState` holds live heap objects for the whole session, and the value heap is
    /// *thread-local* — so two embed sessions on one thread share one heap's live-object registry.
    ///
    /// [`Vm::teardown`]'s backup mark-sweep reclaims everything unreachable from the tearing-down
    /// VM's roots; with a sibling session still alive, that set wrongly includes the sibling's live
    /// objects, and the sibling's own later teardown then double-frees them (heap corruption). So a
    /// session's teardown runs the destructive sweeps ONLY when it is the **last** owner on the
    /// thread — an earlier session's cycle garbage is reclaimed by the final owner's empty-root sweep
    /// instead (deferred, never double-freed). Plain runs register nothing (count stays 0), so their
    /// teardown is unchanged. Per-thread, so one-session-per-thread (the concurrent model) sweeps
    /// normally on each thread.
    static SESSION_HEAP_OWNERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Register a live session heap-owner on this thread (a [`VmSession`] began holding persistent state).
/// Only [`VmSession`] (behind `compile`) registers; a plain/AOT run keeps the count at 0.
#[cfg(feature = "compile")]
pub(crate) fn session_owner_enter() {
    SESSION_HEAP_OWNERS.with(|c| c.set(c.get() + 1));
}

/// Retire a session heap-owner (its `SessionState` is being torn down). Called *before* the owner's
/// [`Vm::teardown`], so the remaining count that gates the sweep reflects only the *siblings*.
#[cfg(feature = "compile")]
pub(crate) fn session_owner_exit() {
    SESSION_HEAP_OWNERS.with(|c| c.set(c.get().saturating_sub(1)));
}

/// Whether the VM tearing down now is the last heap-owner on its thread — i.e. no *other* live
/// session's objects sit in the shared registry that a destructive sweep would wrongly reclaim.
fn is_last_heap_owner() -> bool {
    SESSION_HEAP_OWNERS.with(|c| c.get()) == 0
}

/// The number of live session heap-owners on this thread — the safepoint-GC gate's input: a
/// mid-run trace sweep is sound only when at most this VM's own session shares the thread's heap
/// registry (a sibling session's live objects are not in this VM's roots). See
/// [`Vm::maybe_safepoint_gc`].
pub(crate) fn session_heap_owner_count() -> usize {
    SESSION_HEAP_OWNERS.with(|c| c.get())
}

impl<'m> Vm<'m> {
    /// Tear the VM down after its entry chunk(s) ran and drain the [`RunResult`]: reap reference
    /// cycles, drain channel buffers, clear the reactive graph, destroy the globals in reverse binding
    /// order (running each destructor), reap any remaining cycle garbage, and join outstanding isolate
    /// workers. Split from [`Vm::run_top`] so a session runs this **once** at the end rather than after
    /// every entry (REPL-on-VM R0); leak residency must reach zero here.
    pub(crate) fn teardown(&mut self, mode: noeta_value::CollectorMode) -> RunResult {
        // Exit reached: suspend + disarm the safepoint-GC trigger. The destructor bodies teardown
        // runs below execute against a heap mid-surgery, and the exit collections reclaim
        // everything a pending safepoint would have.
        self.gc_suspended = true;
        noeta_value::safepoint_gc_disarm();
        // Reap reference cycles the program may have tied through `mut` fields / cells / closures that
        // refcounting alone cannot reclaim (e.g. a self-recursive nested `fn`). The two collectors run at
        // different points: the **trace** marks from the live globals *before* teardown (the frame stack
        // is unwound, so the globals are the whole root set) and sweeps everything unreachable; the
        // **trial-deletion** path instead reaps its buffered candidates *after* teardown, once every frame
        // and global release has had a chance to buffer the cycle's roots.
        // The two trace sweeps below reclaim everything unreachable from *this* VM's roots — sound
        // only when no sibling session's live objects share the thread's heap registry. When another
        // session is still alive (`!is_last_heap_owner`), skip them: this session's own cycle garbage
        // is instead reaped by the final owner's empty-root sweep (deferred, never double-freed). The
        // refcount releases below still run — they only touch this session's own graph.
        let sweep = is_last_heap_owner();
        if sweep && mode == noeta_value::CollectorMode::Trace {
            let mut roots: Vec<Value> = self
                .persist
                .globals
                .iter()
                .copied()
                .filter(|v| !v.is_unbound())
                .collect();
            // The extensions' retained arena (higher-order-abi H4) holds a `+1` on every value
            // an extension owns across dispatches — the same graph treatment: feed them in as
            // roots so the sweep cannot reclaim a value the arena release below would then
            // double-free.
            roots.extend(self.persist.ext_arena.iter().copied().flatten());
            // Embed handles (server-hmr F3) hold a `+1` each — the same root treatment as the
            // arena, so a host-held value is not reclaimed out from under the host.
            roots.extend(self.persist.embed_handles.iter().copied().flatten());
            // Traced futures (native-otel T5c) hold a `+1` each — the same graph treatment.
            roots.extend(self.sched.traced_futures.iter().map(|t| t.future));
            let garbage = collect_trace(&roots);
            self.reclaim_cycle_garbage(garbage);
        }
        // Release any messages still buffered in channels at program end (isolates I.1) — undrained
        // `send`s. Draining here keeps residency at zero; `release_value` runs any message destructor. A
        // `Shared` channel (I.4c) holds `Wire` copies, not heap `Value`s, so dropping it frees cleanly.
        for chan in std::mem::take(&mut self.persist.channels) {
            if let Channel::Local { buffer, .. } = chan {
                for (msg, _) in buffer {
                    self.release_value(msg);
                }
            }
        }
        // Release every value still in the extensions' retained arena (higher-order-abi H4):
        // values an extension held across dispatches and the program never released (an
        // undisposed `Cell`, an undisposed signal — reactivity lives here too since H5).
        // Destructor-aware, so
        // residency returns to zero — the leak oracle's proof the arena's refcounting is exact.
        for value in std::mem::take(&mut self.persist.ext_arena)
            .into_iter()
            .flatten()
        {
            self.release_value(value);
        }
        self.persist.ext_arena_free.clear();
        // Release every value a host still holds a handle to (server-hmr F3): a forgotten handle
        // reclaims here, destructor-aware, so residency returns to zero.
        for value in std::mem::take(&mut self.persist.embed_handles)
            .into_iter()
            .flatten()
        {
            self.release_value(value);
        }
        self.persist.embed_handles_free.clear();
        // Release any still-traced futures (native-otel T5c) — an abandoned `with_span`-async
        // future whose span never ended. The reference releases destructor-aware (residency 0);
        // the span simply stays unended (the recorder/exporter only consume ended spans).
        for traced in std::mem::take(&mut self.sched.traced_futures) {
            self.release_value(traced.future);
        }
        // Destroy the globals at program end in reverse declaration order, running each
        // destructor on its last reference — the deterministic destruction the spec requires.
        for slot in self.persist.global_order.clone().into_iter().rev() {
            let v = std::mem::replace(&mut self.persist.globals[slot as usize], Value::unbound());
            if !v.is_unbound() {
                self.release_value(v);
            }
        }
        // Backup collection (object-model slice 2c): a reference `class` cycle rooted in the globals
        // (`a.next = b; b.next = a`) survives the teardown above — each member still holds the other, so
        // refcounting never reaches zero. With the globals now gone there are **no roots left**, so every
        // still-live object is unreachable garbage; trace-collect from an empty root set to reclaim it,
        // running each member's `destruct` exactly once. (The pre-teardown trace above only catches
        // cycles already unreachable mid-run.)
        if sweep && mode == noeta_value::CollectorMode::Trace {
            let garbage = collect_trace(&[]);
            self.reclaim_cycle_garbage(garbage);
        }
        if sweep && mode == noeta_value::CollectorMode::TrialDeletion {
            let garbage = noeta_gc::collect_trial_deletion();
            self.reclaim_cycle_garbage(garbage);
        }

        // Join any isolate worker threads not already harvested (a structured scope harvests + joins its
        // isolates at `}`, so this is normally empty — defensive against an early exit). Their output
        // still comes home: the join is this worker's harvest point, so its block is appended here,
        // ahead of the `RunResult` this teardown builds. A worker whose outcome nobody will read
        // still wrote what it wrote, and dropping it is the bug this path used to share.
        for slot in std::mem::take(&mut self.isolates.isolates) {
            if let Some(h) = slot.handle {
                let _ = h.join();
            }
            // The thread has ended, so its report — if it sent one at all — is already queued.
            if let Ok(report) = slot.result.try_recv() {
                self.merge_isolate_output(report.output);
            }
        }
        // Drop any stall-registry worker slots not released by a harvest (isolates I.4c) — an early
        // exit joins the worker here without going through `finish_isolate`. Balanced by count, so the
        // registry returns to a clean `active`.
        while self.registered_workers > 0 {
            self.deregister_worker_stall();
        }
        // Every worker is joined, so nothing borrows the shared region: free any promoted
        // argument graphs (P-PAR S2) — normally already emptied by `finish_isolate` at in-flight
        // count 0; defensive here so the leak oracle's zero-residency balance holds on early
        // exits too.
        self.free_shared_region();

        // Shut the off-thread compile service down LAST (P-PAR S4): the destructors above may
        // have called compiled code, and shutdown drops the code pages with the engine. The
        // mirrors are cleared first so no stale entry can outlive its pages; the service's final
        // compile accounting parks on the VM for the stats entry points.
        #[cfg(feature = "jit")]
        if let Some(service) = self.tier1.jit_service.take() {
            self.tier1.jit_entries.clear();
            self.tier1.jit_fast.clear();
            self.tier1.jit_osr_entries.clear();
            self.tier1.jit_final_stats = service.shutdown(self.tier1.jit_drain_at_exit);
        }

        // A deliberate `os.exit(code)` wins over the diagnostic-derived code (there are no
        // diagnostics on that path — the halt is clean). Otherwise the code is derived from whether
        // the run **aborted** — an *error* — not from whether it said anything: a program's exit
        // code is its own outcome, and an advisory diagnostic is not a failure. (Every runtime
        // diagnostic is an abort today, so this is the same value; spelling it `is_empty()` is how
        // the first advisory runtime diagnostic would silently start failing programs.)
        let exit_code = self
            .out
            .requested_exit
            .unwrap_or(u8::from(noeta_diagnostics::has_errors(&self.out.diagnostics)).into());
        RunResult {
            stdout: std::mem::take(&mut self.out.stdout),
            stderr: std::mem::take(&mut self.out.stderr),
            exit_code,
            diagnostics: std::mem::take(&mut self.out.diagnostics),
        }
    }
}

/// Run one real-thread isolate to completion (isolates I.4b), on its own thread. Builds a fresh VM with
/// its own heap (thread-local), host, and executor from `factory`, seeds globals from the parent's
/// marshalled snapshot, rebuilds the arguments, calls `callee(args)` and drives the resulting future to
/// completion, then marshals the result back to `Send` [`isolate::Wire`]. An abort inside the isolate
/// (a panic) comes back as [`IsolateOutcome::Failed`], which the parent re-raises at the `.await`. The
/// worker tears down its own globals/channels so its thread-local heap returns to zero residency.
///
/// `cancel` is the parent's cancellation flag (isolate-cancel): the worker installs it on its own VM
/// so the dispatch loop's safepoints and its scheduler rounds poll it, and unwinds to
/// [`IsolateOutcome::Cancelled`] when it is set. Teardown runs either way, so a cancelled worker
/// frees its heap exactly like a completed one.
///
/// The [`IsolateReport`] carries the worker's **program output** home alongside its outcome — read
/// after the worker's teardown, so a destructor that echoes on the way out is included, and shipped
/// on every arm (a cancelled or failed worker wrote what it wrote). See [`IsolateOutput`] for the
/// ordering contract the parent merges it under.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_isolate_worker(
    module: &Arc<Module>,
    factory: &IsolateFactory,
    profile_seam: Option<(ProfileHookFactory, ProfileSink)>,
    proto: u32,
    iso_args: Vec<isolate::IsoArg>,
    wire_globals: Vec<(u32, isolate::Wire)>,
    unshippable_globals: Vec<(u32, String)>,
    trace: Option<noeta_stdlib::TraceContext>,
    registry: Option<&'static noeta_stdlib::registry::Registry>,
    stall_tracked: bool,
    cancel: Arc<noeta_stdlib::CancelSignal>,
    span: Span,
) -> IsolateReport {
    noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    let (mut host, mut executor) = factory();
    // Arm the executor and the host against this worker's cancellation (interruptible-io) *before*
    // the VM is built, so the request can never land in the window between construction and the
    // first suspension: a wake that already fired runs the hook at registration. The executor is
    // built on THIS thread by the factory (a `RealExecutor` owns a tokio runtime, which is not
    // `Send`), which is why the wake travels out to the parent through a shared handle rather than
    // the executor travelling home.
    //
    // Both, and for different holes. The executor's hook ends a *wait* — a worker parked on a long
    // timer. The host's ends a *read* — a worker parked on a child that has stopped talking, which
    // no timer will ever end. They share the token and nothing else.
    host.set_cancel(cancel.flag(), cancel.wake());
    executor.set_cancel_wake(cancel.wake());
    let mut wvm = Vm::load(module, host, executor);
    // Per-isolate profiling (injected by `noeta profile`): this worker gets its own collector, and
    // the seam propagates so isolates IT spawns are profiled too. The display name is the spawned
    // function's; its unique `#n` is assigned at HARVEST under the sink's lock (spawn-time
    // numbering would race — concurrent isolates all read the same sink length).
    let profile_fn_name = profile_seam.as_ref().map(|(factory, _)| {
        let fn_name = module.protos[proto as usize]
            .name
            .clone()
            .unwrap_or_else(|| "<anonymous>".to_string());
        wvm.profiler = Some(factory(&fn_name));
        fn_name
    });
    if let Some(seam) = &profile_seam {
        wvm.isolates.profile_seam = Some(seam.clone());
    }
    // Resolve native names against the spawner's registry (instance-registry IR3); `None` falls
    // back to the process-global default, exactly like the parent.
    wvm.persist.registry = registry;
    // Inherit the spawner's trace context across the thread boundary (native-otel T5d): the worker
    // has its OWN host (span handles don't transfer), so the W3C context is interned as a remote
    // seed at the worker's root — its spans then continue the spawner's trace, exactly as a
    // cooperative task inherits via its Task context (T5a). Real-path parity with the sandbox.
    if let Some(ctx) = trace
        && wvm.persist.host.tel_enabled()
    {
        let seed = wvm.persist.host.tel_intern_remote(ctx);
        wvm.sched.ctx_current.push(seed);
    }
    wvm.isolates.parallel_isolates = true;
    wvm.isolates.isolate_module = Some(Arc::clone(module));
    wvm.isolates.isolate_factory = Some(factory.clone());
    // Install the parent's cancellation flag (isolate-cancel) *before* any user code runs, so a
    // cancel that lands during startup is honored at the body's first safepoint.
    wvm.isolates.cancel_flag = Some(cancel.flag());
    wvm.isolates.cancel_signal = Some(cancel);
    // Seed the worker's globals from the parent's snapshot so the isolate body can call other
    // top-level functions (and read value-type constants). Slots match: parent and worker share the
    // same `Arc<Module>`, so a global's `GlobalId` is identical on both sides (P-VMT-GSLOT).
    for (slot, wire) in &wire_globals {
        let value = isolate::rebuild(wire, &wvm.persist.shapes, &mut wvm.persist.channels);
        wvm.persist.globals[*slot as usize] = value;
        wvm.persist.global_order.push(*slot);
    }
    // Record the globals the parent could not ship (isolates I.4b): their slots stay unbound, but
    // reading one now names the offending global + type instead of "cannot find `x`".
    wvm.isolates.unshippable_globals = unshippable_globals.into_iter().collect();
    let arg_vals: Vec<Value> = iso_args
        .iter()
        .map(|a| match a {
            isolate::IsoArg::Copied(w) => {
                isolate::rebuild(w, &wvm.persist.shapes, &mut wvm.persist.channels)
            }
            // A borrowed shared-region root (P-PAR S2): usable as-is — no rebuild, no retain.
            // The worker's ordinary retain/release discipline no-ops on it (shared tag), its
            // COW gates copy instead of mutating (`is_uniquely_owned` is false), and the
            // parent's region outlives this thread (freed only after the join).
            isolate::IsoArg::Borrowed(root) => root.value(),
        })
        .collect();
    // Arm the worker's own safepoint-GC trigger (per-isolate: all trigger state is thread-local),
    // so a cycle-building isolate body bounds its residency at its own safepoints.
    noeta_value::safepoint_gc_arm(noeta_value::safepoint_gc_default_threshold());
    let callee = Value::closure(proto, Vec::new());
    // Participate in the global all-parties-blocked deadlock check (isolates I.4c) iff the parent
    // does, so a cross-isolate deadlock among workers resolves to E0010 rather than spinning. The
    // worker's `active` **slot is registered by the parent at spawn** (not here), so `active` never
    // lags this thread's startup — the fix for the startup-window false positive.
    wvm.stall_active = stall_tracked;
    // This depth-0 call/drive holds `callee` (and then `future`) only in Rust locals — root them
    // through `transient_roots` so a safepoint collection inside the body stays exact.
    wvm.transient_roots.push(callee);
    let outcome = match wvm.call_value(callee, arg_vals, span) {
        Ok(future) => {
            wvm.transient_roots.push(future);
            let result = wvm.drive_future(future, span, Some((&[], &[])));
            release(future);
            result
        }
        Err(abort) => Err(abort),
    };
    wvm.transient_roots.clear();
    release(callee);
    // Worker teardown below runs destructors against a heap being dismantled — stop collecting.
    wvm.gc_suspended = true;
    noeta_value::safepoint_gc_disarm();
    // Hand the worker's finished collector to the sink (before teardown — destructor ops after the
    // program's own work are not the profile's subject). The `#n` is the sink's running count,
    // assigned under the push's own lock so concurrent isolates never collide.
    if let (Some((_, sink)), Some(fn_name)) = (&profile_seam, profile_fn_name)
        && let Some(hook) = wvm.profiler.take()
        && let Ok(mut sink) = sink.lock()
    {
        let name = format!("isolate {fn_name} #{}", sink.len() + 1);
        sink.push((name, hook));
    }
    let outcome = match outcome {
        Ok(result) => {
            let marshalled = isolate::marshal(result, &wvm.persist.shapes, &wvm.persist.channels)
                .map(IsolateOutcome::Done)
                .unwrap_or_else(|e| {
                    // The body completed; only the result failed to ship — there is no abort stack.
                    IsolateOutcome::Failed(IsolateFailure {
                        message: format!("isolate result is not shippable: {e}"),
                        trace: Vec::new(),
                    })
                });
            wvm.release_value(result);
            marshalled
        }
        // A safepoint cancellation unwinds as an ordinary abort but is **not** a failure
        // (isolate-cancel): the worker was asked to stop and did. Reported before the failure arm so
        // its (empty) diagnostics are never rendered as an error.
        Err(_abort) if wvm.isolates.cancel_observed => IsolateOutcome::Cancelled,
        // Ship the worker's own abort traceback home with the message (plain data — it crosses the
        // boundary like any `Wire`), so the parent's rendered trace includes the worker's frames.
        Err(_abort) => IsolateOutcome::Failed(IsolateFailure {
            message: wvm
                .out
                .diagnostics
                .last()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| "isolate aborted".to_string()),
            trace: std::mem::take(&mut wvm.out.abort_trace),
        }),
    };
    // Tear the worker down so its thread-local heap returns to zero residency: release the JIT
    // inline caches' closure pins (S4.2), reap reference cycles, destroy globals in reverse
    // declaration order, then drain any channel buffers. This mirrors the main heap's
    // [`Vm::teardown`] exit reapers (isolates I.4b worker-teardown gap): a worker body can strand a
    // reference cycle (`a.next = b; b.next = a` on a `class`) that refcounting alone never reclaims,
    // so without a cycle pass here it — and its `__destruct` — leaked until the thread died. `gc_
    // suspended` is already set (above), and `reclaim_cycle_garbage` manages it around each
    // destructor, so these explicit collections run correctly after the safepoint trigger is
    // disarmed. The worker always collects in `Trace` mode (set at entry).
    #[cfg(feature = "jit")]
    for v in std::mem::take(&mut wvm.tier1.jit_cache_pins) {
        release(v);
    }
    // Pre-teardown trace: the frame stack is unwound, so the still-bound globals (plus the arena /
    // traced-future roots released below) are the whole root set — sweep everything unreachable
    // from them, running each dead member's `__destruct` exactly once (container-before-contained),
    // exactly as the main heap does. This reclaims a cycle already stranded mid-run.
    {
        let mut roots: Vec<Value> = wvm
            .persist
            .globals
            .iter()
            .copied()
            .filter(|v| !v.is_unbound())
            .collect();
        roots.extend(wvm.persist.ext_arena.iter().copied().flatten());
        roots.extend(wvm.sched.traced_futures.iter().map(|t| t.future));
        let garbage = collect_trace(&roots);
        wvm.reclaim_cycle_garbage(garbage);
    }
    for chan in std::mem::take(&mut wvm.persist.channels) {
        if let Channel::Local { buffer, .. } = chan {
            for (msg, _) in buffer {
                wvm.release_value(msg);
            }
        }
    }
    // Release the worker's extension arena (per-isolate, higher-order-abi H4/H5): whatever its
    // program's extensions still held — signals, cells — drops here, destructor-aware.
    for value in std::mem::take(&mut wvm.persist.ext_arena)
        .into_iter()
        .flatten()
    {
        wvm.release_value(value);
    }
    // And its still-traced futures (native-otel T5c), same treatment.
    for traced in std::mem::take(&mut wvm.sched.traced_futures) {
        wvm.release_value(traced.future);
    }
    for slot in wvm.persist.global_order.clone().into_iter().rev() {
        let value = std::mem::replace(&mut wvm.persist.globals[slot as usize], Value::unbound());
        if !value.is_unbound() {
            wvm.release_value(value);
        }
    }
    // Backup collection: a reference `class` cycle rooted in the globals survives the destruction
    // above (each member still holds the other), but with the globals now gone there are no roots
    // left — trace from an empty root set to reclaim it, running each member's `__destruct` exactly
    // once. The main heap's teardown ends the same way.
    let garbage = collect_trace(&[]);
    wvm.reclaim_cycle_garbage(garbage);
    // Take the worker's output **last** — after teardown — so a `__destruct` that echoes on the way
    // out is part of the block the parent merges, exactly as it would be for the main program. On a
    // live run the completed lines have already been streamed and drained, so what is left here is
    // the unterminated tail and nothing else.
    IsolateReport {
        output: IsolateOutput {
            stdout: std::mem::take(&mut wvm.out.stdout),
            stderr: std::mem::take(&mut wvm.out.stderr),
        },
        outcome,
    }
}

impl<'m> Vm<'m> {
    /// Materialize the `#[type_name(...)]` attributes from the module manifest into a
    /// `List<Attributed<T>>` — each a real `T` struct (built from its stored args) paired with its
    /// target. Shapes are built fresh from the shared reflection info; because shape equality is
    /// structural (name + fields), they match the tree-walker's by construction.
    pub(crate) fn materialize_attributes(&self, type_name: &str) -> Value {
        let attributed_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::ATTRIBUTED,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::ATTRIBUTED),
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
                let t_shape =
                    noeta_object::intern_shape(Shape::object(kind, type_name, fields.clone()));
                let t_value = Value::object(t_shape, values);
                Value::object(attributed_shape, vec![Value::string(&a.target), t_value])
            })
            .collect();
        Value::list(items)
    }

    /// Materialize the `(declaration, Role)` index from the module's reflection info into a
    /// `List<RoleBinding>` — each `{ target: string, role: Role }`. Shapes are built fresh; because
    /// shape equality is structural (name + variant + fields), the `Role` enum and `RoleBinding`
    /// struct match the tree-walker's by construction. (P2.7.)
    pub(crate) fn materialize_roles(&self, role_enum: Option<&str>) -> Value {
        let binding_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::ROLE_BINDING,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::ROLE_BINDING),
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .roles
            .iter()
            // `roles_of::<E>()` keeps only bindings of enum `E`; bare `roles_of()` keeps all.
            .filter(|r| role_enum.is_none_or(|e| r.enum_name == e))
            .map(|r| {
                Value::object(
                    binding_shape,
                    vec![
                        Value::string(&r.target),
                        make_role(&r.enum_name, &r.variant),
                    ],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a callable's declared parameter list from the module's reflection info into a
    /// `List<ParamInfo>` — each `{ name: string, type: Type, optional: bool, attrs: List<dyn> }`.
    /// `type` is the prelude `Type` ADT value built from the parameter's declared type (the same
    /// `build_type_value` `type_of` uses), `optional` reports whether the parameter declared a
    /// default, and `attrs` holds the parameter's `#[...]` attribute instances. The `ParamInfo`
    /// shape is built fresh; because shape equality is structural, it matches the tree-walker's by
    /// construction. An unknown target yields an empty list.
    ///
    /// `attrs` is **joined from the attribute manifest**, not carried in the parameter record: the
    /// rows are exactly the ones `attributes_of::<T>()` returns for the same parameter, reached
    /// through the shared `param_attributes_for` key. So the two query surfaces are two renderings
    /// of one table, and a parameter attribute cannot be visible through one and missing from the
    /// other.
    pub(crate) fn materialize_params(&self, target: &str) -> Value {
        let info_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::PARAM_INFO,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::PARAM_INFO),
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .params_for(target)
            .iter()
            .map(|p| {
                Value::object(
                    info_shape,
                    vec![
                        Value::string(&p.name),
                        build_type_value(&p.ty),
                        Value::bool(p.optional),
                        self.materialize_param_attrs(target, &p.name),
                    ],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a callable's declared **return type** from the module's reflection info into a
    /// `?Type` — `some(t)` for a known callable, `none` when the target names none. `t` is built by
    /// the very same `build_type_value` `materialize_params` uses for `ParamInfo.type`, so the two
    /// reflection surfaces cannot render a declared type differently. The tree-walker materializes it
    /// the same way, so the values agree across the differential by construction.
    ///
    /// `none` for an unknown target rather than a fabricated `Type.Unit`: `void` is a real return
    /// type, so an unknown callable must be distinguishable from a `void` one.
    pub(crate) fn materialize_returns(&self, target: &str) -> Value {
        match self.module.reflection.returns_for(target) {
            Some(repr) => crate::values::make_some(build_type_value(repr)),
            None => crate::values::make_none(),
        }
    }

    /// One parameter's `#[...]` attributes, materialized into a `List<dyn>` of attribute-struct
    /// instances. Each instance is built exactly as `materialize_attributes` builds it — same
    /// `attribute_shape`, same `materialize_args` field resolution — so the value a consumer reads
    /// off `ParamInfo.attrs` is indistinguishable from the one it would read off an `Attributed`.
    pub(crate) fn materialize_param_attrs(&self, callable: &str, param: &str) -> Value {
        self.materialize_attr_instances(
            self.module.reflection.param_attributes_for(callable, param),
        )
    }

    /// A member's manifest rows, materialized into a `List<dyn>` of attribute-struct instances —
    /// the one builder both `ParamInfo.attrs` and `FieldSpec.attrs` go through, so "a field's
    /// attributes are built exactly as a parameter's" is a shared call rather than a claim two
    /// copies have to keep. An empty row set yields an empty list, never an absence.
    pub(crate) fn materialize_attr_instances(
        &self,
        records: Vec<&noeta_ast::reflect::AttributeRecord>,
    ) -> Value {
        let items: Vec<Value> = records
            .into_iter()
            .map(|a| {
                let shape = noeta_ast::reflect::attribute_shape(&a.name, &self.module.reflection);
                let kind = if shape.is_struct {
                    ShapeKind::Struct
                } else {
                    ShapeKind::Class
                };
                let values: Vec<Value> =
                    noeta_ast::reflect::materialize_args(a, &shape.fields, &shape.defaults)
                        .iter()
                        .map(|v| attr_value_to_vm(v, &self.module.reflection))
                        .collect();
                let t_shape =
                    noeta_object::intern_shape(Shape::object(kind, &a.name, shape.fields.clone()));
                Value::object(t_shape, values)
            })
            .collect();
        Value::list(items)
    }

    /// Materialize the qualified trait names a value's nominal type implements into a sorted,
    /// deduped `List<string>` — the reflection `traits_of(value)`. Reads the SAME membership table
    /// (`Module::reflection.trait_impls`) `NarrowTarget::DynTrait` tests, so the query and the
    /// narrowing cannot disagree; a non-nominal value yields the empty list (mirroring
    /// `fields_of`'s non-object answer). The tree-walker reads the identical shared table, so the
    /// values agree across the differential by construction.
    pub(crate) fn materialize_traits(&self, value: Value) -> Value {
        let items: Vec<Value> = vm_nominal_name(&value)
            .map(|n| {
                self.module
                    .reflection
                    .traits_for(&n)
                    .into_iter()
                    .map(Value::string)
                    .collect()
            })
            .unwrap_or_default();
        Value::list(items)
    }

    /// Materialize a struct/class instance's fields into a `List<FieldEntry>` (`{ name, value }`,
    /// declaration order) — the value-level reflection `fields_of` (derive layer 3). Any other
    /// value yields the empty list. The shape is built fresh (structural equality matches the
    /// tree-walker's by construction); each carried field value is retained since the new entry
    /// object holds a fresh reference.
    pub(crate) fn materialize_fields(&self, value: Value, private_fields: bool) -> Value {
        let entry_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::FIELD_ENTRY,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::FIELD_ENTRY),
        ));
        let Some((type_name, fields)) = value.object_fields_for_reflection() else {
            return Value::list(Vec::new());
        };
        // Which fields this call site may see. `private_fields` is the checker's answer for the
        // site; when it is false the door reports only what the caller could have read itself, and
        // the visibility bits come from the reflection artifact — the same `field_public` the
        // `construct` door refuses a private field by. A type with no private fields (every struct,
        // and a class that declared them all `pub`) filters nothing, so the common case is a lookup
        // that finds every name.
        let hidden = match private_fields {
            true => Vec::new(),
            false => self
                .module
                .reflection
                .field_specs(type_name)
                .into_iter()
                .filter(|spec| !spec.public)
                .map(|spec| spec.name.to_string())
                .collect(),
        };
        let items: Vec<Value> = fields
            .into_iter()
            .filter(|(name, _)| !hidden.contains(name))
            .map(|(name, field_value)| {
                noeta_gc::retain(field_value);
                Value::object(entry_shape, vec![Value::string(&name), field_value])
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a declared type's field schema into a `List<FieldSpec>` (`{ name, type, optional,
    /// attrs }`, declaration order) — the type-level reflection `field_specs_of`. An unknown or
    /// non-fielded type yields the empty list. Each `type` is the field's declared type (precise,
    /// from the reflection artifact). The shape is built fresh; structural shape equality matches the
    /// tree-walker's, so the materialized values agree across the differential by construction.
    ///
    /// `attrs` is **joined from the attribute manifest**, exactly as `materialize_params` joins a
    /// parameter's: the rows are the ones `attributes_of::<T>()` returns for the same field, reached
    /// through the shared `field_attributes_for` key. So the field door and the parameter door hand
    /// a schema deriver the same shape, and a field attribute cannot be visible through one query
    /// and missing from the other.
    pub(crate) fn materialize_field_specs(&self, type_name: &str) -> Value {
        let spec_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::FIELD_SPEC,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::FIELD_SPEC),
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .field_specs(type_name)
            .into_iter()
            .map(|spec| {
                Value::object(
                    spec_shape,
                    vec![
                        Value::string(spec.name),
                        build_type_value(spec.ty),
                        Value::bool(spec.optional),
                        self.materialize_attr_instances(
                            self.module
                                .reflection
                                .field_attributes_for(type_name, spec.name),
                        ),
                    ],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a declared enum's variant schema into a `List<VariantSpec>` (`{ name, payload,
    /// backing }`, declaration order) — the type-level reflection `variants_of`. An unknown type, or
    /// one that is not an enum, yields the empty list (the same contract `materialize_field_specs`
    /// answers a non-fielded type with). Each payload entry is a `FieldSpec` built by the SAME
    /// construction `materialize_field_specs` uses, so a variant payload and a struct field are the
    /// same value shape; `backing` goes through the shared `attr_value_to_vm`, so a backed value
    /// materializes exactly as the same literal does in an attribute argument. The tree-walker's
    /// `materialize_variant_specs` builds each element the same way, so the values agree across the
    /// differential by construction.
    pub(crate) fn materialize_variant_specs(&self, type_name: &str) -> Value {
        let spec_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::FIELD_SPEC,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::FIELD_SPEC),
        ));
        let variant_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::VARIANT_SPEC,
            noeta_ast::reflect::prelude_struct_fields(noeta_ast::reflect::VARIANT_SPEC),
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .variant_specs(type_name)
            .into_iter()
            .map(|variant| {
                let payload: Vec<Value> = variant
                    .payload
                    .into_iter()
                    .map(|spec| {
                        Value::object(
                            spec_shape,
                            vec![
                                Value::string(spec.name),
                                build_type_value(spec.ty),
                                Value::bool(spec.optional),
                                // Empty, and that is the true answer rather than a stub: a variant
                                // payload slot has no syntax for an attribute (the `#[…]` a variant
                                // bears is the *variant*'s, keyed `Enum.Variant`).
                                Value::list(Vec::new()),
                            ],
                        )
                    })
                    .collect();
                let backing = match variant.backing {
                    Some(value) => crate::values::make_some(crate::values::attr_value_to_vm(
                        value,
                        &self.module.reflection,
                    )),
                    None => crate::values::make_none(),
                };
                Value::object(
                    variant_shape,
                    vec![Value::string(variant.name), Value::list(payload), backing],
                )
            })
            .collect();
        Value::list(items)
    }

    /// `construct(name, fields)` — build a struct/class value of the type named by `name_val` from the
    /// field values `fields_val` (declaration order), reusing the SAME slot/defaults construction path
    /// `Op::MakeStruct` uses so defaults and full-initialization are honored identically. Returns a
    /// `Result<dyn, string>`: an unknown type, a non-list `fields`, an arity/scalar-type mismatch, or a
    /// missing non-defaulted field is a recoverable `Err(message)` (via `err_shape`); success wraps the
    /// object in `Ok` (via `ok_shape`). Validation runs through the shared `plan_construct` — the same
    /// one the tree-walker uses — so both backends agree on every accept/reject and every message.
    ///
    /// A target that implements `Validate` has its `validate()` run on the freshly-built value before
    /// the door hands it back — the same re-entry the `json`/`from_bytes` decode doors make (see
    /// [`noeta_ast::reflect::construct_validates`]), through the same `validate_message`, so a
    /// rejection is the door's own `Err(message)` carrying the validator's words.
    pub(crate) fn construct_dynamic(
        &mut self,
        name_val: Value,
        fields_val: Value,
        ok_shape: u32,
        err_shape: u32,
        span: Span,
    ) -> Result<Value, Abort> {
        let err_of = |vm: &Self, msg: String| {
            let shape = vm.persist.shapes[err_shape as usize];
            Value::enum_value(shape, vec![Value::string(&msg)])
        };
        let Some(type_name) = name_val.as_string() else {
            return Ok(err_of(
                self,
                format!(
                    "construct type name must be a string, found {}",
                    name_val.type_name()
                ),
            ));
        };
        // What the name refers to, decided by the shared resolver the tree-walker also runs — before
        // any field validation, so an unconstructible name reports as such rather than "no field X".
        // An `Enum.Variant` spelling builds the case directly (its payload IS the value: no defaults,
        // no slot table), which is why it can be answered here without touching the shape tables.
        let variant_plan =
            match noeta_ast::reflect::resolve_construct_target(&self.module.reflection, &type_name)
            {
                noeta_ast::reflect::ConstructTarget::Rejected(msg) => Err(msg),
                noeta_ast::reflect::ConstructTarget::Variant {
                    enum_name,
                    variant,
                    index,
                    payload,
                } => Ok(Some((
                    noeta_object::intern_shape(
                        Shape::enum_variant(
                            enum_name,
                            variant,
                            payload.iter().map(|s| s.name.to_string()).collect(),
                            false,
                        )
                        .with_variant_index(index),
                    ),
                    plan_variant_payload(&type_name, &payload, &fields_val),
                    // A validated ENUM validates on its own name, not the `"Enum.Variant"` spelling
                    // the call site used: membership is keyed on the type. Decided here, under the
                    // reflection borrow, because the re-entry below needs `&mut self`.
                    noeta_ast::reflect::construct_validates(&self.module.reflection, enum_name),
                ))),
                noeta_ast::reflect::ConstructTarget::Fielded => Ok(None),
            };
        match variant_plan {
            Err(msg) => return Ok(err_of(self, msg)),
            Ok(Some((shape, payload, validates))) => {
                let data = match payload {
                    Err(msg) => return Ok(err_of(self, msg)),
                    Ok(data) => data,
                };
                let value = Value::enum_value(shape, data);
                if validates {
                    // `validate_message` consumes its argument, so retain first: the case stays owned
                    // here and is either handed on inside `Ok` or released on a rejection.
                    retain(value);
                    if let Some(msg) = self.validate_message(value, span)? {
                        release(value);
                        return Ok(err_of(self, msg));
                    }
                }
                let ok = self.persist.shapes[ok_shape as usize];
                return Ok(Value::enum_value(ok, vec![value]));
            }
            Ok(None) => {}
        }
        // The `fields` argument is a `List<dyn>` (positional, declaration order) or a
        // `Map<string, dyn>` (named — the sparse, any-order form a framework binding `--field` flags
        // produces). Both converge on `named: Vec<(field, value)>` and a shared plan validation, so the
        // two backends and the two forms agree. `realize_list`/`map_values` hand back values that share
        // their container's references (not retained), so each value placed into the object is
        // retained and the realized list (if any) released after — the `Op::Invoke` protocol.
        let (named, to_release, plan): ConstructResolve = if fields_val.is_list() {
            let realized = fields_val.realize_list();
            let values = realized.list_items().expect("checked is_list");
            let value_reprs: Vec<noeta_ast::reflect::TypeRepr> =
                values.iter().map(vm_type_repr).collect();
            let info = &self.module.reflection;
            let specs = info.field_specs(&type_name);
            match noeta_ast::reflect::plan_construct(&type_name, &specs, &value_reprs) {
                Ok(fill) => {
                    let named = fill.iter().map(|s| s.to_string()).zip(values).collect();
                    (named, Some(realized), Ok(()))
                }
                Err(msg) => {
                    realized.release();
                    return Ok(err_of(self, msg));
                }
            }
        } else if fields_val.is_map() {
            let keys = fields_val.map_keys().expect("checked is_map");
            let vals = fields_val.map_values().expect("checked is_map");
            let named: Vec<(String, Value)> = keys
                .iter()
                .zip(vals)
                .filter_map(|(k, v)| match k {
                    noeta_stdlib::MapKey::Str(s) => Some((s.as_str().to_owned(), v)),
                    _ => None,
                })
                .collect();
            let reprs: Vec<(String, noeta_ast::reflect::TypeRepr)> = named
                .iter()
                .map(|(n, v)| (n.clone(), vm_type_repr(v)))
                .collect();
            let info = &self.module.reflection;
            let specs = info.field_specs(&type_name);
            let plan = noeta_ast::reflect::plan_construct_named(&type_name, &specs, &reprs);
            (named, None, plan)
        } else {
            return Ok(err_of(
                self,
                format!(
                    "construct fields must be a list or a map, found {}",
                    fields_val.type_name()
                ),
            ));
        };
        if let Err(msg) = plan {
            if let Some(list) = to_release {
                list.release();
            }
            return Ok(err_of(self, msg));
        }
        // Build via the same slot/defaults path `MakeStruct` uses: find the interned shape by name,
        // place each provided value into its declaration-order slot, then fill unset slots from field
        // defaults (run in global scope). The plan guaranteed every unset slot is defaulted.
        //
        // A type constructed ONLY dynamically (no `T { … }` literal anywhere in the program) has no
        // compiled shape in `module.shapes`, so the shape is rebuilt from the reflection artifact —
        // same kind, name, and field order the compiler would have interned, and shape interning is
        // structural, so a dynamically built instance and a literal one share the one shape.
        //
        // A **native** fielded target is the one case where the artifact's key and the runtime identity
        // differ: reflection registers it as `std.http.Frame` while a value of it carries the canonical
        // short shape name (`Frame`) plus a *stamped* qualified reflected type — the pair a source
        // literal `Frame { … }` produces. So the shape is built under the registry's own name and the
        // qualified identity is stamped below, which makes a constructed instance the same value as a
        // literal one: the interned shape is shared, `==` holds, and it marshals into native code. The
        // tree-walker applies the identical rule off the identical registry lookup (`native_fielded_repr`).
        let native = noeta_stdlib::registry::default_registry()
            .and_then(|reg| reg.resolve_fielded(&type_name));
        let shape_name = native.map(|cl| cl.name).unwrap_or(type_name.as_str());
        let shape = match self.module.shapes.iter().find(|s| {
            s.name == shape_name && matches!(s.kind, ShapeKind::Struct | ShapeKind::Class)
        }) {
            Some(shape) => noeta_object::intern_shape(shape.clone()),
            None => {
                let info = self
                    .module
                    .reflection
                    .type_named(&type_name)
                    .expect("validated type is in the reflection artifact");
                let kind = match info.kind {
                    noeta_ast::reflect::TypeKind::Class => ShapeKind::Class,
                    _ => ShapeKind::Struct,
                };
                noeta_object::intern_shape(Shape::object(kind, shape_name, info.fields.clone()))
            }
        };
        let mut slots: Vec<Option<Value>> = vec![None; shape.fields.len()];
        for (name, value) in &named {
            if let Some(idx) = shape.fields.iter().position(|f| f == name) {
                retain(*value);
                slots[idx] = Some(*value);
            }
        }
        if let Some(list) = to_release {
            list.release();
        }
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
                    Ok(v) => slots[i] = Some(v),
                    Err(abort) => {
                        for slot in slots.into_iter().flatten() {
                            release(slot);
                        }
                        return Err(abort);
                    }
                }
            }
        }
        let slots: Vec<Value> = slots
            .into_iter()
            .map(|s| s.unwrap_or_else(Value::unit))
            .collect();
        let object = Value::object(shape, slots);
        // The stamped reflected identity for a native fielded type (see above) — the qualified name
        // `type_of` must report, which the short shape name alone cannot supply. Written before the
        // value escapes, so no other reference can observe the untagged state.
        if let Some(cl) = native {
            use noeta_stdlib::NominalType;
            let repr = match cl.kind {
                noeta_stdlib::FieldedKind::Class => {
                    noeta_ast::reflect::TypeRepr::Class(cl.qualified(), Vec::new())
                }
                noeta_stdlib::FieldedKind::Struct => {
                    noeta_ast::reflect::TypeRepr::Struct(cl.qualified(), Vec::new())
                }
            };
            object.set_reflect(Some(Rc::new(repr)));
        }
        // Bottom-up, exactly as the recipe walk is: every field value handed in was built and (if its
        // own type validates) checked at its own door before it reached this call, and the defaulted
        // slots were filled above — so the type's own `validate` sees a complete, already-valid value,
        // and a rejection short-circuits before the object escapes into `Ok`. The reflected tag is
        // already stamped, so a native type's validator dispatches to its own `validate`.
        if noeta_ast::reflect::construct_validates(&self.module.reflection, &type_name) {
            // `validate_message` consumes its argument; retain so `object` stays owned here.
            retain(object);
            if let Some(msg) = self.validate_message(object, span)? {
                release(object);
                return Ok(err_of(self, msg));
            }
        }
        let ok = self.persist.shapes[ok_shape as usize];
        Ok(Value::enum_value(ok, vec![object]))
    }

    /// Record a runtime diagnostic and produce the unwind token.
    pub(crate) fn error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Abort {
        self.out
            .diagnostics
            .push(Diagnostic::error(code, span, message));
        Abort
    }

    /// The abort a call takes when it reaches a **forwarding generic** without supplying the type
    /// arguments its prototype declares. The entry points with no static callee type behind them —
    /// a `dyn` receiver, a handle, `invoke`, a first-class value — carry none, and binding
    /// positionally anyway would lay a value argument into a type-argument slot and read it as an
    /// index into the type table. Shares one message with the tree-walker, so the two backends
    /// cannot word it differently.
    pub(crate) fn no_instantiation(
        &mut self,
        callee: Option<&str>,
        declared: usize,
        supplied: usize,
        span: Span,
    ) -> Abort {
        let message = noeta_ast::reflect::no_instantiation_message(
            callee.unwrap_or("<anonymous>"),
            declared,
            supplied,
        );
        self.error(DiagnosticCode::InvalidTypeArguments, span, message)
    }

    /// Convert a native-dispatch [`noeta_stdlib::StdError`] into the unwind token. Two kinds are
    /// NOT diagnostics. `Exit` (`os.exit(code)`, stdlib-gaps) records the requested code and aborts
    /// cleanly — nothing is reported, stdout is kept. `Interrupted` (interruptible-io) is a
    /// cancellation arriving through a value rather than through a safepoint: a host leaf blocked
    /// outside the interpreter reads the same flag a safepoint would and stops, so honoring it here
    /// is honoring it in the one place that decides. Reporting it instead would make a *cancelled*
    /// worker a *failed* one — its parent's `join()` re-raises a panic rather than yielding the
    /// cancelled outcome, which is the opposite of what asking it to stop means.
    ///
    /// Mirrors the tree-walker's `std_dispatch_error` (which has no run-level cancellation, so no
    /// leaf there can produce `Interrupted`).
    pub(crate) fn std_dispatch_error(
        &mut self,
        error: noeta_stdlib::StdError,
        span: Span,
    ) -> Abort {
        if let noeta_stdlib::ErrorKind::Exit(code) = error.kind {
            self.out.requested_exit = Some(code);
            return Abort;
        }
        // Only while the request is live. An interruption that outlives the cancellation it belongs
        // to — the flag is cleared the moment it is honored — is a real IO failure, and is reported
        // as one rather than silently ending a run nobody asked to stop.
        if error.kind == noeta_stdlib::ErrorKind::Interrupted && self.cancel_requested() {
            return self.observe_cancel();
        }
        self.error(stdlib_error_code(error.kind), span, error.message)
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
    pub(crate) fn reclaim_cycle_garbage(&mut self, garbage: noeta_gc::Garbage) {
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
        // Suspend the safepoint poll while the dead subgraph is pinned and half-freed: a destructor
        // body (exit reclaim only — safepoint garbage is destructor-free by construction) runs the
        // dispatch loop, whose polls must not start a nested collection over this state.
        let saved_suspended = std::mem::replace(&mut self.gc_suspended, true);
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
        self.gc_suspended = saved_suspended;
    }

    /// Store `value` in the embed-handle table (server-hmr F3), taking ownership of its reference,
    /// and return the handle. Reuses a freed slot when one is available. Only the embed API
    /// (`VmSession`) mints handles, so this is `compile`-gated; the table itself stays (GC roots it).
    #[cfg(feature = "compile")]
    pub(crate) fn embed_handle_store(&mut self, value: Value) -> crate::session::EmbedHandle {
        let idx = match self.persist.embed_handles_free.pop() {
            Some(idx) => {
                self.persist.embed_handles[idx as usize] = Some(value);
                idx
            }
            None => {
                self.persist.embed_handles.push(Some(value));
                (self.persist.embed_handles.len() - 1) as u32
            }
        };
        crate::session::EmbedHandle::from_index(idx)
    }

    /// Release an embed handle's value (destructor-aware) and free its slot (server-hmr F3).
    #[cfg(feature = "compile")]
    pub(crate) fn embed_handle_release(&mut self, handle: crate::session::EmbedHandle) {
        let idx = handle.index();
        if let Some(value) = self.persist.embed_handles[idx as usize].take() {
            self.release_value(value);
            self.persist.embed_handles_free.push(idx);
        }
    }

    pub(crate) fn release_value(&mut self, value: Value) {
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
    pub(crate) fn set_field_fast(
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
            if v.is_uniquely_owned() {
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
        let num_registers = self.module.protos[proto as usize].num_registers as usize;
        let (mut frames, mut regs) = self.pooled_run_stacks(num_registers);
        retain(instance);
        regs[0] = instance;
        frames.push(Frame {
            proto,
            base: 0,
            pc: 0,
            ret_dst: 0,
            ret_transform: RetTransform::None,
            upvalues: Vec::new(),
        });
        // **A destructor is uninterruptible** (isolate-cancel): lift the worker's cancellation flag
        // for the duration, so a cancel that lands mid-cleanup is observed at the next safepoint in
        // ordinary code instead of truncating the cleanup. It has to be lifted rather than merely
        // ignored, because the abort a cancellation raises is *discarded* right below — observing it
        // here would end the destructor's body silently and then let the worker carry on as if
        // nothing had been asked. `None` on every non-worker VM, so this is a pair of moves of a
        // null pointer. Not restored once the request has been honored: `observe_cancel` clears the
        // flag permanently, and the unwind behind it runs the remaining destructors through here.
        let armed = self.isolates.cancel_flag.take();
        // A destructor returns unit (its body is run for its effects); discard it. An abort
        // inside a destructor has already recorded its diagnostic.
        if let Ok(v) = self.run(frames, regs) {
            release(v);
        }
        if !self.isolates.cancel_observed {
            self.isolates.cancel_flag = armed;
        }
    }
}
