# lang-parser

The parser (a [`chumsky`] parser-combinator grammar).

- **Takes in:** a token stream (`&[Token]`)
- **Emits:** an AST (`Parsed`). Built with `chumsky` (statements via `choice`/`recursive`, the expression grammar via the `pratt` combinator) over the `logos` token stream; the public surface is just `parse`, so the implementation can change freely.

[`chumsky`]: https://docs.rs/chumsky/1.0.0-alpha.8/chumsky/

Part of the `lang` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md` for the crate map and where each kind of change goes).
