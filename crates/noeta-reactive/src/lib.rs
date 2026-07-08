//! The reactive-graph core: the deterministic bookkeeping behind server-side signals
//! (`signal` / `computed` / `effect`), architecture §9.4.
//!
//! # What lives here (and what does not)
//!
//! This crate owns the **oracle-critical half** of reactivity — the graph structure and its
//! *scheduling* — and nothing else. Concretely: the node table, the bidirectional dependency edges
//! (sources ↔ subscribers), the dirty-propagation walk, the dynamic **current-computing stack** that
//! turns a dependency read inside a `computed`/`effect` body into a subscription, the dirty-effect
//! queue, and the deterministic flush. It is **value-generic** (`ReactiveGraph<V>`): it never inspects
//! a value, never runs a closure, does no I/O. The one backend-specific step — *"run this node's body
//! closure"* — is threaded in as a callback (`run: &mut dyn FnMut(V) -> V`), which is where each
//! backend plugs in its own closure-invocation seam (the tree-walker's `call_closure`, the VM's
//! `call_value`). Because the algorithm here is shared **verbatim** by both backends, the scheduling is
//! **differential-by-construction**: two backends running the same program drive the same graph through
//! the same deterministic order, so `RunResult`s agree without a per-backend reimplementation to keep in
//! sync.
//!
//! # The two properties the differential + leak oracles depend on
//!
//! 1. **Determinism.** Effect execution order is a pure function of the program: within a flush round
//!    effects run in ascending [`NodeId`] order (creation order — sources are created before the
//!    consumers that read them, so this approximates a topological order), and a `set` that dirties
//!    more effects mid-flush schedules them into the next round. No wall-clock, hash-order, or thread
//!    dependence. **Value** glitch-freedom is separate and comes for free from the evaluation model
//!    (below): reading a `computed` always forces it fresh, so no observer ever sees a half-updated
//!    graph.
//! 2. **No leaks.** A reactive graph is deliberately cycle-shaped (a signal points at its subscribers,
//!    which point back at their sources). Disposal must sever every edge so the leak oracle's residency
//!    stays 0 — [`ReactiveGraph::dispose`] unsubscribes a node from all its sources and frees its slot.
//!
//! # Evaluation model — lazy-memo (the SolidJS model)
//!
//! A `computed` is **lazy**: it recomputes on read only when a dependency has changed (its `dirty`
//! flag is set), and returns its memo otherwise. An `effect` is **eager**: it is queued on creation and
//! reruns whenever a dependency changes. A `set` marks dependents dirty and enqueues affected effects
//! but runs *nothing* itself; the caller drives a [`ReactiveGraph::flush`] (or batches several `set`s
//! and flushes once). This is the proven correct-and-efficient model and — critically — it is what
//! makes value glitch-freedom automatic: because a dirty `computed` is pulled fresh on read, a diamond
//! (`A → B`, `A → C`, `B & C → D`) recomputes its sink `D` exactly once with consistent inputs.
//!
//! # Ownership & disposal (reactivity S4)
//!
//! Every node is owned by the program's single implicit reactive scope — its lifetime is the
//! isolate's. There are no nested reactive scopes yet, so ownership is flat: a node lives until it is
//! explicitly disposed or the program ends. Two disposal paths:
//!
//! - **Explicit, effect-only.** `effect(...)` returns a handle with `.dispose()`, which
//!   [`dispose`](ReactiveGraph::dispose)s its node — severing every subscription so it stops rerunning
//!   and freeing its slot. This is the surface's only manual disposal: a `signal`/`computed` is *not*
//!   independently disposable (there is no `.dispose()` on them). A `computed` is a pure derivation
//!   with no side effects to stop, and exposing per-signal disposal would invite use-after-dispose for
//!   no gain; both are reclaimed with the scope. (The core's `dispose` accepts any kind — it is what
//!   `clear` and, later, a scope teardown build on — but the *language surface* only wires it to
//!   effects.)
//! - **Scope end.** At program exit the backend calls [`clear`](ReactiveGraph::clear), dropping every
//!   node and releasing every held value. For a backend whose value type is externally refcounted
//!   (the VM's `GcVal`) this is what returns residency to zero — the leak oracle's proof obligation,
//!   which holds across arbitrary create/dispose churn (see the `dispose_churn` conformance case).
//!
//! Deferred (not yet built): a **nested owner tree** (SolidJS-style), where an `effect`/`computed`
//! that creates child nodes during its run owns them and disposes them when it reruns or is itself
//! disposed. Without it, a node created *inside* a body that runs repeatedly accumulates until scope
//! end — reachable but advanced, and invisible to the leak oracle (which measures end-of-program
//! residency, and `clear` reclaims everything). It lands with the transport/UI layers that need it.
//!
//! This crate is `unsafe`-free (an arena of indices, no raw pointers) and has no dependencies.

use std::cell::RefCell;
use std::fmt;

/// The most effect executions a single [`flush`](ReactiveGraph::flush) will perform before declaring
/// the update non-convergent and returning [`FlushOverflow`]. A well-behaved reactive graph settles in
/// a number of steps bounded by its propagation depth — far below this. The bound only trips on a
/// *self-reinforcing cycle*: an `effect` that changes a signal it depends on (directly or through
/// others) re-queues itself every round, so the flush would never terminate. Bounding it turns a hang
/// (or, with the backends' nested-flush suppression, a runaway loop) into a deterministic runtime
/// error — the same number on both backends, so the abort is differential-identical. Deliberately
/// generous: any real update is orders of magnitude under it, so a trip is a genuine bug, not a tuning
/// artifact.
pub const MAX_FLUSH_STEPS: u32 = 10_000;

