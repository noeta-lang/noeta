# Slice S0 — Type lattice + `dyn` foundation

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Depends on:** nothing. **Determinism posture:** pure vocabulary — `dyn` is not yet produced by any program, so verdicts are unchanged; the differential + conformance must stay **107 matched / 0 skipped** and **113 passed**, which is this slice's proof of correctness.

## Goal
Introduce the inferred-static lattice vocabulary — the nameable top `dyn`, the directional subtype relation, and a clean split between the internal *inference hole* and the explicit top — **without changing a single checker verdict**.

## Why first
The whole track rests on two distinctions the lattice doesn't yet make: (1) `Unknown` is overloaded as both "absence of information" (an inference hole that must eventually resolve) *and* "the universal top" — but inferred-static needs the hole to become an *error* while keeping an explicit, sanctioned top; (2) there is no directional `<:` relation, only structural `==`, yet bidirectional checking's whole job is check-mode subsumption (`synth <: expected`). Landing the vocabulary first, inert, lets every later slice consume it without also inventing it.

## What shipped
- **`Type::Dyn`** — the explicit, user-nameable top (`dyn` / `Any`). Distinct from `Type::Unknown`, which is re-documented as the internal inference hole. `Dyn.is_gradual()` is **false** (it is concrete, user-written information, not a missing type).
- **`Type::subtype(sub, sup)`** — the directional widening relation, the single home for the rules the bidirectional checker will consume:
  - inference holes (`Unknown`/`Var`) are bidirectionally compatible (no false positives on missing info — gradual behavior, preserved until the strict flip removes the holes themselves);
  - `dyn` is the top: `T <: dyn` for all `T`, but `dyn <: T` is **false** (narrowing out of `dyn` is the explicit checked `x.as<T>()`, never implicit);
  - containers covariant in elements; functions contravariant in params, covariant in return; everything else identity.
- **Desugaring:** `dyn` and `Any` both map to `Type::Dyn` in `Type::from_ref` and count as built-in names (`is_builtin_name`), so `let x: dyn = …` resolves to the top rather than an unknown `Named`. `dyn` lexes as a plain `Ident`, so no lexer/grammar change was needed.
- **Display:** `Type::Dyn` prints as `dyn`.

## Files
- `crates/lang-types/src/lib.rs` — the `Dyn` variant, `subtype()`, `from_ref`/`is_builtin_name`/`Display` updates, module doc rewrite, and 5 new unit tests (dyn desugaring, widen-into-but-not-out-of-dyn, identity/distinctness, holes bidirectional, container covariance / fn contravariance).

## Determinism / oracle posture
Not differential-relevant: nothing produces `Type::Dyn` yet and the checker is unchanged, so both backends and every expectation are untouched. The slice is proven by the oracle + conformance staying **exactly** at the baseline (107/0, 113), confirming zero verdict change. The new behavior is exercised by the `lang-types` unit tests (13 total, all passing).

## Definition of done — met
`Type::Dyn` + `subtype()` exist and are unit-tested; `dyn`/`Any` desugar and parse as the top; `cargo test -p lang-types` green (13); differential **107 matched / 0 skipped / backends agree** and conformance **113 passed** — both identical to the pre-slice baseline; clippy + fmt clean.
