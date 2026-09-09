# Editor & AI Tooling

Noeta's editor story ships in four layers, all in-tree: **static syntax highlighting** (TextMate and tree-sitter grammars), a **language server** (`noeta lsp`), a **debugger** (`noeta dap`, covered on [Debugging](Debugging)), and an **agent surface** (`noeta mcp`), a Model Context Protocol server that hands AI coding agents the same compiler ground truth the editor gets.

## Installing, per editor

### VS Code / VSCodium

The [**Noeta** extension](https://marketplace.visualstudio.com/items?itemName=noeta.noeta) on the Visual Studio Marketplace bundles everything on this page: the static TextMate grammar, the language server, the debugger type, the profiler view, and MCP auto-registration. Its source is [`editors/vscode-noeta/`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta).

1. Run [`noeta ide --vscode`](The-CLI#noeta-ide). It downloads the `.vsix` matching your toolchain's version from the GitHub release, verifies it against the release's `SHA256SUMS`, and installs it into the first of `code`, `codium` or `code-insiders` on your PATH. Pick one explicitly with `--bin <NAME|PATH>`. Installing from the [Marketplace listing](https://marketplace.visualstudio.com/items?itemName=noeta.noeta) works as well, and the CLI path is what pins the extension to the toolchain you are running, so it is also what covers VSCodium and offline installs.
2. Open a `.noe` file. Highlighting is immediate, and the extension starts `noeta lsp` automatically; set `noeta.server.path` if the binary is not on your PATH.
3. After a `noeta upgrade`, re-run `noeta ide --vscode` so the extension moves in step with the toolchain.

From a source checkout, symlink the folder into `~/.vscode/extensions/` instead, which works identically for VSCodium. The extension's README has the details and a `sample.noe` exercising every construct.

The TextMate grammar is **static**: it colorizes without running the compiler, instantly and offline. It covers keywords, the three string forms with `${…}` interpolation, every numeric literal form, primitive and container types, PascalCase user types, `@directive` and tier blocks, `#[attribute]`s, and the full operator set.

### Neovim, Helix, Zed

These editors wire the two pieces, grammar and language server, with their own mechanisms.

1. Clone the [tree-sitter grammar](https://github.com/noeta-lang/noeta/tree/main/editors/tree-sitter-noeta) and run `tree-sitter generate` in it; the generated parser is not committed. It is built from the real lexer and parser surface, models Noeta's newline-terminated statements and case-insensitive identifiers, and is validated against the language's conformance corpus. Its README records the coverage it reaches over the repository's own `.noe` files and the constructs outside it.
2. Register the grammar for the `.noe` extension the way your editor takes a local tree-sitter grammar. Neovim wants an nvim-treesitter parser config entry; Helix wants a `[[grammar]]` with a path source plus a `[[language]]` entry in `languages.toml`; Zed wants a local extension wrapping the grammar.
3. Point your editor's LSP client at `noeta lsp` over stdio for `.noe` files. Diagnostics, hover, completion, and the rest of the [server's feature table](#the-language-server-noeta-lsp) work in any LSP client.
4. On Neovim, optionally wire `noeta dap` into nvim-dap for debugging; the config snippet is on the [Debugging](Debugging) page.

For a project with third-party text tiers (`@tier(<name>, text: "<lang>")`), `noeta grammar tree-sitter --out <dir>` emits a per-project overlay so those `@<name> { … }` bodies parse and highlight as their language. The static grammar's `@doc` to markdown rule is the fallback.

## The language server (`noeta lsp`)

`noeta lsp` speaks LSP over stdio, and the VS Code extension starts it automatically for `.noe` files. It is a thin adapter over the compiler's incremental [salsa query graph](Architecture-and-Pipeline) (`tokens → ast → checked → bytecode`, plus the module graph), so an edit re-checks only what changed and the diagnostics you see are the *actual compiler's* diagnostics, live.

| Feature | Notes |
|---|---|
| **Live diagnostics** | Every `E0xxx` with its span, on every keystroke (incremental `didChange`), including **inside `@test`/`@bench`/`@debug` blocks**. Each is checked as the shape its own build compiles, exactly as [`noeta check`](The-CLI#noeta-check) does, so a tier body's type error underlines where you wrote it. |
| **Project diagnostics** | The whole workspace rather than only the open files: the server answers `workspace/diagnostic` by running the same project walk `noeta check` runs, with your unsaved buffers overlaid on the files on disk. A fault in a module nobody has opened reaches the problems panel. It is a *pull*, asked for on idle rather than on every keystroke, so the per-edit path stays as narrow as it was. |
| **Hover** | What the cursor is on decides what you get; see [What hover answers](#what-hover-answers) below. |
| **Go to definition** | Cross-module: a name defined in an imported module resolves to that file. |
| **Find references / rename** | Including struct and class **members**. Rename is prepare-checked, so what is not renameable cannot be renamed. |
| **Completion** | Identifiers in scope, members after `.` (including the bare-dot and mid-whitespace trigger positions), and **type positions**: annotations and signatures. |
| **Signature help** | Parameter hints while typing a call, for free functions and methods. |
| **Document outline** | Types, functions and methods, for the breadcrumb and symbol views. |
| **Semantic tokens** | Compiler-accurate token coloring layered over the static grammar. |
| **Inlay hints** | rust-analyzer style: the inferred type of every un-annotated binding (`mut xs`&nbsp;`: List<int>`&nbsp;`= …`) and of inference-typed closure parameters, plus parameter **names** at call sites. Types show their in-scope short name, and hover keeps the fully-qualified identity. Packed storage is marked compactly on the label (`: Vec3 · packed`, `: List<Cell> · SoA`). Annotated bindings, reassignments, and same-named arguments show nothing. Toggle with VS Code's `editor.inlayHints.enabled`. |

### What hover answers

| Cursor on | You get |
|---|---|
| A **callable's name** | Its whole declaration (`fn add(a: int, b: int): int`), at a call site as well as at the declaration, so you read the parameters rather than the call's result type alone. |
| A **type name** | Its declaration: fields or variants, and method signatures. |
| An **embedded-language tier** name (`@sql { … }`) | A description of its body: the declared language, and for an expression tier its value type. |
| A **decorator directive** (`@role`, `@packed`, `@derive`, …) | A description in place. |
| Inside a **`use`** | Imported items and module path segments both resolve, and a **namespace group** (`http` from `use std.http`) lists its members. |
| Anything else | The inferred static type of the expression, in surface syntax (`List<int>`, `Result<Order, OrderError>`). |

Non-default storage adds a fact line: a `@packed` type shows `@packed — 12 bytes`, and a `List<packed>` shows `flat packed storage — 12 bytes/element, row-major`, or `column-major (SoA)` for `@packed(Layout.Column)`. Whatever matched, the declaration's `@doc` prose follows after a rule.

### Under load

Every open document in a directory shares one salsa workspace, so a file is parsed once however many tabs are open. The expensive requests (diagnostics, semantic tokens, completion) run off the message loop and are **cancelled when a newer edit supersedes them**: a stale computation answers `ContentModified` and the client silently re-asks. A burst of keystrokes produces one diagnostics publish for the final text.

The same salsa graph powers the [debugger](Debugging)'s launch compile and the conformance harness, so all three tools read one source of truth.

## Debugging

`noeta dap` is a full DAP server: breakpoints, line-granular stepping, stack, scopes and variables, and a debug console that is a REPL over the paused program, closures included. It debugs the **production VM**, same bytecode, JIT unarmed. See [Debugging](Debugging).

## Tracing the architecture

`@role` is the decorator that confers a typed architectural role (entry point, handler layer, persistence boundary, and so on) on the declarations an attribute annotates; see [Attributes & Reflection](Attributes-and-Reflection). Every `@role`-bearing declaration gets a **CodeLens** reading `⚑ Layer.Handler · trace call paths`.

Running that lens opens the **trace view**, as do **Trace Call Paths from Here** in the Architecture sidebar's context menu and **Noeta: Trace Call Paths** in the palette for the whole role surface. The view renders the same static call-graph walk `noeta mcp`'s `trace` tool serves.

The layout is a role-colored **boundary rail** over collapsible call trees whose indent rails are tinted by role, so the layers read as colored bands. Each pill in the rail is a toggle: click to highlight every path reaching that boundary, which mutes the other pills, and double-click to jump to it. Every row jumps to source.

The walk's honesty markers stay visible. Dynamic and external callees are dimmed, *passed-as-value* references are badged because a callback registration is part of the flow without being a syntactic call, recursion is marked, and truncation is explicit. Trivial low-level calls sit behind a toggle so the architectural shape stays foregrounded.

The header's **Tree | Lanes** switcher turns the same trace into the **swimlane view**: the call trees collapsed to the *role graph*, one column per role, a card per role-bearing function, and edges connecting each bearer to the nearest bearers it reaches. Non-role intermediate calls collapse away, and a connection that exists only through a passed-as-value chain renders dashed. It is the layered architecture diagram, Handler to Service to Store as columns, derived from the code. The boundary rail filters here too, dimming everything except an active boundary's upstream cards and edges.

## Profiling

`noeta profile` reports where a program spends its time: a wall-time **flamegraph** by sampling, the exact call-count and self-time table plus an exact call-tree flamegraph with `--instrument`, or the bytes-weighted **memory flamegraph** with `--alloc`. It runs the same production VM at tier-0 and, like the debugger, sits outside the differential oracle.

The VS Code extension renders profiles **in-editor**: an interactive flame-graph view with click-to-source, a sortable function table, hot-line annotations in the source itself, and a **thread picker** when the run spawned isolates. All three modes are commands, **Noeta: Profile File (Sampling / Instrumenting / Allocations)**, also in the run-button dropdown. See [Profiling](Profiling).

## The agent surface (`noeta mcp`)

`noeta mcp` is a **Model Context Protocol** server over stdio: the compiler's adapter for an AI coding agent, the way `noeta lsp` is its adapter for an editor and `noeta dap` for a debug UI. Agents have little Noeta in their training data, so the server's first job is **grounding**: every answer comes from the real compiler, its real documentation, and its CI-tested examples.

Register it with an agent, Claude Code for instance:

```sh
claude mcp add noeta -- noeta mcp
```

In **VS Code 1.101+** the Noeta extension registers the server automatically through the editor's MCP provider API, so agents running in the editor discover it with no configuration. The extension's `noeta.server.path` setting points at the binary for `lsp`, `dap`, and `mcp` alike.

### Node identity

Every graph tool emits an `id` on each node, and two tools' answers join on it exactly.

| Field | What it holds |
|---|---|
| `name` | The post-link name, namespace-qualified in a package (`app.main.handle`), `Type.method` for a method. What `trace`, `impact` and `callers` are addressed by. |
| `kind` | `function`, `method`, `struct`, `class`, `enum`, `variant`, `field`, `trait`, `impl`, `module`, `external`, `dynamic`, `unresolved`. |
| `file` | The declaring file, relative to the project root. `null` for a callee with no declaration here. |
| `span` | The declared name's `start`/`end` byte offsets plus its 1-based `line` and `column`. `null` alongside a `null` file. |

`(file, span.start, span.end)` is the join key: it is the declaration's name span, which is what the engine itself keys nodes by. `symbols`, `trace`, `reflect`, `module_graph`, `definition`, `references`, `impact` and `callers` all carry it.

```json
{ "name": "joined.alpha.shared", "kind": "function", "file": "src/alpha.noe",
  "span": { "start": 7, "end": 13, "line": 1, "column": 8 } }
```

### Ground

Orient before writing a line.

| Tool | Answers |
|---|---|
| `docs_search` / `docs_get` | Search and read this documentation. Also exposed as MCP *resources*. |
| `code_search` | The **project's own declarations**, ranked against a name (`place_order`), a qualified path (`orders.place_order`), or a plain sentence (`where does an order get written to the database`). It indexes every declaration of the linked workspace — functions, methods, types, variants, fields, traits, and whatever a `@test`/`@bench` block declares — over its name, its qualified path, its `@doc` prose, its signature, its `@role` bindings and attributes, its file path, and the identifiers its body mentions. `matched_fields` names the fields that earned each hit, `kind` and `roles` narrow the set, and every result carries an `id` the graph tools accept. |
| `examples_find` | CI-tested example programs by feature, concept, or diagnostic code. |
| `stdlib_api` | The real standard-library signatures, from the compiler's own registry. |
| `explain_diagnostic` | What an `E0xxx` means and how to fix it, from the compiler's explanation catalog (the text [`noeta explain`](The-CLI#noeta-explain) prints), with real programs that trigger it. |
| `project_docs` | The *project's own* `@doc { … }` blocks, each resolved to what it documents. |
| `doc_browse` | The navigable tree the editor's docs browser shows: root, modules, declarations, members. |
| `doc_page` | One node's signature and prose. |

The three project-documentation tools work from a parse alone, so they read work-in-progress code. They are distinct from `docs_search`, which reads this language guide.

`code_search` is the first call when the question names no symbol. It reports `linked` and `link_diagnostics` the way the graph tools do, and under a failed link it ranks the entry file's own parse, where names are unqualified.

### Understand

The compiler's semantic answers.

| Tool | Answers |
|---|---|
| `check` | Type-checks code, returning the same JSON diagnostics `noeta check --format json` emits, over the same shapes: once as the source ships, then once per dev-tier block it declares, with `tiers_checked` naming which. A `@test` body that does not compile is an error here rather than a surprise at `noeta test`. `file` takes a **project directory** as readily as a single `.noe`, running the same walk `noeta check` runs, so the agent and the command line cannot disagree about whether a project is clean. |
| `type_at` | The inferred type at a symbol or position, plus a `layout` storage fact for `@packed` and flat-list types, worded as editor hover words it. |
| `symbols` | A declaration outline, carrying each node's `@role` bindings and, for a declaration written inside a `@test`/`@bench` block, its `tier`. `scope: "workspace"` outlines every module of the project; the default `scope: "file"` outlines the entry alone. |
| `definition` / `references` / `completions` / `signature` | Navigation over the **same `noeta-ide` engine the language server serves**, so agent and editor cannot disagree. A `file` entry resolves cross-file through sibling modules and dependency packages. `definition` and `references` also take a `symbol` the entry file does not contain, resolving it across the workspace by post-link name or unique leaf; a leaf naming several declarations comes back as a `candidates` list. |

### Introspect

The compiler's artifacts.

`trace`, `reflect` and `module_graph` read the merged workspace program, and each reports a `linked` flag with `link_diagnostics` in `check`'s JSON shape. Under a failed link the answer comes from the entry file's own parse: names lose their qualification, so `trace` marks every node `unverified`, and a callee that names one of the project's own modules is reported with `id.kind` of `unresolved` rather than as an external target.

| Tool | Answers |
|---|---|
| `ast` / `bytecode` / `pipeline` / `module_graph` | The syntax tree, the VM disassembly, a per-stage health summary, and the `use` import graph. A module node's `namespace` is the module path its file's location derives, each import edge carries the `targets` it resolves onto, and a dependency package's imported module is a node marked `external` with its `package`. |
| `reflect` | The [attributes and `@role` reflection manifest](Attributes-and-Reflection): which declarations are entry points, trust boundaries, persistence boundaries, sinks, or layers, each with its source location and joinable with every other tool. A role conferred by a **dependency package's** `@role`-bearing attribute is indexed exactly like one declared in the file at hand, and the answer matches what `roles_of()` gives in-language. Tagging a package's tool attribute `@role(Semantic.TrustBoundary)` is what makes "what can a language model reach in this program?" answerable off the architecture graph. |
| `trace` | The **static call path from a role or a function**. `trace(from: "EntryPoint")` starts at every function bearing the role and walks the call graph; `from` also takes a function name, either qualified (`App.Lib.add`) or as the bare name `symbols` reports (`add`, `Counter.bump`). A name several functions carry comes back with the candidates, and an unknown one names the closest declarations. Each node is a function with its own roles, declaration and call sites; external calls and dynamic callees are labeled leaves, and passed-function references such as handler registrations are followed as `reference` edges. A function reached a second time is marked `shared` rather than unfolded again, so the answer grows with the reachable functions rather than with the paths through them. The `boundaries` summary answers which persistence and trust boundaries the entry point reaches. |
| `callers` | The same graph walked **backwards** from a declaration: level 1 is its direct callers, level 2 theirs, up to `depth` (default 3, max 16). Each edge carries the using declaration's `id`, the node it uses, the call site, and whether the use is a `call` or a `reference`. A use written in a module's top-level statements is reported as that module. |
| `impact` | What breaks if a declaration changes: the reverse transitive closure over the project's call graph. Address the change by `symbol` or by an `edit` (`edit_file` plus `new_source`, diffed in memory against what is on disk; nothing is written). Returns the impacted declarations with their ids and the `@test`/`@bench` functions among them, or `attributed: false` with the reason an unattributable edit forces a full rerun. |
| `path` | **How one declaration reaches another**: the k shortest chains of calls and passed references between `from` and `to` (default 3, max 10), each route listing its nodes with their ids, the edge kind of every hop, and the call site the hop is written at. A route may end on an external or dynamic callee (`math.sqrt`, a closure-valued field) and never runs through one. When the graph offers more routes than `k`, they are ranked by the resource each carries, decayed at every hop and split across a node's callees, so a route through a wide dispatcher ranks below a direct one; the survivors are emitted weakest first, putting the strongest route last. `ranker: "shortest"` orders by hop count instead. |
| `context_map` | **The declarations worth reading around a set of seeds, under a token budget.** Seed with declaration names, files, module paths, or a `@role` (which seeds every bearer); the call graph and the `use` import graph are ranked from there with personalized PageRank, and the strongest connected declarations are emitted as signatures grouped by file until `budget_tokens` runs out (default 4096, counted as characters over four). Each node carries its `id`, its `rank` and `score`, and the `via` edge that pulled it in, and every node in the map is connected to a seed. `ranker` swaps the ranking for `degree` (importance blind to the seeds) or `random`, which is what the alternatives are for: measuring the default against them. |
| `architecture` | **The project as its role graph**: one node per `@role` bound anywhere in it, one edge per pair of roles, aggregated from the call graph with every declaration bearing no role collapsed out of the edges. Each edge carries how many declaration-to-declaration connections it stands for and a few of them by name, and an edge reached only through passed references is marked. `connections` carries the graph the aggregation was taken over, one entry per pair of role-bearing declarations joined by a chain through declarations bearing no role, so a reader can ask which boundaries one entry point reaches. Reports every `(declaration, role)` binding, and for declarations bearing no role at all, how many there are and the most-connected of them. Read it before `trace`, which unfolds one entry point of the same graph. |

### Execute

Run and observe.

| Tool | Answers |
|---|---|
| `run` / `eval` / `test` | Runs a program (stdout, exit code, traceback), evaluates an expression (value and type), or runs `@test` blocks. **Sandboxed and deterministic by default**: in-memory fs, logical clock, seeded random, with `real: true` opting into the real host. All three are bounded by liveness limits, so a runaway loop is stopped in-VM rather than hanging: `run` reports `limit_hit`, `eval` returns with `limit_hit` set, and a spinning `test` case fails with `limit_hit`. Every bound is defaulted and tunable per call through `limits`. |
| `debug_start` / `debug_inspect` / `debug_step` / `debug_eval` / `debug_stop` | Interactive debug sessions over the VM's own debugger seam: pause at entry or at breakpoints, read the call stack and live locals, step by line, and evaluate expressions in a paused frame, type-checked against the program first. A runaway `continue` lands in an inspectable `limit` **pause**, so an agent chasing an infinite loop sees the live counter mid-spin. |

### Transform

| Tool | Answers |
|---|---|
| `format` | Canonical formatting, the same engine as `noeta fmt`. Declines on unparseable source. |

Every tool that takes a `file` analyzes **the whole program**: the entry, its sibling modules, and the packages its `noeta.toml` depends on, each under its own language edition. Dependency resolution is a read-only query, so asking a question never rewrites `noeta.lock`.

A failing tool fails **one request, not the session**. An internal error comes back as a JSON-RPC error naming the tool, and the server keeps serving, so you retry it or ask something else without reconnecting.

`noeta dump <file>` is useful to an agent or a human alongside these: it prints the exact VM bytecode a program compiles to, which fast paths fired, and how names and constants are laid out. See [The CLI](The-CLI#noeta-dump).