/// Returned by [`flush`](ReactiveGraph::flush)/[`run_pending`](ReactiveGraph::run_pending) when the
/// update did not converge within [`MAX_FLUSH_STEPS`]. A zero-size marker — the backend maps it to its
/// reactive-cycle runtime diagnostic. Distinct from an effect body *aborting* (a panic / `?`), which
/// the backend captures through its own call seam and propagates as itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushOverflow;

/// Index into the graph's node table — the id a `signal`/`computed`/`effect` value carries.
///
/// A plain table index (like `noeta_value::ChannelId`), pinned to `u32`. Freed slots are reused, so an
/// id is only meaningful until its node is [`disposed`](ReactiveGraph::dispose).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    /// Wrap a backing-table index.
    #[inline]
    pub fn from_index(index: usize) -> Self {
        NodeId(index as u32)
    }

    /// The index into the backing table.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Which flavor of reactive node this is. Determines read semantics (a `Signal` returns its stored
/// value; a `Computed` recomputes-on-read when dirty; an `Effect` is never read, only run) and dirty
/// propagation (a dirtied `Computed` propagates lazily; a dirtied `Effect` is queued to run).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Signal,
    Computed,
    Effect,
}

impl NodeKind {
    /// The surface type name — the reserved `Named` type the checker uses (`Signal<T>`/`Computed<T>`/
    /// `Effect`) and the name a handle value reports from `type_name`. Both backends and the checker
    /// key on these identical strings.
    pub fn type_name(self) -> &'static str {
        match self {
            NodeKind::Signal => "Signal",
            NodeKind::Computed => "Computed",
            NodeKind::Effect => "Effect",
        }
    }
}

/// One node in the reactive graph.
struct Node<V> {
    kind: NodeKind,
    /// Live? A disposed slot is `false` and awaits reuse from the free list.
    live: bool,
    /// A `Computed`/`Effect` whose dependencies changed and must (re)run before its value is trusted.
    /// A `Signal` is never dirty.
    dirty: bool,
    /// An `Effect` currently sitting in the flush queue (dedupes multiple dirtying edges into one run).
    queued: bool,
    /// Nodes this node read during its last (re)computation — its dependencies. Cleared and rebuilt on
    /// every recompute, so a dependency that stops being read is correctly unsubscribed.
    sources: Vec<NodeId>,
    /// Nodes that read this node — the ones to dirty when this node changes.
    subscribers: Vec<NodeId>,
    /// A `Signal`'s current value, or a `Computed`'s memoized last result. `None` for an `Effect`, and
    /// for a `Computed` that has never run.
    content: Option<V>,
    /// The closure a `Computed`/`Effect` runs to (re)compute. `None` for a `Signal`.
    body: Option<V>,
}

impl<V> Node<V> {
    fn placeholder() -> Self {
        Node {
            kind: NodeKind::Signal,
            live: false,
            dirty: false,
            queued: false,
            sources: Vec::new(),
            subscribers: Vec::new(),
            content: None,
            body: None,
        }
    }
}

/// The mutable interior, held behind a single [`RefCell`] so the public API is `&self`. That is
/// non-negotiable: recomputing a node runs a user closure that **reenters** the graph to read its
/// dependencies, and reentrancy through `&mut self` is impossible in Rust. Every method borrows this
/// transiently and — the load-bearing discipline — **releases the borrow before invoking the `run`
/// callback**, so the reentrant reads inside it can borrow freely.
struct Inner<V> {
    nodes: Vec<Node<V>>,
    /// Reusable slots vacated by [`ReactiveGraph::dispose`].
    free: Vec<NodeId>,
    /// The stack of nodes currently (re)computing. Its top is the node that a dependency read should
    /// subscribe. A stack (not a single slot) because reading a dirty `computed` inside another
    /// node's body recomputes it, nesting a frame.
    computing: Vec<NodeId>,
    /// How many live `Computed` nodes are currently dirty (H5): maintained on every clean↔dirty
    /// transition so a client can gate a memo-read fast path on "no memo anywhere is stale"
    /// without scanning.
    dirty_computeds: usize,
    /// Scratch buffers (H5 perf): the flush swaps `round_scratch` with the queue each round and
    /// the dirty walk reuses `dirty_scratch` as its worklist, so a 1M-set hot loop performs no
    /// per-cycle allocations in here.
    round_scratch: Vec<NodeId>,
    dirty_scratch: Vec<NodeId>,
    /// Effects dirtied since the last flush, awaiting a run. Drained (and sorted for determinism) per
    /// flush round.
    queue: Vec<NodeId>,
    /// True while a [`flush`](ReactiveGraph::flush) loop is running. A `set` performed *inside* a
    /// running effect body must not start a *nested* flush — it enqueues, and the ongoing flush picks
    /// it up next round (the coalescing model). The backends consult [`is_flushing`] to decide whether
    /// their `signal.set`/`.update` should drive a flush or just enqueue.
    ///
    /// [`is_flushing`]: ReactiveGraph::is_flushing
    flushing: bool,
}

impl<V> Inner<V> {
    /// Record that whatever node is currently computing depends on `source`. No-op at top level (a
    /// read outside any `computed`/`effect` body — e.g. `signal.get()` in ordinary code — subscribes
    /// nothing). Dedupes so re-reading the same dependency in one body does not duplicate the edge.
    fn record_dependency(&mut self, source: NodeId) {
        let Some(&subscriber) = self.computing.last() else {
            return;
        };
        if subscriber == source {
            return;
        }
        let sub_node = &mut self.nodes[subscriber.index()];
        if sub_node.sources.contains(&source) {
            return;
        }
        sub_node.sources.push(source);
        self.nodes[source.index()].subscribers.push(subscriber);
    }

