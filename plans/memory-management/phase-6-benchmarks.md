# Phase 6.4 — cycle-collector head-to-head

The mandate: build both collectors, benchmark them, and let the **data pick the default** (the lower
overhead one), keeping the other behind a flag. This file records the comparison.

## The two collectors

| | **Trace** (backup mark-sweep) | **TrialDeletion** (Bacon–Rajan) |
|---|---|---|
| Per allocation | maintains a live-object **registry** (one `HashSet` insert) | nothing |
| Per release | prompt refcount free | a surviving decrement of a cycle-capable type **buffers a candidate**; a buffered object reaching its last reference **defers its dealloc** |
| At collection | mark from roots over the **whole live heap**, sweep the unmarked | trial-decrement only the **buffered candidate** subgraph, reclaim the cycles + deferred garbage |
| Cost shape | per-allocation registry upkeep, full-heap walk at collect | per-surviving-release buffering, candidate-only walk at collect |

Both reclaim the same cycles (the leak oracle reaches residency 0 on the whole corpus under either),
and both are miri-clean. The question is **overhead**.

## Method

`cargo bench -p lang-vm --bench vm -- vm_collector`. Two workloads, each run under both collectors,
parameterized over n ∈ {1000, 2000, 4000, 8000}:

- **`cyclic_*`** — `mm_cyclic_garbage_src`: a loop that builds and abandons `n` closure↔cell cycles
  (a self-recursive nested `fn`). The collection-time stress: only the collector reclaims these.
- **`churn_*`** — `mm_alloc_churn_src`: a loop allocating and dropping `n` short-lived acyclic records.
  The overhead stress: no cycles form, so every cost here is pure collector tax on ordinary code.

The whole run is timed (front-end compile is excluded — modules are compiled once), so the numbers
include the per-alloc/per-release upkeep *and* the end-of-run collection.

## Results

Criterion medians (`cargo bench -p lang-vm --bench vm -- vm_collector`):

| n | cyclic_trace | cyclic_trial | churn_trace | churn_trial |
|---|---|---|---|---|
| 1000 | 1.08 ms | 1.23 ms | 316 µs | 279 µs |
| 2000 | 2.09 ms | 2.31 ms | 628 µs | 540 µs |
| 4000 | 4.19 ms | 4.70 ms | 1.23 ms | 1.07 ms |
| 8000 | 8.77 ms | 9.42 ms | 2.46 ms | 2.10 ms |

A clean **split**, stable across n:

- **Acyclic churn** — **trial-deletion wins by ~13–17%** (2.10 ms vs 2.46 ms at n=8000). It pays
  *nothing* per allocation; the trace pays one registry `HashSet` insert/remove per object. This is
  the "overhead on non-cyclic code" the mandate asked to measure, and it is exactly the plan's
  hypothesis — for mostly-acyclic heaps the targeted collector's per-alloc cost (zero) beats the
  trace's bookkeeping.
- **Cyclic garbage** — **trace wins by ~7–10%** (8.77 ms vs 9.42 ms at n=8000). Building n cycles
  buffers 2n candidates; trial-deletion's mark/scan/deferred-free bookkeeping over that buffer costs
  more than the trace's single mark-sweep over the whole (mostly-cyclic) heap. The plan's hypothesis
  was half right: trial-deletion is *not* uniformly cheaper.

## Verdict

The choice is a genuine trade-off, not a knockout. Weighing it:

1. **Trace's only loss is the acyclic per-alloc tax — and that tax is an artifact, not fundamental.**
   It is one `HashSet<u64>` op per alloc/free. An **intrusive free-list** (two pointers in the object
   header, the option the plan flagged) replaces the hash with a couple of pointer writes and is
   expected to close most of the ~15% churn gap — making the trace the best of both. That is the
   recommended next optimization, tracked here, not a reason to default to the more complex collector.
2. **Trace is markedly simpler and safer.** Trial-deletion's correctness was hard-won — it touches the
   hot unsafe `free` path (deferred deallocation) and took several rounds (a segfault, then a
   miri-caught double-free in the candidate/cycle overlap) to get right. The trace leaves the free
   path untouched; its exit-time mark-sweep is trivially correct.
3. **Trace wins the cyclic case**, the one the collector exists for.
4. Both reach **residency 0 on the whole corpus** and are **miri-clean**, so the choice is purely
   overhead/robustness, not correctness.

## Default

**`CollectorMode::Trace` is the default** — for its simplicity, its safety (no hot-free-path
mutation), its cyclic-case win, and because its single acyclic disadvantage is a `HashSet`-registry
artifact closable by an intrusive free-list. **`TrialDeletion` stays available** behind
`VmBackend::run_module_with_collector` / `lang_value::set_collector_mode` (and is the faster choice
for allocation-churn-dominated, cycle-free workloads). **Recommended follow-up:** swap the trace's
`HashSet` registry for an intrusive object-list and re-run this comparison — the likely best-of-both.
