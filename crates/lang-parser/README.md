# lang-parser

The parser (hand-written recursive descent).

- **Takes in:** a token stream (`&[Token]`)
- **Emits:** an AST (`Parsed`). Hand-written for diagnostic/error-recovery control; the public surface is just `parse`, so the implementation can change freely.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