    /// Begin recomputing `node`: sever its old dependency edges (so a dependency dropped this run is
    /// unsubscribed) and push it as the current computing node. Returns its body closure to run.
    fn begin_compute(&mut self, node: NodeId) -> V
    where
        V: Clone,
    {
        let old_sources = std::mem::take(&mut self.nodes[node.index()].sources);
        for src in old_sources {
            let subs = &mut self.nodes[src.index()].subscribers;
            if let Some(pos) = subs.iter().position(|&s| s == node) {
                subs.swap_remove(pos);
            }
        }
        self.computing.push(node);
        self.nodes[node.index()]
            .body
            .clone()
            .expect("computed/effect node has a body")
    }

    /// Finish recomputing `node`: pop the computing stack, store the memo (for a `computed`), and clear
    /// its dirty flag.
    fn end_compute(&mut self, node: NodeId, result: Option<V>) {
        let popped = self.computing.pop();
        debug_assert_eq!(popped, Some(node), "unbalanced begin/end_compute");
        let was_dirty_computed = {
            let n = &mut self.nodes[node.index()];
            let was = n.kind == NodeKind::Computed && n.dirty;
            if n.kind == NodeKind::Computed {
                n.content = result;
            }
            n.dirty = false;
            was
        };
        if was_dirty_computed {
            self.dirty_computeds -= 1;
        }
    }

    /// Propagate a change out of `node`: dirty every dependent `computed` (lazily — mark, do not run)
    /// and queue every dependent `effect`. The `dirty`/`queued` guards make this walk visit each
    /// transitively-affected node once, so a diamond enqueues its sink effect a single time.
    fn mark_dirty_subscribers(&mut self, node: NodeId) {
        // An explicit worklist over indexed reads (no subscriber-vec clone, no recursion): the
        // visit ORDER differs from the old depth-first recursion, but the visited SET is
        // identical (the dirty/queued guards dedupe), and the flush sorts each round — so
        // effect execution order is unchanged. Alloc-free via the reused scratch.
        let mut work = std::mem::take(&mut self.dirty_scratch);
        work.clear();
        work.push(node);
        while let Some(current) = work.pop() {
            let sub_count = self.nodes[current.index()].subscribers.len();
            for i in 0..sub_count {
                let sub = self.nodes[current.index()].subscribers[i];
                let n = &mut self.nodes[sub.index()];
                match n.kind {
                    NodeKind::Computed => {
                        if !n.dirty {
                            n.dirty = true;
                            self.dirty_computeds += 1;
                            work.push(sub);
                        }
                    }
                    NodeKind::Effect => {
                        if !n.queued {
                            n.queued = true;
                            n.dirty = true;
                            self.queue.push(sub);
                        }
                    }
                    NodeKind::Signal => {
                        debug_assert!(false, "a signal cannot be a subscriber (it has no sources)");
                    }
                }
            }
        }
        self.dirty_scratch = work;
    }
}

/// The reactive graph. Value-generic and driven by the backend: read/set/flush take a `run` callback
/// (`&mut dyn FnMut(V) -> V`) for the one thing this crate cannot do generically — invoke a node's body
/// closure. See the [module docs](crate) for the model and the oracle properties.
pub struct ReactiveGraph<V> {
    inner: RefCell<Inner<V>>,
}

impl<V: Clone> fmt::Debug for ReactiveGraph<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The stored `V`s are opaque here (backend values / closures), so summarize the graph shape
        // rather than requiring `V: Debug`.
        let inner = self.inner.borrow();
        f.debug_struct("ReactiveGraph")
            .field("live_nodes", &inner.nodes.iter().filter(|n| n.live).count())
            .field("free_slots", &inner.free.len())
            .field("queued_effects", &inner.queue.len())
            .field("computing_depth", &inner.computing.len())
            .finish()
    }
}

impl<V: Clone> Default for ReactiveGraph<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Clone> ReactiveGraph<V> {
    /// An empty graph.
    pub fn new() -> Self {
        ReactiveGraph {
            inner: RefCell::new(Inner {
                nodes: Vec::new(),
                free: Vec::new(),
                computing: Vec::new(),
                dirty_computeds: 0,
                round_scratch: Vec::new(),
                dirty_scratch: Vec::new(),
                queue: Vec::new(),
                flushing: false,
            }),
        }
    }

    /// Allocate a node, reusing a freed slot when one is available (so slot ids stay dense and disposal
    /// does not leak table growth).
    fn alloc(inner: &mut Inner<V>, node: Node<V>) -> NodeId {
        if let Some(id) = inner.free.pop() {
            inner.nodes[id.index()] = node;
            id
        } else {
            let id = NodeId::from_index(inner.nodes.len());
            inner.nodes.push(node);
            id
        }
    }

    /// Create a `signal` holding `initial`. Reading it subscribes the current computing node; setting
    /// it dirties dependents.
    pub fn signal(&self, initial: V) -> NodeId {
        let mut inner = self.inner.borrow_mut();
        let mut node = Node::placeholder();
        node.kind = NodeKind::Signal;
        node.live = true;
        node.content = Some(initial);
        Self::alloc(&mut inner, node)
    }

    /// Create a lazy `computed` from `body` (a closure value the backend knows how to run). It is
    /// created dirty and computes on first [`read`](Self::read).
    pub fn computed(&self, body: V) -> NodeId {
        let mut inner = self.inner.borrow_mut();
        let mut node = Node::placeholder();
        node.kind = NodeKind::Computed;
        node.live = true;
        node.dirty = true;
        node.body = Some(body);
        inner.dirty_computeds += 1;
        Self::alloc(&mut inner, node)
    }

    /// Create an eager `effect` from `body`. It is created dirty and queued; call [`run_pending`] (or
    /// [`flush`](Self::flush)) to run it the first time — mirroring how a real `set` schedules a rerun.
    ///
    /// [`run_pending`]: Self::run_pending
    pub fn effect(&self, body: V) -> NodeId {
        let mut inner = self.inner.borrow_mut();
        let mut node = Node::placeholder();
        node.kind = NodeKind::Effect;
        node.live = true;
        node.dirty = true;
        node.queued = true;
        node.body = Some(body);
        let id = Self::alloc(&mut inner, node);
        inner.queue.push(id);
        id
    }

