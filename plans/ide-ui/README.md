# IDE architecture navigation — call hierarchy, role lenses, trace UI

**Status: U0 done (`722e3a09`); U1 next.** Branch `ide-ui`, worktree `.claude/worktrees/ide-ui`
(rebased onto main `362b4873`, which merged the profiler VS Code UI).

The role-graph work (merged, `fef79e06`) gave agents a role-enriched architectural graph: the
`@role` index with source locations, roles on the `symbols` outline and `module_graph` nodes, the
static call graph (`noeta_ide::callgraph`), and the `trace` tool — `trace(from: "EntryPoint")`
unfolds the full request path with a role-boundaries summary. This arc surfaces the same engine
**to humans in the editor**: navigate the call graph in VS Code's native UI, see roles on the
declarations that bear them, and open an entry point's request trace with one click.

## Reconnaissance (what exists, verified)

- **`noeta_ide::callgraph`** builds the function-level call graph as a join over the existing
  indices (`DefUse::refs()`, member occurrences + `expr_types` + `MemberTable`, decl inventory for
  enclosing-function). Edges carry call sites and a call-vs-reference flag; module calls are
  `External`, closure-valued calls `Dynamic`. Forward edges only today — *incoming* calls are the
  same edge list scanned in reverse (no new analysis).
- **`noeta_ast::reflect`** role/attribute records carry `target_span` (R1), so a role binding maps
  to its declaration site precisely. `DocumentStore` (noeta-ide) already serves every LSP feature;
  the LSP is a thin `tower-lsp-server` adapter.
- **`tower-lsp-server` 0.23** (the pinned version) exposes `prepare_call_hierarchy` /
  `incoming_calls` / `outgoing_calls`, `code_lens` / `code_lens_resolve`, and custom JSON-RPC
  methods — everything below is protocol-supported, no fork needed.
- **The VS Code extension** (`editors/vscode-noeta`, plain JS, 0.5.x) already runs the language
  client, the debugger, and MCP registration. Virtual read-only documents
  (`workspace.registerTextDocumentContentProvider`) and `TreeDataProvider` need no build step.
- **The MCP `trace` tool** renders the same data for agents; the LSP/UI variants must reuse
  `noeta_ide::callgraph` + `noeta_ast::reflect` so editor and agent can never disagree.

## Coordination risk

A parallel session has a **`profiler-vscode-ui`** worktree touching the same extension
(`editors/vscode-noeta`) — and main just gained editor run/build tasks (`82eefbc4`). Before
merging any slice that edits `package.json`/`extension.js`, **rebase onto current main** and
expect both-added contribution-point conflicts (commands, menus). Keep extension diffs small and
additive. *(Resolved for now: the branch is rebased onto the merged profiler UI, `362b4873`.)*

## One extension, one package (profiler-UI unification)

The profiler merge already left the structure right: **one extension**, one `activate()` that
calls `registerDebugging`/`registerTasks`/`registerMcp`/`registerProfiling`, features as sibling
modules (`src/profile.js`), and one `noeta.server.path` setting resolving the binary for LSP, DAP,
tasks, MCP, and profiler alike. The remaining seams were mechanical and are fixed on this branch:

- **`src/toolchain.js`** is now the single `noetaCommand()` resolver (was duplicated in
  `extension.js` and `profile.js` — identical, but one drift away from two settings).
- **Command palette normalization**: every command uses `"category": "Noeta"` (three run/build
  commands had `Noeta:` baked into the title instead — same palette rendering, inconsistent
  everywhere else, e.g. keybinding UI and menus).

Conventions this arc's UI slices follow (and future arcs should too):

1. New client features live in their own module exporting `register<Feature>(context)` (U2 →
   `src/trace.js`), wired from `activate()` — the `registerProfiling` pattern.
2. New commands: `noeta.*` id, `category: "Noeta"`, editor-title placements join the existing
   `noeta@N` / `profile@N` groups rather than inventing new menus.
3. Anything that shells out resolves the binary through `src/toolchain.js`; anything analytical
   goes through the language client (one server, one engine) — never a second `noeta lsp`/`mcp`
   spawn.
4. One version bump + one CHANGELOG entry per merged arc (this arc: 0.6.0 → 0.7.0 at U2, when the
   extension actually changes behavior).
