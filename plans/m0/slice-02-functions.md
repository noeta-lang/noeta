# Slice 2 — Functions, closures, calls, pipeline `|>`

Status: todo

## Goal
`fn` declarations, arrow closures, function application, and the `|>` pipeline operator.

## Scope
- In: `fn name(p: T, ...): R { ... }` declarations (types parsed, not checked in M0); arrow closures `fn(a, b) => expr`; calls `f(args)`; `return`; the `|>` operator (`x |> f(...)` ≡ `f(x, ...)`); builtins `map`/`filter`/`sum` so pipelines chain.
- Out: methods on classes (Slice 6), `?` (Slice 7).

## Checklist (vertical slice)
- [ ] Grammar / AST: fn decl, params (name-first types), closure expr, call expr, pipeline expr (kept as its own node — sugar stays in the AST), `return` stmt.
- [ ] Checker rule: n/a.
- [ ] Bytecode: n/a.
- [ ] Eval op: closures capture env; call frames; `return`; pipeline evaluation; `map`/`filter`/`sum` builtins over lists.
- [ ] Conformance cases: a function, a closure, a `|>` chain producing a known result.
- [ ] Snapshots: AST for a pipeline program.

## Notes / traps
- Keep `|>` as a distinct AST node (don't desugar in the parser) so later diagnostics can point at the pipeline.
- `map`/`filter` take a closure; ensure closure values are first-class.

## Definition of done
- Conformance cases pass for fn/closure/`|>`.
- fmt/clippy clean; zero `unsafe`.
