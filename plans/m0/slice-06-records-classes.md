# Slice 6 — Records & classes

Status: todo

## Goal
Structural records and classes with fields, methods, `.` access, the all-fields literal, named constructors, and `..` structural update.

## Scope
- In: `type Item = { price: float, qty: int }` structural records; `class` with fields declared in body (immutable default, `mut` field opt-in), methods, `.` field/method access; the all-fields literal `Order { id: ..., ... }` (must set every field); named constructors as ordinary associated fns returning `Self` (`new`, `draft`); `..spread` structural update `Money { amount: 300, ..a }`.
- Out: trait dispatch / operator overloading / derives (M1); packed value types (M2); literal visibility enforcement (parse the intent; full private-by-default checking is M1).

## Checklist (vertical slice)
- [ ] Grammar / AST: record type alias, class decl (fields + methods), all-fields literal, spread in literal, associated-fn calls (`Type.fn(...)`), member access `.`.
- [ ] Checker rule: n/a — but the all-fields literal **must require every field at eval time** (the full-initialization choke point) and error if a field is missing.
- [ ] Bytecode: n/a.
- [ ] Eval op: object values (field bag); method dispatch via `.`; static-vs-instance disambiguation (left side is a type vs a value); all-fields literal construction; `..` spread fills unnamed fields shallowly.
- [ ] Conformance cases: construct via `new`, call a method, access a field, structural update, and a **negative** case for a missing field in the literal.
- [ ] Snapshots: AST for a class program.

## Notes / traps
- Consider splitting 6a (records) and 6b (class methods) into separate commits if the diff is large.
- Constructors are inferred (no `self`, returns enclosing type); transformations take `self` — keep this distinction in the AST for M1 tooling even if not enforced now.

## Definition of done
- Conformance cases pass for record/class construction, methods, `.` access, structural update, and missing-field error.
- fmt/clippy clean; zero `unsafe`.
