# lang-compiler

The bytecode compiler: AST → `Chunk`.

- **Takes in:** an AST `Program` (from `lang-ast`); `PRELUDE_NAMES` (from `lang-builtins`).
- **Emits:** a `lang-bytecode` `Chunk`, or `Unsupported` for any construct outside the VM's current subset (the differential harness skips those).

M1.0 lowers literals, bindings (`mut`/immutable + reassignment), `echo`, unary/binary arithmetic, comparison, short-circuit logic, and `~` concatenation — mirroring the M0 tree-walker's evaluation order and exact diagnostic text/spans so the differential oracle sees identical `RunResult`s.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
