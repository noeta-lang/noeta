//! VM **lifecycle**: [`Vm::load`] (derived-table resolution), [`Vm::teardown`]
//! (globals destruction, cycle reaping, channel drain), the value-release /
//! destructor cluster, the session heap-owner accounting, and the real-thread
//! isolate machinery ([`IsolateFactory`], `run_isolate_worker`, the
//! scheduler-owned `Channel`/`Task` state). Every item is moved verbatim from
//! the crate root purely to shrink `lib.rs` — no behavior change.

use crate::*;

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

/// A spawned worker isolate (isolates I.4b): the channel its result (a marshalled [`isolate::Wire`], or
/// a failure) arrives on, and the thread's join handle (taken to join at teardown).
pub(crate) struct IsolateSlot {
    pub(crate) result: std::sync::mpsc::Receiver<Result<isolate::Wire, IsolateFailure>>,
    pub(crate) handle: Option<std::thread::JoinHandle<()>>,
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

/// The outcome of polling a future once (Track A.3): ready with a value, or still pending.
pub(crate) enum Poll {
    Ready(Value),
    Pending,
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

/// Whether a value matches a narrowing target (`x.as<T>()`). Generics are erased, so only the
/// runtime **head constructor** is tested. The primitive/collection kinds compare against
/// [`Value::type_name`] — the same canonical strings the M0 tree-walker matches on, so both
/// backends decide a narrowing identically; `Named` (a user struct/class/enum, or the built-in
/// `Option`/`Result`) matches by shape name; `Dyn` always matches (no-op narrowing).
pub(crate) fn narrow_matches(v: Value, target: &NarrowTarget) -> bool {
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
        NarrowTarget::AnyOf(members) => {
            return members.iter().any(|m| narrow_matches(v, m));
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
            return narrow_matches(v, head)
                && noeta_ast::reflect::narrow_args_match(args, &vm_type_repr(&v));
        }
    };
    v.type_name() == kind
}

impl<'m> Vm<'m> {
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
                    noeta_bytecode::PackedFieldDef::Bool => noeta_object::PackedKind::Bool,
                    noeta_bytecode::PackedFieldDef::Struct(idx) => {
                        noeta_object::PackedKind::Struct(packed_schemas[*idx as usize])
                    }
                })
                .collect();
            packed_schemas.push(noeta_object::intern_schema(noeta_object::PackedSchema {
                shape: shapes[def.shape as usize],
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
        let mut methods: HashMap<String, HashMap<String, u32>> = HashMap::new();
        for m in &module.methods {
            methods
                .entry(m.type_name.clone())
                .or_default()
                .insert(m.method.clone(), m.proto);
        }
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
            destructors,
            field_defaults,
            destruct_reachable,
            comparable_derives,
            tojson_derives,
            deserialize_recipes,
            sched: SchedState {
                scopes: Vec::new(),
                ctx_current: Vec::new(),
                tel_on,
                traced_futures: Vec::new(),
            },
            ctx_table_pool: Vec::new(),
            reentry_pool: Vec::new(),
            cache_pool: Vec::new(),
            run_depth: 0,
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
            },
            out: RunOutput {
                stdout: String::new(),
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
                jit_requested: Vec::new(),
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

impl<'m> Vm<'m> {
    /// Tear the VM down after its entry chunk(s) ran and drain the [`RunResult`]: reap reference
    /// cycles, drain channel buffers, clear the reactive graph, destroy the globals in reverse binding
    /// order (running each destructor), reap any remaining cycle garbage, and join outstanding isolate
    /// workers. Split from [`Vm::run_top`] so a session runs this **once** at the end rather than after
    /// every entry (REPL-on-VM R0); leak residency must reach zero here.
    pub(crate) fn teardown(&mut self, mode: noeta_value::CollectorMode) -> RunResult {
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
        // isolates at `}`, so this is normally empty — defensive against an early exit).
        for slot in std::mem::take(&mut self.isolates.isolates) {
            if let Some(h) = slot.handle {
                let _ = h.join();
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
            self.tier1.jit_final_stats = service.shutdown(self.tier1.jit_drain_at_exit);
        }

        // A deliberate `os.exit(code)` wins over the diagnostic-derived code (there are no
        // diagnostics on that path — the halt is clean).
        let exit_code = self
            .out
            .requested_exit
            .unwrap_or(if self.out.diagnostics.is_empty() {
                0
            } else {
                1
            });
        RunResult {
            stdout: std::mem::take(&mut self.out.stdout),
            exit_code,
            diagnostics: std::mem::take(&mut self.out.diagnostics),
        }
    }
}

/// Run one real-thread isolate to completion (isolates I.4b), on its own thread. Builds a fresh VM with
/// its own heap (thread-local), host, and executor from `factory`, seeds globals from the parent's
/// marshalled snapshot, rebuilds the arguments, calls `callee(args)` and drives the resulting future to
/// completion, then marshals the result back to `Send` [`isolate::Wire`]. An abort inside the isolate
/// (a panic) comes back as `Err(message)`, which the parent re-raises at the `.await`. The worker tears
/// down its own globals/channels so its thread-local heap returns to zero residency.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_isolate_worker(
    module: &Arc<Module>,
    factory: &IsolateFactory,
    profile_seam: Option<(ProfileHookFactory, ProfileSink)>,
    proto: u32,
    iso_args: Vec<isolate::IsoArg>,
    wire_globals: Vec<(u32, isolate::Wire)>,
    trace: Option<noeta_stdlib::TraceContext>,
    registry: Option<&'static noeta_stdlib::registry::Registry>,
    stall_tracked: bool,
    span: Span,
) -> Result<isolate::Wire, IsolateFailure> {
    noeta_value::set_collector_mode(noeta_value::CollectorMode::Trace);
    let (host, executor) = factory();
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
    // Seed the worker's globals from the parent's snapshot so the isolate body can call other
    // top-level functions (and read value-type constants). Slots match: parent and worker share the
    // same `Arc<Module>`, so a global's `GlobalId` is identical on both sides (P-VMT-GSLOT).
    for (slot, wire) in &wire_globals {
        let value = isolate::rebuild(wire, &wvm.persist.shapes, &mut wvm.persist.channels);
        wvm.persist.globals[*slot as usize] = value;
        wvm.persist.global_order.push(*slot);
    }
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
    let callee = Value::closure(proto, Vec::new());
    // Participate in the global all-parties-blocked deadlock check (isolates I.4c) iff the parent
    // does, so a cross-isolate deadlock among workers resolves to E0010 rather than spinning. The
    // worker's `active` **slot is registered by the parent at spawn** (not here), so `active` never
    // lags this thread's startup — the fix for the startup-window false positive.
    wvm.stall_active = stall_tracked;
    let outcome = match wvm.call_value(callee, arg_vals, span) {
        Ok(future) => {
            let result = wvm.drive_future(future, span);
            release(future);
            result
        }
        Err(abort) => Err(abort),
    };
    release(callee);
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
    let message = match outcome {
        Ok(result) => {
            let marshalled = isolate::marshal(result, &wvm.persist.shapes, &wvm.persist.channels)
                .map_err(|e| {
                    // The body completed; only the result failed to ship — there is no abort stack.
                    IsolateFailure {
                        message: format!("isolate result is not shippable: {e}"),
                        trace: Vec::new(),
                    }
                });
            wvm.release_value(result);
            marshalled
        }
        // Ship the worker's own abort traceback home with the message (plain data — it crosses the
        // boundary like any `Wire`), so the parent's rendered trace includes the worker's frames.
        Err(_abort) => Err(IsolateFailure {
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
    // inline caches' closure pins (S4.2), destroy globals in reverse declaration order, then
    // drain any channel buffers.
    #[cfg(feature = "jit")]
    for v in std::mem::take(&mut wvm.tier1.jit_cache_pins) {
        release(v);
    }
    for slot in wvm.persist.global_order.clone().into_iter().rev() {
        let value = std::mem::replace(&mut wvm.persist.globals[slot as usize], Value::unbound());
        if !value.is_unbound() {
            wvm.release_value(value);
        }
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
    message
}

impl<'m> Vm<'m> {
    /// Materialize the `#[type_name(...)]` attributes from the module manifest into a
    /// `List<Attributed<T>>` — each a real `T` struct (built from its stored args) paired with its
    /// target. Shapes are built fresh from the shared reflection info; because shape equality is
    /// structural (name + fields), they match the tree-walker's by construction.
    pub(crate) fn materialize_attributes(&self, type_name: &str) -> Value {
        let attributed_shape = noeta_object::intern_shape(Shape::object(
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
            "RoleBinding",
            vec!["target".to_string(), "role".to_string()],
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
    /// `List<ParamInfo>` — each `{ name: string, type: Type }`. `type` is the prelude `Type` ADT
    /// value built from the parameter's declared type (the same `build_type_value` `type_of` uses).
    /// The `ParamInfo` shape is built fresh; because shape equality is structural, it matches the
    /// tree-walker's by construction. An unknown target yields an empty list.
    pub(crate) fn materialize_params(&self, target: &str) -> Value {
        let info_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::PARAM_INFO,
            vec!["name".to_string(), "type".to_string()],
        ));
        let items: Vec<Value> = self
            .module
            .reflection
            .params_for(target)
            .iter()
            .map(|p| {
                Value::object(
                    info_shape,
                    vec![Value::string(&p.name), build_type_value(&p.ty)],
                )
            })
            .collect();
        Value::list(items)
    }

    /// Materialize a struct/class instance's fields into a `List<FieldEntry>` (`{ name, value }`,
    /// declaration order) — the value-level reflection `fields_of` (derive layer 3). Any other
    /// value yields the empty list. The shape is built fresh (structural equality matches the
    /// tree-walker's by construction); each carried field value is retained since the new entry
    /// object holds a fresh reference.
    pub(crate) fn materialize_fields(&self, value: Value) -> Value {
        let entry_shape = noeta_object::intern_shape(Shape::object(
            ShapeKind::Struct,
            noeta_ast::reflect::FIELD_ENTRY,
            vec!["name".to_string(), "value".to_string()],
        ));
        let items: Vec<Value> = value
            .object_fields_for_reflection()
            .unwrap_or_default()
            .into_iter()
            .map(|(name, field_value)| {
                noeta_gc::retain(field_value);
                Value::object(entry_shape, vec![Value::string(&name), field_value])
            })
            .collect();
        Value::list(items)
    }

    /// Record a runtime diagnostic and produce the unwind token.
    pub(crate) fn error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Abort {
        self.out
            .diagnostics
            .push(Diagnostic::error(code, span, message));
        Abort
    }

    /// Convert a native-dispatch [`noeta_stdlib::StdError`] into the unwind token. The
    /// distinguished `Exit` kind (`os.exit(code)`, stdlib-gaps) is NOT a diagnostic: it records
    /// the requested code and aborts cleanly — nothing is reported, stdout is kept. Mirrors the
    /// tree-walker's `std_dispatch_error`.
    pub(crate) fn std_dispatch_error(
        &mut self,
        error: noeta_stdlib::StdError,
        span: Span,
    ) -> Abort {
        if let noeta_stdlib::ErrorKind::Exit(code) = error.kind {
            self.out.requested_exit = Some(code);
            return Abort;
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
        // A destructor returns unit (its body is run for its effects); discard it. An abort
        // inside a destructor has already recorded its diagnostic.
        if let Ok(v) = self.run(frames, regs) {
            release(v);
        }
    }
}
