# lang-bytecode

The register bytecode IR.

- **Takes in:** `BinaryOp`/`UnaryOp` (from `lang-ast`), `Span` (from `lang-span`), `Diagnostic` (from `lang-diagnostics`).
- **Emits:** the `Op` opcode set, the `Chunk` (one function prototype: code + constant pool + precomputed diagnostics + parameter/register counts), the `Module` (the prototype table — proto 0 is the top-level program, the rest are functions/closures), and a disassembler (`Chunk::disassemble`/`Module::disassemble`) producing stable text for snapshot tests.

Pure data — it knows nothing about runtime values. Register-based (Lua/Dalvik style), not stack-based, for a friendlier base for the later specializing interpreter.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
