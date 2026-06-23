# Slice S4.4 — Operator-trait checking (concrete + body-side, unified)

Status: **done**

> **Track:** inferred-static type system. **Closes** two of the three gaps recorded in `plans/deferred.md` ("Generic & operator enforcement completeness"): operator-trait checking on concrete types (the leniency S4.3a introduced) and body-side bound enforcement beyond ordering. **Determinism / oracle posture:** front-end only; differential holds at **0 skipped**. Effect: more compile-time rejections of operator misuse the runtime would otherwise raise.

## What shipped

A single mechanism replaces both the ad-hoc arithmetic leniency (S4.3a) and the ordering-only body-side check (S4.3c). A binary operator that maps to a trait (`required_operator_trait`: `+`→`Add`, `-`→`Sub`, `*`→`Mul`, `/`→`Div`, `< <= > >=`→`Comparable`) requires **each operand** to satisfy it, via `operand_satisfies_operator`:

- a `dyn`/hole defers (runtime dispatch — never a false positive);
- an in-scope **type parameter** is licensed only by its declared **bounds**;
- any other type by the **satisfaction model** (`satisfies` — the built-in table for scalars/numerics plus the `@derive`/`impl` index for user types).

Failure is reported once per operator, with the flavor chosen by the operand: an **unbounded type parameter** → `E0025` (a missing bound, fixable at the declaration); any other concrete mismatch → `E0007` (the same "cannot apply" the runtime raised). `%` (no trait — numerics only), `~`/`==`/`!=` (universal: display-concat / structural-equality fallbacks), and the logical operators impose no requirement, matching the backends' actual dispatch (verified against `eval_binary`).

This makes the checker catch, statically: `obj + obj` where the type does not `impl Add`; `obj < obj` where it is not `Comparable`; and the body-side `fn f<T>(a: T, b: T) { a + b }` / `{ a < b }` for any unbounded `T`. A type that *does* `impl Add` / derive `Comparable`, or a parameter bounded by it, is accepted.

## Why the universal operators are exempt

`eval_binary` dispatches `==`/`!=` to a user `eq` method **or** falls back to structural `values_equal` (defined for every value), and `~` to list-concat **or** a display-concat fallback — both total. So equality and concat impose no static requirement; a `<T: Equatable>` bound is satisfiable by everything and so body-side `==` needs no check.

## Files

- `crates/lang-check/src/lib.rs` — `required_operator_trait`, `operand_satisfies_operator`, `unbounded_type_param`, `report_operator_error`; rewritten arithmetic + ordering arms of `synth_binary`; removed the S4.3c `unbounded_ordering_param`.
- `tests/conformance/generics/` — `concrete_operator_unsupported.lang` (`E0007`), `unbounded_arithmetic.lang` (`E0025`); `unbounded_comparison.lang` still green.
- `crates/lang-check/src/tests.rs` — 3 new tests (unbounded arithmetic, concrete-without-trait arithmetic, concrete non-Comparable ordering).

## Verification

Conformance **144 / differential 132 matched / 0 skipped / backends agree**; workspace tests, clippy, fmt clean.
