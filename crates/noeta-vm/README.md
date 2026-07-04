# noeta-vm

The Tier-0 register virtual machine.

- **Takes in:** an AST `Program` (compiled via `noeta-compiler` to a `noeta-bytecode` `Module`); `Value`/`noeta-gc` for the runtime.
- **Emits:** a `RunResult` (implements `Backend` as `VmBackend`); `try_run` returns `Unsupported` for programs outside the VM's current subset.

The second `Backend` after the M0 tree-walker, cross-checked against it by the differential oracle. A frame-based machine: each `fn`/closure/method call pushes a `Frame` (its own register file, program counter, and caller return slot); top-level bindings and function names share a by-name global environment. At startup it builds one shared `Rc<Shape>` per shape-table entry (so equal-built objects share a shape) and a `(type, method) → prototype` dispatch map; an instance method call resolves through it and pushes a `[receiver, args...]` frame. A class's `destruct` block is the runtime-invoked destructor: when the last reference to a global-scoped instance drops (reassignment, or program end in reverse declaration order), `release_value` runs it before freeing — deterministic destruction matching the tree-walker. The native `map`/`filter` builtins re-enter the VM to call their closure argument per element, by running a fresh frame stack to completion — the dispatch loop's frame stack is a local, not VM state, so this is ordinary Rust recursion over the shared globals/stdout/diagnostics. Memory is refcounted: each register and global owns one reference, overwrites release the old value, `Move`/`LoadGlobal`/`Call`-args retain, a returned value is retained across frame teardown, a heap collection or object owns one reference to each element/slot it holds (freed recursively), and exit releases everything — `miri`-checked for no leaks or double-frees.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
