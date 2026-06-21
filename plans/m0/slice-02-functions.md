# Slice 2 — Functions, closures, calls, pipeline `|>`

Status: done

## Goal
`fn` declarations, arrow closures, function application, and the `|>` pipeline operator.

## Scope
- In: `fn name(p: T, ...): R { ... }` declarations (types parsed, not checked in M0); arrow closures `fn(a, b) => expr`; calls `f(args)`; `return`; the `|>` operator (`x |> f(...)` ≡ `f(x, ...)`); builtins `map`/`filter`/`sum` so pipelines chain.
- Out: methods on classes (Slice 6), `?` (Slice 7).

## Checklist (vertical slice)
- [x] Grammar / AST: `FnDecl`, `Param`, `TypeRef` (parsed, unchecked), `Stmt::{Return, Expr}`, `Expr::{Call, Closure, Pipeline}`; postfix call + pipeline in the Pratt loop; type-annotation and block parsing.
- [x] Checker rule: n/a.
- [x] Bytecode: n/a.
- [x] Eval op: lexical scope chain (`Rc<Scope>` with parent links), closures capturing their defining scope, call frames with arity checks, `return` flow, pipeline threading, `next_id` builtin.
- [x] Conformance cases: `functions/basic.lang`, `functions/closures_and_pipeline.lang`.
- [x] Snapshots: fn+pipeline and closure+call parser snapshots.

## Outcome
First-class functions and closures with a proper lexical scope chain (forward references
work; self-recursion lands with `if` in Slice 3). `next_id()` wired as a deterministic
builtin. 36 tests; 8 conformance cases; fmt/clippy clean.

Notes / deferred:
- `map`/`filter`/`sum` moved to **Slice 3** (they need List, which lands there).
- The scope chain uses non-atomic `Rc` (matches the per-isolate design). A global function
  capturing the global scope forms an `Rc` cycle that leaks until process exit — exactly
  what the planned M1 cycle collector addresses; documented in `lang-eval`.
- `TypeRef` annotations are parsed and retained but not interpreted (M1 checker).

## Definition of done
- Conformance cases pass for fn/closure/`|>`.
- fmt/clippy clean; zero `unsafe`.
