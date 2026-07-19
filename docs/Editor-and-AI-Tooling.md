# Editor & AI Tooling

Noeta's editor story ships in four layers, all in-tree: **static syntax highlighting** (TextMate +
tree-sitter grammars), a **language server** (`noeta lsp`), a **debugger** (`noeta dap`, on its own
page: [Debugging](Debugging)), and an **agent surface** (`noeta mcp`) — a Model Context Protocol
server that hands AI coding agents the same compiler ground truth the editor gets.

## Syntax highlighting

A TextMate grammar and VS Code extension live in
[`editors/vscode-noeta/`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta). The
grammar is **static** — it colorizes without running the compiler, instantly and offline — and
covers the whole surface: keywords, the three string forms with `${…}` interpolation, every numeric
literal form, primitive and container types, PascalCase user types, `@directive`/tier blocks,
`#[attribute]`s, and the full operator set. Install by symlinking the folder into
`~/.vscode/extensions/` (VSCodium works identically); the extension's README has details and a
`sample.noe` exercising every construct.

For **Neovim / Helix / Zed**, a
[tree-sitter grammar](https://github.com/noeta-lang/noeta/tree/main/editors/tree-sitter-noeta)
parses ≈99% of the conformance corpus and models Noeta's newline-terminated statements and
case-insensitive identifiers faithfully (run `tree-sitter generate` there first; see its README).
For a project with third-party text tiers (`@tier(<name>, text: "<lang>")`), `noeta grammar
tree-sitter --out <dir>` emits a per-project overlay so those `@<name> { … }` bodies parse and
highlight as their language — the static grammar's `@doc` → markdown rule is the fallback.

## The language server (`noeta lsp`)

`noeta lsp` speaks LSP over stdio, and the VS Code extension starts it automatically for `.noe`
files. It is a thin adapter over the compiler's incremental [salsa query graph](Architecture-and-Pipeline)
(`tokens → ast → checked → bytecode`, plus the module graph), so an edit re-checks only what
changed — the diagnostics you see are the *actual compiler's* diagnostics, live.

What it does today:

| Feature | Notes |
|---|---|
| **Live diagnostics** | Every `E0xxx` with its span, on every keystroke (incremental `didChange`). |
| **Hover types** | The inferred static type of the expression under the cursor, in surface syntax (`List<int>`, `Result<Order, OrderError>`). Non-default storage adds a fact line: a `@packed` type shows `@packed — 12 bytes`, a `List<packed>` shows `flat packed storage — 12 bytes/element, row-major` (or `column-major (SoA)` for `@packed(Layout.Column)`). |
| **Go to definition** | Cross-module: a name defined in an imported module resolves to that file. |
| **Find references / rename** | Including struct/class **members**; rename is prepare-checked so you can't rename what isn't renameable. |
| **Completion** | Identifiers in scope, members after `.` (including the bare-dot and mid-whitespace trigger positions), and **type positions** (annotations, signatures). |
| **Signature help** | Parameter hints while typing a call — free functions and methods. |
| **Document outline** | Types, functions, methods for the breadcrumb/symbol views. |
| **Semantic tokens** | Compiler-accurate token coloring layered over the static grammar. |
| **Inlay hints** | rust-analyzer style, same spelling as hover: the inferred type of every un-annotated binding (`mut xs`&nbsp;`: List<int>`&nbsp;`= …`) and of inference-typed closure parameters; parameter **names** at call sites (`scale(`&nbsp;`factor:`&nbsp;`2, …)`). Annotated bindings, reassignments, same-named identifier arguments, and uninferred (`dyn`) params show nothing. Packed storage is marked compactly on the type label (`: Vec3 · packed`, `: List<Vec3> · flat`, `: List<Cell> · SoA`); byte sizes stay hover-only. Toggle with VS Code's `editor.inlayHints.enabled`. |

Under load the server stays honest about *which* version it answers for: every open document in
a directory shares one salsa workspace (one parse per file, however many tabs are open), the
expensive requests (diagnostics, semantic tokens, completion) run off the message loop and are
**cancelled when a newer edit supersedes them** — a stale computation answers `ContentModified`
and the client silently re-asks — and a burst of keystrokes produces one diagnostics publish for
the final text, not one per keystroke.

The same salsa graph powers the [debugger](Debugging)'s launch compile and the conformance
harness, so all three tools read one source of truth.

## Debugging

`noeta dap` is a full DAP server: breakpoints, line-granular stepping, stack/scopes/variables, and
a debug console that is effectively a REPL over the paused program (closures included). It debugs
the **production VM** — same bytecode, JIT unarmed. See [Debugging](Debugging).

## Tracing the architecture

Every `@role`-bearing declaration gets a **CodeLens** (`⚑ Layer.Handler · trace call paths`);
running it — or **Trace Call Paths from Here** in the Architecture sidebar's context menu, or
**Noeta: Trace Call Paths** from the palette for the whole role surface — opens the **trace view**:
the same static call-graph walk `noeta mcp`'s `trace` tool serves, rendered as a role-colored
**boundary rail** (each pill a toggle: click to highlight every path reaching that boundary —
the other pills mute while one is active — double-click to jump to it) over collapsible call
trees whose indent rails are tinted by role, so the layers read as colored bands. Every row jumps
to source; the walk's honesty markers are visible — dynamic and external callees dimmed,
*passed-as-value* references badged (a callback registration is part of the flow but never a
syntactic call), recursion marked, truncation explicit — and trivial low-level calls stay hidden
behind a toggle so the architectural shape stays foregrounded.

The header's **Tree | Lanes** switcher turns the same trace into the **swimlane view**: the call
trees collapsed to the *role graph* — one column per role, a card per role-bearing function, and
edges connecting each bearer to the nearest bearers it reaches (the non-role intermediate calls
collapse away; a connection that exists only through a passed-as-value chain renders dashed). It
is the layered architecture diagram — Handler → Service → Store as columns — derived from the
code, never hand-drawn. The boundary rail filters here too: an active boundary dims everything
except its upstream cards and edges.

## Profiling

`noeta profile` reports where a program spends its time — a wall-time **flamegraph** (sampling),
the exact call-count/self-time table *and* exact call-tree flamegraph (`--instrument`), or the
bytes-weighted **memory flamegraph** (`--alloc`). Same production VM, tier-0, and — like the
debugger — outside the differential oracle. The VS Code extension renders profiles **in-editor**:
an interactive flame-graph view with click-to-source, a sortable function table, hot-line
annotations in the source itself, and a **thread picker** when the run spawned isolates. All three
modes are commands (**Noeta: Profile File (Sampling / Instrumenting / Allocations)**, also in the
run-button dropdown). See [Profiling](Profiling).

## The agent surface (`noeta mcp`)

`noeta mcp` is a **Model Context Protocol** server over stdio — the compiler's adapter for an AI
coding agent, the way `noeta lsp` is its adapter for an editor and `noeta dap` for a debug UI.
Agents have essentially no Noeta in their training data, so the server's first job is **grounding**:
every answer comes from the real compiler, its real documentation, and its CI-tested examples — not
from a model's guess.

Register it with an agent, e.g. Claude Code:

```sh
claude mcp add noeta -- noeta mcp
```

In **VS Code 1.101+** the Noeta extension registers the server automatically (via the editor's MCP
provider API), so agents running in the editor — Copilot agent mode and friends — discover it with
no configuration; the extension's `noeta.server.path` setting points at the binary for `lsp`, `dap`,
and `mcp` alike.

### The tools

**Ground** — orient before writing a line:
- `docs_search` / `docs_get` — search and read this documentation (also exposed as MCP *resources*).
- `examples_find` — CI-tested example programs by feature, concept, or diagnostic code.
- `stdlib_api` — the real standard-library signatures, straight from the compiler's own registry.
- `explain_diagnostic` — what an `E0xxx` means, with real programs that trigger and fix it.

**Understand** — the compiler's semantic answers:
- `check` — type-check code; the same JSON diagnostics `noeta check --format json` emits.
- `type_at` / `symbols` — the inferred type at a symbol/position (plus a `layout` storage fact for `@packed`/flat-list types, same wording as editor hover); a file's declaration outline.
- `definition` / `references` / `completions` / `signature` — navigation over the **same
  `noeta-ide` engine the language server serves**, so agent and editor can never disagree; a `file`
  entry resolves cross-file through sibling modules and dependency packages.

**Introspect** — the compiler's artifacts:
- `ast` / `bytecode` / `pipeline` / `module_graph` — the syntax tree, the VM disassembly, a
  per-stage health summary, and the `use` import graph (each module labeled with the `@role`
  bindings it declares).
- `reflect` — the [attributes & `@role` reflection manifest](Attributes-and-Reflection): which
  declarations are entry points, trust boundaries, persistence boundaries, sinks, or layers —
  each with its source location, joinable with every other tool. The `symbols` outline carries
  the same roles per node, so the architecture shows on the map itself.
- `trace` — unfold the **static call path from a role**: `trace(from: "EntryPoint")` starts at
  every function bearing the role and walks the call graph — each node a function with its own
  roles, declaration and call sites; external module calls and dynamic callees are labeled
  leaves, and passed-function references (handler registrations, callbacks) are followed as
  `reference` edges. The `boundaries` summary answers the architectural question directly: which
  persistence/trust boundaries does this entry point reach.

**Execute** — run and observe, not just read:
- `run` / `eval` / `test` — run a program (stdout/exit/traceback), evaluate an expression
  (value + type), run `@test` blocks. **Sandboxed and deterministic by default** (in-memory fs,
  logical clock, seeded random; `real: true` opts into the real host), and always bounded by
  liveness limits — a runaway loop is stopped, never hung.
- `debug_start` / `debug_inspect` / `debug_step` / `debug_eval` / `debug_stop` — interactive debug
  sessions over the VM's own debugger seam: pause at entry or breakpoints, read the call stack and
  live locals, step by line, evaluate expressions in a paused frame (type-checked against the
  program first). A runaway `continue` lands in an inspectable `limit` **pause** — for an agent
  chasing an infinite loop, seeing the live counter mid-spin *is* the diagnosis.

**Transform**:
- `format` — canonical formatting, the same engine as `noeta fmt`; declines on unparseable source.

Also useful to an agent (or a human): `noeta dump <file>` prints the exact VM bytecode a program
compiles to — what actually runs, which fast paths fired, how names and constants are laid out. See
[The CLI](The-CLI#noeta-dump).