5. If U3's sidebar happens, it becomes the shared "Noeta" view container — the natural future home
   for architecture navigation *and* profiler entry points, but only if the go/no-go says build it.

## Decisions

1. **Native surfaces first, custom UI second.** Call hierarchy and CodeLens render in VS Code's
   built-in UI with zero client-side rendering code; the sidebar tree view comes last, only after
   the native surfaces prove the value in use.
2. **One engine.** Every UI reads `noeta_ide::callgraph`/`reflect` through the `DocumentStore` —
   never a re-implementation in the LSP or the extension. (The M5 lesson: shared engine, thin
   adapters.)
3. **Trace rendering = a virtual document, not a webview.** A read-only
   `noeta-trace:` document with the rendered tree and clickable `file:line` links (DocumentLink or
   plain `path:line` — VS Code linkifies both) beats a canvas for actually navigating code, and
   carries no webview maintenance tax.
4. **Static-analysis honesty carries into the UI.** Dynamic callees and external module calls
   render as labeled leaves; the reference-vs-call distinction shows in the item detail. The tree
   is a navigation aid, never presented as a completeness proof.
5. **Depth/size budgets are the server's job.** The LSP call-hierarchy answers are per-level
   (VS Code expands lazily — no budget needed); the trace document reuses the MCP trace's
   depth-6/16 and 500-node budgets and prints what was truncated.

## Slices

| Slice | Deliverable | Why | How (verified seams) |
|---|---|---|---|
| **U0** | `callgraph` in the `DocumentStore` | Both UIs need per-document graph queries | `DocumentStore` methods `outgoing_calls(uri, position)` / `incoming_calls(uri, position)` / `function_at(uri, position)` over `noeta_ide::callgraph::build` on the workspace's merged program + `linked_checked_ide` expr_types (texts from the salsa inputs). Role lookup joined from `reflect::build`. Unit-tested in noeta-ide like every other feature (the store fixtures are the pattern). |
| **U1** | **LSP call hierarchy** | Navigate the call graph in VS Code's native peek tree (`Shift+Alt+H`) — valuable even ignoring roles; Noeta lacks it today | `prepare_call_hierarchy` (the function decl under the cursor → `CallHierarchyItem`, roles in `detail`, e.g. `Semantic.Persistence`), `incoming_calls`/`outgoing_calls` per level (reverse/forward edge scan), `call_hierarchy_provider: true` capability. External/dynamic callees appear as items with no target range... **decision: skip them in the hierarchy** (LSP items need real locations) — they stay in the trace document (U2). Gate: DocumentStore-level fixtures + one wire-shape test, per the LSP test pattern. |
| **U2** | **Role CodeLens + trace virtual document** | The roles become visible and actionable: `⚑ Semantic.EntryPoint · trace request path` above `fn handle`; click → the rendered trace | LSP `code_lens` over the reflect index (R1 spans place the lens; one lens per role binding in the file). The lens carries a client command `noeta.showTrace` with `(uri, function)` args. Extension: register the command; it calls a **custom LSP request** `noeta/trace` (DocumentStore renders the same tree the MCP tool serves, as markdown/plaintext with `file:line` per node + boundaries summary at top) and opens it via a `noeta-trace:` TextDocumentContentProvider. `code_lens_provider` capability + extension command/contribution (small, additive — see coordination risk). |
| **U3** | **Sidebar “Architecture” view** (assess after U1+U2) | Roles as groups → traces as expandable trees with jump-to-source — the full navigation UI | Extension `TreeDataProvider` fed by custom requests `noeta/architecture` (the role index, grouped) + `noeta/trace` (lazy children per node). Refresh on save. **Go/no-go decided after using U1+U2** — if the peek tree + lens-opened traces cover real navigation, U3 may not pay for its client-side surface area. |

Each slice: green tests + fmt/clippy before commit; extension changes validated with `node --check`
+ manual smoke notes in the commit message (the extension has no test harness).

## Out of scope

- Webview graph canvas (demos well, navigates worse than trees, maintenance tax).
- Dynamic/runtime traces in the editor — that's the OTEL arc's territory (`std.telemetry` server
  traces); this arc is static navigation.
- LSP type hierarchy (a different protocol feature; nothing role-shaped about it today).