    /// The current value of `node`, subscribing the current computing node to it. A `signal` returns
    /// its stored value; a `computed` recomputes first if dirty (pulling its own dirty dependencies
    /// fresh — this is what makes reads glitch-free), then returns its memo; an `effect` has no value.
    ///
    /// `run` is the backend's closure-invocation seam; it is only called for a dirty `computed`.
    pub fn read(&self, node: NodeId, run: &mut dyn FnMut(V) -> V) -> V {
        // Phase 1 — borrow transiently: record the edge, and decide what to do. For a dirty computed,
        // begin_compute here (clears old edges, pushes the computing frame) and hand back its body.
        let body = {
            let mut inner = self.inner.borrow_mut();
            debug_assert!(inner.nodes[node.index()].live, "read of a disposed node");
            inner.record_dependency(node);
            match inner.nodes[node.index()].kind {
                NodeKind::Signal => {
                    return inner.nodes[node.index()]
                        .content
                        .clone()
                        .expect("signal always has a value");
                }
                NodeKind::Effect => {
                    debug_assert!(false, "an effect has no readable value");
                    return inner.nodes[node.index()]
                        .content
                        .clone()
                        .expect("unreachable");
                }
                NodeKind::Computed => {
                    if !inner.nodes[node.index()].dirty {
                        return inner.nodes[node.index()]
                            .content
                            .clone()
                            .expect("a clean computed has a memo");
                    }
                    inner.begin_compute(node)
                }
            }
        };
        // Phase 2 — NO borrow held: run the body. Reentrant reads inside it record edges against this
        // node (now the top of the computing stack) and may recursively recompute dirty dependencies.
        let result = run(body);
        // Phase 3 — reborrow: memoize and clear dirty.
        {
            let mut inner = self.inner.borrow_mut();
            inner.end_compute(node, Some(result.clone()));
        }
        result
    }

    /// Read `node` **without** subscribing (an untracked peek). Recomputes a dirty `computed` first,
    /// like [`read`](Self::read), but records no dependency edge.
    pub fn peek(&self, node: NodeId, run: &mut dyn FnMut(V) -> V) -> V {
        let is_dirty_computed = {
            let inner = self.inner.borrow();
            let n = &inner.nodes[node.index()];
            n.kind == NodeKind::Computed && n.dirty
        };
        if is_dirty_computed {
            // Recompute via read, but shield it from the caller's subscription by not being inside a
            // computing frame for the caller. read() itself pushes `node` as the frame, so the
            // recompute's own inner reads still subscribe correctly; only the *caller* is not
            // subscribed, which is exactly peek semantics.
            return self.read(node, run);
        }
        let inner = self.inner.borrow();
        inner.nodes[node.index()]
            .content
            .clone()
            .expect("signal or clean computed has a value")
    }

    /// Set a `signal`'s value, dirtying dependent `computed`s and queuing dependent `effect`s. Runs
    /// nothing itself — call [`flush`](Self::flush) to drive the queued effects (or batch several sets
    /// and flush once).
    pub fn set(&self, node: NodeId, value: V) {
        let mut inner = self.inner.borrow_mut();
        debug_assert!(inner.nodes[node.index()].live, "set of a disposed node");
        debug_assert_eq!(
            inner.nodes[node.index()].kind,
            NodeKind::Signal,
            "only a signal can be set"
        );
        inner.nodes[node.index()].content = Some(value);
        inner.mark_dirty_subscribers(node);
    }

    /// Signal that `node`'s value changed **without replacing its stored `V`** — dirtying and
    /// queuing exactly as [`set`](Self::set) does. For a client whose `V` is a stable *handle* to
    /// externally-stored content (the higher-order-abi extension stores arena-cell ids and
    /// updates the cell in place, H5): the handle never changes, only the content behind it.
    pub fn touch(&self, node: NodeId) {
        let mut inner = self.inner.borrow_mut();
        debug_assert!(inner.nodes[node.index()].live, "touch of a disposed node");
        inner.mark_dirty_subscribers(node);
    }

    /// Run all queued effects to a fixpoint. Each round drains the queue in ascending [`NodeId`] order
    /// (the determinism guarantee), runs each effect's body via `run`, and repeats if running them
    /// queued more (an effect that `set`s a signal — its set enqueues into *this* flush rather than
    /// starting a nested one, because [`is_flushing`](Self::is_flushing) is true). An effect body's
    /// reentrant reads resubscribe it, so an effect that stops reading a signal stops rerunning.
    ///
    /// Bounded at [`MAX_FLUSH_STEPS`] effect runs: a self-reinforcing cycle (an effect that changes a
    /// signal it depends on) would never settle, so the flush returns [`FlushOverflow`] instead of
    /// looping forever. The [`flushing`](Inner::flushing) flag is set for the whole loop and cleared on
    /// every exit (empty queue, overflow, or a body-driven abort that unwinds through `run`).
    pub fn flush(&self, run: &mut dyn FnMut(V) -> V) -> Result<(), FlushOverflow> {
        self.inner.borrow_mut().flushing = true;
        let result = self.flush_loop(run);
        self.inner.borrow_mut().flushing = false;
        result
    }

