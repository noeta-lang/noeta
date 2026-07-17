# tree-sitter-noeta

A [tree-sitter](https://tree-sitter.github.io/) grammar for the [Noeta](https://noeta.dev) programming language (`.noe`), for editors in the tree-sitter ecosystem (Neovim, Helix, Zed, Emacs).

This complements the TextMate grammar in `../vscode-noeta/` (which serves VS Code). Both target syntax highlighting; this one additionally exposes a full concrete syntax tree that structural editing, folding, and selection features can build on.

## Status

Parses **≈96% of the repository's Noeta** — 642 of 666 `.noe` files clean (2026-07 sweep); the remainder are intentional syntax-error tests, features newer than the grammar (expression tiers `@name{…}`, kernel-method `impl vec.Kernels` bundles), or the two documented gaps (multi-line structured attribute arguments and declared third-party text tiers). Built from the real lexer/parser surface and validated against the language's conformance corpus.

Highlights:

- **Case-insensitive identifiers.** Noeta does not reserve casing — `struct point {}` and `mut Total = 5` are both legal — so type-ness is decided by grammatical position, not by a `[A-Z]` heuristic. (The highlight query keeps a PascalCase heuristic only for bare identifiers whose position is genuinely ambiguous.)
- **Newline-terminated statements.** An external scanner (`src/scanner.c`) emits an automatic terminator at a line end, and suppresses it after a trailing operator or before a leading continuation (`.`, `|>`, a binary operator). The scanner tracks no bracket depth: continuation inside a multi-line `(...)`/`[...]` falls out of the grammar (terminators are only valid in statement positions, inside `{ }` blocks at any depth), so termination is **brace-relative by construction** — the same depth story as the compiler's `newline_boundaries` after the terminator-barrier change (`a` ⏎ `(n)` is two statements at every nesting level, including inside a bracket-nested closure body). Pinned by `test/corpus/termination.txt`.
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
