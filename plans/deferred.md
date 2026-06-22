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
| A **reference** to a prelude value/builtin as a value (e.g. storing `len`/`map` in a variable) | M1.2 | Higher-order use of a builtin by value; needs first-class native-function values (slice F2) |
| Forward / mutual capture among nested `fn`s (a closure capturing a local declared *after* it) | F1 | A program with forward references between nested closures; needs pre-declared cells for all celled locals |

## Type checker / inference hardening

| Item | Source | Trigger |
|---|---|---|
| Promote `E0006 ImmutableAssignment` from a **runtime** check to a **static** one (the "M1.7b" ownership/immutability analysis — no `slice-07b` was ever written) | M1.7 | Wanting `mut`-correctness caught at compile time; a corpus case asserting the static diagnostic |
| Full Hindley–Milner unification + let-generalization (the checker is gradual/`Unknown`-tolerant today) | M1.7 | Inference gaps that gradual typing lets through and that users hit in practice |

## Traits / generics (the M1.8 deferred tail)

All recorded in `m1/slice-08-traits.md` under "Todo (deferred past M1.8)". None reach `RunResult` today.

| Item | Source | Trigger |
|---|---|---|
| User-facing `Ordering.Less` construction; register `Ordering` as a namable prelude enum | M1.8 | A program constructing/matching `Ordering` values directly (dispatch already delegates via `.compare()`) |
| Standalone top-level `impl Attribute for X {}` + the `#[Foo(...)]`-requires-`Attribute` gate | M1.8 | Gating *which* records may be used as data attributes; needs the top-level `impl` construct (our `impl`s are class-body-nested) |
| Nested-object fields in derived `Comparable` (recurse into sub-objects) | M1.8 | A `@derive(Comparable)` type whose fields are themselves objects |
| `Callable` (`a(...)`), `Members` / `DynamicCall` protocols routed to user objects | M1.8 | Objects used as functions / dynamic member dispatch (agentic/proxy surface) |
| Monomorphic shape specialization + bounded type parameters (`<T: Comparable>`) — generics are erased-for-storage today | M1.8 | The packed/perf reification path (relates to M2 "packed value types"); or a need to *constrain* a type parameter |
| Richer record-valued `#[attr(...)]` arguments (identifiers only today) | M1.8 | A data attribute needing structured (non-identifier) arguments |

## Diagnostics source attribution

| Item | Source | Trigger |
|---|---|---|
| `SourceMap` / global-coordinate spans for a **check/runtime** diagnostic landing inside a merged-in cross-module declaration body (latent — also noted in the roadmap M1.9 row) | M1.9 | A real cross-module *body* error to surface (every negative case so far is raised against the entry source, where it renders correctly) |

## Performance (invisible to `RunResult` — behavior is already correct)

| Item | Source | Trigger |
|---|---|---|
| Inline caches for member access and trait-method call sites (currently a hashmap/shape lookup) | M1.4, M1.8 | A benchmark showing dispatch/property lookup as a hot spot vs. the M2.0 baseline |
| `gc-arena` tracing path for `__destruct`-free classes (refcount is the only path today) | M1.6 | A benchmark demonstrating refcount overhead that tracing would remove |
| Lazy real-disk reads behind the `fs.open` handle (M2.5 snapshots the file at open; surface is final) | M2.5 | Real workloads reading files too large to buffer whole |

## Notes

- **Stale code comments to tidy when next in the area** (not deferrals — already done, but the comments read like live "not yet" items): `lang-eval/src/lib.rs` "Comparisons stay built-in for now (M1.8b)" (Comparable/Ordering shipped in M1.8b) and `lang-ast/src/lib.rs` "return type … not yet checked in M0" (checked since M1.7).
- The roadmap's **M2 "Deferred follow-ups"** bullet duplicates the lazy-`fs.open` row above by design — the roadmap is the milestone index; this file is the complete cross-milestone registry.
