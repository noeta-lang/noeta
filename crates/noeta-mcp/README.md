# noeta-mcp

`noeta mcp` — the Model Context Protocol server: Noeta's agent-native tooling adapter.

- **Takes in:** MCP tool calls over stdio (via the official `rmcp` SDK).
- **Emits:** structured tool results — diagnostics, docs, examples, stdlib signatures, navigation, execution, and debug results — an AI agent can act on directly.

This is the third leg of the editor-tooling story. Where `noeta lsp` is a *read* adapter over the compiler's salsa query graph (for a human at a cursor) and `noeta dap` is a *control* adapter over the running VM (for a human debug UI), `noeta mcp` is the adapter for an AI agent — a consumer that addresses code by name/snippet, has ~zero Noeta in its training data, and lives in a tight "does this compile, what's wrong, what does `E0007` mean" loop. It spans several pillars: Ground (`docs_search`/`docs_get`, `examples_find`), Understand (`check`, `explain_diagnostic`, `type_at`, `symbols`), Introspect (`stdlib_api`, `ast`, `module_graph`, `reflect`), Navigate (`definition`/`references`/`completions`/`signature`, riding the same `noeta-ide` engine the LSP serves), Execute (`run`/`eval`/`test` against the VM through the `Debugger` seam, `format` wrapping `noeta-fmt`), and debug (`debug_*` sessions compiling with debug info through the live session compiler). Diagnostics use the `schema` feature of `noeta-diagnostics` so `check` returns the same canonical JSON shape as `noeta check --format json` — agent and CLI never disagree.

## One request, one whole program

Every tool taking a `source`/`file` pair funnels through `resolve_workspace` → `ResolvedWorkspace::workspace`, the single place a request becomes a salsa `Workspace`. A `file` pulls in the entry's sibling `.noe` modules **and** the packages its `noeta.toml` depends on (the read-only *query* resolution — answering a question must never rewrite `noeta.lock`), each source analyzed under its own package's edition and keyed by the loader's canonical `SourceId` ordering so a dependency-module span locates to its real file.

Funnelling matters because the failure mode is silent: while the workspace was siblings-only, `check` reported errors on programs `noeta run` compiles cleanly, and `reflect` listed a dependency's attribute on its target while reporting no role for it — the `@role` tag lives in the package, which was never linked.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
