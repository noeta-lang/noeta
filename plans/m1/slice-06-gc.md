# Slice M1.6 — GC cycle collector + `__destruct` + tracing path

Status: done (M1.6a — `destruct` + deterministic destruction; M1.6b — trial-deletion cycle collector. The `gc-arena` tracing path is a deliberate, documented deferral — see below.)

## Goal
Complete the GC floor: synchronous deterministic destruction for the acyclic case, a cycle collector for reference cycles, and a `gc-arena` tracing path as an invisible optimization.

## Scope
- In: synchronous `__destruct` invocation on refcount-zero (acyclic, in program order); a trial-deletion cycle collector reclaiming reference cycles (best-effort destruction order for cycles); deterministic-destruction conformance cases; the `gc-arena` tracing path wired **only** for statically-`__destruct`-free classes, as an internal throughput optimization that must not change any `RunResult`.
- Out: generational/moving GC (never planned); per-isolate non-atomic refcount tuning beyond the single-isolate default (M2 isolates).

## Checklist (vertical slice)
- [x] Grammar / AST: `destruct` block surface — lexer `destruct` keyword, `ClassDecl.destructor: Option<Vec<Stmt>>`, parser (a distinct class member, **not** a method).
- [x] Checker rule: n/a (M1.7 may add a "has `__destruct` ⇒ refcount-managed" classification).
- [x] Bytecode: destructor prototype registration on class definition (`Module.destructors`).
- [x] VM / GC: last-reference destructor dispatch in **both** backends, deterministic ordering. *(Cycle collector + tracing path → M1.6b.)*
- [x] Conformance cases: `gc/destruct_order.lang` (deterministic ordering + reassignment), differential-identical. *(`gc/cycle_reclaimed.lang` deferred to M1.6b — no cycle can form without field mutation; see below.)*
- [x] Snapshots: none required (behavior is in stdout/ordering).

## Definition of done
- [x] Deterministic `__destruct` ordering matches the spec (global scope; differential-identical) — M1.6a.
- [x] Cycles are reclaimed (no leak, no use-after-free; miri-verified) — M1.6b.
- [~] The `gc-arena` tracing path changes no `RunResult` — **deferred** (documented below); nothing to assert until it exists.
- [x] miri green over the whole `lang-value`/`lang-gc`/`lang-vm` surface; fmt/clippy clean.

## Notes / traps
- `__destruct` determinism is the decisive constraint — the tracing path is an optimization for destructor-free classes only and must never make destruction best-effort for code that didn't opt in.
- `gc-arena` carries its own vetted `unsafe`; the *glue* to it is still miri-gated.

## Outcome (M1.6a — `destruct` + deterministic destruction)

This is the **first M1 slice to add a genuinely new language feature** (not a port of existing M0 behavior), so it landed in **both** backends at once — by the post-Thrust-A rule, the destructor's observable output must match across the oracle and the VM. Differential stays at 100% (33 cases matched, zero divergence).

**Surface.** `destruct { ... }` is a distinct class member (lexer keyword, `ClassDecl.destructor`, parser) — not a method, not directly callable, with the instance's fields in scope.

**The decisive obstacle — and the fix.** The differential oracle caught a real cross-model bug: the VM's monotonic register allocator keeps an object's constructor result alive in a *lingering temporary register*, inflating its refcount so a reassigned value's last reference is hidden (its destructor never fired). The fix makes `StoreGlobal` **transfer ownership** (move the value out of the dead source temp instead of retaining a duplicate), matching the tree-walker's direct-binding model. This realigned the reference counts so "last reference drops" means the same thing in both backends.

**Both backends.** A `destruct` block compiles like a parameterless method (receiver in register 0, fields resolving against it). Destruction runs when an object's last reference drops, at two points: **reassignment** (the displaced value) and **program end** (top-level bindings in **reverse declaration order**). The VM gained `Value::refcount()`, a `release_value` that runs the destructor on the about-to-be-final release, and ordered `global_order`; the M0 tree-walker gained ordered scopes (`Scope.order` + `drain_reverse`), `AssignOutcome::Assigned(old)`, and a matching `destroy_value`/`destroy_globals`.

**Scope deliberately bounded (consistent in both → no divergence):** destruction fires for **global-scoped** objects (the canonical "program order" case). Function-local and nested-cascade destruction are not yet wired — *and absent in both backends identically*, so the oracle stays green; they extend incrementally. The cycle collector and `gc-arena` tracing path are **M1.6b**.

### Why the cycle collector / `gc/cycle_reclaimed.lang` is deferred to M1.6b

No reference cycle can form in the language yet: objects are immutable after construction (no field-assignment op exists), and construction can't tie a knot (each field value must already exist). So a trial-deletion collector has no reachable cyclic garbage to exercise, and `gc/cycle_reclaimed.lang` cannot be written as a program. M1.6b adds the collector with a Rust unit test that wires a cycle directly in the heap (RunResult-invisible); a corpus case waits on field mutation (a later slice).

## Outcome (M1.6b — trial-deletion cycle collector)

Added a **Bacon–Rajan synchronous trial-deletion cycle collector** (`lang_gc::CycleCollector`) — the cycle-reclaiming half of the GC floor (architecture §5). The unsafe mechanism lives in `lang-value`'s heap (per-object `Color` + buffered flags in the header, raw non-freeing refcount edits, child enumeration, a child-preserving `free_shallow`, and a `set_slot` mutation primitive — also the foundation for future field assignment); the policy (mark-gray → scan → gather-white → free) lives in `lang-gc`. Two `miri`-checked unit tests wire a real heap cycle via `set_slot`: one verifies an unreachable `A ↔ B` cycle is reclaimed with no leak; the other verifies an externally-referenced object is **spared** (counts restored, no premature free). A subtle correctness point: collection gathers all white objects first, then frees in a flat pass, so a freed member is never dereferenced while tracing.

**Not yet wired into the VM's `release` path, by design:** the language cannot form a reference cycle (objects are immutable after construction — no field-assignment op exists), so no program produces cyclic garbage to buffer as candidate roots. The collector activates once field mutation lands and `release` begins buffering. Wiring it now would add hot-path cost for zero reachable benefit.

### Why the `gc-arena` tracing path is deferred (not a stub — an explicit deferral)

The tracing path manages **`__destruct`-free classes** via a tracing collector instead of refcounting, purely to avoid per-object refcount overhead (architecture §5: "an internal optimization only … must never change observable semantics"). It is deferred deliberately:

1. **Pure throughput optimization, RunResult-invisible.** Destructor-free objects have no observable lifecycle, so the tracing path can change *nothing* a conformance case or the differential oracle can see — there is no behavior to land or assert, only a performance characteristic.
2. **Unjustified without benchmarks.** No `criterion` benchmark yet shows refcounting as a hot-path cost (the M1 bench harness is reserved but unpopulated). Optimizing an unmeasured path is premature.
3. **Heavy dependency for no reachable gain.** `gc-arena` brings its own vetted `unsafe` and a tracing discipline; integrating it (and the cross-heap-reference invariants §5 warns about) is a sub-project whose payoff is moot while no cycles form and no benchmark demands it.

The trigger to revisit: a benchmark demonstrating refcount overhead on a hot allocation path, *after* field mutation makes the object graph rich enough to matter. This is recorded so the deferral is a decision, not an omission.
