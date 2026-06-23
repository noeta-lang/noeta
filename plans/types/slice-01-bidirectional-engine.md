# Slice S1 — Bidirectional engine rewrite at parity

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Depends on:** S0 (`Type::subtype`). **Determinism posture:** pure engine swap — gradual tolerance stays on, every statement enters checking mode with an *open* (`Unknown`) expectation, so subsumption is a no-op and verdicts are identical. The differential + conformance must stay **107 matched / 0 skipped** and **113 passed**, which is this slice's proof.

## Goal
Replace the single best-effort `infer()` pass with the **bidirectional** architecture (synthesis + checking judgments and a subsumption rule) the rest of the track hangs real expectations on — **without changing a single verdict**.

## Why a separate, behavior-preserving slice
The engine swap and the behavior change (rejecting more programs) are independent risks. Landing the architecture first, proven flat by the oracle, de-risks every later tightening — exactly the M2.1 keystone discipline ("land the plumbing behavior-preserving, prove it with the differential"). S2+ then only flip on expectations against an already-tested machine.

## What shipped
- **`Checker::synth(expr, env) -> Type`** — synthesis mode (the former `infer`, renamed wholesale along with `synth_binary`/`synth_call`/`synth_member`/`synth_match`). Subexpression recursion is synthesis; behavior unchanged.
- **`Checker::check(expr, expected, env) -> Type`** — checking mode. Forms that absorb an expectation propagate it inward:
  - a list literal against `List<T>` checks each element against `T`;
  - a closure against a function type adopts the expected parameter types (an explicit annotation still wins) and checks its body against the expected return.
  Every other form synthesizes and is then subsumed.
- **`Checker::subsume(actual, expected, span)`** — requires `actual <: expected` via `Type::subtype`; a violation is `E0007` (the same code the arithmetic mismatch path uses). An inference hole on either side makes `subtype` hold, so missing information never produces a false positive.
- **Statement positions enter `check`** (`Echo`/`Binding`/`Expr`/`Return` value sinks) with an `Unknown` expectation — behavior-identical to bare synthesis, but structurally the bidirectional shape. S2 swaps the `Unknown` at `Return` for the declared return type.

## Files
- `crates/lang-check/src/lib.rs` — module-doc rewrite (bidirectional, not HM; gradual tolerance noted as being removed across the track), `synth` rename, new `check`/`subsume`, statement sinks routed through `check`.
- `crates/lang-check/src/tests.rs` — 5 white-box tests driving `Checker::check` directly with concrete expectations (identity + widen-into-`dyn` pass; concrete violation fires `E0007`; open expectation is a no-op; `List<int>` propagates to a bad element; `fn(int)->int` vs `fn(int)->string` closure), proving the machinery is live rather than dead pending S2.

## Determinism / oracle posture
Not differential-relevant: production callers pass only `Unknown` expectations this slice, so subsumption never fires and both backends consume an identically-checked program. Proven by the oracle + conformance staying **exactly** at baseline (107/0, 113). The check path itself is covered by the white-box unit tests (28 checker tests total).

## Definition of done — met
`synth`/`check`/`subsume` exist and are unit-tested; statements run in checking mode; all 23 pre-existing checker tests pass unchanged plus 5 new ones; differential **107 matched / 0 skipped / backends agree** and conformance **113 passed** — identical to the pre-slice baseline; clippy + fmt clean.
