# noeta-builtins

The prelude support (value-agnostic parts).

- **Takes in:** nothing
- **Emits:** prelude metadata (`PRELUDE_NAMES`, the names the parser/checker treat as built-in). The value-returning builtins themselves (the `Ok`/`Err`/`some`/`none` constructors, `echo`, `assert`, `panic`) are implemented in each backend, since they construct backend-specific values. (`IdGen` left with the id-entropy arc — `id.next_id()` is a registry function over the Host's `Ids` capability.)

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
