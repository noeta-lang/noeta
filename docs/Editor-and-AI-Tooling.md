# Editor & AI Tooling (LSP / MCP)

This page describes where editor integration and the agentic tooling surface stand today, honestly.

> [!IMPORTANT]
> **Neither an LSP server nor an MCP endpoint ships yet.** Both are on the roadmap (milestone M2/M3), not in the current toolchain. There is no `lang-lsp`, `lang-server`, or `lang-mcp` crate, and no `lsp`/`serve` subcommand. Today's tooling is the CLI: [`run`](The-CLI), [`repl`](The-CLI), [`test`](Testing), [`bench`](Benchmarking), and [`doc`](Documentation-and-Tiers).

## What exists today

While there is no protocol server, a surprising amount of the *infrastructure* an LSP and an MCP surface would build on is already in place:

- **An incremental query graph.** The whole compiler is a [salsa](Architecture-and-Pipeline) query graph (`tokens → ast → checked → bytecode`), and the module graph is salsa-queried too, so editing one module recomputes only its dependents. This is the same machinery that powers a responsive language server — it exists so that an LSP can be layered on without re-architecting the compiler.
- **Precise, typed diagnostics.** Every error is a typed variant with a stable `E0xxx` code and a source span, rendered in one place. These are exactly the diagnostics a server would forward to an editor.
- **A reflection manifest.** The compiler builds a queryable manifest of a program's declarations, their `#[…]` attributes, and their `@role(…)` semantic tags (see [Attributes & Reflection](Attributes-and-Reflection)). This is the intended backbone of the agentic MCP surface — the plan is for agents to query a labeled architectural graph (roles, boundaries, data flows) through MCP tools.

## The vision (roadmap)

The design intent, not yet shipped:

- **Embedded LSP** — completion, go-to-definition, hover types, and live diagnostics driven by the salsa graph, so an edit re-checks only what changed.
- **Agentic MCP surface** — tools that let an AI agent query the program's semantic-role graph (`@role`/`@semantic` tags): which declarations are entry points, trust boundaries, persistence boundaries, sinks, or layers, and how data flows between them. The reflection manifest and the semantic-role vocabulary already exist; what is missing is the server that exposes them over the protocol.
- **Editor grammars and a VS Code extension** — syntax highlighting and the LSP client (M3).

When these land, this page will document them. Until then, treat any mention of "the LSP" or "MCP tools" in the design documents as forward-looking.

## Using `lang` with an editor now

In the meantime:

- Point your editor's build/run task at `lang run <file>`; diagnostics print with source spans and stable codes.
- `lang doc` extracts your `@doc { … }` prose as Markdown — usable in a docs pipeline today.
- The `.lang` extension has no bundled TextMate/Tree-sitter grammar yet; most editors' generic highlighting handles the C-family surface reasonably.
