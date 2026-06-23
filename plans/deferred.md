# Deferred backlog — items pushed past the slice that introduced them

Slices are marked **done** when they meet their gate, but several deliberately left
non-gate work for later. Recorded *only* inside a done slice, such an item is effectively
invisible — a future planner reading the roadmap never sees it. This file is the single
discoverable home for every cross-slice deferral, so nothing is lost between milestones.

**Discipline:** when a slice defers something non-gate, add a row here in the same commit.
When a later slice picks it up, strike the row and point to the slice that closed it. Each row
names the **source slice** (the rationale lives there) and a concrete **trigger** — the
condition that should make us actually do it, so we neither do it prematurely nor forget it.

Nothing here is a correctness gap in shipped behavior: each item is either **latent**
(unreachable by the current corpus, so the differential's `0 skipped` gate still holds),
a **perf** optimization that is invisible to `RunResult`, or a **hardening** task. If any
"latent" item ever becomes reachable, it stops being optional — see its note.

## VM completeness (latent — would break `0 skipped` if a program reached it)

The compiler returns [`Unsupported`] for these, and the differential harness *skips* such a
program. No corpus case exercises any of them today, so the gate holds — but the moment a
program does, that program silently leaves the differential.

**Closed by slice F1 (`plans/followups/slice-f1-upvalues.md`)** — the VM now has real upvalue
machinery (closed cells; see [`lang_compiler::freevars`]):

| Item | Source | Status |
|---|---|---|
| ~~True upvalues — a nested closure capturing an enclosing **function's local**~~ | M1.2, M1.5 | **Done (F1)** — `closures/capture_param`, `transitive_capture` |
| ~~Nested `fn` declarations inside a function body~~ | M1.2 | **Done (F1)** — `closures/counter_nested_fn`, `recursive_nested_fn` |
| ~~Bare `x = expr` non-local reassignment **inside a function**~~ | M1.2, M1.5 | **Done (F1)** — captured-local + global reassignment (`closures/global_mutate_from_fn`) |

Still open (narrowed from the original cluster):

| Item | Source | Trigger to implement |
|---|---|---|
| A closure **inside a method** capturing `self` or a field (method-context capture) | M1.2, F1 | A closure in a method body that reads `self`/a field; needs `self` threaded as a capturable upvalue |
| ~~A **reference** to a prelude collection builtin (`len`/`map`/`filter`/`sum`) as a value~~ | M1.2 | **Done (F2)** — `Payload::NativeFn`; `closures/builtin_as_value`, `builtin_value_on_object`. Still open: the `Ok`/`Err`/`some` constructors, `panic`, `next_id` as first-class *values* (exotic; need hand-matched runtime arity/error text) |
| Forward / mutual capture among nested `fn`s (a closure capturing a local declared *after* it) | F1 | A program with forward references between nested closures; needs pre-declared cells for all celled locals |

## Type checker / inference hardening

> **Superseded by a direction decision (2026-06-22): the language targets an INFERRED-STATIC type system with an explicit `dyn` escape — not the gradual/`Unknown`-tolerant checker shipped today.** Annotations stay optional because inference reconstructs them; an un-inferable type is a *compile error*, and `dyn`/`Any` is the only sanctioned dynamic boundary (the sole place runtime trait dispatch survives). This **absorbs the two rows below and the bounded-generics row in the next section** into one milestone-scale type-system track (≈ M1.7 + M1.8 redone under a soundness mandate, plus trait coherence and the `dyn` story), and it *gates* the packed-types/SIMD perf work. The inference engine is **bidirectional checking with local inference, not classical Hindley–Milner** — subtyping (`dyn` widening, directional method resolution, record width) is load-bearing and defeats HM's unification core; see the checker row below for the full rationale. Note the track is a **reclassification, not an addition**: everything that runs today is already the `dyn` path (all current trait dispatch is runtime shape lookup), so the work demotes today's behavior to the marked `dyn` fallback and builds a static dispatch layer on top — and static types buy *soundness now, perf only later* (once the packed-types tier consumes them). **In progress** on branch `types-inferred-static` (planning + slice docs in `plans/types/`): S0 lattice/`dyn`, S1 bidirectional engine, S2 mandatory signatures (E0022), S3a/S3b stdlib typing + argument checking, and **S3c inference completion** are done (forward propagation, optional binding annotations, list-building L1–L3 — `~` concatenation + `[..xs, x]` spread + declaring-scope assignment so accumulators infer forward — and **E0023 CannotInfer** for an undeterminable binding). Remaining: S4 bounded generics (E0024), S5 trait coherence (E0025), S6 `dyn` narrowing `x.as<T>()`, S8 *declared* (never inferred) union/intersection types, S7 finalize. Notable course-corrections recorded there: the language had no list-building, so the accumulator case was *built* (L1–L3) rather than designed around, which also removed any need for a backward-inference solver. Rationale + full status in the `type-system-direction` memory.

| Item | Source | Trigger |
|---|---|---|
| Promote `E0006 ImmutableAssignment` from a **runtime** check to a **static** one (the "M1.7b" ownership/immutability analysis — no `slice-07b` was ever written) | M1.7 | Folded into the inferred-static track above |
| Static inference engine for the inferred-static checker — **bidirectional checking with local inference, NOT classical Hindley–Milner** (the checker is gradual/`Unknown`-tolerant today) | M1.7 | Folded into the inferred-static track above (becomes *the* checker, not a permissive add-on). **Engine decided: bidirectional, not HM.** Classical unification-based HM requires *no subtyping* (it solves symmetric `t1 = t2`); we make subtyping load-bearing via `dyn` (top type, implicit widening + checked `.as::<T>()?` narrowing), directional method/overload resolution, and likely record width subtyping — any one defeats HM. The HM-preserving routes are worse (drop subtyping → rigid language; MLsub/algebraic subtyping → exotic union/intersection types, non-local errors). We also already require boundary annotations, which forgoes HM's no-annotation guarantee while still paying its costs; bidirectional instead consumes those signatures as propagation anchors (subtyping lives in check-mode subsumption, errors stay local) and decomposes cleanly into salsa queries (HM's global `let`-generalization solve does not). Rationale in the `type-system-direction` memory. |

