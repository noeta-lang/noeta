# tree-sitter-noeta

A [tree-sitter](https://tree-sitter.github.io/) grammar for the [Noeta](https://noeta.dev) programming language (`.noe`), for editors in the tree-sitter ecosystem (Neovim, Helix, Zed, Emacs).

This complements the TextMate grammar in `../vscode-noeta/` (which serves VS Code). Both target syntax highlighting; this one additionally exposes a full concrete syntax tree that structural editing, folding, and selection features can build on.

## Status

Parses **≈99% of valid Noeta** — 459 of 466 corpus programs clean; the remainder are either intentional syntax-error tests or two documented gaps (multi-line structured attribute arguments and `@doc { … }` verbatim-Markdown bodies). Built from the real lexer/parser surface and validated against the language's conformance corpus.

Highlights:

- **Case-insensitive identifiers.** Noeta does not reserve casing — `struct point {}` and `mut Total = 5` are both legal — so type-ness is decided by grammatical position, not by a `[A-Z]` heuristic. (The highlight query keeps a PascalCase heuristic only for bare identifiers whose position is genuinely ambiguous.)
- **Newline-terminated statements.** An external scanner (`src/scanner.c`) emits an automatic terminator at a line end, and suppresses it after a trailing operator or before a leading continuation (`.`, `|>`, a binary operator) — matching Noeta's own synthetic-`;` rule.
- **Nestable block comments**, also handled by the scanner (`/* /* … */ … */`).
- The restricted control-flow head (`if x { … }` is `x` + a body, not a struct literal `x { … }`), string interpolation holes, turbofish reflection calls, tier/decorator blocks, and metadata attributes.

## Build

The generated parser (`src/parser.c`, `src/grammar.json`, `src/node-types.json`) is **not** committed — regenerate it from the grammar:

```sh
npx tree-sitter generate     # grammar.js + src/scanner.c -> src/parser.c
npx tree-sitter test         # run the corpus in test/
npx tree-sitter parse FILE   # inspect a parse tree
```

## Layout

- `grammar.js` — the grammar (source of truth).
- `src/scanner.c` — external scanner: nestable block comments + automatic newline terminators.
- `queries/highlights.scm` — the highlight query (capture names follow the tree-sitter conventions).
- `test/corpus/` — `tree-sitter test` fixtures.

## Roadmap

- `queries/injections.scm` (highlight `${…}` interpolation holes as embedded expressions), `locals.scm`, `folds.scm`.
- Close the two parse gaps (multi-line structured attributes; a scanner mode for `@doc` verbatim text).
