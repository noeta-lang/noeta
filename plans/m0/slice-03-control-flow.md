# Slice 3 — Control flow + collections

Status: todo

## Goal
`if`/`else if`/`else`, `for ... in`, indexed `for (i, x) in xs.enumerate()`, and List/Map literals + their core methods.

## Scope
- In: `if cond { } else if cond { } else { }` (no parens around conditions, braces required); `for x in iterable { }`; `for (i, x) in xs.enumerate() { }`; List literal `[1, 2, 3]` and type `List<T>`; Map literal `{"a": 1}` and type `Map<K, V>`; methods `.count()` / `len(x)` / `.enumerate()`.
- Out: `match` (Slice 5).

## Checklist (vertical slice)
- [ ] Grammar / AST: if/else-if/else chain, for-in (with optional tuple-destructure binding), list literal, map literal, index/method-call exprs.
- [ ] Checker rule: n/a.
- [ ] Bytecode: n/a.
- [ ] Eval op: branch evaluation; iteration over lists/maps; tuple destructuring in `for`; `enumerate`/`count`/`len`.
- [ ] Conformance cases: an if-chain, a `for..in` sum, an enumerate loop printing `{i}: {x}` (uses interpolation — may stub until Slice 4 or use concatenation).
- [ ] Snapshots: AST for an if + for program.

## Notes / traps
- Map iteration order must be deterministic in test mode (sort keys) — required for stable conformance output.
- List `[..]` and Map `{..}` are distinct types with distinct literals (no PHP dual-purpose array).

## Definition of done
- Conformance cases pass for if/for/enumerate and List/Map literals.
- fmt/clippy clean; zero `unsafe`.
