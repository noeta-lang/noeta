# Editor & AI Tooling

Noeta's editor story ships in four layers, all in-tree: **static syntax highlighting** (TextMate and tree-sitter grammars), a **language server** (`noeta lsp`), a **debugger** (`noeta dap`, covered on [Debugging](Debugging)), and an **agent surface** (`noeta mcp`), a Model Context Protocol server that hands AI coding agents the same compiler ground truth the editor gets.

## Installing, per editor

### VS Code / VSCodium

The extension in [`editors/vscode-noeta/`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta) bundles everything on this page: the static TextMate grammar, the language server, the debugger type, the profiler view, and MCP auto-registration.

1. Run [`noeta ide --vscode`](The-CLI#noeta-ide). It downloads the `.vsix` matching your toolchain's version from the GitHub release, verifies it against the release's `SHA256SUMS`, and installs it into the first of `code`, `codium` or `code-insiders` on your PATH. Pick one explicitly with `--bin <NAME|PATH>`. The release asset is the extension's distribution channel, so this path covers VS Code, VSCodium, and offline installs alike.
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

### Ground

Orient before writing a line.

| Tool | Answers |
|---|---|
| `docs_search` / `docs_get` | Search and read this documentation. Also exposed as MCP *resources*. |
| `examples_find` | CI-tested example programs by feature, concept, or diagnostic code. |
| `stdlib_api` | The real standard-library signatures, from the compiler's own registry. |
| `explain_diagnostic` | What an `E0xxx` means and how to fix it, from the compiler's explanation catalog (the text [`noeta explain`](The-CLI#noeta-explain) prints), with real programs that trigger it. |
| `project_docs` | The *project's own* `@doc { … }` blocks, each resolved to what it documents. |
| `doc_browse` | The navigable tree the editor's docs browser shows: root, modules, declarations, members. |
| `doc_page` | One node's signature and prose. |

The three project-documentation tools work from a parse alone, so they read work-in-progress code. They are distinct from `docs_search`, which reads this language guide.

### Understand

The compiler's semantic answers.

| Tool | Answers |
|---|---|
| `check` | Type-checks code, returning the same JSON diagnostics `noeta check --format json` emits, over the same shapes: once as the source ships, then once per dev-tier block it declares, with `tiers_checked` naming which. A `@test` body that does not compile is an error here rather than a surprise at `noeta test`. `file` takes a **project directory** as readily as a single `.noe`, running the same walk `noeta check` runs, so the agent and the command line cannot disagree about whether a project is clean. |
| `type_at` | The inferred type at a symbol or position, plus a `layout` storage fact for `@packed` and flat-list types, worded as editor hover words it. |
| `symbols` | A file's declaration outline, carrying each node's `@role` bindings. |
| `definition` / `references` / `completions` / `signature` | Navigation over the **same `noeta-ide` engine the language server serves**, so agent and editor cannot disagree. A `file` entry resolves cross-file through sibling modules and dependency packages. |

### Introspect

The compiler's artifacts.

| Tool | Answers |
|---|---|
| `ast` / `bytecode` / `pipeline` / `module_graph` | The syntax tree, the VM disassembly, a per-stage health summary, and the `use` import graph with each module labeled by the `@role` bindings it declares. |
| `reflect` | The [attributes and `@role` reflection manifest](Attributes-and-Reflection): which declarations are entry points, trust boundaries, persistence boundaries, sinks, or layers, each with its source location and joinable with every other tool. A role conferred by a **dependency package's** `@role`-bearing attribute is indexed exactly like one declared in the file at hand, and the answer matches what `roles_of()` gives in-language. Tagging a package's tool attribute `@role(Semantic.TrustBoundary)` is what makes "what can a language model reach in this program?" answerable off the architecture graph. |
| `trace` | The **static call path from a role**. `trace(from: "EntryPoint")` starts at every function bearing the role and walks the call graph; each node is a function with its own roles, declaration and call sites. External module calls and dynamic callees are labeled leaves, and passed-function references such as handler registrations are followed as `reference` edges. The `boundaries` summary answers which persistence and trust boundaries the entry point reaches. |

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
