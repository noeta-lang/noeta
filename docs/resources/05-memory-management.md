# Memory management — the as-built model

*The durable reference for how the runtime reclaims memory. `01-architecture.md §5` states the
**design intent** (a refcount + cycle-collector floor with deterministic `__destruct`); this document
records the **implementation** that the memory-management migration (`plans/memory-management/`,
Phases 0–7) actually shipped, and why it is shaped the way it is.*

---

## 1. The model in one paragraph

Both execution tiers run a **shared, lowered Core IR** (A-normal form: every intermediate value
named, control flow structured). On that IR, **precise non-atomic reference counting** is applied as
a sequence of **compiler passes** — last-use/liveness analysis inserts `drop`s, threads reuse tokens,
and chooses in-place mutation — not as advisory annotations over the AST. The tree-walker
*interprets* the RC-annotated Core IR (reclaiming through Rust `Rc`); the bytecode VM *lowers* it 1:1
(reclaiming through a manual refcount heap). A **backup tracing collector** reaps the one thing
refcounting cannot — reference cycles — and runs only at safepoints. Deterministic `__destruct` is
preserved everywhere and fires at an object's **last use**, not at scope teardown.

This is Lean 4's reference-counting model (precise RC + reuse + a runtime `RC == 1` check), adapted to
this runtime's shared-nothing isolates, with an LXR-principled backup trace for cycles.

---

## 2. Why reference counting (not tracing-by-default)

Three constraints make RC the backbone rather than a tracing GC:

1. **Deterministic observable `__destruct`.** The language promises a destructor runs synchronously
   when an object's last reference drops, in program order (resource cleanup — file handles,
   transactions, locks — depends on it). A tracing collector reclaims at unpredictable times, which
   would make finalization best-effort for *all* code. RC gives prompt, ordered finalization for free.
2. **Shared-nothing isolates make RC cheap.** Because each isolate owns its heap and never shares it
   (see `01-architecture.md §7`), refcounts are **non-atomic** — plain integer increments/decrements,
   none of the cache-line contention that makes atomic RC slow.
3. **Immutable value semantics make uniqueness the only path to mutation** — which **reuse** exploits.
   A unique value (refcount 1) can be mutated in place instead of copied; the same uniqueness check
   that protects value semantics also unlocks the in-place fast path.

We keep the **runtime `RC == 1` check** (we *measured* static uniqueness elision as worthless on this
VM) and use static analysis only to place `drop`s and choose reuse — not Perceus' full elision, not
Roc's heavy uniqueness solver, not Immix. Tracing remains available only as the **backup** cycle
collector (§5), never the primary reclaimer.

---

## 3. Why a shared Core IR (not span-keyed facts over the AST)

An earlier draft routed ownership through `Span`-keyed facts over the AST (the `type_of_sites`
pattern) to avoid re-platforming the two backends. That was reversed. Three reasons, the first
decisive:

1. **The AST does not model the values RC operates on.** Reference counting acts on *materialized
   values and their copies* — temporaries, the receiver copy in `obj.field`, register moves. None of
   these exist in the AST. (We proved this the hard way in the perf sweep's reuse prototype: the
   entity that blocked reuse was a retained receiver *temporary* with no AST node.) A Core IR in ANF
   names every temporary, so "this value is born here, dies there, its cell is reused by that
   constructor" is a first-class edge.
2. **Precise RC is a program transformation, and transformations want an IR.** Inserting and moving
   `dup`/`drop`, threading reuse tokens, and re-running drop analysis after reuse is a pipeline of
   passes mutating a representation — natural on an IR, against the grain as a `HashMap<Span, Fact>`
   that two backends re-interpret.
3. **Agreement by construction.** If both backends execute the *same* RC-annotated IR, reclamation
   order **is one program** — identity is structural, not "two hand-written interpretations that
   happen to coincide." For something as subtle as last-use destructor ordering, that is materially
   more correct.

