# noeta-diagnostics

The one error catalog and the single diagnostic renderer.

- **Takes in:** `Diagnostic` values from any pipeline stage
- **Emits:** the stable `DiagnosticCode` catalog (`E0xxx`) and the only `ariadne`-backed `render` function. Stages never format errors themselves.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
