# Slice L3 — Assignment updates the declaring scope (accumulators infer)

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`), list-building prerequisites (L1–L3). **Depends on:** L1/L2 (list-building). **Determinism posture:** a checker-only change to how a binding's *type* is tracked across scopes — no runtime change — so the differential is untouched (**117 / 0 skipped**). Runtime mutation semantics were already correct; this aligns the checker's type tracking with them.

## Goal

Make the accumulator idiom infer. With L1/L2, `mut acc = []; for x in xs { acc = acc ~ [x]; }` is now *expressible*; this slice makes the checker *infer* its element type. The element type comes from the loop-body reassignment by **forward** inference — `acc ~ [x]` already names `[x]`, so the first reassignment refines `acc` from `List<?>` to `List<int>`. The only missing piece was that the refinement has to *persist past the loop*. This is exactly why no backward-inference solver is needed.

## The fix

Previously every binding (`bind`) inserted into the *innermost* scope frame, so a reassignment in a nested scope (a loop/`if` body) shadowed the outer binding and reverted on scope exit — the accumulator's refined type was lost after the loop. Now a bare assignment routes through a new `assign`:

- **`mut x = …`** (a fresh `mut` declaration) and an **annotated `x: T = …`** declaration → `bind` (innermost frame, even if it shadows). These introduce a new variable.
- **bare `x = …`** → `assign`: if `x` already exists in an enclosing frame it is a *reassignment* — update the type *there*; only a not-yet-seen name is a fresh binding in the innermost frame.

So `acc = acc ~ [x]` inside the loop updates `acc` in the function-body frame where it was declared; after the loop, `acc : List<int>` flows into the declared return type. A wrong signature (`List<string>`) is caught at `return acc` ("expected `List<string>`, found `List<int>`").

## Traps handled
- **Runtime unaffected**: this only changes the checker's compile-time type map; the tree-walker/VM mutation semantics (and the differential) are untouched.
- **Declarations vs reassignments**: `mut`/annotated forms stay innermost-scoped (genuine new variables, shadowing intact); only bare `x = …` mutates an enclosing binding — matching the language's "`mut` reassignment mutates the variable" semantics.
- **Single-pass soundness**: the body is checked once; the first reassignment supplies the element type, so the post-loop type is the refined one. No fixpoint/second pass needed.

## Files
- `crates/lang-check/src/lib.rs` — `assign` helper; the `Stmt::Binding` arm dispatches `bind` vs `assign` on `mut_decl`/annotation.
- `crates/lang-check/src/tests.rs` — 2 new tests (accumulator infers + is checked at the return; nested reassignment updates the outer binding).
- `tests/conformance/collections/list_accumulator.lang` — end-to-end accumulators (`~` and spread), both backends.

## Determinism / oracle posture
Conformance **123 passed**; differential **117 matched / 0 skipped / backends agree**. 58 checker tests (2 new); clippy + fmt clean.

## Definition of done — met
The accumulator pattern is expressible (L1/L2) and its element type infers forward and persists past the loop, checked at boundaries (L3). With list-building complete, `E0023` (S3c.4) now fires only for a genuinely never-constrained binding, fixable via the S3c.2 annotation.