## Traits / generics (the M1.8 deferred tail)

All recorded in `m1/slice-08-traits.md` under "Todo (deferred past M1.8)". None reach `RunResult` today.

| Item | Source | Trigger |
|---|---|---|
| ~~User-facing `Ordering.Less` construction; register `Ordering` as a namable prelude enum~~ | M1.8 | **Done (F3)** — `Ordering` registered as a built-in enum in both backends (`traits/ordering_construct`); construction builds the same value `.compare()` returns |
| Standalone top-level `impl Attribute for X {}` + the `#[Foo(...)]`-requires-`Attribute` gate | M1.8 | **Folded into a deliberate "attribute system" pass** (with richer args below + how the manifest feeds the agentic/MCP tooling). Parser/grammar work; the manifest is *not* in `RunResult`, so the differential can't cover it — design it holistically rather than piecemeal |
| ~~Nested-object fields in derived `Comparable` (recurse into sub-objects)~~ | M1.8 | **Done (F3)** — `compare_field` recurses into object fields in both backends (`traits/derive_comparable_nested`); non-object/non-primitive fields (e.g. lists) still bail |
| `Callable` (`a(...)`), `Members` / `DynamicCall` protocols routed to user objects | M1.8 | Objects used as functions / dynamic member dispatch (agentic/proxy surface) |
| Monomorphic shape specialization + bounded type parameters (`<T: Comparable>`) — generics are erased-for-storage today | M1.8 | **Bounded params fold into the inferred-static type-system track** (enforced statically, not at runtime); monomorphic specialization is the packed/perf reification path it then unlocks (M2 "packed value types") |
| Richer record-valued `#[attr(...)]` arguments (identifiers only today) | M1.8 | **Folded into the "attribute system" pass** (with the `impl Attribute` gate above). Parser + manifest change with no `RunResult` (oracle) coverage — design with the manifest's tooling consumers |

## Diagnostics source attribution

| Item | Source | Status |
|---|---|---|
| ~~`SourceMap` / source attribution for a **check/runtime** diagnostic landing inside a merged-in cross-module declaration body~~ | M1.9 | **Done (slice F4, `plans/followups/slice-f4-sourcemap.md`).** Resolved via **`SourceId` on `Span`** (not the global-coordinate shift-visitor the original sketch proposed): the parser stamps each span with the file's `SourceId`, `Linked` carries a `SourceMap` (`lang-span`), and `lang-conformance`/`lang-cli` resolve each diagnostic through it. Correct by construction — no AST visitor. Covered by `tests/conformance/modules/cross_module_error/` (`E0008` rendered at `models.lang:8:12`) + loader/span unit tests. The original repro (sibling `1/0` rendered against the entry, inside a comment) now renders against the sibling. |

## Performance (invisible to `RunResult` — behavior is already correct)

| Item | Source | Trigger |
|---|---|---|
| Inline caches for member access and trait-method call sites (currently a hashmap/shape lookup) | M1.4, M1.8 | A benchmark showing dispatch/property lookup as a hot spot vs. the M2.0 baseline |
| `gc-arena` tracing path for `__destruct`-free classes (refcount is the only path today) | M1.6 | A benchmark demonstrating refcount overhead that tracing would remove |
| Lazy real-disk reads behind the `fs.open` handle (M2.5 snapshots the file at open; surface is final) | M2.5 | Real workloads reading files too large to buffer whole |
| Copy-on-write / unique-owner in-place list append (the `~` concat operator copies the whole left `Vec` each time, so an accumulator loop `acc = acc ~ [x]` / `acc ~= [x]` is **O(n²)**) | L1 (list-building) | A workload building a large list in a loop. **Approach:** when the left operand holds the *only* reference to its list, mutate the backing buffer in place instead of copying (Swift/OCaml/Roc-style COW) — same immutable semantics, append becomes O(1) amortized, the loop O(n). Tree-walker side is straightforward (lists are `Rc<Vec>` → gate on `Rc::strong_count == 1` / `Rc::get_mut`); the VM side needs uniqueness info from its heap allocator (ties into the `gc-arena` row above). Alternatives if COW proves insufficient: a persistent vector (RRB / `im::Vector`, O(log n) with structural sharing) or a mutable `.push` (O(1) but makes lists the second mutable heap type — the change L1 deliberately avoided). |

## Notes

- ~~**Stale code comments to tidy**~~ — **Done (F3)**: corrected the `lang-eval` operator-dispatch comment (comparisons *do* trait-dispatch via `Comparable`, and `Ordering` is now a namable type) and the `lang-ast` "return type not yet checked in M0" / "is M1.8b" comments (checked since M1.7; the trait wiring shipped).
- The roadmap's **M2 "Deferred follow-ups"** bullet duplicates the lazy-`fs.open` row above by design — the roadmap is the milestone index; this file is the complete cross-milestone registry.
