# noeta-reactive

The reactive-graph core: the deterministic bookkeeping behind server-side signals (`signal`/`computed`/`effect`), architecture §9.4.

- **Takes in:** nothing beyond its own generics — it is **value-generic** (`ReactiveGraph<V>`), never inspects a value, never runs a closure, does no I/O. The one backend-specific step ("run this node's body closure") is threaded in as a callback.
- **Emits:** the node table, bidirectional dependency edges (sources ↔ subscribers), the dirty-propagation walk, the dynamic current-computing stack, the dirty-effect queue, and a deterministic [`ReactiveGraph::flush`].

This crate owns the oracle-critical half of reactivity — graph structure and scheduling — and nothing else. Because the algorithm here is shared verbatim by both backends (the tree-walker's `call_closure`, the VM's `call_value` plug into the same callback), the scheduling is differential-by-construction: two backends running the same program drive the same graph through the same deterministic order, so their `RunResult`s agree without a per-backend reimplementation to keep in sync. Evaluation is lazy-memo (the SolidJS model): a `computed` recomputes on read only when dirty; an `effect` is eager, queued on creation and rerun whenever a dependency changes; `set` marks dependents dirty and enqueues affected effects but runs nothing itself until the caller flushes. Effect order within a flush round is ascending `NodeId` (creation order), with no wall-clock, hash-order, or thread dependence — determinism the differential and leak oracles both depend on. Disposal severs every graph edge a node has (explicit `effect(...).dispose()`, or owner-tree teardown when a computed/effect body creates nested reactive nodes), so the leak oracle's residency stays 0.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
