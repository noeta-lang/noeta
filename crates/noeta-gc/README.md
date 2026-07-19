# noeta-gc

Garbage collection: the runtime-wide memory-management floor.

- **Takes in:** `Value`/`Color` (from `noeta-value`).
- **Emits:** the GC policy — `retain`/`release` over the refcount primitives, plus `CycleCollector`, a Bacon–Rajan synchronous trial-deletion cycle collector.

The GC is refcount + a cycle collector (architecture §5). This crate owns the *policy* (when to free; the collection algorithm); the unsafe refcount/graph *mechanism* lives in `noeta-value`'s heap. `__destruct` ordering is the VM's job (a destructor needs the interpreter, so the collector hands identified garbage back as a `Garbage` set and the VM runs destructors before freeing). The collector is active — field mutation shipped, so closure/scope self-captures can form cycles. Two collectors exist: the default `Trace` (mark-from-roots then sweep; nothing on the hot `free` path) and `TrialDeletion` (the Bacon–Rajan synchronous trial-deletion collector, behind `set_collector_mode`, which wins on acyclic churn). Both reach heap residency 0 on the whole corpus, both backends, `miri`-clean. (The once-deferred `gc-arena` tracing path was superseded: cycle collection ships via these two collectors instead.)

Both collectors also run **mid-run at interpreter safepoints** (`collect_trace_safepoint` / `collect_trial_deletion_safepoint`), bounding a cycle-building loop's peak residency. A safepoint collection never runs a destructor: dead weakly-connected components containing a destructor-bearing member are deferred intact to the exit collection (trial deletion restores their trial decrements and re-buffers them), and the trace variant additionally aborts wholesale if its garbage set does not exactly refcount-balance — so a missed root can cost liveness until exit, never a use-after-free. See the [Memory Management](../../docs/Memory-Management.md) wiki page.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
