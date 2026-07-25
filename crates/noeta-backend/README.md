# noeta-backend

The execution-backend seam shared by every runtime.

- **Takes in:** an AST `Program` (from `noeta-ast`).
- **Emits:** the `Backend` trait and the `RunResult` type (stdout, exit code, diagnostics).

Extracted from `noeta-eval` in M1 so the reference Core-IR interpreter (`noeta-eval`) and the bytecode VM (`noeta-vm`) are siblings — neither depends on the other, and the differential oracle compares their `RunResult`s, never their internal value models.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
