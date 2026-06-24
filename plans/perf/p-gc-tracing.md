# P-GC — `gc-arena` tracing path for `__destruct`-free classes (+ VM-side COW)

Status: **planned** (sweep item #3). Source: deferred backlog "`gc-arena` tracing path for
`__destruct`-free classes (refcount is the only path today)" (M1.6).

## The cost

The VM heap is refcounted (the only path). Refcount has two costs this slice targets: per-operation
inc/dec traffic, and the inability to collect reference cycles (a cycle of `__destruct`-free objects
leaks until process exit). A tracing path for classes that declare **no `__destruct`** removes the
refcount traffic for those objects and collects their cycles — `__destruct`-bearing classes keep the
refcount path because they need deterministic, ordered finalization.

## The fix

- Route `__destruct`-free class instances through a `gc-arena` tracing arena; keep refcount for
  classes with a destructor (deterministic finalization order is observable for those).
- The split is a *compile-time* property of the class (does its decl have `__destruct`?), so the
  backend picks the allocation path per shape.

## Carries: VM-side P-COW

This slice also lands the **VM half of copy-on-write list append** (deferred from P-COW), because it
needs exactly what this slice builds: **uniqueness information from the heap allocator**. With the
arena/uniqueness machinery in place, the VM's `~` concat can mutate a uniquely-owned backing buffer
in place (mirroring the eval-side COW), closing the temporary eval-O(n)/VM-O(n²) asymmetry. After
this slice both backends are O(n) on the accumulator loop.

## Benchmark (validates the gain)
- `allocation_list` (existing) — alloc/free churn; tracing should reduce per-op overhead.
- The Phase 0 parameterized accumulator on the **VM** backend — should drop from O(n²) to O(n),
  matching the eval column. Record both backends' after-numbers side by side.
- A cycle-leak probe (allocate a cycle of `__destruct`-free objects in a loop; under refcount memory
  grows, under tracing it's collected) — a memory, not time, validation.

## Risk & sequencing
Heaviest, structural item — touches the ownership model and the heap object header. Sequenced last
of the three VM items so it can absorb the IC key (P-IC) and the COW uniqueness need together.
Behavior is unchanged for `__destruct`-free classes (no observable finalization), so differential
stays 0-skipped / agree.

## Verification
Conformance + differential unchanged. Workspace/clippy/fmt clean; **miri** on the heap path (this is
the unsafe-adjacent slice — extra miri scrutiny). Bench + cycle-probe numbers recorded. Update
`plans/deferred.md` to strike the VM-COW half of the P-COW row. Branch `types-inferred-static`.