```
        lang-ast (surface AST)
            │  typecheck (lang-check)
            │  lower (AST + types → Core IR, ANF)
        lang-ir  ── named temporaries, structured control flow, dup/drop/reuse slots
            │
        lang-ir-passes  ── precise-RC pipeline ON the IR:
            │     last-use/liveness → drop insertion → reuse-token threading → mutate-when-unique
    ┌───────┴───────────────────────────────┐
 lang-eval                             lang-compiler → lang-bytecode → lang-vm
 INTERPRETS the RC-annotated           LOWERS it 1:1 (drops → Op::Drop, reuse → in-place ops);
 Core IR; reclaims via Rust Rc;        reclaims via manual RC on lang-value/heap
 runs __destruct at IR drop points            │
                                       lang-gc — RC release + cycle reaper (trace ⟷ trial-deletion)
                                              │
                          leak oracle: heap residency == 0 at clean exit (both backends, CI gate)
```

**Policy vs mechanism.** The Core IR with its RC ops *is* the policy (what to reclaim, where, in what
order, what to reuse). Each tier supplies only the *mechanism*: tree-walk + Rust `Rc`; bytecode +
manual RC; later, a JIT + native codegen as a third consumer (§6).

---

## 4. The load-bearing safety invariant

**Static analysis is an optimization input, never a soundness requirement.** Reclamation correctness
always rests on the **runtime refcount** (freed iff the count hits zero) and on scope/teardown
releasing whatever a pass did not. Therefore every `drop` a pass inserts must be **conservative in the
"never too early" direction** — proven dead, or omitted:

- A **late** drop costs promptness only — the value is still reclaimed at scope teardown, never a
  process leak.
- An **early** drop would be a use-after-free, so it must be impossible by construction.

A bug in any RC pass can cost performance, never memory safety. This is what lets the analysis be
aggressive without being dangerous, and it is gated every phase by the property test + miri + the leak
oracle (§7).

---

## 5. Destruction and cycles (the destructor spec)

**Deterministic `__destruct` at last use.** When an object's last reference drops — at the precise IR
point the liveness pass identified, not at the end of the enclosing scope — its destructor runs
synchronously. Children are destroyed **container-before-contained** in declared order (fields, enum
payloads, then collection elements), the order both backends walk identically because they walk the
*same* IR.

**Cycles are the one hole RC cannot close.** Under value semantics a shared mutation *copies*, so
ordinary objects cannot form cycles; the only cycles are **closure/scope self-captures** (a function
holding the scope that captures it). Once mutable fields exist (Phase 5) these become reachable, so a
collector is load-bearing. Two were built and benchmarked head-to-head, the data picking the default
(`plans/memory-management/phase-6-benchmarks.md`):

| | **Trace** (default) | **TrialDeletion** (flag) |
|---|---|---|
| Per allocation | one live-registry insert | nothing |
| Per release | prompt refcount free | buffers a candidate root; defers a buffered last-ref dealloc |
| At collection | mark from roots over the whole live heap, sweep the unmarked | trial-decrement only the buffered subgraph |
| Wins | cyclic garbage (~7–10%), simpler, leaves the hot free path untouched | acyclic churn (~13–17%, zero per-alloc cost) |

**`CollectorMode::Trace` is the default** — for its simplicity, its safety (it never mutates the hot
`free` path), and its win on the cyclic case the collector exists for; its only loss (a per-allocation
`HashSet` registry op on acyclic code) is an artifact closable by an intrusive free-list.
`TrialDeletion` stays available behind `lang_value::set_collector_mode` /
`VmBackend::run_module_with_collector`.

Either collector **identifies** garbage and hands it back as a `lang_gc::Garbage` set; the **VM**
reclaims it — running each fresh member's `__destruct` (while the dead subgraph is still allocated)
before freeing — because a destructor needs the interpreter, which the collector does not have. For
cycle-reclaimed members, intra-cycle destruction order is **best-effort** (a deterministic tie-break;
real programs do not depend on it), matching the weaker guarantee any RC system gives for cycles.

Both collectors reach **residency 0 on the whole corpus, both backends**, and are miri-clean.

---

## 6. JIT readiness

The shared Core IR makes a future JIT a *third IR consumer*, not a rewrite:

- **A JIT wants exactly this IR.** ANF with named temporaries and explicit RC ops is most of the way
  to SSA — the natural input for native codegen. The JIT lowers the *same* RC-annotated IR the
  interpreter runs and the VM compiles, supplying only a better mechanism. The RC ops are already
  placed, so it can **elide/coalesce** inc/dec across hot regions (Swift ARC, Lobster, Nim), which
  non-atomic RC makes plain-integer cheap.
