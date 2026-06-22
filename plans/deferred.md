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

> **Superseded by a direction decision (2026-06-22): the language targets an INFERRED-STATIC type system with an explicit `dyn` escape — not the gradual/`Unknown`-tolerant checker shipped today.** Annotations stay optional because inference reconstructs them; an un-inferable type is a *compile error*, and `dyn`/`Any` is the only sanctioned dynamic boundary (the sole place runtime trait dispatch survives). This **absorbs the two rows below and the bounded-generics row in the next section** into one milestone-scale type-system track (≈ M1.7 + M1.8 redone under a soundness mandate, plus trait coherence and the `dyn` story), and it *gates* the packed-types/SIMD perf work. Decided, not yet planned — needs its own planning pass. Rationale + spec sketch in the `type-system-direction` memory.

| Item | Source | Trigger |
|---|---|---|
| Promote `E0006 ImmutableAssignment` from a **runtime** check to a **static** one (the "M1.7b" ownership/immutability analysis — no `slice-07b` was ever written) | M1.7 | Folded into the inferred-static track above |
| Full Hindley–Milner unification + let-generalization (the checker is gradual/`Unknown`-tolerant today) | M1.7 | Folded into the inferred-static track above (becomes *the* checker, not a permissive add-on) |

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

| Item | Source | Trigger |
|---|---|---|
| `SourceMap` / global-coordinate spans for a **check/runtime** diagnostic landing inside a merged-in cross-module declaration body (latent — also noted in the roadmap M1.9 row) | M1.9 | **Confirmed real & severe** (a sibling-module `1/0` renders against `main.lang:2:85`, inside a comment). The fix is a *global-coordinate re-architecture*: a full mutable AST span-shift visitor (`lang-ast`) re-bases every merged-in span; a new `SourceMap` (`lang-span`) maps global offset → (source, local); the loader assigns per-module bases + carries the map on `Linked`; `lang-conformance`/`lang-cli` resolve each diagnostic's span through it. **Not differential-covered** (both backends produce the same wrong offset and agree) — covered only by hand-written multi-file conformance fixtures. A real ~5-crate slice, not a quick fix; sequence as its own pass. Repro: an entry that `use`s a sibling whose method body does `x / y` with `y == 0`. |

## Performance (invisible to `RunResult` — behavior is already correct)

| Item | Source | Trigger |
|---|---|---|
| Inline caches for member access and trait-method call sites (currently a hashmap/shape lookup) | M1.4, M1.8 | A benchmark showing dispatch/property lookup as a hot spot vs. the M2.0 baseline |
| `gc-arena` tracing path for `__destruct`-free classes (refcount is the only path today) | M1.6 | A benchmark demonstrating refcount overhead that tracing would remove |
| Lazy real-disk reads behind the `fs.open` handle (M2.5 snapshots the file at open; surface is final) | M2.5 | Real workloads reading files too large to buffer whole |

## Notes

- ~~**Stale code comments to tidy**~~ — **Done (F3)**: corrected the `lang-eval` operator-dispatch comment (comparisons *do* trait-dispatch via `Comparable`, and `Ordering` is now a namable type) and the `lang-ast` "return type not yet checked in M0" / "is M1.8b" comments (checked since M1.7; the trait wiring shipped).
- The roadmap's **M2 "Deferred follow-ups"** bullet duplicates the lazy-`fs.open` row above by design — the roadmap is the milestone index; this file is the complete cross-milestone registry.
