# lang-compiler

The bytecode compiler: AST → `Module`.

- **Takes in:** an AST `Program` (from `lang-ast`); `PRELUDE_NAMES` (from `lang-builtins`).
- **Emits:** a `lang-bytecode` `Module` (the prototype table), or `Unsupported` for any construct outside the VM's current subset (the differential harness skips those).

Lowers literals, bindings (`mut`/immutable + reassignment), `echo`, unary/binary arithmetic, comparison, short-circuit logic, `~` concatenation, and **functions** — `fn` declarations, calls, arrow closures, the `|>` pipeline, `return`, and `if`/`else`. Names resolve through a two-level model: frame-local registers for parameters/locals, a by-name global table for top-level bindings and function names (which is also where a function's free variables resolve, faithfully — see the crate docs). The lowering mirrors the M0 tree-walker's evaluation order and exact diagnostic text/spans so the differential oracle sees identical `RunResult`s.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