- **The one real constraint is rooting for the backup trace.** Trial-deletion is RC-graph-driven (no
  stack scan, JIT-clean); a backup *trace* must enumerate roots in native frames. So root enumeration
  lives behind one seam — `enumerate_roots(&mut visitor)` — which the interpreter fills today (globals,
  live frame registers, open upvalue cells) and a JIT fills with stack maps later, and the trace runs
  **only at safepoints**.
- **Deterministic `__destruct` limits RC-elision to regions without observable finalization** — a
  pre-existing *language* commitment (cf. Swift `deinit`), neither introduced nor worsened here.

Net: the IR-centered design is strictly friendlier to a JIT than the AST-facts design would have been;
the only forward tax is stack maps *if* a backup trace runs over JIT frames, contained by the
safepoint-only policy and the `enumerate_roots` seam.

---

## 7. The backend-independence tradeoff (and how we keep what matters)

A shared IR **correlates the two backends' failures**: a bug in the shared lowering or an RC pass is
invisible to a differential that compares two consumers of that same IR. That is a real loss — the
differential's power has always come from independence — and it is addressed deliberately:

- **Share *semantics*, keep *memory mechanism* independent — the MM-relevant axis.** The tree-walker
  reclaims via Rust `Rc`; the VM via a manual refcount heap + bytecode. Both consume the same Core IR,
  but the thing MM correctness hinges on — *did the manual refcount machine reclaim the same things at
  the same points as a straightforward `Rc` model* — is still cross-checked, because the two
  executions remain genuinely different machines. We give up independence on "what the program means"
  and keep it on "how memory is managed."
- **Replaced the one lost oracle with several targeted ones:**
  - the **leak oracle** — heap residency `== 0` at clean exit, both backends, whole corpus (a hard CI
    gate);
  - a **static-≤-dynamic last-use property test** — the analysis may never claim a death before the
    real one, checked against a dynamic trace over the corpus;
  - **IR-interpreter ↔ VM differential** — 0 skipped, the two mechanisms agree on every program;
  - **miri** on every refcount/collector path.

**The AST tree-walker is retired as a reference oracle (Phase 7).** It fired destructors only at
global teardown, so once destruction became observable at last use it could no longer reproduce the
reference semantics; promoting it to a *semantics-updated* third oracle would mean maintaining a
second destruction model — pure cost, re-introducing exactly the "two hand-written interpretations
coinciding" fragility the shared IR removed. The Core-IR interpreter is the sole reference; the
`Interpreter` machinery survives only as the shared executor of destructor bodies and leaf semantics
the IR interpreter reuses, and as an AST-walk baseline for the perf benches and property tests
(neither an oracle). The lowering is **total over the parsed language** (gate:
`ir_lowering_is_total_over_the_corpus`), so the reference lowers unconditionally with no fallback.

---

## 8. Verification discipline (every phase, the durable guarantee)

- Conformance green (the count only grows); **differential matched / 0 skipped / both backends agree**.
- **Leak oracle** — residency `== 0` at clean exit, both backends, whole corpus. Leaks are a test
  failure, and the `KNOWN_LEAKS` allowlist is **empty** (any leak fails the gate).
- **Static-≤-dynamic last-use** property test green over the corpus.
- `cargo test --workspace` + targeted **miri** on every refcount/unsafe/collector path.
- clippy + fmt clean.
- Benchmarks: before/after for the metrics each phase moves (`plans/memory-management/phase-7-benchmarks.md`).

---

## 9. Explicit non-goals

- **Full LXR / Immix** (mark-region moving heap, concurrent trace): large-heap server wins our
  per-isolate small heaps would not observe. We adopt LXR's *principle* (RC + occasional backup trace)
  on the existing `Box<Obj>` heap.
- **`gc-arena` / MMTk migration:** evaluated and not taken — the per-isolate small heaps do not
  motivate them, and a tracing-by-default heap would forfeit deterministic finalization.
- **SSA / optimizing IR beyond ANF:** the Core IR is ANF — enough to name values, place RC, and feed a
  future JIT. A full optimizing layer (GVN, etc.) is a separate, non-precluded concern.
- **The JIT itself:** §6 keeps the door open; building it is a later milestone.

---

*See `plans/memory-management/README.md` for the full phase-by-phase rationale, `phase-6-benchmarks.md`
for the collector head-to-head, and `phase-7-benchmarks.md` for the cumulative before/after.*
