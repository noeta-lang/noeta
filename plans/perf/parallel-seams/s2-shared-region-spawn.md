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

### Audit obligations found during S1

- **COW uniqueness vs the shared tag (CRITICAL)**: `alloc_shared` leaves `refcount: 1` (frozen —
  retain/release no-op on shared), and the VM's in-place-mutation gates are bare
  `refcount() == 1` (`vm/lib.rs:2034/2334/2342/2370/3537`, plus the `*_in_place` preconditions
  in `noeta-value`). A worker's `l ~= [x]` on a borrowed shared list would pass the check and
  mutate the cross-thread shared buffer in place. S2 must add `is_uniquely_owned()`
  (`refcount == 1 && !is_shared`) and convert every uniqueness gate, with a conformance case
  (worker appends to a borrowed corpus → must copy, parent's corpus unchanged).

- **`set_reflect` on shared objects**: heap objects carry a mutable `Option<Rc<TypeRepr>>` reflect
  tag (`heap.rs`), written by `set_reflect` at construction sites. `alloc_shared` sets
  `reflect: None` and `Wire` rebuilds don't tag either (behavioural parity ✓), but S2 must verify
  no worker-side path calls `set_reflect` on a *borrowed* shared object — that would be a
  cross-thread data race on the tag (and an `Rc<TypeRepr>` clone across threads). If any such
  path exists, promotion-freezes-the-tag is the rule to enforce.

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

## Shipped design (deltas from the sketch)

- Args promote **per-VM** (region + memo + retained sources on `Vm`), not per-scope: freed when
  the last in-flight isolate is joined (`finish_isolate` at count 0) and defensively at teardown.
  Coarser than per-scope but sound with less bookkeeping; revisit if a long-lived isolate pins a
  big drained corpus.
- Promotability is a pre-walk (`Value::is_promotable_graph`): `Send` **data** kinds only — a
  function value/bound method/channel endpoint keeps the `Wire` copy path (`IsoArg::Copied` vs
  `IsoArg::Borrowed(SharedRoot)`). `SharedRoot` is the one sanctioned `Value` thread-crossing,
  living in `noeta-value` (the crate that owns `unsafe`) with the safety argument on the type.
- The memo retains each first-promoted source root so an entry can never alias a
  freed-and-reallocated address; sources release at region free.
- COW hardening shipped with it: `Value::is_uniquely_owned()` (`refcount == 1 && !shared`)
  replaced every bare `refcount() == 1` gate in the VM (8 sites) and the in-place preconditions
  in `noeta-value` — a worker "mutating" a borrowed corpus copies, verified end-to-end by the
  new CLI real-path test (`run_real_isolate_borrowed_arg_mutation_is_isolated`).

## Numbers

2026-07-07, `tests/bench/parallel-seams/run.py` (median of 7), 100k-record corpus.

| Fixture | wall before → after | cpu/run before → after | max RSS before → after |
|---|--:|--:|--:|
| `fanout_n1` | 112.1 → **100.4 ms** | 110.9 → 98.1 ms | 94 → **67 MB** |
| `fanout_n2` | 131.6 → **102.6 ms** | 175.7 → 109.6 ms | 143 → **68 MB** |
| `fanout_n4` | 168.1 → **107.3 ms** | 289.3 → 130.0 ms | 189 → **70 MB** |
| `fanout_n8` | 232.6 → **126.6 ms** (**1.84×**) | 519.7 → 177.0 ms (**2.9×**) | 224 → **73 MB** (**3.1×**) |

Both S2 targets hit: **residency ~flat in N** (one promotion, N borrows — +6 MB across 8
workers vs ~+50 MB/worker before) and **wall near-flat in N** (the serialized per-worker marshal
is gone; the residual ~3.7 ms/worker is thread spawn + worker VM setup + the actual compute).
n1 improves too: one promotion copy beats the old marshal→`Wire`→rebuild double conversion.
Ping-pong fixtures unchanged (S3's regime).
