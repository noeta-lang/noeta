# lang-value

The M1 runtime value: a NaN-boxed 64-bit word, and its operator semantics.

- **Takes in:** `BinaryOp`/`UnaryOp` (from `lang-ast`), `DiagnosticCode` (from `lang-diagnostics`).
- **Emits:** `Value` (immediate unit/bool/int/float + heap-pointer strings and boxed i64), the refcount primitives (`inc_ref`/`dec_ref`/`free`), and `apply_binary`/`apply_unary` (mirroring the M0 tree-walker exactly).

This is **the one crate with `unsafe`** in M1.0 — the NaN-box pointer round-trip (via exposed provenance) and heap-header access, kept small and `miri`-gated. It deliberately does not inherit the workspace `unsafe_code = "forbid"` lint.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
