# Slice 5 — `match` + enums

Status: todo

## Goal
Plain, backed, and algebraic enums, and `match` with destructuring.

## Scope
- In: plain `enum Color { Red; Green; Blue; }`; backed `enum Status: string { Pending = "pending"; ... }`; algebraic `enum OrderError { Empty; NegativePrice(index: int) }`; `match expr { Pattern => expr, ... }` with variant + binding patterns and literal patterns; enum value construction `Status.Pending`, `OrderError.NegativePrice(index: i)`.
- Out: exhaustiveness checking (a checker/M1 concern).

## Checklist (vertical slice)
- [ ] Grammar / AST: enum decl (plain/backed/algebraic variants), match expr + arms + patterns.
- [ ] Checker rule: n/a — **M0 `match` is runtime-dispatched and non-exhaustive**.
- [ ] Bytecode: n/a.
- [ ] Eval op: enum values; pattern matching with binding extraction; backed-enum value access.
- [ ] Conformance cases: match over an algebraic enum binding inner data; match over a backed enum. Write cases asserting **runtime behavior**, not exhaustiveness errors.
- [ ] Snapshots: AST for an enum + match program.

## Notes / traps
- Exhaustiveness is deliberately deferred to M1; leave a note so M1 can switch it on and reuse these corpus cases.

## Definition of done
- Conformance cases pass for all three enum kinds and destructuring match.
- fmt/clippy clean; zero `unsafe`.
