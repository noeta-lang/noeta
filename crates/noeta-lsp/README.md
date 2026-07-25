# noeta-lsp

The Noeta language server (`noeta lsp`).

- **Takes in:** LSP JSON-RPC requests over stdio.
- **Emits:** an LSP `Backend` (via `tower-lsp-server`) — diagnostics, hover, go-to-definition, references, rename, symbols, signature help, semantic tokens, completion, inlay hints, and formatting.

A thin wire adapter over the shared IDE engine (`noeta_ide::DocumentStore`) — since MCP slice M5, every language feature lives in `noeta-ide`, where `noeta mcp` reads the same implementation. This crate owns only what is LSP-specific: the `tower-lsp` transport and lifecycle, position-encoding negotiation, and mechanical conversions between the engine's positional types and their `ls_types` wire counterparts (field-compatible by construction). The `Backend` holds the store behind a `Mutex`; most handlers lock it, do fast synchronous salsa work, and release before awaiting client I/O. The three expensive paths — diagnostics publish, semantic tokens, completion — instead run over a `DocumentStore::snapshot` on a blocking thread, so a newer edit cancels an in-flight run (salsa unwinds it) rather than queueing behind it.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
