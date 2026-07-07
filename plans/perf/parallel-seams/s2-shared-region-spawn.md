# S2 — P-PAR-SHARE: wire `SharedRegion` into the real spawn path

## Today

`try_spawn_isolate_real` (`crates/noeta-vm/src/scheduler.rs:367`) marshals every argument — and
every shippable global — into a `Wire` deep copy **per worker**; the worker rebuilds fresh
objects on its own heap. `isolate score(bigCorpus)` fanned to N workers copies the corpus N
times (S0a measures this). Meanwhile `SharedRegion`
(`crates/noeta-value/src/heap.rs:1081`) — promote-once into `shared`-tagged objects
(retain/release no-op, no refcount races), borrow zero-copy, `free_all` wholesale — is built and
miri-proven (I.3) but has **no caller on the real path**.

## The change

1. **Promotion at spawn.** In `try_spawn_isolate_real`, promote each `Send` argument graph into
   a region instead of marshalling it; hand the worker the promoted root `Value` directly (it is
   a plain pointer into shared-tagged objects — after S1 every embedded handle is `Arc`, so the
   raw bits are `Send` for this purpose; wrap in a small `SendWire`-style newtype with the
   safety argument documented). Unshippable args (channel endpoints) keep the existing
   cooperative fallback; channels-in-args keep `Wire` (they are endpoints, not data).
2. **Promote once across the fan-out.** The region + its promotion memo live on the *scheduler*
   (or the owning scope entry), keyed by source-object identity, so N spawns of the same corpus
   in one scope hit the memo and share one promoted graph. This is the whole point — a
   per-spawn region would still promote N times.
3. **Lifetime.** The region must outlive every borrowing worker. Free at the structured join:
   `free_all` only after every isolate spawned against the region has been joined
   (`finish_isolate`, `scheduler.rs:57` — thread `handle.join()` already happens there). The
   simplest sound shape: region owned by the scope entry that owns the isolate futures, freed
   when the scope pops (after `join_scope` guarantees all tasks completed). Workers never free
   shared objects (`free_shared` is region-only); worker-side retain/release on them are no-ops
   by the shared tag.
4. **Globals** stay on the `Wire` snapshot path for now (they are usually small: functions +
   constants). If S0a shows a big-global pattern matters, extend the same memo to globals as a
   follow-up, not in this slice. *(Deferral noted per standing directive — flag at review, it
   is listed here up front.)*
5. **Result path unchanged** (`Wire` back from worker): results are worker-heap values whose
   owner is about to exit; copying back is correct and cheap relative to inputs.

## Semantics / oracle posture

Copy ≡ borrow for immutable `Send` value types — observable behaviour is identical by
construction, and the checker's E0042 `Send` classifier already guarantees nothing mutable or
identity-bearing reaches an isolate argument. The deterministic sandbox keeps copying
(`Wire`) — the differential never sees a region, same as I.4. The **leak oracle** and **miri**
are the real gates: promoted-graph residency must return to 0 at the join, and no worker may
touch a freed region (structured join is the proof obligation).

## Gate

- **S0a re-run:** fan-out wall time and peak residency — expect ~1× corpus residency instead of
  ~N×, and spawn cost flat in N for the copy component.
- CLI isolate integration tests green; existing I.3 `SharedRegion` unit/miri tests still green.
- A new test: N workers, one big arg, assert (via heap counters) promotion happened once.

## Numbers

_Before (S0a) / after table to be recorded here._
