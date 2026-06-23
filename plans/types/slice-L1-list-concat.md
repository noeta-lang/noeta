# Slice L1 — List concatenation via `~`

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`), list-building prerequisites (L1–L3). **Why:** the corpus audit during S3c found the language had *no* way to grow a list — lists are immutable and `~` only display-concatenated. That gap is what made the polymorphic-literal/accumulator story moot. Rather than design the type system around the gap, L1–L3 add list-building; this slice is the concatenation operator.

## Decision

`~` is the one deliberate concatenation operator. `list ~ list` builds a **new** list (immutable — operands untouched); every other operand pairing keeps today's display-based concatenation (each side stringified to a `string`), so `1 ~ true` is still `"1true"`. A mutable `.push` was rejected: it would make lists the second mutable heap value type (after `FileHandle`), a large GC/aliasing change in both backends; concatenation preserves the immutable-value model and is cheap. Single-element push is L2's spread (`[..acc, x]`); `~` merges two lists.

## What shipped

- **Runtime (both backends, shared semantics):** the `Concat` arm of `apply_binary` in `crates/lang-eval/src/ops.rs` (tree-walker) and `crates/lang-value/src/ops.rs` (VM) now returns a concatenated list when both operands are lists, else the unchanged display-concat string. The two ports stay byte-identical, so the differential holds.
- **Checker:** `synth_binary`'s `Concat` arm yields `List<unify(A, B)>` for two lists (element types unified; a concrete clash widens to `List<dyn>`, mirroring heterogeneous-list recovery), else `string`. So `[1] ~ [2]` is a `List<int>` that flows through a `List<int>` return and fails a `string` one.

## Traps handled
- **`~`'s lenient display behavior is preserved** for every non-(list, list) pairing — the existing `1 ~ true` → `"1true"` and `"users/" ~ 42` corpus uses are unchanged (no corpus case ever concatenated two lists, so the new branch is purely additive).
- **Immutability**: concatenation allocates a fresh list; no operand is mutated, so no aliasing/GC surprise and the value model is untouched.

## Files
- `crates/lang-eval/src/ops.rs`, `crates/lang-value/src/ops.rs` — list-concat `Concat` arm (both ports).
- `crates/lang-check/src/lib.rs` — `synth_binary` `Concat` typing.
- `crates/lang-check/src/tests.rs` — 2 new tests (concat-of-lists typing, result flows through a signature).
- `tests/conformance/collections/list_concat.lang` — end-to-end, both backends.

## Determinism / oracle posture
Conformance **121 passed**; differential **115 matched / 0 skipped / backends agree**. 55 checker tests (2 new), 74 lang-value + 5 lang-eval; clippy + fmt clean.

## Definition of done — met
`list ~ list` concatenates into a new list in both backends (differential-agreed) and types as `List<unify>`; display-concat unchanged elsewhere. L2 (spread `[..xs, x]`) and L3 (assignment updates the declaring scope, so accumulators infer) follow.
