# noeta-ir

The Core IR: a lowered, A-normal-form (ANF) representation shared by both backends.

- **Takes in:** a checked `Program` (the AST from `noeta-ast`).
- **Emits:** the ANF `Core IR` — `Atom`/`Rvalue`/`Stmt`/`Block` nodes with structured control flow — plus the AST→IR lowering and a pretty-printer.

The tree-walker and the register VM each used to re-derive evaluation order from the AST. That leaves the *intermediate* values of an expression anonymous — in `acc.x + 1`, the field load `acc.x` has no AST node, so neither backend can name it, compute its last use, or reuse its storage. ANF fixes this by making every intermediate value an explicit `let` binding over atoms (`let t0 = acc.x; let t1 = t0 + 1`), so a later pass (`noeta-ir-passes`) can attach precise reference-counting decisions to concrete IR nodes, and both backends execute the *same* annotated program (agreement by construction).

Control flow stays **structured** (no arbitrary `goto`): `if`/`while`/`for`/`match` are nodes with sub-blocks, which keeps the backward last-use analysis and the IR tree-interpreter simple. Source variables (`Atom::Var`) stay name-keyed in the lexical scope chain (closures capture them, reassignment flows through them) while ANF temporaries are single-use by construction. `Rvalue` covers the compound operations — `Object`, `Binary`, `CallMethod`, `MakeList`, `Spawn`/`SpawnIsolate`, `MakeChannel`, `TypedModuleCall`, `Await` — and lowering itself adds no `drop`/`reuse` annotations; those are filled by `noeta-ir-passes`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
