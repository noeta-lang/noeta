# noeta-mcp

`noeta mcp` — the Model Context Protocol server: Noeta's agent-native tooling adapter.

- **Takes in:** MCP tool calls over stdio (via the official `rmcp` SDK).
- **Emits:** structured tool results — diagnostics, docs, examples, stdlib signatures, navigation, execution, and debug results — an AI agent can act on directly.

This is the third leg of the editor-tooling story. Where `noeta lsp` is a *read* adapter over the compiler's salsa query graph (for a human at a cursor) and `noeta dap` is a *control* adapter over the running VM (for a human debug UI), `noeta mcp` is the adapter for an AI agent — a consumer that addresses code by name/snippet, has ~zero Noeta in its training data, and lives in a tight "does this compile, what's wrong, what does `E0007` mean" loop. It spans several pillars: Ground (`docs_search`/`docs_get`, `examples_find`), Understand (`check`, `explain_diagnostic`, `type_at`, `symbols`), Introspect (`stdlib_api`, `ast`, `module_graph`, `reflect`), Navigate (`definition`/`references`/`completions`/`signature`, riding the same `noeta-ide` engine the LSP serves), Execute (`run`/`eval`/`test` against the VM through the `Debugger` seam, `format` wrapping `noeta-fmt`), and debug (`debug_*` sessions compiling with debug info through the live session compiler). Diagnostics use the `schema` feature of `noeta-diagnostics` so `check` returns the same canonical JSON shape as `noeta check --format json` — agent and CLI never disagree.

## One request, one whole program

Every tool taking a `source`/`file` pair funnels through `resolve_workspace` → `ResolvedWorkspace::workspace`, the single place a request becomes a salsa `Workspace`. A `file` pulls in the entry's sibling `.noe` modules **and** the packages its `noeta.toml` depends on (the read-only *query* resolution — answering a question must never rewrite `noeta.lock`), each source analyzed under its own package's edition and keyed by the loader's canonical `SourceId` ordering so a dependency-module span locates to its real file.

Funnelling matters because the failure mode is silent: while the workspace was siblings-only, `check` reported errors on programs `noeta run` compiles cleanly, and `reflect` listed a dependency's attribute on its target while reporting no role for it — the `@role` tag lives in the package, which was never linked.

## One identity across the graph tools

`analyze::NodeId` is the identity every graph answer carries: the post-link `name`, a `kind` from the shared `NodeKind` enum, the declaring `file` relative to the project root, and the declared name's `span` (byte range plus line/column). `symbols`, `trace`, `reflect`, `module_graph`, `definition`, `references`, `impact` and `callers` all emit it, and `(file, span.start, span.end)` joins two answers exactly.

`graph::DeclIndex` is the one walk behind it. It inventories the linked program's declarations under their post-link names, descends into `@tier { … }` blocks, and answers by exact name, by unique leaf (with the candidate list on a tie), and by span. A tool that needs "which declaration is this?" asks it rather than re-deriving an answer.

## Finding the node to start from

`code_search` ranks the project's own declarations against a query that may be a name, a qualified path, or a sentence with no identifier in it. It is the entry to the graph: every other tool here needs an address already, and this is what produces one.

The ranking is `noeta_ide::search`'s BM25F over eight fields — leaf name, qualified path, kind and tier, `@role` bindings and attributes, `@doc` prose, signature, body identifiers, and file path — with an exact-name and a prefix-name boost on top, so typing `place_order` returns the declaration rather than the prose about it. `matched_fields` reports which fields earned a hit, `kind` and `roles` narrow the set before scoring, and the result carries the shared `NodeId`. The index is built per call from the prepared workspace and the whole thing is deterministic: no model, no randomness, ties broken by name.

## Walking the graph backwards

`impact` and `callers` are the reverse direction of the walk `trace` runs forward. `callers` is one hop at a time over `noeta_ide::callgraph`, reporting each use with its site and whether it is a call or a passed reference. `impact` is the transitive closure `noeta test --watch` narrows on, driven through `noeta_ide::impact::ImpactSession`.

The session grew two in-memory doors for it. `impact_of_sources` takes each edited file's new text from the caller instead of reading it back off disk, so an agent can ask what an unsaved edit would break; `impact_of_decls` seeds the closure from declaration names, which is the question an agent holding a name rather than a diff is asking.

## Link status is part of the answer

A graph tool falls back to the entry file's own parse when the workspace does not link, so `analyze::LinkStatus` rides on `trace`, `reflect` and `module_graph` as `linked` plus `link_diagnostics` in `check`'s JSON shape. The fallback changes what the answer means: names lose their qualification and a call into a sibling module resolves to nothing, so `trace` marks every node `unverified` and degrades a callee naming one of the project's own modules to `unresolved` rather than calling it external.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
