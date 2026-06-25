# Phase 0 — Foundations & invariants

The safety net and the measuring stick, built *before* any reclamation change. Nothing here changes
observable behavior; it makes the rest of the track provable and measurable.

## 0.1 Leak oracle (the answer to "I can't tell if we leak")

A permanent CI gate asserting **zero live heap residency at clean program exit**, in *both* backends.
A cycle leak — or any missed release — becomes a test failure, regardless of which collector is in use.

- **VM:** add a process-wide (per-isolate) **allocation counter** to `lang-value/heap.rs`: `alloc`
  increments a thread-local/`Cell` live-object count, `free`/`free_shallow` decrement it. Expose
  `heap::live_count() -> usize`. After `VmBackend::run_module` completes *and* all globals + frames are
  released, assert `live_count() == 0` (in a test/oracle build; behind a cheap cfg so release builds
  pay nothing, or always-on since it's one integer).
- **Tree-walker:** harder (Rust `Rc` frees implicitly). Provide `heap`-equivalent accounting for the
  eval `Value` aggregates via a shared **`live_count`** hook, OR — simpler and sufficient — run the
  eval corpus under a Rust **allocation-tracking global allocator** in the oracle test binary and
  assert no retained `ObjectValue`/`Closure`/`Scope` after `run` returns. Prefer an explicit eval-side
  counter (construct/drop of the Rc payloads) for parity with the VM number.
- **Harness:** extend the conformance/differential runner with a `--check-leaks` mode (and make it the
  default in CI) that, for every program, asserts both backends end at residency 0. Programs that
  *intentionally* retain to process exit (none today; the eval scope-cycle is the known offender Phase
  5 fixes) are the only allowed exceptions, listed explicitly.

Until Phase 6 fixes the tree-walker's scope/closure cycle, the oracle will *flag* that known leak —
which is the point: it documents the exact debt and turns its fix into a measurable gate.

## 0.2 Destructor-ordering specification

Write `plans/memory-management/destructor-order-spec.md` pinning the rules the expanded semantics
(Phase 3) will follow, agreed *before* implementation so both backends target one spec:

- **When:** a `__destruct` runs at the **last use** of the last owning reference (RC reaches zero),
  not at scope end — for *every* scope (global, local, nested block, function frame), not just globals.
- **Order within a scope:** reverse order of *construction* (LIFO) — matches today's global rule,
  generalized.
- **Fields vs container:** when an object is destroyed, its `destruct` runs **first**, then its fields
  are released (and any field reaching zero runs its own `destruct`) — i.e. **container before
  contained**, depth-first. (Decide and pin: this is the RAII-natural order and what `free`'s recursive
  release already implies structurally; Phase 3 makes the field destructors actually fire.)
- **Reassignment:** the displaced value is destroyed immediately (today's rule, retained).
- **`?` early return / `break` / `continue` / panic unwinding:** values live at the abandoned point are
  destroyed in reverse-construction order as control leaves their scope. Pin the interaction with `?`
  precisely (the early-returned value is *moved out*, not destroyed).

This spec is the contract Phase 1's analysis encodes and Phase 3 implements identically in both
backends.

## 0.3 Benchmark baseline (the "before")

Capture current MM performance so Phase 6 can prove the migration's effect:

- Reuse existing benches (`vm.rs`, `eval.rs`) — dispatch, property access, allocation, the reuse
  matrix.
- Add MM-stress benches: **allocation churn** (build/drop many short-lived objects), **destructor-heavy**
  (many objects with `destruct` blocks), **deep-structure free** (one large nested object torn down),
  and a **cyclic-garbage** micro-bench (build N cycles, drop the roots) to be wired once Phase 6
  collectors exist. Record peak live-count alongside time (the leak counter doubles as a
  peak-residency meter).
- Snapshot the numbers in `phase-0-benchmarks.md`. This is the column every later phase compares to.

## Verification gate

- Leak oracle compiles and runs over the full corpus; both backends reported (the known eval
  scope-cycle is recorded as the one expected non-zero, to be driven to zero by Phase 5).
- Baseline numbers recorded. No behavior change; conformance + differential unchanged.
- miri clean on the new heap counter paths.
