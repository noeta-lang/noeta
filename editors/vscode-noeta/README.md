# Noeta for Visual Studio Code

Language support for the [Noeta](https://noeta.dev) programming language (`.noe`): a **static
grammar** that colorizes instantly and offline, plus the **`noeta lsp` language server** for semantic
features backed by the compiler itself.

## Features

- **Language server** (`noeta lsp`) — the extension launches the compiler's own language server, so
  every semantic feature reflects exactly what the compiler sees:
  - **Live diagnostics** as you type — the same errors (`E0xxx`) the compiler reports, with their
    labels, across a whole module's imports.
  - **Hover types** — the inferred type of the expression under the cursor.
  - **Go-to-definition** — jump to a local, parameter, function, type, field, or method — including
    across modules.
  - **Find references & rename** — every use of a value or member symbol (type-aware, so a same-named
    field on another type is left alone), across modules; rename validates the new name first.
  - **Signature help** — the called function's or method's signature with the active argument
    highlighted as you type the call.
  - **Semantic highlighting** — compiler-accurate colouring that tells a function from a variable from
    a property, overlaid on the static grammar.
  - **Document outline** — the symbol tree for breadcrumbs and `@`-symbol search.
  - **Completion** — keywords, in-scope names, a receiver type's members after `.`, and type names in
    annotation position.
- **Debugging** (`noeta dap`) — run a `.noe` file under the compiler's own bytecode VM (JIT off, so
  every frame is inspectable) through VS Code's debug UI:
  - **Breakpoints** — click the gutter of any executable line; **stepping** — step over / into / out,
    line by line.
  - **Call stack, scopes, and variables** — each paused frame's locals with their values and types,
    read straight from the live VM.
  - Press **F5** on a `.noe` file to debug it (no `launch.json` needed); output appears in the Debug
    Console. See **Debugging** below.
- **Syntax highlighting** for the full Noeta surface:
  - keywords — control flow, declarations (`fn`/`struct`/`class`/`enum`/`impl`), concurrency (`async`/`spawn`/`isolate`/`channel`), and the operator words `as`/`is`
  - the three string forms — `"…"`, `'…'`, and backtick templates — with `${…}` interpolation holes highlighted as embedded expressions and `\${` recognized as an escape
  - every numeric form — decimal, `0x`/`0o`/`0b`, floats with exponents, the `f32` suffix, and the fixed-width integer suffixes (`i8`…`u64`)
  - primitive and container types (`int`, `string`, `List`, `Map`, …) and PascalCase user types
  - directives and tier blocks (`@derive(…)`, `@role(…)`, `@test`, `@bench(…)`, `@doc`) and metadata attributes (`#[Name(…)]`)
  - the pipeline `|>`, ranges `..`/`..=`, spread `...`, and the coalescing `??`/`??=` operators
- **Editor configuration** — comment toggling (`//`, `/* */`), bracket matching, auto-closing pairs, and indentation rules.

## Requirements

The language server ships with the Noeta toolchain as the `noeta lsp` subcommand. Build it and put it
on your `PATH`:

```sh
cargo build --release          # produces target/release/noeta
```

Point the extension at it with the **`noeta.server.path`** setting (an absolute path such as
`.../target/release/noeta`), or leave it as the default `noeta` if the binary is on your `PATH`. If
the server can't be launched, the highlighting still works — only the semantic features are disabled,
and the reason appears in the **Noeta Language Server** output channel.

## Install (from source)

No published Marketplace release yet. The extension is not bundled, so install its runtime dependency
first, then load it:

```sh
cd editors/vscode-noeta
npm install                    # fetches vscode-languageclient

# then either: open this folder in VS Code and press F5 (Run Noeta Extension), or
# symlink it into your extensions directory and reload:
ln -s "$PWD" ~/.vscode/extensions/noeta-0.3.0
```

Any `.noe` file then picks up the `noeta` language mode and the server starts automatically. Run
**Noeta: Restart Language Server** from the command palette after rebuilding the server. To package a
`.vsix`, install [`vsce`](https://github.com/microsoft/vscode-vsce), run `npm install --omit=dev`
(so only the runtime dependency is packaged), then `vsce package`.

## Debugging

The extension registers a **`noeta` debug type** backed by the `noeta dap` subcommand (the Debug
Adapter Protocol server that ships with the toolchain, launched via the same `noeta.server.path`).

To debug the file you're editing, just press **F5** and pick **Noeta** if prompted — with no
`launch.json`, the extension runs the active `.noe` file. For a saved configuration, add this to
`.vscode/launch.json`:

```json
{
  "type": "noeta",
  "request": "launch",
  "name": "Debug Noeta file",
  "program": "${file}",
  "stopOnEntry": false
}
```

- `program` — the `.noe` file to run (defaults to the active file).
- `stopOnEntry` — pause before the first instruction instead of running to the first breakpoint.

Set breakpoints in the gutter, then step with the debug toolbar; the **Variables** view shows each
frame's locals. The program's `echo` output goes to the Debug Console. Debugging runs the same
production VM as `noeta run`, only with the JIT unarmed so every frame stays inspectable.

## Testing the grammar

`sample.noe` in this folder exercises every construct the grammar covers — open it after installing and confirm each token category colorizes. To inspect the scope assigned to the token under the cursor, run **Developer: Inspect Editor Tokens and Scopes** from the VS Code command palette.

## Scope reference

The grammar emits standard TextMate scopes so it inherits sensible colors from any theme. The Noeta-specific leaf scopes (all suffixed `.noeta`) are namespaced under the root scope `source.noeta`.

## Roadmap

- **`noeta lsp`** — the language server over the compiler's salsa query graph is **wired in**: live
  diagnostics, hover, go-to-definition, find references, rename, signature help, semantic highlighting,
  document outline, and completion — most working across modules.
- A tree-sitter grammar for editors outside the TextMate ecosystem (Neovim, Zed, Helix).

See `docs/Editor-and-AI-Tooling.md` in the repository for the full plan.
