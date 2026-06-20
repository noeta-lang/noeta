# lang-lexer

The lexer.

- **Takes in:** source text (`&Source`)
- **Emits:** a flat token stream (`Lexed`) plus lexing `Diagnostic`s. Token kinds are defined declaratively with `logos`.

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
