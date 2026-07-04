# lang-backend

The execution-backend seam shared by every runtime.

- **Takes in:** an AST `Program` (from `lang-ast`).
- **Emits:** the `Backend` trait and the `RunResult` type (stdout, exit code, diagnostics).

Extracted from `lang-eval` in M1 so the tree-walker (`lang-eval`) and the bytecode VM (`lang-vm`) are siblings — neither depends on the other, and the differential oracle compares their `RunResult`s, never their internal value models.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
