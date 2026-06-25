# Phase 6 — Cycle collection + leak guarantee

Phase 5's mutable fields make object cycles possible — close the one hole RC can't. Build the
LXR-principled backup trace *if it earns its place*, benchmark it against the dormant trial-deletion
collector, and make leaks impossible to ship undetected. The Phase-0 leak oracle is the judge.

## 6.1 Wire the dormant trial-deletion collector

`lang-gc`'s Bacon–Rajan collector is complete and miri-tested but never called. Wire it:

- `release` buffers a **candidate root** (`add_candidate`, Purple) when a decrement leaves refcount > 0
  on a type that *can* be cyclic (heap-bearing fields / cells / closures — the only cycle sources).
  Immutable pointer-free values are never buffered.
- Trigger `collect()` on a buffer/allocation threshold and once at clean exit (so the leak oracle sees a
  fully collected heap). Reclaimed objects still run `__destruct` (unreachable ⇒ last reference gone);
  pin a deterministic tie-break for intra-cycle order (e.g. allocation order) in the spec so both
  backends agree.

### 6.1a Closure-captured destruction (carried over from Phase 4.3)

Phase 4.3 implemented container-before-contained destruction (spec §4) for **objects (fields), enum
payloads, and collection (list/map/set) elements** — the structurally-owned, declared-order children
both backends walk identically by construction. It deliberately **excluded closure-captured values**,
which are *lexical* (VM upvalue cells; eval an `Rc<Scope>` with parent links) rather than structural,
have no shared canonical walk order across the two backends, and are the cycle-prone path this phase
owns. The consequence is a **known gap**: a closure that holds the **last reference** to a
destructor-bearing value does **not** fire that value's `destruct` when the closure dies — its memory is
reclaimed (no leak) but the observable side-effect is skipped, **consistently in both backends** (so the
differential stays green; this is today's defined behavior, not a divergence).

Two sub-cases, both landing here:

- **Cyclic capture** (`fn → captured scope → fn`): RC structurally cannot reach it — refcount never
  hits zero — so it was *always* this phase's job. The collector's reclamation walk (6.1 trial-deletion /
  6.2 backup trace) must run `__destruct` on a reclaimed captured value, under the same deterministic
  tie-break as any other cycle member.
- **Acyclic capture** (a closure returned from a factory, captured value uniquely owned, no cycle): RC
  *can* reach it — the captured value's refcount hits zero at the closure's last use — so this is a true
  RC-destruction gap, not a cycle. Closing it means teaching closure release to walk its captured cells /
  scope bindings container-before-contained (run nothing for the closure itself — closures have no
  `destruct` — then `release_value`/`destroy_value` each captured value), the §4 walk extended to the
  one child kind 4.3 skipped. The cross-backend ordering 4.3 lacked is exactly what 6.3's structural fix
  (eval `Scope.parent` → `Weak`, or top-level fns stored outside the captured scope) makes tractable:
  with the parent edge no longer an owning child, a closure's *owned* captures become a walkable,
  matchable set. Sequence it **after 6.3** so eval has a canonical capture-walk order to match the VM's
  upvalue-cell order.

Verification when this lands: a `gc/` conformance case where an acyclic returned closure holds the last
reference to a `Resource` and its `destruct` fires at the closure's last use — **differential agrees**;
plus the cyclic-capture case reclaimed-and-destructed by the collector. Both fold into this phase's
residency-0 gate.

## 6.2 Build the LXR-principled backup trace

Adopt LXR's *principle* (RC backbone + occasional backup trace reaping cycles **and** floating garbage)
as a **stop-the-world mark-sweep over the existing `Box<Obj>` heap** — explicitly **not** Immix/
mark-region (README §7). Simpler to reason about than trial-deletion's trial-decrement-of-live-objects;
catches everything RC missed.

- **Root enumeration behind one seam** (JIT-readiness, README §6): `enumerate_roots(&mut visitor)` — the
  interpreter visits globals + every live frame register + upvalue cells; a future JIT implements it
  with stack maps. Build the seam now; only the interpreter uses it.
- Mark from roots over `heap::children`; sweep frees unmarked via `free_shallow`. Run **only at
  safepoints** (allocation points / between top-level steps) so the root set is always walkable.
- Same deterministic `__destruct` + tie-break as 6.1.

## 6.3 Fix the tree-walker's scope/closure cycles

Eval leaks `global function → captured global scope → function` via Rust `Rc`, no collector. Fix by the
leak oracle:

- **Structural (preferred):** make the global `Scope.parent` a `Weak`, or store top-level functions
  outside the scope they capture, so the cycle never forms — a local, provable fix driving the eval leak
  counter to zero without tracing Rust's `Rc` graph.
- **Fallback:** a mark-sweep over the eval `Value`/`Scope` `Rc` graph at clean exit (harder; needs
  `children`-style enumeration over eval values) — only if the structural fix can't cover all cases.

The leak oracle must reach **residency 0 for the tree-walker too** — closing the debt Phase 0 recorded.

## 6.4 Benchmark both collectors; data picks the default

The mandate's head-to-head: on the cyclic-garbage bench + a mixed workload, compare **trial-deletion**
vs **backup-trace** on collection time, pause distribution, and the buffering/marking overhead they
impose on *non-cyclic* code. Record in `phase-6-benchmarks.md`; **the lower-overhead one becomes the
default**, the other kept behind a flag for comparison. Honest hypothesis to test (not assume): for
small, mostly-acyclic per-isolate heaps, trial-deletion's targeted collection likely beats a full-heap
trace on overhead, while the trace is more robust to floating garbage and simpler — let the numbers and
the leak oracle decide, and document the call. (See `plans/perf/research-notes.md`.)

## Verification gate

- Leak oracle **residency 0 on cyclic-garbage programs, both backends** — leaks are now a test failure.
- Conformance + **differential 0 skipped / agree**, including destructor order for cycle-reclaimed
  objects (the tie-break).
- miri on collector wiring + backup trace + `enumerate_roots`. clippy + fmt clean.
- Both collectors benchmarked; default chosen by data and justified.
