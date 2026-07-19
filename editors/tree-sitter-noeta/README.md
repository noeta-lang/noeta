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
npm test                     # static corpus (tree-sitter test) + the generated-overlay corpus
npx tree-sitter parse FILE   # inspect a parse tree
```

## Per-project text tiers

A tier block `@name { … }` may open a **verbatim** text body (`@doc`, or any `@tier(<name>, text:
"<lang>")` a project or extension declares) that the compiler never lexes as code. A *static*
grammar cannot know which `@name`s open such a body — extensions and programs declare their own — so
this grammar recognizes only the std default (`doc`) and parses the rest as code.

`noeta grammar tree-sitter --out <this-dir>` closes the gap for a given project (the tree-sitter
analogue of the VS Code extension's `generated-tiers.tmLanguage.json` generator). It scans the
project's declared tiers — from the compiler's own tier discovery, plus installed native tiers — and
writes an overlay into this grammar checkout:

- **`project-tiers.json`** — the verbatim-body tier-name token list `grammar.js` reads. Present, it
  widens `TEXT_TIER_NAMES` so `@spec { … }` bodies parse as `text_body` prose; absent (or invalid),
  `grammar.js` falls back to the static `doc`-only set. `.gitignore`d, so a checkout stays static by
  default.
- **`queries/injections.scm`** — regenerated with one language-injection rule per tier, so each
  verbatim body highlights as its declared language.

Re-run `tree-sitter generate` after writing the overlay (or pass `--generate`) to rebuild the parser.

## Layout

- `grammar.js` — the grammar (source of truth); its `TEXT_TIER_NAMES` reads an optional `project-tiers.json` overlay.
- `src/scanner.c` — external scanner: nestable block comments, automatic newline terminators, and verbatim text-tier bodies.
- `queries/highlights.scm` — the highlight query (capture names follow the tree-sitter conventions).
- `queries/injections.scm` — language injections for text-tier bodies (the static `@doc` → markdown rule; regenerated per-project by `noeta grammar tree-sitter`).
- `test/corpus/` — the static `tree-sitter test` fixtures; `test/project/` — the generated-overlay corpus (`npm run test:project`).

## Roadmap

- `queries/locals.scm`, `queries/folds.scm`.
- Close the remaining parse gap (multi-line structured attribute arguments).
