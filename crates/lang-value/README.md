# lang-value

The M1 runtime value: a NaN-boxed 64-bit word, and its operator semantics.

- **Takes in:** `BinaryOp`/`UnaryOp` (from `lang-ast`), `DiagnosticCode` (from `lang-diagnostics`), `Shape` (from `lang-object`).
- **Emits:** `Value` (immediate unit/bool/int/float + heap-pointer strings, boxed i64, closures, lists/maps, and shaped objects/enums), the refcount primitives (`inc_ref`/`dec_ref`/`free`, which release a collection's or object's elements recursively), the cycle-collector mechanism the `lang-gc` `CycleCollector` drives (`Color`, per-object color/buffered flags, raw non-freeing refcount edits, child enumeration, a child-preserving `free_shallow`, and the `set_slot` mutation primitive), `apply_binary`/`apply_unary` (mirroring the M0 tree-walker exactly, including structural object/enum equality), and `compare_primitive`/`structural_compare` (the primitive total order behind `.compare()` and the field-wise object ordering `#[derive(Comparable)]` synthesizes).

This is **the one crate with `unsafe`** in M1.0 — the NaN-box pointer round-trip (via exposed provenance) and heap-header access, kept small and `miri`-gated. It deliberately does not inherit the workspace `unsafe_code = "forbid"` lint.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
