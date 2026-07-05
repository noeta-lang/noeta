# Changelog

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
