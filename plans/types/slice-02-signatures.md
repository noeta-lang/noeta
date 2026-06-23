# Slice S2 — Signature requirement + return checking

Status: **done**

> **Track:** inferred-static type system (see `plans/types/README.md`). **Depends on:** S1 (the `check`/`subsume` machinery). **Determinism posture:** the first behavior change — named functions now require full signatures and their `return`s are checked. The loose corpus is migrated in the same slice so conformance + differential stay green; both backends share the checker, so any new rejection is identical on both and `--differential` stays at **0 skipped**.

## Goal
Make signatures mandatory at named boundaries — a type on every parameter and a return type on every named `fn`/method — and check each `return` against the declared return type. Closures and local bindings stay inferred.

## What shipped
- **`E0022 MissingSignature`** (new, append-only): emitted per unannotated parameter (at the parameter name) and once per missing return type (at the function name). Closures (`fn(x) => …`) and destructors carry no signature requirement; only named `FnDecl`s do.
- **Return checking:** `Checker` tracks the enclosing function's declared return type (`current_ret`, saved/restored around each function so nested declarations do not clobber it). Each `return <value>` is `check`ed against it — a concrete violation is `E0007` (e.g. `fn f(): int { return "x"; }`), while a hole or a `dyn` return absorbs anything (still gradual / the escape).
- **`Type: Default`** (lang-types): `Unknown` is the lattice default, so `Checker` keeps deriving `Default` with the new `current_ret` field.

## Corpus migration (11 files)
Every loose named function gained an accurate signature; annotations are erased at runtime, so `RunResult` — and the differential — is unchanged:
- straightforward int/void returns: `functions/recursion.lang` (`fib`/`fact`), `bindings/shadowing.lang`, `closures/{counter_nested_fn,recursive_nested_fn,global_mutate_from_fn,capture_immutable_error}.lang`;
- `bool` predicate params: `results/{coalesce_default,option_round_trip}.lang`;
- list params with an unknown element → `List<dyn>`: `results/question_propagates_err.lang`;
- **functions returning a closure → `: dyn`** (there is no function-type annotation syntax; `dyn` is the sanctioned escape, and a function value widens into it): `closures/{capture_param,transitive_capture}.lang`.

`closures/capture_immutable_error.lang` keeps its `E0006 at 8:9` expectation — only return types were added on existing `fn` lines, so no line/column shifted.

## New negative conformance cases (the iron rule)
- `functions/missing_param_type.lang` — `E0022` on an unannotated parameter.
- `functions/missing_return_type.lang` — `E0022` on a missing return type.
- `functions/return_type_mismatch.lang` — `E0007` on a `return` that violates the declared type.

## Files
- `crates/lang-diagnostics/src/lib.rs` — `MissingSignature` → `E0022` (enum, `ALL`, `code`).
- `crates/lang-types/src/lib.rs` — `#[derive(Default)]` + `#[default]` on `Unknown`.
- `crates/lang-check/src/lib.rs` — `current_ret` field, `require_signature`, return checking at `Stmt::Return`, module-doc update (E0022 + return-checking; the stale "not yet HM" paragraph corrected to "deliberately not HM").
- `crates/lang-check/src/tests.rs` — two existing tests migrated to annotated signatures; 6 new tests (E0022 on param/return, fully-annotated clean, closures/locals exempt, return checked against declared type incl. `dyn`, nested-fn return isolation).
- `tests/conformance/**` — 11 migrated, 3 new negative cases.

## Determinism / oracle posture
New static rejections come from the shared checker, so both backends reject identically — the differential is unaffected and stays at **0 skipped**. The migration is behavior-preserving (erased annotations), so every migrated case's `RunResult` is unchanged. Verified: conformance **116 passed** (113 + 3), differential **110 matched / 0 skipped / backends agree** (107 + 3 new error cases, which both backends agree on).

## Definition of done — met
Named functions require full signatures (`E0022`); returns are checked against the declaration; the loose corpus is migrated; 34 checker tests pass (incl. 6 new); conformance 116 / differential 110 / 0 skipped; clippy + fmt clean.
