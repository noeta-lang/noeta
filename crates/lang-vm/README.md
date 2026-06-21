# lang-vm

The Tier-0 register virtual machine.

- **Takes in:** an AST `Program` (compiled via `lang-compiler` to a `lang-bytecode` `Chunk`); `Value`/`lang-gc` for the runtime.
- **Emits:** a `RunResult` (implements `Backend` as `VmBackend`); `try_run` returns `Unsupported` for programs outside the M1.0 subset.

The second `Backend` after the M0 tree-walker, cross-checked against it by the differential oracle. Memory is refcounted: each register owns one reference, overwrites release the old value, `Move` retains, and exit releases everything — `miri`-checked for no leaks or double-frees.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
