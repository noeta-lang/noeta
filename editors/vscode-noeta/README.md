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
  - **Call hierarchy** — `Shift+Alt+H` peeks a function's callers/callees (cross-module) over the
    compiler's static call graph, `@role` bindings shown on each item.
  - **Role lenses & request traces** — `@role`-bearing declarations show a
    `⚑ Enum.Variant · trace request path` lens; clicking opens a read-only trace document that
    unfolds the full static path from that entry point (boundaries summary, clickable `path:line`
    links, external/dynamic calls labeled honestly). Also **Noeta: Trace Request Path** in the
    palette for the active file's whole architectural surface.
  - **Architecture sidebar** — the Noeta activity-bar view lists every `@role` and its bearers,
    unfolding each function's calls lazily; jump to source, or use the context menu for the full
    trace, the call hierarchy, or a focused profile run.
- **Testing** — `@test` fns appear in VS Code's Test Explorer with run arrows in the editor
  gutter (discovered by the compiler's own tier walk); runs use `noeta test --json` under the
  hood, so a failure shows its assertion message and captured output. `#[Skip]`/`#[Name]`/
  `#[Group]` are honored, and single tests run with `--name`.
- **Debugging** (`noeta dap`) — run a `.noe` file under the compiler's own bytecode VM (JIT off, so
  every frame is inspectable) through VS Code's debug UI:
  - **Breakpoints** — click the gutter of any executable line; **stepping** — step over / into / out,
    line by line.
  - **Call stack, scopes, and variables** — each paused frame's locals with their values and types,
    read straight from the live VM.
  - Press **F5** on a `.noe` file to debug it (no `launch.json` needed); output appears in the Debug
    Console. See **Debugging** below.
- **Profiling** (`noeta profile`) — profile a `.noe` file and read the result without leaving the
  editor:
  - **Noeta: Profile File (Sampling)** — in the run-button dropdown or the command palette — runs
    the wall-clock sampling profiler and opens an interactive **flame graph**: click to zoom (with
    breadcrumbs, `esc` resets), hover for sample counts, double-click or ctrl/cmd+click a frame to
    jump to its source line. A **Functions** tab shows the sortable per-function table (self/total
    samples, hottest line).
  - **Hot-line annotations** — after a sampling run, the profiled sources show each hot line's
    share of samples inline (`▕ 12.4%`), cleared when you edit the file or with **Noeta: Clear
    Profile Line Annotations** (`noeta.profile.lineAnnotations` turns them off).
  - **Noeta: Profile File (Instrumenting)** — exact per-function call counts and self/total time
    in the same view.
  - **Profile slices** — "Profile Focused on This Function" (Architecture view context menu)
    re-roots every sample stack at that function, so the flame graph shows only the part of the
    run you care about; a bar reports the slice's share and one click restores the whole run.
  - The program's own output streams to the **Noeta Profile** output channel. The view opens any
    `*.noeprof.json` artifact — standard speedscope JSON (a CLI-made profile drops right in, and
    the same file still loads at [speedscope.app](https://www.speedscope.app)) or the
    instrumenting profiler's JSON. Sampling rate: `noeta.profile.hz`.
- **AI agents** (`noeta mcp`) — on VS Code 1.101+ the extension registers the compiler's Model
  Context Protocol server with the editor, so an agent (Copilot agent mode and friends) can search
  the language docs and examples, `check` code, navigate symbols with the same engine the language
  server uses, inspect the AST/bytecode/reflection manifest, run code in a sandbox, and drive
  interactive debug sessions — all ground truth from the compiler, discovered automatically.
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

## Text tiers and embedded languages

A text tier's `@<name> { … }` body is verbatim prose the compiler never lexes as code. The built-in
`doc` tier is handled by this grammar: the body scopes as `meta.embedded.block.markdown` (markdown
highlighting via the built-in grammar), brace counting matches the compiler exactly (braces nest;
`\{`/`\}` are literal braces, `\\` a literal backslash), and prose punctuation can no longer leak
string scopes into the code below the block.

A **third-party package** declaring its own text tier (`@tier(spec, text: "xml")`) ships a VS Code
extension with one *injection grammar* targeting `L:source.noeta` — a `begin`/`end` rule of the same
shape as this grammar's `text-tier-blocks` (copy it, swap the tier name and the embedded-language
include). That is the whole mechanism: no cooperation from this extension needed, and the tier's
bodies highlight in its declared language for every user of the package's extension.

## Roadmap

- **`noeta lsp`** — the language server over the compiler's salsa query graph is **wired in**: live
  diagnostics, hover, go-to-definition, find references, rename, signature help, semantic highlighting,
  document outline, and completion — most working across modules.
- A tree-sitter grammar for editors outside the TextMate ecosystem (Neovim, Zed, Helix).

See `docs/Editor-and-AI-Tooling.md` in the repository for the full plan.
