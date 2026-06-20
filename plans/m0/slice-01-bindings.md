# Slice 1 — Bindings, literals, arithmetic, `~` concat

Status: todo

## Goal
Immutable-by-default bindings with `mut`, the core scalar types, arithmetic, and `~` string concatenation, all end-to-end.

## Scope
- In: `name = expr` (immutable) and `mut name = expr`; reassignment `name = expr`; the **immutability error** when reassigning a non-`mut` binding (names the binding, explains immutability, suggests `mut`); int/float/bool/string literals; arithmetic `+ - * / %`; comparison `== != < <= > >=`; boolean `&& || !`; `~` concatenation.
- Out: functions (Slice 2), collections (Slice 3), interpolation (Slice 4).

## Checklist (vertical slice)
- [ ] Grammar / AST: let-binding (with `mut` flag), assignment, binary/unary expr nodes, literal nodes.
- [ ] Checker rule: n/a.
- [ ] Bytecode: n/a.
- [ ] Eval op: binding env with mutability; arithmetic/comparison/logical/`~` evaluation; emit the immutability diagnostic on illegal reassignment.
- [ ] Conformance cases: positive (arithmetic, `~`, `mut` reassign) + **negative** (`// expect: error E... at L:C` for non-`mut` reassignment).
- [ ] Snapshots: AST for a binding+arithmetic program; rendered diagnostic for the immutability error.

## Notes / traps
- The immutability error message is a first-class product concern — snapshot its rendered `ariadne` output.
- `~` is a built-in operator on strings in M0 (the `Concat` trait unification is M1).

## Definition of done
- Conformance cases pass for arithmetic, `~`, `mut`/immutable bindings, and the immutability error (with correct span).
- fmt/clippy clean; zero `unsafe`.
