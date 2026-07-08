# Changelog

## 0.4.0

Adds **formatting** via the `noeta lsp` server (the same engine as `noeta fmt`):

- **Format Document** and **format-on-save** — reformats the whole file into the canonical style.
  A safety check re-parses the result, so formatting can never change what a program means.
- **Format-on-type** — reformatting a block the moment you type its closing `}` (quiet while the
  code is still mid-typed).
- Both are **on by default** for `.noe` files (`editor.formatOnSave` / `editor.formatOnType`), along
  with 4-space indentation.

## 0.3.0

Adds **debugging** via the `noeta dap` Debug Adapter Protocol server:

- Press **F5** on a `.noe` file to run it under the production bytecode VM (JIT off) through VS Code's
  debug UI — no `launch.json` required. New `noeta` debug type with a `program`/`stopOnEntry` launch
  configuration.
- **Breakpoints** on any executable line (including bare `return` lines), **step over/into/out**, the
  **call stack**, and a **Variables** view with each frame's locals, values, and types — read live
  from the VM. Program `echo` output goes to the Debug Console.
- The debugger uses the existing `noeta.server.path` setting (the same `noeta` binary serves `lsp` and
  `dap`).

Language-server changes since 0.2.0 (all served automatically by `noeta lsp` — no client change):

- **Find references** and **rename** for value *and* member symbols (type-aware, cross-module), with
  `prepareRename` validation.
- **Signature help** for function and method calls, with the active argument highlighted.
- **Semantic highlighting** — compiler-accurate identifier colouring overlaid on the grammar.
- Richer **completion**: member completion on a bare `.` trigger, in-scope locals in whitespace, and
  type names in annotation position.
- Faster editing: `didChange` no longer re-reads sibling files from disk on every keystroke.

## 0.2.0

Wires in the `noeta lsp` language server, adding semantic features on top of the static grammar:

- **Live diagnostics**, **hover types**, **go-to-definition** (locals, parameters, functions, types,
  fields, methods — across modules), a **document outline**, and **completion** (keywords, in-scope
  names, and a receiver type's members after `.`).
- New settings: `noeta.server.path` (where to find the `noeta` executable) and `noeta.trace.server`
  (JSON-RPC tracing). New command: **Noeta: Restart Language Server**.
- Requires the `noeta` toolchain (the server is its `noeta lsp` subcommand) and a one-time
  `npm install` of the client dependency; see the README.

## 0.1.0

First release. Static TextMate grammar and editor configuration for `.noe`:

- Syntax highlighting for the full Noeta surface — keywords, the three string
  forms with `${…}` interpolation, every numeric literal form (decimal, hex,
  octal, binary, floats, `f32` and `i8`…`u64` suffixes), primitive and container
  types, PascalCase user types, `@directive`/tier blocks, `#[attribute]`s, and
  the full operator set (`|>`, `..`/`..=`, `...`, `??`/`??=`).
- Comment toggling, bracket matching, auto-closing pairs, and indentation rules.

No language server yet — semantic features arrive with `noeta lsp`.
