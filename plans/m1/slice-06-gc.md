# Slice M1.6 — GC cycle collector + `__destruct` + tracing path

Status: todo

## Goal
Complete the GC floor: synchronous deterministic destruction for the acyclic case, a cycle collector for reference cycles, and a `gc-arena` tracing path as an invisible optimization.

## Scope
- In: synchronous `__destruct` invocation on refcount-zero (acyclic, in program order); a trial-deletion cycle collector reclaiming reference cycles (best-effort destruction order for cycles); deterministic-destruction conformance cases; the `gc-arena` tracing path wired **only** for statically-`__destruct`-free classes, as an internal throughput optimization that must not change any `RunResult`.
- Out: generational/moving GC (never planned); per-isolate non-atomic refcount tuning beyond the single-isolate default (M2 isolates).

## Checklist (vertical slice)
- [ ] Grammar / AST: `destruct` block surface (if not already parsed) as a distinct construct — **not** a trait, not an ordinary function (GC invokes it).
- [ ] Checker rule: n/a (M1.7 may add a "has `__destruct` ⇒ refcount-managed" classification).
- [ ] Bytecode: destructor hook registration on class definition.
- [ ] VM op / GC: refcount-zero destructor dispatch, cycle-collector roots + trial deletion, tracing-path selection (`lang-gc`).
- [ ] Conformance cases: `gc/destruct_order.lang` (deterministic ordering), `gc/cycle_reclaimed.lang`; assert tracing path leaves output identical via `--differential`.
- [ ] Snapshots: none required (behavior is in stdout/ordering).

## Definition of done
- Deterministic `__destruct` ordering matches the spec; cycles are reclaimed (no leak under stress mode).
- The `gc-arena` tracing path changes no `RunResult` (differential-asserted).
- miri green over the whole `lang-gc`/`lang-vm` surface; fmt/clippy clean.

## Notes / traps
- `__destruct` determinism is the decisive constraint — the tracing path is an optimization for destructor-free classes only and must never make destruction best-effort for code that didn't opt in.
- `gc-arena` carries its own vetted `unsafe`; the *glue* to it is still miri-gated.
