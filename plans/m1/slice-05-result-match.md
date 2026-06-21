# Slice M1.5 — Result/Option/`?`/`??` + match

Status: todo

## Goal
Compile `match`, `?` propagation, and `??` coalescing to bytecode, completing the language-feature surface the VM must reproduce.

## Scope
- In: `match` compilation to jump tables / decision trees over shapes (variant dispatch + binding destructuring + literal patterns + wildcard); `Try` (`?`) as early-return via frame unwinding in the VM (on `Err`/`none`); `Coalesce` (`??`) fallback on `Err`/`none`; the `Result`/`Option` constructors (`Ok`/`Err`/`some`/`none`) and `panic` reusing the M1.4 enum representation; runtime non-exhaustive-match error.
- Out: **static** exhaustiveness checking (M1.7 promotes the runtime error to a compile error); typed `?` (M1.7).

## Checklist (vertical slice)
- [ ] Grammar / AST: none (reuses M0 `Match`/`Try`/`Coalesce`).
- [ ] Checker rule: n/a (M1.7).
- [ ] Bytecode: match decision-tree opcodes, `?`-unwind, `??`-fallback, constructor builtins.
- [ ] VM op: pattern dispatch, frame unwind on `?`, panic → E0010 (`lang-vm`).
- [ ] Conformance cases: existing `enums/`, `results/`, match-fallthrough, panic cases run on `VmBackend`.
- [ ] Snapshots: disassembly for a `match` with a decision tree and a `?`-propagating function.

## Definition of done
- **Thrust A gate:** `cargo run -p lang-cli -- test --differential` shows **100% corpus coverage** with zero backend divergence — including the full §14 `examples/orders.lang` demo. The tree-walker is now frozen as the pure oracle.
- miri green; fmt/clippy clean.

## Notes / traps
- `?` early-return is the M0 `Unwind::Return` mechanism translated to VM frame unwinding — keep the call-boundary catch semantics identical.
- After this slice the tree-walker stops being the forcing function and becomes CI insurance; any new post-Thrust-A feature must land in both backends in the same slice (or be explicitly oracle-exempt).