    /// The flush fixpoint itself — see [`flush`](Self::flush). Split out so the `flushing` flag is
    /// cleared on *every* return path without repeating the reset at each one.
    fn flush_loop(&self, run: &mut dyn FnMut(V) -> V) -> Result<(), FlushOverflow> {
        let mut steps: u32 = 0;
        loop {
            // Drain this round's queue under a transient borrow, sorted for a deterministic
            // order. The round buffer is swapped with a reused scratch (both keep their
            // capacity), so a hot set→flush loop allocates nothing here.
            let mut round: Vec<NodeId> = {
                let mut inner = self.inner.borrow_mut();
                if inner.queue.is_empty() {
                    return Ok(());
                }
                let mut scratch = std::mem::take(&mut inner.round_scratch);
                scratch.clear();
                std::mem::swap(&mut scratch, &mut inner.queue);
                scratch
            };
            round.sort_unstable();
            for &effect in &round {
                // Clear queued/get body under a transient borrow; skip if disposed mid-flush.
                let body = {
                    let mut inner = self.inner.borrow_mut();
                    let n = &mut inner.nodes[effect.index()];
                    if !n.live || n.kind != NodeKind::Effect {
                        continue;
                    }
                    n.queued = false;
                    inner.begin_compute(effect)
                };
                // No borrow held — run the effect body; its reads resubscribe it.
                let _ = run(body);
                {
                    let mut inner = self.inner.borrow_mut();
                    inner.end_compute(effect, None);
                }
                steps += 1;
                if steps > MAX_FLUSH_STEPS {
                    return Err(FlushOverflow);
                }
            }
            // Hand the round buffer's capacity back for the next round/flush.
            self.inner.borrow_mut().round_scratch = round;
        }
    }

    /// Alias for [`flush`](Self::flush), named for the create-then-run path: a fresh `effect` is queued
    /// on creation, and `run_pending` runs it (and any others) the first time.
    pub fn run_pending(&self, run: &mut dyn FnMut(V) -> V) -> Result<(), FlushOverflow> {
        self.flush(run)
    }

    /// Whether a [`flush`](Self::flush) is currently running. The backends consult this on
    /// `signal.set`/`.update`: at top level (`false`) a set drives a flush; *inside* a running effect
    /// body (`true`) it only enqueues, coalescing into the ongoing flush rather than recursing into a
    /// nested one. Keeping this single source of truth in the shared core is what makes the coalescing
    /// behavior — and therefore the abort/step bound — identical on both backends.
    pub fn is_flushing(&self) -> bool {
        self.inner.borrow().flushing
    }

    /// Dispose `node`: unsubscribe it from all its sources, drop its stored value/body, and free its
    /// slot for reuse. This is the leak-oracle contract — after disposing every effect, no edges dangle
    /// and residency returns to baseline. (Disposing a node that still has subscribers also detaches
    /// those back-edges so they cannot observe a dead node.)
    pub fn dispose(&self, node: NodeId) {
        let mut inner = self.inner.borrow_mut();
        if !inner.nodes[node.index()].live {
            return;
        }
        // Detach from sources (stop being their subscriber).
        let sources = std::mem::take(&mut inner.nodes[node.index()].sources);
        for src in sources {
            let subs = &mut inner.nodes[src.index()].subscribers;
            if let Some(pos) = subs.iter().position(|&s| s == node) {
                subs.swap_remove(pos);
            }
        }
        // Detach from subscribers (drop their edge to this now-dead node).
        let subscribers = std::mem::take(&mut inner.nodes[node.index()].subscribers);
        for sub in subscribers {
            let srcs = &mut inner.nodes[sub.index()].sources;
            if let Some(pos) = srcs.iter().position(|&s| s == node) {
                srcs.swap_remove(pos);
            }
        }
        // Drop values, clear flags, mark the slot free.
        {
            let n = &inner.nodes[node.index()];
            if n.kind == NodeKind::Computed && n.dirty {
                inner.dirty_computeds -= 1;
            }
        }
        let n = &mut inner.nodes[node.index()];
        n.live = false;
        n.dirty = false;
        n.queued = false;
        n.content = None;
        n.body = None;
        inner.free.push(node);
    }

    /// Visit every value the graph currently holds — each live node's `content` (a signal's value or a
    /// computed's memo) and `body` (a computed/effect closure) — by shared reference, without touching
    /// its lifetime. A backend whose value type is externally refcounted uses this to feed the graph's
    /// held values into its **GC root set**: a value reachable only through the graph is otherwise
    /// invisible to a mark-from-roots collector, which would reclaim it as garbage and leave the graph
    /// holding a dangling reference (a double-free at [`clear`](Self::clear)). Registering them as roots
    /// is the reactive analogue of scanning the channel buffers or the register stack.
    pub fn for_each_value(&self, mut f: impl FnMut(&V)) {
        let inner = self.inner.borrow();
        for node in &inner.nodes {
            if !node.live {
                continue;
            }
            if let Some(content) = &node.content {
                f(content);
            }
            if let Some(body) = &node.body {
                f(body);
            }
        }
    }

    /// Drop every node, releasing all stored values. Called at program end so a backend whose value
    /// type manages an external refcount (the VM's `GcVal`) returns to zero residency — dropping each
    /// `Node<V>` drops its `content`/`body`, firing that value type's `Drop`. After this the graph is
    /// empty (as if freshly constructed).
    pub fn clear(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.nodes.clear();
        inner.free.clear();
        inner.computing.clear();
        inner.queue.clear();
        inner.dirty_computeds = 0;
        inner.round_scratch.clear();
        inner.dirty_scratch.clear();
    }

    /// The number of live nodes — for the leak assertion in tests (create N, dispose N, expect 0).
    pub fn live_count(&self) -> usize {
        let inner = self.inner.borrow();
        inner.nodes.iter().filter(|n| n.live).count()
    }

    /// The kind of `node` (test/introspection helper).
    pub fn kind(&self, node: NodeId) -> NodeKind {
        self.inner.borrow().nodes[node.index()].kind
    }

    /// Whether some node is currently (re)computing — a dependency read right now would record an
    /// edge (H5): a client's inlined-read fast path must be OFF while this is true.
    pub fn tracking(&self) -> bool {
        !self.inner.borrow().computing.is_empty()
    }

