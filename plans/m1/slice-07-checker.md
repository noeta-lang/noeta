# Slice M1.7 — Type checker: types + inference + ADT/exhaustiveness + ownership

Status: todo

## Goal
The single largest irreducible piece: an ML-grade, gradual-but-real type checker built as salsa queries, with inference, exhaustive `match`, typed `?`, and immutability/ownership analysis.

## Scope
- In:
  - **`lang-types`** crate: the `Type` lattice, `TypeId` interning, the `?T` = `Option<T>` desugar as a type.
  - **`lang-check`** crate: the `checked_ast(db)` salsa query — Hindley-Milner-style inference (annotations optional, name-first), ADT/exhaustive-`match` **static** check (promote M1.5's runtime non-exhaustive error to a compile error), `?`-on-non-`Result`/`Option` rejection, unhandled-`Result` lint, immutable-by-default + ownership analysis (eliding defensive copies).
  - New diagnostics catalog entries (E0011+), each with a negative conformance case (`// expect: error EXXXX at L:C`).
- Out: trait resolution / operator dispatch (M1.8 — though the checker scaffolds the trait/impl tables in `lang-types`); generics specialization (M1.8); module-aware resolution (M1.9).

## Checklist (vertical slice)
- [ ] Grammar / AST: type-annotation surface already in M0 AST (`TypeRef`, `Param.ty`, `Fn.ret`); wire it into checking.
- [ ] Checker rule: inference + exhaustiveness + `?`-typing + ownership/immutability, as salsa queries inserted between `ast(db)` and `chunk(db)`.
- [ ] Bytecode: `lang-compiler` consumes `checked_ast` (type info may inform lowering; behavior unchanged where untyped).
- [ ] VM op: none new (checking is compile-time); the M1.5 runtime non-exhaustive error becomes unreachable for checked programs.
- [ ] Conformance cases: positive (well-typed programs still run) **and** negative (`// expect: error E00xx`) for each rule: type mismatch, non-exhaustive match, `?` misuse, immutability violation, unhandled `Result`.
- [ ] Snapshots: rendered-`ariadne` snapshots for each new diagnostic (the error-quality gallery).

## Definition of done
- **Thrust B gate (partial):** every static-error class above has a negative conformance case with the correct code + span; the diagnostic gallery snapshots them.
- All previously-passing programs still typecheck and run differential-identical.
- The checker is a salsa query (incremental by construction); fmt/clippy clean.

## Notes / traps
- Build it as queries from the first commit — do not write straight-line passes and retrofit salsa. The M1.1 plumbing means `checked_ast` slots into an existing graph.
- Gradual: unannotated code must keep working (inference fills gaps); annotations are constraints, not a wall.
- This slice is large; if it grows unwieldy, split inference (7a) from ownership/immutability (7b) — but keep each independently corpus-verifiable.
