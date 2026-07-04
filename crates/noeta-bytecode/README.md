# lang-bytecode

The register bytecode IR.

- **Takes in:** `BinaryOp`/`UnaryOp` (from `lang-ast`), `Span` (from `lang-span`), `Diagnostic` (from `lang-diagnostics`), `Shape` (from `lang-object`).
- **Emits:** the `Op` opcode set (arithmetic/branching, `Call`/`Return`, the `MakeList`/`MakeMap`/iteration ops, `CallBuiltin` for the collection prelude, and the object-model ops `MakeRecord`/`MakeOpaque`/`MakeEnum`/`LoadField`/`CallMethod`), the `Builtin` dispatch tag, the `Chunk` (one function prototype: code + constant pool + precomputed diagnostics + parameter/register counts), the `Module` (the prototype table plus the `shapes` layout table and `methods` dispatch table — proto 0 is the top-level program, the rest are functions/closures/methods), and a disassembler (`Chunk::disassemble`/`Module::disassemble`) producing stable text for snapshot tests.

Pure data — it knows nothing about runtime values. Register-based (Lua/Dalvik style), not stack-based, for a friendlier base for the later specializing interpreter.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
