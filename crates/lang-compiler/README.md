# lang-compiler

The bytecode compiler: AST → `Module`.

- **Takes in:** an AST `Program` (from `lang-ast`); `PRELUDE_NAMES` (from `lang-builtins`).
- **Emits:** a `lang-bytecode` `Module` (the prototype table), or `Unsupported` for any construct outside the VM's current subset (the differential harness skips those).

Lowers literals, bindings (`mut`/immutable + reassignment), `echo`, unary/binary arithmetic, comparison, short-circuit logic, `~` concatenation, **functions** (`fn` declarations, calls, arrow closures, the `|>` pipeline, `return`, `if`/`else`), **collections** — `[...]`/`{...}` literals, `for`-in (with `(i, x)` pair destructuring), the `len`/`map`/`filter`/`sum` builtins, and the `.count()`/`.enumerate()` methods — and **string interpolation** (`"… {expr} …"`, lowered to `~` concatenation of each part's display form). Names resolve through a two-level model: frame-local registers for parameters/locals, a by-name global table for top-level bindings and function names (which is also where a function's free variables resolve, faithfully — see the crate docs); a callee resolving to a prelude collection builtin lowers to a dedicated `CallBuiltin` op (a user binding of the same name shadows it). The lowering mirrors the M0 tree-walker's evaluation order and exact diagnostic text/spans so the differential oracle sees identical `RunResult`s.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
