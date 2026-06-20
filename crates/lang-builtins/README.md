# lang-builtins

The M0 prelude support (value-agnostic parts).

- **Takes in:** nothing
- **Emits:** deterministic identity generation (`IdGen`, backing `next_id`) and prelude metadata. Value-returning builtins live in `lang-eval` during M0.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
