# lang-vm

The Tier-0 register virtual machine.

- **Takes in:** an AST `Program` (compiled via `lang-compiler` to a `lang-bytecode` `Module`); `Value`/`lang-gc` for the runtime.
- **Emits:** a `RunResult` (implements `Backend` as `VmBackend`); `try_run` returns `Unsupported` for programs outside the VM's current subset.

The second `Backend` after the M0 tree-walker, cross-checked against it by the differential oracle. A frame-based machine: each `fn`/closure call pushes a `Frame` (its own register file, program counter, and caller return slot); top-level bindings and function names share a by-name global environment. Memory is refcounted: each register and global owns one reference, overwrites release the old value, `Move`/`LoadGlobal`/`Call`-args retain, a returned value is retained across frame teardown, and exit releases everything — `miri`-checked for no leaks or double-frees.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
