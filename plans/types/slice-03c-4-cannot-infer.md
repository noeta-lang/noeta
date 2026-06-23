# Slice S3c.4 — Hard `E0023 CannotInfer` (fixable via annotation)

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`), the closing sub-slice of S3c. **Depends on:** S3c.2 (binding annotations, the fix path) + L1–L3 (list-building, which removed the accumulator from the "uninferable" set). **Determinism posture:** a new static rejection from the shared checker → identical on both backends → differential unaffected (**117 / 0 skipped**).

## What this turned into

The original S3c was "remove residual hole-tolerance; any unresolved hole is `E0023`." Two findings reshaped it (recorded in the session plan and the `README`):

1. A naive flip is unsound (rejects valid `return none`) and was unfixable (bindings had no annotation). → fixed by S3c.1 propagation + S3c.2 annotations.
2. The language had no list-building at all, so the accumulator — the main case a hole would resolve through — could not even be written. → fixed by L1–L3, after which accumulators infer *forward* and need no backward solver.

So by the time `E0023` lands, almost everything resolves: literals propagate (S3c.1), maps infer (S3c.1), annotations pin (S3c.2), accumulators infer and persist (L1–L3). What remains genuinely uninferable is a narrow, sharp case.

## The rule

`E0023` fires for an **immutable, un-annotated binding to a context-free polymorphic literal** — `x = []`, `m = {}`, `x = none`. The reasoning is airtight: an immutable binding can never be reassigned (that is `E0006`), so its element/payload type is fixed at the binding site, and a zero-information literal supplies none — nothing, anywhere, can ever determine it. The fix is an annotation (`x: List<int> = []`) or, for a collection you build up, a `mut` accumulator whose later writes supply the type.

The trigger is **syntactic** (`is_uninferable_literal`: empty list, empty map, or `none`), so a hole inherited from a call result (`xs = f()` where `f: list`) is never mistaken for one. It is narrow by construction:

| Form | Verdict |
|---|---|
| `x = []` / `m = {}` / `x = none` (immutable, no annotation) | **E0023** |
| `x: List<int> = []` (annotated) | clean — annotation pins it |
| `mut acc = []; … acc = acc ~ [x] …` | clean — accumulator infers (L3) |
| `echo []`, `len([])`, `[].first()` (expression position) | clean — only a *binding* commits to a type |
| `x = [1, 2]`, `m = {"a": 1}` (non-empty) | clean — elements inferred |

This mirrors Rust (`let v = Vec::new();` / `let n = None;` are "type annotations needed"), with annotations as the same escape hatch.

No conflict warning was needed: with list-building giving *forward* accumulator inference, there is no "joined to `dyn` on conflict" path, so the warning plumbing considered earlier is moot and was not built.

## Files
- `crates/lang-diagnostics/src/lib.rs` — `CannotInfer` → `E0023` (appended: enum, `ALL`, `code()`).
- `crates/lang-check/src/lib.rs` — `is_uninferable_literal`; the `Stmt::Binding` arm emits `E0023` for an immutable, un-annotated context-free literal.
- `crates/lang-check/src/tests.rs` — 3 new tests (fires; fixed by annotation / `mut`; quiet in expression position and on typed values).
- `tests/conformance/bindings/cannot_infer_empty_literal.lang` — the error, both backends.

## Determinism / oracle posture
Conformance **124 passed**; differential **117 matched / 0 skipped / backends agree**. 61 checker tests (3 new); clippy + fmt clean. The corpus has no contextless-literal bindings, so the new rejection is purely additive.

## Definition of done — met
`E0023 CannotInfer` is emitted for the one genuinely-undeterminable binding form and is fixable via the S3c.2 annotation or a `mut` accumulator; expression-position empties and typed values are untouched. This closes the S3c inference-completion arc. Remaining type-system track: S4 bounded generics → S5 coherence → S6 `dyn` narrowing → S8 declared unions → S7 finalize.
