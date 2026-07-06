# Editor & AI Tooling (LSP / MCP)

This page describes where editor integration and the agentic tooling surface stand today, honestly.

> [!IMPORTANT]
> **Neither an LSP server nor an MCP endpoint ships yet.** Both are on the roadmap (milestone M2/M3), not in the current toolchain. There is no `noeta-lsp`, `noeta-server`, or `noeta-mcp` crate, and no `lsp`/`serve` subcommand. Today's tooling is the CLI — [`run`](The-CLI), [`repl`](The-CLI), [`test`](Testing), [`bench`](Benchmarking), [`doc`](Documentation-and-Tiers) — plus a **static syntax-highlighting extension** for VS Code (see below).

## Syntax highlighting (ships now)

A TextMate grammar and VS Code extension live in [`editors/vscode-noeta/`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta). It is **static** — it colorizes source without running the compiler, so it works instantly and offline — and covers the whole surface: keywords, the three string forms with `${…}` interpolation, every numeric literal form, primitive and container types, PascalCase user types, `@directive`/tier blocks, `#[attribute]`s, and the full operator set. Install it by symlinking the folder into `~/.vscode/extensions/`; `editors/vscode-noeta/README.md` has the details and a `sample.noe` that exercises every construct. This is the first editor-tooling slice; the extension is structured to host the `noeta lsp` client when it lands, at which point highlighting and semantics share one extension.

## What exists today

While there is no protocol server, a surprising amount of the *infrastructure* an LSP and an MCP surface would build on is already in place:

- **An incremental query graph.** The whole compiler is a [salsa](Architecture-and-Pipeline) query graph (`tokens → ast → checked → bytecode`), and the module graph is salsa-queried too, so editing one module recomputes only its dependents. This is the same machinery that powers a responsive language server — it exists so that an LSP can be layered on without re-architecting the compiler.
- **Precise, typed diagnostics.** Every error is a typed variant with a stable `E0xxx` code and a source span, rendered in one place. These are exactly the diagnostics a server would forward to an editor.
- **A reflection manifest.** The compiler builds a queryable manifest of a program's declarations, their `#[…]` attributes, and their `@role(…)` semantic tags (see [Attributes & Reflection](Attributes-and-Reflection)). This is the intended backbone of the agentic MCP surface — the plan is for agents to query a labeled architectural graph (roles, boundaries, data flows) through MCP tools.

## The vision (roadmap)

The design intent, not yet shipped:

- **Embedded LSP** — completion, go-to-definition, hover types, and live diagnostics driven by the salsa graph, so an edit re-checks only what changed.
- **Agentic MCP surface** — tools that let an AI agent query the program's semantic-role graph (`@role`/`@semantic` tags): which declarations are entry points, trust boundaries, persistence boundaries, sinks, or layers, and how data flows between them. The reflection manifest and the semantic-role vocabulary already exist; what is missing is the server that exposes them over the protocol.
- **Editor grammars and a VS Code extension** — the VS Code (TextMate) grammar and a [tree-sitter grammar](https://github.com/noeta-lang/noeta/tree/main/editors/tree-sitter-noeta) (for Neovim/Zed/Helix) now both exist; still to come is folding the LSP client into the VS Code extension (M3). The tree-sitter grammar parses ≈99% of the conformance corpus and models Noeta's newline-terminated statements and case-insensitive identifiers faithfully (see its README).

When the remaining pieces land, this page will document them. Until then, treat any mention of "the LSP" or "MCP tools" in the design documents as forward-looking.

## Using `noeta` with an editor now

In the meantime:

- Point your editor's build/run task at `noeta run <file>`; diagnostics print with source spans and stable codes.
- `noeta doc` extracts your `@doc { … }` prose as Markdown — usable in a docs pipeline today.
- `noeta dump <file>` prints the VM bytecode a program compiles to — useful for an agent (or human) reasoning about *what actually runs*: which opcodes a construct lowers to, whether a reuse/in-place fast path fired, how names and constants are laid out. See [The CLI](The-CLI#noeta-dump).
- For **VS Code**, install the bundled extension in `editors/vscode-noeta/` for proper `.noe` highlighting. For **Neovim / Helix / Zed**, point the editor at the tree-sitter grammar in `editors/tree-sitter-noeta/` (run `tree-sitter generate` there first).
