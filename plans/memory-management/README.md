# Memory-management migration — shared Core IR, precise RC + reuse, backup-trace cycles

**Mandate (user, 2026-06-24):** build the ideal MM model *right*, **no components deferred**, and do
not let the *current codebase's shape* constrain the design — if the mature/correct foundation is a
shared lowered IR rather than span-keyed annotations over the AST, plan for that too. Resources and
time are not constraints. Then **benchmark the result against what we have now**, and keep a future
**JIT** unforeclosed.

This is a milestone-scale track. Every phase keeps the differential oracle green, conformance green,
miri-clean, clippy+fmt clean, **and** passes a new leak oracle.

> **Supersedes the first draft of this plan.** The initial version routed ownership through
> `Span`-keyed facts over the AST (the `type_of_sites` pattern) to avoid re-platforming the two
> backends. That was a migration-cost decision masquerading as an architecture one. Unconstrained, the
> correct foundation is a **shared lowered Core IR** that both backends execute; reference counting,
> reuse, and last-use destruction are **IR transformations**, not advisory annotations. See §2 for why,
> including the honest tradeoff this makes against backend independence and how we compensate.

---

## 1. Target model (the decision)

A **shared, lowered Core IR** (A-normal form: every intermediate value explicitly named, control flow
structured) is the single program both execution tiers run. On that IR we run **precise non-atomic
reference counting with reuse and runtime `RC == 1` checks** (Lean 4's model) as a sequence of
**passes** that insert `dup`/`drop`, choose reuse, and pick in-place mutation. The tree-walker
*interprets* the RC-annotated Core IR; the VM *lowers* it to bytecode 1:1. A **backup tracing collector**
reaps cycles; deterministic `__destruct` is preserved and extended to every scope.

Why **Lean's RC** (settled earlier, unchanged): deterministic observable `__destruct` forbids tracing
as *primary*; shared-nothing isolates make RC non-atomic (plain integer ops); immutable value
semantics make uniqueness the only path to mutation, which reuse exploits. We keep the runtime
`RC == 1` check (we *measured* static elision as worthless on this VM) and use static analysis only to
place drops and choose reuse. Not full Perceus' elision, not Roc's heavy uniqueness solver, not Immix.

---

## 2. Why a shared Core IR, not span-keyed facts (the reversal, argued)

Three reasons, the first decisive:

1. **The AST does not model the values RC operates on.** Reference counting acts on *materialized
   values and their copies* — temporaries, the receiver copy in `obj.field`, register moves. **None of
   these exist in the AST.** We proved this the hard way in P-REUSE: the entity that blocked reuse was a
   retained receiver *temporary* with no AST node, invisible to an AST-level analysis, which is why we
   ended up with the `drop_receivers` heuristic instead of a principled answer. A Core IR in ANF names
   every temporary, so "this value is born here, dies there, and its cell is reused by that constructor"
   is a first-class edge — last-use and reuse live where the values actually live.

2. **Precise RC is a program transformation, and transformations want an IR.** Perceus/Lean insert and
   move `dup`/`drop`, thread reuse tokens (a freed cell handed to a later constructor), specialize, and
   *re-run* drop analysis after reuse. That is a pipeline of passes mutating a representation. Doing it as
   a `HashMap<Span, Fact>` over an immutable AST that two backends then *re-interpret* is fighting the
   grain; doing it as IR passes is the grain.

3. **Agreement by construction beats two interpretations of hints.** The differential's core promise is
   "both backends behave identically." Span-facts share only the *analysis*; each backend still *acts*
   on it independently (eval runs destructors at spans, the VM inserts drops at spans), so identity is
   only as good as two hand-written interpretations coinciding — exactly the fragile "both backends omit
   local destructors *identically* by careful hand-construction" we found in today's code. If both
   backends execute the **same RC-annotated IR**, reclamation order *is one program*; identity is
   structural. For something as subtle as last-use destructor ordering, that is materially more correct.

### The honest cost: backend independence — and how we keep what matters

A shared IR **correlates the two backends' failures**: a bug in the shared lowering or an RC pass is
invisible to a differential that compares two consumers of that same IR. That is a real loss; the
differential's power has always come from independence. We address it deliberately rather than wave it
away:

- **Share *semantics*, keep *memory mechanism* independent — which is the MM-relevant axis.** The
  tree-walker reclaims via **Rust `Rc`**; the VM via a **manual refcount heap + bytecode**. Both consume
  the same Core IR, but the thing MM correctness hinges on — *did the manual refcount machine reclaim the
  same things at the same points as a straightforward `Rc` model* — is still cross-checked, because the
  two *executions* (interpret-with-Rc vs compile-to-bytecode-and-run-with-manual-RC) remain genuinely
  different. We give up independence on "what the program means," keep it on "how memory is managed."
  That is the right place to share and the right place to stay independent.
- **Replace the one lost oracle with several targeted ones:** the **leak oracle** (heap residency 0 at
  exit, both backends); a **static-≤-dynamic last-use property test** (the analysis may never claim a
  death before the real one, checked against a dynamic trace over the whole corpus); **IR golden +
  differential tests** (lowering faithfulness, pass output); and **miri** on every refcount path.
- **Retain the current naive AST-walker as a transitional reference oracle.** During the migration the
  new IR-interpreter is differential-tested against the *old* AST-walker (§Phase 1); we keep the old
  walker until the IR path is proven, and re-decide at the end whether to retire it or keep it as a
  permanent third, maximally-independent oracle.

### The load-bearing safety invariant (unchanged)

Static analysis is an **optimization input, never a soundness requirement.** Reclamation correctness
always rests on the **runtime refcount** (freed iff count hits zero) and on scope/teardown releasing
whatever a pass didn't. Therefore every last-use/`drop` a pass inserts must be **conservative in the
"never too early" direction** — proven dead or omitted. A late drop costs promptness (caught by
teardown, never a process leak); an early drop would be a UAF, so it must be impossible by
construction. A bug in any RC pass can cost performance, never memory safety — and the property test +
miri + leak oracle gate every phase.

---

## 3. Architecture target

```
            lang-ast (surface AST)
                │  typecheck
            lang-check ──► Checked { types, type_of_sites, … }
                │  lower (AST + types → Core IR, ANF)
            lang-ir  ── Core IR: named temporaries, structured control flow, slots for dup/drop/reuse
                │
            lang-ir-passes  ── precise-RC pipeline ON the IR:
                │     last-use/liveness → dup/drop insertion → reuse-token threading
                │     → mutate-when-unique → (drops carry destructor-relevance)
                │     all conservative ("never too early"); runtime RC is the backstop
        ┌───────┴────────────────────────────┐
   lang-eval                            lang-compiler ──► lang-bytecode ──► lang-vm
   INTERPRETS the RC-annotated          LOWERS the RC-annotated Core IR → bytecode 1:1
   Core IR; reclaims via Rust Rc;       (drops → Op::Drop, reuse → in-place ops, last-use
   runs __destruct at IR drop points    moves → consuming moves); reclaims via manual RC
                                                │
                                        lang-value/heap (RC primitives, in-place mutation)
                                                │
                                        lang-gc (RC release + cycle reaper)
                                   trial-deletion ⟷ backup mark-sweep trace (built, benchmarked, winner=default)
                                                │
                            leak oracle: live heap residency == 0 at clean exit (both backends, CI gate)
```

**Policy vs mechanism, sharpened by the IR.** The Core IR with RC ops *is* the policy (what to reclaim,
where, in what order, what to reuse). Each tier supplies the *mechanism*: tree-walk + Rust Rc; bytecode
+ manual RC; later, **JIT + native codegen — a third IR consumer** (§6). The IR makes the
policy/mechanism split concrete instead of conventional.

---

## 4. Phases (each its own green, benchmarked commit; none deferred)

| Phase | Title | Delivers |
|------:|-------|----------|
| **0** | [Foundations & invariants](phase-0-foundations.md) | Leak oracle (heap-residency CI gate, both backends); destructor-ordering **spec**; MM benchmark baseline (the "before"). |
| **1** | [Core IR + lowering + IR interpreter](phase-1-core-ir.md) | `lang-ir` (ANF Core IR); `AST → Core IR` lowering; an IR tree-interpreter as a new eval path, **differential-tested against the existing AST-walker** (faithfulness proof). No RC change; VM still compiles from AST. |
| **2** | [VM lowers from Core IR](phase-2-vm-on-ir.md) | Re-point the compiler: **Core IR → bytecode** (behavior-preserving). Now *both* backends execute the same lowered IR — the shared-foundation milestone — still with today's reclamation. |
| **3** | [Precise RC on the IR](phase-3-rc-passes.md) | The RC pipeline as IR passes: last-use/liveness, `dup`/`drop` insertion, reuse-aware register lowering (subsumes P-REUSE's targeted drops + monotonic allocator). Prompt reclamation; behavior-invisible (destructors still globals-only). |
| **4** | [Expanded deterministic destruction](phase-4-destruction.md) | `__destruct` at last use for locals, nested scopes, **and fields** — **correct by construction** (one RC-annotated IR, both backends). The observable upgrade, de-risked by the shared IR. |
| **5** | [Generalized reuse + mutate-when-unique](phase-5-reuse-mutation.md) | Reuse for **all** constructors as an IR pass (subsumes COW + record reuse); mutable fields & `FileHandle` as in-place-**when-unique** (same uniqueness machinery). |
| **6** | [Cycle collection + leak guarantee](phase-6-cycles.md) | Wire trial-deletion; build the LXR-principled **backup mark-sweep trace** with a JIT-ready `enumerate_roots` seam; fix the tree-walker's scope/closure cycles; leak oracle green on cyclic garbage; **benchmark both collectors, data picks the default**. |
| **7** | [Finalize & full benchmark](phase-7-finalize.md) | Retire superseded code (and decide the AST-walker oracle's fate); the full **before/after** benchmark vs Phase 0; MM model doc; memory update. |

**Ordering rationale:** the IR foundation must land first and faithfully (P1–P2) before anything builds
on it; reclamation correctness (P3) is invisible and goes before the one observable change (P4), which
is now safe because both backends run the *same* IR; performance generalizations (P5) then cycles (P6,
which needs P5's mutation to even form object cycles), then the head-to-head benchmark the mandate
names (P7).

---

## 5. Verification discipline (every phase)

- Conformance green (count only grows); **differential matched / 0 skipped / agree**.
- **Leak oracle** — residency `== 0` at clean exit, both backends, whole corpus (hard gate from P0 on).
- **Static-≤-dynamic last-use** property test green over the corpus (from P3 on).
- `cargo test --workspace` + targeted **miri** on every refcount/unsafe/collector path touched.
- clippy + fmt clean.
- **Benchmark** — before/after for the metrics each phase moves; the cumulative comparison is P7.

---

## 6. JIT readiness (forward-compatibility; strengthened by the IR)

The shared Core IR makes a future JIT *easier*, not harder — it becomes a third IR consumer:

- **A JIT wants exactly this IR.** ANF with named temporaries and explicit RC ops is the natural input
  for native codegen (it is most of the way to SSA). A JIT lowers the *same* RC-annotated Core IR the
  interpreter runs and the VM compiles — the policy/mechanism split (§3) means the JIT only supplies a
  better mechanism. The RC ops are already placed; the JIT can **elide/coalesce** inc/dec across hot
  regions (mature: Swift ARC, Lobster, Nim), which non-atomic RC makes plain-integer cheap and which
  tracing GC would instead burden with write barriers + safepoint maps.
- **The one real constraint is the cycle collector's rooting** (unchanged from the prior analysis):
  trial-deletion is RC-graph-driven → no stack scan → JIT-clean; a backup *trace* must enumerate roots
  in native frames → needs precise stack maps + safepoints. So we keep trial-deletion primary, run any
  backup trace **only at safepoints**, and put root enumeration behind one `enumerate_roots(&mut
  visitor)` seam (Phase 6) the interpreter fills today and a JIT fills with stack maps later.
- **Deterministic `__destruct`** limits RC-elision to regions without observable finalization — a
  pre-existing *language* commitment (cf. Swift `deinit`), neither introduced nor worsened here; a JIT
  honors it as it honors any observable effect.

**Net:** the IR-centered design is strictly friendlier to a JIT than the AST-facts design would have
been; the only forward tax is stack maps *if* a backup trace runs over JIT frames, contained by the
safepoint-only policy and the `enumerate_roots` seam.

---

## 7. Explicit non-goals (deliberate scope, not deferrals)

- **Full LXR / Immix** (mark-region moving heap, concurrent trace): large-heap server wins our
  per-isolate small heaps wouldn't observe; we adopt LXR's *principle* (RC + occasional backup trace)
  on the existing `Box<Obj>` heap and let Phase 6's benchmark earn it. (`plans/perf/research-notes.md`.)
- **SSA/optimizing IR beyond ANF.** The Core IR is ANF — enough to name values, place RC, and feed a
  future JIT. A full optimizing SSA layer (GVN, etc.) is a separate concern; ANF doesn't preclude it.
- **The JIT itself.** §6 keeps the door open; building it is a later milestone.
