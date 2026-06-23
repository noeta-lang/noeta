# Slice L2 — List spread `[..xs, x]`

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`), list-building prerequisites (L1–L3). **Depends on:** L1 (`~` concatenation). **Determinism posture:** spread is pure parser-level sugar over `~`, so there is no new runtime/AST/checker surface — the differential is unaffected (**116 / 0 skipped**).

## Decision

A list literal may contain spread elements at any position: `[..xs, x, ..ys]`. Spread reuses the existing `..expr` token (the same one object literals use for `{..base}`); `..` is spread-only (no range syntax), so `[..a]` is unambiguous. Spread is **the** way to push a single element (`[..xs, x]`), complementing `~`, which merges two lists — together they cover element-push and array-merge.

## What shipped — desugaring, nothing else

`[..a, x, ..b]` desugars **in the parser** to `[] ~ a ~ [x] ~ b`: consecutive plain elements group into `[...]` chunks, each spread contributes its operand, and the chunks fold left-to-right with `~` (L1), starting from an empty list so the result is always list-shaped. A list with no spreads is still a plain `Expr::List` (byte-identical to before). Because the desugaring lands entirely on L1's concatenation:
- **No AST change** — `Expr::List` is untouched; the result is a `Concat` tree.
- **No runtime change** — both backends already concatenate lists (L1).
- **No checker change** — the result types through the existing `synth_binary` `Concat` rule (`List<unify>`), so `[..xs, 99]` is a `List<int>` and a wrong-typed spread is caught through the concat result.

## Traps handled
- **Spread of a non-list**: falls through `~` to display-concatenation (a string), exactly as the operator does elsewhere — consistent, if lenient; a stricter "spread expects a list" check can come later.
- **`[..a]` alone**: desugars to `[] ~ a` (a fresh list copy), never to a bare scalar, because the fold always starts from an empty list.
- **Spans**: every synthesized `List`/`Concat` node carries the list literal's span, so a type error inside a spread points at the literal.

## Files
- `crates/lang-parser/src/lib.rs` — `desugar_list_literal` + the list-element parser (`..expr` | `expr`).
- `crates/lang-check/src/tests.rs` — 1 new test (spread typing through the concat result).
- `tests/conformance/collections/list_spread.lang` — end-to-end, both backends.

## Determinism / oracle posture
Conformance **122 passed**; differential **116 matched / 0 skipped / backends agree**. 56 checker tests (1 new), 17 parser tests; clippy + fmt clean.

## Definition of done — met
`[..xs, x, ..ys]` parses and builds the spliced list in both backends, typed as the unified list, with spread as the single-element-push idiom. L3 (assignment updates the declaring scope, so `acc = [..acc, x]` accumulators infer) follows.
