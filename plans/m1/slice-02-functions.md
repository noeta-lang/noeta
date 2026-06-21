# Slice M1.2 — Functions, calls, closures, pipeline

Status: todo

## Goal
Compile function declarations, calls, arrow closures, and the `|>` pipeline to register bytecode with proper call frames and closure capture.

## Scope
- In: `Stmt::Fn` and `Expr::Closure` lowering; register call frames; `CALL`/`RETURN` opcodes; argument passing in registers; closure capture as heap upvalue objects (refcounted); `Return` statement; `|>` lowering (`x |> f(a)` → `f(x, a)`); recursion via captured-scope equivalent. Named-argument parse (already positional in M0) preserved.
- Out: collections (M1.3), objects/methods (M1.4), `?`/match (M1.5), type-checking of arities/signatures (M1.7).

## Checklist (vertical slice)
- [ ] Grammar / AST: none (reuses M0 `Fn`/`Closure`/`Pipeline`/`Return`).
- [ ] Checker rule: n/a (M1.7).
- [ ] Bytecode: `CALL`/`RETURN`, closure/upvalue ops, frame layout (`lang-bytecode` + `lang-compiler`).
- [ ] VM op: call-frame push/pop, upvalue capture/close, pipeline threading (`lang-vm`).
- [ ] Conformance cases: existing `functions/*.lang`, closure capture, recursion, `|>` cases must now run on `VmBackend`.
- [ ] Snapshots: disassembly snapshots for a function + a closure-capturing chunk.

## Definition of done
- All M0 `functions/`, closure, recursion, and pipeline corpus cases run differential-identical on `VmBackend`; coverage % climbs accordingly.
- miri green on touched unsafe crates; fmt/clippy clean.

## Notes / traps
- Closures can capture cyclically; the cycle collector is M1.6. Tolerate the leak here (M0 did too) and document it — do not block this slice on GC.
- Keep the calling convention register-based (Lua/Dalvik style), not stack-based — it is the foundation for later Tier-1 specialization.
