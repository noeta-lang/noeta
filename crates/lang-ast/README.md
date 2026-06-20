# lang-ast

The abstract syntax tree: pure data, no behavior.

- **Takes in:** nothing (consumes only `lang-span`)
- **Emits:** AST node types (every node carries a `Span`), the `SyntaxKind` tag set, and a stable `Pretty` printer for snapshots.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
