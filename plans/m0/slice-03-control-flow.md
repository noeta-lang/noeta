# Slice 3 — Control flow + collections

Status: done

## Goal
`if`/`else if`/`else`, `for ... in`, indexed `for (i, x) in xs.enumerate()`, and List/Map literals + their core methods.

## Scope
- In: `if cond { } else if cond { } else { }` (no parens around conditions, braces required); `for x in iterable { }`; `for (i, x) in xs.enumerate() { }`; List literal `[1, 2, 3]` and type `List<T>`; Map literal `{"a": 1}` and type `Map<K, V>`; methods `.count()` / `len(x)` / `.enumerate()`.
- Out: `match` (Slice 5).

## Checklist (vertical slice)
- [x] Grammar / AST: `Stmt::{If, For}`, `ForPattern`, `Expr::{List, Map, Member}`; `.` member access + map/list literals in the Pratt parser. `else if` desugars to a nested `if`.
- [x] Checker rule: n/a.
- [x] Bytecode: n/a.
- [x] Eval op: branch eval with child scopes; for-in over lists (and map values); pair destructuring; `.count()`/`.enumerate()` methods; `len`/`map`/`filter`/`sum` builtins. Recursion now works (`fact` via `if`).
- [x] Conformance cases: `control_flow/if_for.lang`, `collections/list_map_pipeline.lang`.
- [x] Snapshots: control-flow + collections parser snapshot.

## Outcome
`if`/`else if`/`else`, `for ... in`, indexed `for (i, x) in xs.enumerate()`, List/Map
literals, `.count()`/`.enumerate()`, and the `map`/`filter`/`sum` pipeline all work.
Map literal `{...}` vs block `{...}` disambiguated by position. Self-recursion confirmed.
43 tests; 10 conformance cases; fmt/clippy clean.

Note: iterating a map yields its values in deterministic key order; map keys are strings
in M0; bare (uncalled) member access is reserved for record fields (Slice 6).

## Notes / traps
- Map iteration order must be deterministic in test mode (sort keys) — required for stable conformance output.
- List `[..]` and Map `{..}` are distinct types with distinct literals (no PHP dual-purpose array).

## Definition of done
- Conformance cases pass for if/for/enumerate and List/Map literals.
- fmt/clippy clean; zero `unsafe`.