    /// How many live `Computed` nodes are dirty (H5): a client's memo-read fast path must be OFF
    /// while any memo is stale.
    pub fn dirty_computed_count(&self) -> usize {
        self.inner.borrow().dirty_computeds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// A tiny value type standing in for a backend `Value`: either a plain datum (`Data`) or a closure
    /// identified by a key into the test's closure table. This is exactly the shape the real backends
    /// have — the graph stores opaque values and calls back to run the closures.
    #[derive(Clone, Debug, PartialEq)]
    enum TestVal {
        Data(i64),
        Body(&'static str),
    }

    /// A registered test closure: given the harness (so it can reenter the graph to read
    /// dependencies), produce a value. This is the test stand-in for a backend closure.
    type BodyFn = Rc<dyn Fn(&Harness) -> TestVal>;

    /// A test harness that owns the graph plus a table of named closures. The `run` callback dispatches
    /// a `Body("name")` to its registered Rust closure, which is handed a reference back to the harness
    /// so it can read/recompute dependencies — mirroring how a backend's closure reenters the graph.
    struct Harness {
        graph: ReactiveGraph<TestVal>,
        bodies: HashMap<&'static str, BodyFn>,
    }

    impl Harness {
        /// Run a body value: look up its closure and invoke it. Panics if handed a non-body — a
        /// signal should never be "run".
        fn run(self: &Rc<Self>, v: TestVal) -> TestVal {
            match v {
                TestVal::Body(name) => {
                    let f = self.bodies.get(name).expect("known body").clone();
                    f(self)
                }
                TestVal::Data(_) => panic!("tried to run a data value"),
            }
        }

        /// Read a node, threading `run` through so a dirty computed recomputes.
        fn read(self: &Rc<Self>, node: NodeId) -> i64 {
            let me = self.clone();
            let mut run = move |v| me.run(v);
            match self.graph.read(node, &mut run) {
                TestVal::Data(n) => n,
                TestVal::Body(_) => panic!("read produced a body"),
            }
        }

        fn set(self: &Rc<Self>, node: NodeId, v: i64) {
            self.graph.set(node, TestVal::Data(v));
        }

        fn flush(self: &Rc<Self>) {
            let me = self.clone();
            let mut run = move |v| me.run(v);
            self.graph.flush(&mut run).expect("flush converged");
        }

        /// Flush without asserting convergence — returns the raw result so a test can assert overflow.
        fn try_flush(self: &Rc<Self>) -> Result<(), FlushOverflow> {
            let me = self.clone();
            let mut run = move |v| me.run(v);
            self.graph.flush(&mut run)
        }
    }

    // Registering bodies needs `&mut` on the map; since Harness is behind Rc for the reentrant reads,
    // build the closure table first via a small builder, then freeze into the Rc.
    struct Builder {
        graph: ReactiveGraph<TestVal>,
        bodies: HashMap<&'static str, BodyFn>,
    }

    impl Builder {
        fn new() -> Self {
            Builder {
                graph: ReactiveGraph::new(),
                bodies: HashMap::new(),
            }
        }
        fn body(mut self, name: &'static str, f: impl Fn(&Harness) -> TestVal + 'static) -> Self {
            self.bodies.insert(name, Rc::new(f));
            self
        }
        fn finish(self) -> Rc<Harness> {
            Rc::new(Harness {
                graph: self.graph,
                bodies: self.bodies,
            })
        }
    }

    // Helper: read a node from inside a body closure (the reentrant path).
    fn get(h: &Harness, node: NodeId) -> i64 {
        // Reconstruct a run callback bound to this harness. We only have `&Harness` here (not the Rc),
        // so route through a thread-local-free path: call graph.read with a run that borrows `h`.
        let mut run = |v: TestVal| match v {
            TestVal::Body(name) => (h.bodies.get(name).expect("known body").clone())(h),
            TestVal::Data(_) => panic!("run of data"),
        };
        match h.graph.read(node, &mut run) {
            TestVal::Data(n) => n,
            TestVal::Body(_) => panic!("body from read"),
        }
    }

    #[test]
    fn signal_get_set_roundtrip() {
        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(1));
        let h = b.finish();
        assert_eq!(h.read(s), 1);
        h.set(s, 42);
        assert_eq!(h.read(s), 42);
        assert_eq!(h.graph.live_count(), 1);
    }

    #[test]
    fn effect_runs_on_flush_and_reruns_on_change() {
        let runs = Rc::new(Cell::new(0));
        let seen = Rc::new(Cell::new(0));
        let (r2, s2) = (runs.clone(), seen.clone());

        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(10));
        let b = b.body("watch", move |h| {
            r2.set(r2.get() + 1);
            s2.set(get(h, s));
            TestVal::Data(0)
        });
        let _e = b.graph.effect(TestVal::Body("watch"));
        let h = b.finish();

        // Created dirty+queued; runs once on the first flush.
        h.flush();
        assert_eq!(runs.get(), 1);
        assert_eq!(seen.get(), 10);

        // A dependency change reruns it exactly once.
        h.set(s, 20);
        h.flush();
        assert_eq!(runs.get(), 2);
        assert_eq!(seen.get(), 20);

        // Setting to the same value still reruns (no value-equality suppression in the core — that is a
        // later, opt-in policy, not a determinism concern).
        h.set(s, 20);
        h.flush();
        assert_eq!(runs.get(), 3);
    }

    #[test]
    fn computed_is_lazy_and_memoizes() {
        let computes = Rc::new(Cell::new(0));
        let c2 = computes.clone();

        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(3));
        let b = b.body("double", move |h| {
            c2.set(c2.get() + 1);
            TestVal::Data(get(h, s) * 2)
        });
        let d = b.graph.computed(TestVal::Body("double"));
        let h = b.finish();

