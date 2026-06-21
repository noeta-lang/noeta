# Slice M1.2 — Functions, calls, closures, pipeline

Status: done

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

## Outcome
The flat single-chunk VM became a **frame-based machine** with a function-prototype table and a runtime **global environment** — the largest structural change since M1.0.

- **Two-level scope model.** `lang-bytecode` gained a `Module { protos: Vec<Chunk> }` (proto 0 = top level; each `fn`/closure is another proto, `Chunk` now carries `num_params`). Top-level bindings and function names live in a by-name runtime `globals` table (`LoadGlobal`/`StoreGlobal`); parameters and locals live in registers, one file per call frame. A function's free variables resolve to globals at call time — **faithful**, because the tree-walker's captured scope for a top-level function *is* the shared, mutable global scope, so reads see live values. This is why M1.2 needs **no upvalue machinery**: the only functions compiled are defined at the top level, capturing nothing but globals.
- **New opcodes:** `Call`/`Return` (frame push/pop, return value threaded into the caller's register), `MakeClosure`, `LoadGlobal`/`StoreGlobal`, plus `RequireCondBool`/`Jump` for the `if`/`else` statement (needed for recursion). A closure is a new refcounted heap payload (`Payload::Closure(u32)`) in `lang-value` — `type_name` "function", display `<fn>`.
- **Conservatively skipped (no divergence):** nested function/closure definitions (could capture a non-global local — the upvalue path is a later slice), method-call pipelines, bare assignment to a non-local inside a function, and any reference to a prelude builtin (`len`/`map`/`Ok`/`none`/… arrive with collections/results). These return `Unsupported`, so the harness skips them.
- **Refcount discipline** extends cleanly to frames: `Call` retains each argument into the callee's registers; `Return` retains the result across the frame's teardown then transfers that reference to the caller; `Halt` in a non-top frame is an implicit unit return; on exit every frame register *and* every global is released. **miri-clean** over `lang-value`/`lang-gc`/`lang-vm` (deep recursion, closure allocation, the global table — no leaks, no UB).
- **Differential oracle:** 14 matched, 18 skipped, 4 parse-failed — **43.8% coverage** (up from 31.2%), zero divergence. The four newly-covered cases: `functions/basic`, `functions/closures_and_pipeline`, `functions/recursion`, `bindings/shadowing`. (`control_flow/*` use `for`, which lands with collections in M1.3.) The regression floor in `differential_backends_agree` rose to ≥14.

Gates green: `cargo test --workspace`, `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `miri`.

Two disassembly snapshots (a global-binding chunk and the recursive `fib` module) committed.
