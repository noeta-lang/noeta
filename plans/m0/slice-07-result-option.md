# Slice 7 — `Result`/`Option`/`?`

Status: done

## Goal
The recoverable-error spine: `Result`/`Option` values and the `?` propagation operator.

## Scope
- In: `Ok(x)`/`Err(e)`, `some(x)`/`none`, `?T` as sugar for `Option<T>`; `?` postfix operator (early-return the `Err`/`none` from the enclosing fn); `??` fallback (`x ?? expr` handles the `none`/`Err` case). `Result`/`Option` participate in `match` (Slice 5).
- Out: typed exhaustive handling enforced by the checker (M1); panics beyond a built-in `panic(msg)`.

## Checklist (vertical slice)
- [x] Grammar / AST: `?` postfix expr, `??` binary expr, `?T` type sugar (kept as sugar in the AST), `panic` call.
- [x] Checker rule: n/a.
- [x] Bytecode: n/a.
- [x] Eval op: `Result`/`Option` as built-in enum values; `?` short-circuits the current call frame on `Err`/`none`; `??` evaluates the fallback; `panic` unwinds with a runtime diagnostic + nonzero exit.
- [x] Conformance cases: `?` propagating an `Err`; `??` supplying a default; a `none`/`some` round-trip; a `panic` case (`// expect: exit 1`).
- [x] Snapshots: AST for a `?`-using function.

## Notes / traps
- One error hierarchy: `Result`/`Option` for recoverable, `panic` for unrecoverable. No ambient `null`.
- Keep `?T`/`?`/`??` as sugar nodes in the AST so M1 diagnostics can be precise.

## Definition of done
- Conformance cases pass for `?`, `??`, `some`/`none`, and `panic`.
- fmt/clippy clean; zero `unsafe`.

## Outcome (done)
`Result`/`Option` reuse the existing [`EnumValue`] representation (a record-free win from
Slice 5): `Ok`/`Err`/`some` are prelude `Builtin` constructors, `none` is a prelude *value*
binding, and they participate in `match` and structural equality like any enum. Only their
**display** (bare surface constructors — `Ok(x)`, `none` — not `Result.Ok(x)`) and the new
`?`/`??` operators treat them specially (`try_branch`).

New tokens: `??` (`QuestionQuestion`, longest-match before `?`). New AST nodes `Expr::Try`
(`expr?`, postfix at call/member precedence) and `Expr::Coalesce` (`a ?? b`, infix alongside
`||`), kept as sugar per the design. `panic(msg)` is an ordinary `Builtin` → new diagnostic
code **E0010 `Panic`**.

The early-return mechanism: the evaluator's error channel `Aborted` became an enum
`Unwind { Abort, Return(Value) }`. `?` on `Err(e)`/`none` raises `Unwind::Return(value)`,
which propagates up the Rust stack and is converted back to the call's value at the function
boundary (`catch_return` in `call_closure`/`call_method_on`); at top level it just stops the
program (exit 0, no diagnostic). `?`/`??` on `Ok(x)`/`some(x)` unwrap to `x` (or `unit` for
the void `Ok()`); on a non-`Result`/`Option` value they are an `E0007` type error.

10 eval tests; 1 AST snapshot; 1 lexer test; 4 conformance cases (20 total). fmt/clippy
clean; zero `unsafe`.

**Surface decision (`?T`, prefix):** the syntax doc's prose and this crate's `type_parser`
both specify `?T` (prefix) for `Option<T>`; one stray code example wrote postfix `User?`,
contradicting its own comment, and was corrected to `?User`. `??`-with-`return`-RHS
(`x ?? return Err(...)`, syntax §9.5) is **deferred** — it needs `return` in expression
position; M0 `??`'s RHS is an ordinary fallback expression.
