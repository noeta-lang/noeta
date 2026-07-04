# noeta-eval

The tree-walking evaluator.

- **Takes in:** an AST (`&Program`)
- **Emits:** a structured `RunResult` behind the `Backend` trait. Never writes stdout/exits directly — making it a clean differential oracle for the M1 VM.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
