# noeta-eval

The reference evaluation backend: a Core-IR interpreter (not a tree-walker — the original M0 AST
tree-walker this crate began as was retired in the memory-management migration, since it fired
destructors only at teardown and could not reproduce last-use destruction; only the crate's name
history survives).

- **Takes in:** an AST (`&Program`), which it lowers to the Core IR (`noeta-ir`, the same
  RC-annotated IR the VM compiles from) before interpreting it.
- **Emits:** a structured `RunResult` behind the `Backend` trait. Never writes stdout/exits directly — making it a clean differential oracle for the M1 VM.

**Test-only.** Consumed only by the dev-only `noeta-conformance` harness — `noeta-cli` does not link this crate, and `noeta run` executes on the bytecode VM (`noeta-vm`) via `noeta-runner`.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
