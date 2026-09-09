# noeta-ide

The shared Noeta IDE engine (MCP arc, slice M5 — extracted from `noeta-lsp`).

- **Takes in:** open buffers over a `LangDatabase` (`noeta-db`'s salsa query graph), grouped into a [`Workspace`] per directory.
- **Emits:** every editor-facing language feature with **no wire protocol**: live diagnostics, hover types, go-to-definition, find-references, rename, document symbols, signature help, semantic tokens, completion (member/bare-dot/type-position/identifier), inlay type hints, formatting, and call hierarchy.

The [`DocumentStore`] owns the database, the open buffers, and one `Workspace` per directory with an open document — the directory's `.noe` members plus resolved dependency packages, shared by every open document in it. Each document reads its merged program through the entry-parametric `linked_from` query family (memoized per workspace/document), so per-file lex/parse work memoizes once no matter how many documents are open; editing a document calls salsa's `set_text` setter and salsa recomputes only what the edit invalidated — an incremental spine inherited wholesale, not rebuilt here. It's deliberately wire-protocol-free — no `tower-lsp`, no `tokio` — so `noeta lsp` (JSON-RPC) and `noeta mcp` (MCP tools) are both thin adapters over this one implementation and can never drift; the engine speaks its own positional types (`Position`/`Range`/`TextEdit`) that are field-compatible with LSP's but owned here.

## The call graph

`callgraph::build` joins the existing indices — `resolve::DefUse` for value uses and member accesses, `resolve::MemberTable` for what each type declares, and the checker's `expr_types` for receiver types — into the graph the `trace` tool walks and the editor's call hierarchy serves.

A **node** is a function-like declaration: a top-level `fn`, a `Type.method` (from the type's body, a standalone `impl Trait for T`, or a trait's default method), anything a tier block declares, and a `fn` nested in another body, named `<enclosing>.<name>`. Tier-block declarations carry the same qualification as their top-level siblings, so one graph speaks one vocabulary.

An **edge** is a use: `call` when the site is followed by `(`, `reference` when the function is passed as a value. Every syntactic call leaves exactly one edge, under one of three labels.

| Label | What it means | Example |
|---|---|---|
| `function` | A node the graph holds; traversable. | `work()`, `c.bump()`, `Counter.new()`, an `@html { … }` block reaching its tier handler |
| `external` | A known callee outside the program, named by its own identity. | `math.sqrt`, `List.len`, `string.split` |
| `dynamic` | Statically unresolvable, named for the report. | a function-typed parameter or a closure-valued field invoked as a call, a trait default method, a receiver naming no imported module |

`CallGraph::lookup_named` resolves a qualified name exactly and a bare one against the declarations that end in it, reporting the candidates when several do.

`tests/graph/<case>/` is the oracle: each case is a small project with an `expect-graph.txt` manifest of every node and labeled edge, rendered by `callgraph::render` and compared byte for byte through the same linked-and-checked pipeline `trace` runs. `NOETA_GRAPH_DUMP=1 cargo test -p noeta-ide --test graph -- --nocapture` prints what a case currently renders, to read before pinning it.

## Ranking the graph

`rank::build` joins the call graph with the `use` import relation into one ranked graph: nodes are the program's functions and its modules, and an edge is a `call`, a `reference` or an `import`, weighted in that order. The import kind carries both halves of the module relation, a `use` edge between two modules and the membership edge between a module and each declaration it holds, so filtering it out leaves the pure call graph.

`rank::context_map` scores that graph from a set of seeds and grows a connected subgraph under a token budget, taking the highest-scoring reachable declaration each step. A declaration nothing connects to a seed is never emitted, and each emitted node carries the edge that reached it. The first seed is emitted whatever it costs, so a small budget still answers.

`Ranker` chooses the scoring. `Ppr` is personalized PageRank teleporting to the seeds, `Degree` is weighted degree in both directions, and `Random` is a splitmix64 draw from a caller-supplied seed. Selection, budget and connectivity are identical under all three, so a comparison between them measures the ranking alone.

## Paths between two declarations

`paths::find` returns the k shortest simple routes from one node to another with Yen's algorithm over hop count, deterministic down to the adjacency order. Labeled leaves are nodes and they are sinks: a route may end on `math.sqrt` or on a dynamic callee, and no route passes through one.

When the search finds more routes than `k`, `PathRanker::Flow` scores each with PathRAG's residue: a unit of resource leaves the source, decays by `DECAY_ALPHA` per hop and splits across the node's out-edges, and a route scores the mean resource over its nodes. Routes below `PRUNE_THRESHOLD` drop, the strongest is never pruned, and the survivors come back ascending so the strongest route is last. `PathRanker::Shortest` takes hop count alone.

## The role graph

`architecture::role_graph` collapses the call graph to its `@role` bearers: a node per bearing declaration, an edge to each nearest bearer it reaches, and every non-bearing declaration between them collapsed away. A connection that exists only through a passed reference is marked, the way the swimlane view draws it dashed. This is a port of the derivation in `editors/vscode-noeta/media/trace.js`, and the two must agree; `bearer_edges_collapse_non_role_intermediates` pins the shape the port has to keep.

`architecture::architecture` quotients that by role: one node per role with its bearers and its in- and out-degree, one edge per role pair with a count and up to `EXEMPLARS` real declaration pairs, the `(declaration, role)` boundary summary, and an `unassigned` count with exemplars for everything bearing no role.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