        // Lazy: no compute until the first read.
        assert_eq!(computes.get(), 0);
        assert_eq!(h.read(d), 6);
        assert_eq!(computes.get(), 1);
        // Memoized: a second read without a change does not recompute.
        assert_eq!(h.read(d), 6);
        assert_eq!(computes.get(), 1);
        // After a dependency change, the next read recomputes once.
        h.set(s, 5);
        assert_eq!(h.read(d), 10);
        assert_eq!(computes.get(), 2);
    }

    #[test]
    fn diamond_recomputes_sink_once_glitch_free() {
        // A → B, A → C, (B, C) → D(effect). Setting A must run D exactly once, seeing consistent
        // B and C. This is the glitch-freedom + run-once property.
        let d_runs = Rc::new(Cell::new(0));
        let d_sum = Rc::new(Cell::new(0));
        let b_computes = Rc::new(Cell::new(0));
        let c_computes = Rc::new(Cell::new(0));
        let (dr, ds, bc, cc) = (
            d_runs.clone(),
            d_sum.clone(),
            b_computes.clone(),
            c_computes.clone(),
        );

        let a_cell: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let bnode: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let cnode: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let (ac, bn, cn) = (a_cell.clone(), bnode.clone(), cnode.clone());
        let (ac2, bn2, cn2) = (a_cell.clone(), bnode.clone(), cnode.clone());

        let builder = Builder::new();
        let a = builder.graph.signal(TestVal::Data(1));
        a_cell.set(Some(a));
        let builder = builder
            .body("B", move |h| {
                bc.set(bc.get() + 1);
                TestVal::Data(get(h, ac.get().unwrap()) + 1)
            })
            .body("C", move |h| {
                cc.set(cc.get() + 1);
                TestVal::Data(get(h, ac2.get().unwrap()) * 10)
            });
        let b_id = builder.graph.computed(TestVal::Body("B"));
        let c_id = builder.graph.computed(TestVal::Body("C"));
        bnode.set(Some(b_id));
        cnode.set(Some(c_id));
        let builder = builder.body("D", move |h| {
            dr.set(dr.get() + 1);
            let sum = get(h, bn.get().unwrap()) + get(h, cn.get().unwrap());
            ds.set(sum);
            TestVal::Data(0)
        });
        let _d = builder.graph.effect(TestVal::Body("D"));
        let h = builder.finish();

        // Initial flush runs D once: B = 1+1 = 2, C = 1*10 = 10, sum = 12.
        h.flush();
        assert_eq!(d_runs.get(), 1);
        assert_eq!(d_sum.get(), 12);
        assert_eq!(b_computes.get(), 1);
        assert_eq!(c_computes.get(), 1);

        // Set A once: D reruns exactly once (not twice, despite two paths), seeing fresh B and C.
        let _ = (bn2, cn2);
        h.set(a, 2);
        h.flush();
        assert_eq!(
            d_runs.get(),
            2,
            "sink effect must run once per set, not once per path"
        );
        // B = 3, C = 20, sum = 23 — consistent, no glitch (never 2+20 or 3+10).
        assert_eq!(d_sum.get(), 23);
        assert_eq!(b_computes.get(), 2);
        assert_eq!(c_computes.get(), 2);
    }

    #[test]
    fn effect_order_is_deterministic_by_nodeid() {
        // Two effects on the same signal run in creation (NodeId) order, every flush, reproducibly.
        let log = Rc::new(RefCell::new(Vec::<&'static str>::new()));
        let (l1, l2) = (log.clone(), log.clone());

        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(0));
        let b = b
            .body("first", move |h| {
                let _ = get(h, s);
                l1.borrow_mut().push("first");
                TestVal::Data(0)
            })
            .body("second", move |h| {
                let _ = get(h, s);
                l2.borrow_mut().push("second");
                TestVal::Data(0)
            });
        let _e1 = b.graph.effect(TestVal::Body("first"));
        let _e2 = b.graph.effect(TestVal::Body("second"));
        let h = b.finish();

        h.flush();
        h.set(s, 1);
        h.flush();
        h.set(s, 2);
        h.flush();
        assert_eq!(
            *log.borrow(),
            vec!["first", "second", "first", "second", "first", "second"]
        );
    }

    #[test]
    fn dispose_unsubscribes_and_frees_no_leak() {
        let runs = Rc::new(Cell::new(0));
        let r2 = runs.clone();

        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(0));
        let b = b.body("watch", move |h| {
            r2.set(r2.get() + 1);
            let _ = get(h, s);
            TestVal::Data(0)
        });
        let e = b.graph.effect(TestVal::Body("watch"));
        let h = b.finish();

        h.flush();
        assert_eq!(runs.get(), 1);
        h.set(s, 1);
        h.flush();
        assert_eq!(runs.get(), 2);

        // Dispose the effect: it must stop rerunning, and its slot frees.
        h.graph.dispose(e);
        assert_eq!(h.graph.live_count(), 1, "only the signal remains live");
        h.set(s, 2);
        h.flush();
        assert_eq!(runs.get(), 2, "a disposed effect does not rerun");

        // Dispose the signal too → fully empty; the freed slots are reused by the next allocations.
        h.graph.dispose(s);
        assert_eq!(h.graph.live_count(), 0, "no live nodes — no leak");
        let s2 = h.graph.signal(TestVal::Data(9));
        let s3 = h.graph.signal(TestVal::Data(8));
        assert!(s2.index() < 2 && s3.index() < 2, "freed slots are reused");
    }

    #[test]
    fn untracked_peek_does_not_subscribe() {
        // An effect that peeks a signal (rather than reading it) must NOT rerun when that signal
        // changes — peek records no dependency edge.
        let runs = Rc::new(Cell::new(0));
        let r2 = runs.clone();

        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(7));
        let b = b.body("peeker", move |h| {
            r2.set(r2.get() + 1);
            // peek, not read:
            let mut run = |_v: TestVal| panic!("signal peek should not run a body");
            let _ = h.graph.peek(s, &mut run);
            TestVal::Data(0)
        });
        let _e = b.graph.effect(TestVal::Body("peeker"));
        let h = b.finish();

        h.flush();
        assert_eq!(runs.get(), 1);
        h.set(s, 8);
        h.flush();
        assert_eq!(runs.get(), 1, "peek created no subscription, so no rerun");
    }

    #[test]
    fn dynamic_dependencies_resubscribe() {
        // An effect that reads `cond ? a : b` must, after cond flips, depend on the newly-read signal
        // and NOT on the one it stopped reading.
        let runs = Rc::new(Cell::new(0));
        let r2 = runs.clone();

        let a_c: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let b_c: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let cond_c: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let (ac, bc, cc) = (a_c.clone(), b_c.clone(), cond_c.clone());

        let builder = Builder::new();
        let a = builder.graph.signal(TestVal::Data(100));
        let bsig = builder.graph.signal(TestVal::Data(200));
        let cond = builder.graph.signal(TestVal::Data(1));
        a_c.set(Some(a));
        b_c.set(Some(bsig));
        cond_c.set(Some(cond));
        let builder = builder.body("switch", move |h| {
            r2.set(r2.get() + 1);
            if get(h, cc.get().unwrap()) == 1 {
                let _ = get(h, ac.get().unwrap());
            } else {
                let _ = get(h, bc.get().unwrap());
            }
            TestVal::Data(0)
        });
        let _e = builder.graph.effect(TestVal::Body("switch"));
        let h = builder.finish();

        h.flush();
        assert_eq!(runs.get(), 1);

        // cond==1, so it reads `a`; changing `b` must not rerun.
        h.set(bsig, 201);
        h.flush();
        assert_eq!(runs.get(), 1, "not subscribed to b while cond==1");

        // Changing `a` reruns.
        h.set(a, 101);
        h.flush();
        assert_eq!(runs.get(), 2);

        // Flip cond → now reads `b`, unsubscribes `a`.
        h.set(cond, 0);
        h.flush();
        assert_eq!(runs.get(), 3);

        // Now changing `a` must NOT rerun; changing `b` must.
        h.set(a, 102);
        h.flush();
        assert_eq!(
            runs.get(),
            3,
            "unsubscribed from a after the branch flipped"
        );
        h.set(bsig, 202);
        h.flush();
        assert_eq!(runs.get(), 4, "now subscribed to b");
    }

    #[test]
    fn set_inside_effect_coalesces_into_the_flush() {
        // An effect that sets another signal must drive the dependent effect within the same flush
        // (fixpoint), deterministically, without unbounded reentry.
        let downstream_runs = Rc::new(Cell::new(0));
        let dr = downstream_runs.clone();

        let mirror_c: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let src_c: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let (mc, sc) = (mirror_c.clone(), src_c.clone());

        let builder = Builder::new();
        let src = builder.graph.signal(TestVal::Data(0));
        let mirror = builder.graph.signal(TestVal::Data(0));
        src_c.set(Some(src));
        mirror_c.set(Some(mirror));
        let builder = builder
            .body("copier", move |h| {
                // reads src, writes mirror
                let v = get(h, sc.get().unwrap());
                h.graph.set(mc.get().unwrap(), TestVal::Data(v));
                TestVal::Data(0)
            })
            .body("watch_mirror", move |h| {
                dr.set(dr.get() + 1);
                let _ = get(h, mirror);
                TestVal::Data(0)
            });
        let _copier = builder.graph.effect(TestVal::Body("copier"));
        let _watcher = builder.graph.effect(TestVal::Body("watch_mirror"));
        let h = builder.finish();

        h.flush();
        // Initial: both run; watcher runs at least once.
        let baseline = downstream_runs.get();
        assert!(baseline >= 1);

        // Change src → copier runs, sets mirror, which drives watch_mirror within the same flush.
        h.set(src, 5);
        h.flush();
        assert!(
            downstream_runs.get() > baseline,
            "the mirror watcher must be driven by the copier's set within the flush"
        );
    }

    #[test]
    fn is_flushing_true_during_flush_false_outside() {
        // The coalescing flag the backends consult: outside any flush it is false; while an effect
        // body is running (so a set inside it should enqueue, not nest) it is true; and it is cleared
        // once the flush completes.
        let seen: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let se = seen.clone();

        let b = Builder::new();
        let s = b.graph.signal(TestVal::Data(0));
        let b = b.body("probe", move |h| {
            let _ = get(h, s);
            se.set(h.graph.is_flushing());
            TestVal::Data(0)
        });
        let _e = b.graph.effect(TestVal::Body("probe"));
        let h = b.finish();

        assert!(!h.graph.is_flushing(), "not flushing before any flush");
        h.flush();
        assert!(
            seen.get(),
            "is_flushing() is true while an effect body runs"
        );
        assert!(
            !h.graph.is_flushing(),
            "flushing flag cleared after the flush"
        );
    }

    #[test]
    fn runaway_self_writing_effect_overflows_not_hangs() {
        // An effect that reads a signal and then writes it re-queues itself every round — with no
        // value-equality suppression it never settles. The flush must return FlushOverflow after a
        // bounded number of steps (not loop forever), and clear its flushing flag on the way out.
        let s_cell: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let sc = s_cell.clone();

        let builder = Builder::new();
        let s = builder.graph.signal(TestVal::Data(0));
        s_cell.set(Some(s));
        let builder = builder.body("spin", move |h| {
            let v = get(h, sc.get().unwrap());
            h.graph.set(sc.get().unwrap(), TestVal::Data(v + 1));
            TestVal::Data(0)
        });
        let _e = builder.graph.effect(TestVal::Body("spin"));
        let h = builder.finish();

        assert_eq!(
            h.try_flush(),
            Err(FlushOverflow),
            "a self-reinforcing effect must be bounded, not hang"
        );
        assert!(
            !h.graph.is_flushing(),
            "the flushing flag is cleared after overflow"
        );
    }
}
