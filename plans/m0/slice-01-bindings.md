# Slice 1 — Bindings, literals, arithmetic, `~` concat

Status: done

## Goal
Immutable-by-default bindings with `mut`, the core scalar types, arithmetic, and `~` string concatenation, all end-to-end.

## Scope
- In: `name = expr` (immutable) and `mut name = expr`; reassignment `name = expr`; the **immutability error** when reassigning a non-`mut` binding (names the binding, explains immutability, suggests `mut`); int/float/bool/string literals; arithmetic `+ - * / %`; comparison `== != < <= > >=`; boolean `&& || !`; `~` concatenation.
- Out: functions (Slice 2), collections (Slice 3), interpolation (Slice 4).

## Checklist (vertical slice)
- [x] Grammar / AST: `Stmt::Binding` (with `mut_decl`), `Expr::{Int,Float,Bool,Ident,Unary,Binary}`, `BinaryOp`/`UnaryOp`; Pratt parser with precedence/associativity.
- [x] Checker rule: n/a.
- [x] Bytecode: n/a.
- [x] Eval op: env with mutability; arithmetic (int/float promotion), comparison, short-circuit logic, `~` concat; immutability error + runtime `TypeMismatch`/`DivisionByZero`/`UnknownName`.
- [x] Conformance cases: `expressions/arithmetic.lang`, `expressions/comparison.lang`, `bindings/mutable.lang`, and negative `bindings/immutable_error.lang` (E0006 at L:C).
- [x] Snapshots: parser precedence/unary snapshots (`lang-parser`).

## Outcome
Diagnostics catalog grew with E0006 (ImmutableAssignment), E0007 (TypeMismatch), E0008
(DivisionByZero). The immutability error renders with the binding name + `mut` suggestion.
34 tests green; 6 conformance cases; fmt/clippy clean. `lang-eval` gained `lexer`/`parser`
as **dev-dependencies** only (test convenience; no production cycle).

## Notes / traps
- The immutability error message is a first-class product concern — snapshot its rendered `ariadne` output.
- `~` is a built-in operator on strings in M0 (the `Concat` trait unification is M1).

## Definition of done
- Conformance cases pass for arithmetic, `~`, `mut`/immutable bindings, and the immutability error (with correct span).
- fmt/clippy clean; zero `unsafe`.
