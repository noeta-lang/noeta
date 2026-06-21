# lang-bytecode

The register bytecode IR.

- **Takes in:** `BinaryOp`/`UnaryOp` (from `lang-ast`), `Span` (from `lang-span`), `Diagnostic` (from `lang-diagnostics`).
- **Emits:** the `Op` opcode set, the `Chunk` (code + constant pool + precomputed diagnostics + register count), and a disassembler (`Chunk::disassemble`) producing stable text for snapshot tests.

Pure data — it knows nothing about runtime values. Register-based (Lua/Dalvik style), not stack-based, for a friendlier base for the later specializing interpreter.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
