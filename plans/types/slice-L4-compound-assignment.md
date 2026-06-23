# Slice L4 — Compound assignment (`+= -= *= /= %= ~=`)

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`), list-building ergonomics. **Depends on:** L1–L3. **Determinism posture:** pure parser-level desugaring to existing nodes — no AST/runtime/checker change — so the differential is unaffected (**118 / 0 skipped**).

## Goal

Ergonomic in-place update, and in particular the list-append-in-a-loop the accumulator idiom wants: `acc ~= [x]` instead of `acc = acc ~ [x]`.

## What shipped — lexer tokens + parser desugaring

- **Lexer:** six new tokens `+= -= *= /= %= ~=` (logos longest-match keeps `+=` whole, not `+` then `=`).
- **Parser:** the statement assignment tail accepts a compound operator; `name OP= expr` desugars to `name = name OP expr` — a plain `Stmt::Binding` whose value is `Expr::Binary { op, lhs: name, rhs: expr }`. No new AST, runtime, or checker surface; it rides entirely on the existing binary operators (including L1's list `~`).

Because it is a bare `name = …` reassignment, it threads through L3's `assign` (updates the binding in its declaring scope), so `acc ~= [x]` infers its accumulator element type exactly like `acc = acc ~ [x]` and is checked against the declared return.

## Caveat (perf)

`acc ~= [x]` is *ergonomics*, not a speed change: it desugars to `~`, which copies the whole left list, so a loop building n elements is still O(n²). The fix is the copy-on-write / unique-owner in-place optimization recorded in the perf track (`plans/deferred.md`), not this slice.

## Traps handled
- **Longest match**: `~=`/`+=`/… are distinct tokens, so `a ~= b` is one operator, not `~` then `=`.
- **Target must be a name**: same rule as plain assignment — a non-name left side is the existing "invalid assignment target" error.
- **No semantic surprises**: `+= -= *= /=` reuse arithmetic, `~=` reuses concat/display-concat — identical to writing the binary form by hand.

## Files
- `crates/lang-lexer/src/lib.rs` — the six compound tokens (+ `label`/symbol arms).
- `crates/lang-parser/src/lib.rs` — `assign_op` choice + desugaring in `assign_or_expr`.
- `crates/lang-check/src/tests.rs` — 1 new test (`~=` accumulator infers like `~`).
- `tests/conformance/bindings/compound_assignment.lang` — `+= -= *= ~=` end-to-end, both backends.

## Determinism / oracle posture
Conformance **125 passed**; differential **118 matched / 0 skipped / backends agree**. 62 checker tests (1 new), 10 lexer tests; clippy + fmt clean.

## Definition of done — met
`name OP= expr` works for all six operators, desugaring to the binary form; `acc ~= [x]` is the ergonomic append and threads the accumulator inference. The O(n²) cost of repeated `~` is a separate, recorded perf-track item.
