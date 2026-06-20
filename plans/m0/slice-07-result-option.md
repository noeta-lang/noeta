# Slice 7 — `Result`/`Option`/`?`

Status: todo

## Goal
The recoverable-error spine: `Result`/`Option` values and the `?` propagation operator.

## Scope
- In: `Ok(x)`/`Err(e)`, `some(x)`/`none`, `?T` as sugar for `Option<T>`; `?` postfix operator (early-return the `Err`/`none` from the enclosing fn); `??` fallback (`x ?? expr` handles the `none`/`Err` case). `Result`/`Option` participate in `match` (Slice 5).
- Out: typed exhaustive handling enforced by the checker (M1); panics beyond a built-in `panic(msg)`.

## Checklist (vertical slice)
- [ ] Grammar / AST: `?` postfix expr, `??` binary expr, `?T` type sugar (kept as sugar in the AST), `panic` call.
- [ ] Checker rule: n/a.
- [ ] Bytecode: n/a.
- [ ] Eval op: `Result`/`Option` as built-in enum values; `?` short-circuits the current call frame on `Err`/`none`; `??` evaluates the fallback; `panic` unwinds with a runtime diagnostic + nonzero exit.
- [ ] Conformance cases: `?` propagating an `Err`; `??` supplying a default; a `none`/`some` round-trip; a `panic` case (`// expect: exit 1`).
- [ ] Snapshots: AST for a `?`-using function.

## Notes / traps
- One error hierarchy: `Result`/`Option` for recoverable, `panic` for unrecoverable. No ambient `null`.
- Keep `?T`/`?`/`??` as sugar nodes in the AST so M1 diagnostics can be precise.

## Definition of done
- Conformance cases pass for `?`, `??`, `some`/`none`, and `panic`.
- fmt/clippy clean; zero `unsafe`.
