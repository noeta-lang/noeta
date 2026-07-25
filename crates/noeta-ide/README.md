# noeta-ide

The shared Noeta IDE engine (MCP arc, slice M5 — extracted from `noeta-lsp`).

- **Takes in:** open buffers over a `LangDatabase` (`noeta-db`'s salsa query graph), grouped into a [`Workspace`] per directory.
- **Emits:** every editor-facing language feature with **no wire protocol**: live diagnostics, hover types, go-to-definition, find-references, rename, document symbols, signature help, semantic tokens, completion (member/bare-dot/type-position/identifier), inlay type hints, formatting, and call hierarchy.

The [`DocumentStore`] owns the database, the open buffers, and one `Workspace` per directory with an open document — the directory's `.noe` members plus resolved dependency packages, shared by every open document in it. Each document reads its merged program through the entry-parametric `linked_from` query family (memoized per workspace/document), so per-file lex/parse work memoizes once no matter how many documents are open; editing a document calls salsa's `set_text` setter and salsa recomputes only what the edit invalidated — an incremental spine inherited wholesale, not rebuilt here. It's deliberately wire-protocol-free — no `tower-lsp`, no `tokio` — so `noeta lsp` (JSON-RPC) and `noeta mcp` (MCP tools) are both thin adapters over this one implementation and can never drift; the engine speaks its own positional types (`Position`/`Range`/`TextEdit`) that are field-compatible with LSP's but owned here.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
