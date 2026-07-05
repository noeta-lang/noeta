# Noeta for Visual Studio Code

Syntax highlighting and editor configuration for the [Noeta](https://noeta.dev) programming language (`.noe`).

This is the first piece of Noeta's editor tooling. It is a **static grammar** — it colorizes source without running the compiler, so it works instantly and offline. Semantic features (live diagnostics, hover types, go-to-definition, completion) will arrive with the `noeta lsp` language server; this extension is structured to host that client when it lands.

## Features

- **Syntax highlighting** for the full Noeta surface:
  - keywords — control flow, declarations (`fn`/`struct`/`class`/`enum`/`impl`), concurrency (`async`/`spawn`/`isolate`/`channel`), and the operator words `as`/`is`
  - the three string forms — `"…"`, `'…'`, and backtick templates — with `${…}` interpolation holes highlighted as embedded expressions and `\${` recognized as an escape
  - every numeric form — decimal, `0x`/`0o`/`0b`, floats with exponents, the `f32` suffix, and the fixed-width integer suffixes (`i8`…`u64`)
  - primitive and container types (`int`, `string`, `List`, `Map`, …) and PascalCase user types
  - directives and tier blocks (`@derive(…)`, `@role(…)`, `@test`, `@bench(…)`, `@doc`) and metadata attributes (`#[Name(…)]`)
  - the pipeline `|>`, ranges `..`/`..=`, spread `...`, and the coalescing `??`/`??=` operators
- **Editor configuration** — comment toggling (`//`, `/* */`), bracket matching, auto-closing pairs, and indentation rules.

## Install (from source)

No published Marketplace release yet. To use it locally:

```sh
# symlink (or copy) this folder into your VS Code extensions directory
ln -s "$PWD/editors/vscode-noeta" ~/.vscode/extensions/noeta-0.1.0
```

Then reload VS Code. Any `.noe` file will pick up the `noeta` language mode. To package a `.vsix` instead, install [`vsce`](https://github.com/microsoft/vscode-vsce) and run `vsce package` in this directory.

## Testing the grammar

`sample.noe` in this folder exercises every construct the grammar covers — open it after installing and confirm each token category colorizes. To inspect the scope assigned to the token under the cursor, run **Developer: Inspect Editor Tokens and Scopes** from the VS Code command palette.

## Scope reference

The grammar emits standard TextMate scopes so it inherits sensible colors from any theme. The Noeta-specific leaf scopes (all suffixed `.noeta`) are namespaced under the root scope `source.noeta`.

## Roadmap

- `noeta lsp` — a language server over the compiler's salsa query graph: live diagnostics first, then hover types, go-to-definition, and completion.
- A tree-sitter grammar for editors outside the TextMate ecosystem (Neovim, Zed, Helix).

See `docs/Editor-and-AI-Tooling.md` in the repository for the full plan.
