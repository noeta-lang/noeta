# Editor & AI Tooling

Noeta's editor story ships in three layers, all in-tree: **static syntax highlighting** (TextMate +
tree-sitter grammars), a **language server** (`noeta lsp`), and a **debugger** (`noeta dap`, on its
own page: [Debugging](Debugging)). The planned **agentic MCP surface** is the one piece still on the
roadmap.

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

## The language server (`noeta lsp`)

`noeta lsp` speaks LSP over stdio, and the VS Code extension starts it automatically for `.noe`
files. It is a thin adapter over the compiler's incremental [salsa query graph](Architecture-and-Pipeline)
(`tokens → ast → checked → bytecode`, plus the module graph), so an edit re-checks only what
changed — the diagnostics you see are the *actual compiler's* diagnostics, live.

What it does today:

| Feature | Notes |
|---|---|
| **Live diagnostics** | Every `E0xxx` with its span, on every keystroke (incremental `didChange`). |
| **Hover types** | The inferred static type of the expression under the cursor, in surface syntax (`List<int>`, `Result<Order, OrderError>`). |
| **Go to definition** | Cross-module: a name defined in an imported module resolves to that file. |
| **Find references / rename** | Including struct/class **members**; rename is prepare-checked so you can't rename what isn't renameable. |
| **Completion** | Identifiers in scope, members after `.` (including the bare-dot and mid-whitespace trigger positions), and **type positions** (annotations, signatures). |
| **Signature help** | Parameter hints while typing a call — free functions and methods. |
| **Document outline** | Types, functions, methods for the breadcrumb/symbol views. |
| **Semantic tokens** | Compiler-accurate token coloring layered over the static grammar. |

The same salsa graph powers the [debugger](Debugging)'s launch compile and the conformance
harness, so all three tools read one source of truth.

## Debugging

`noeta dap` is a full DAP server: breakpoints, line-granular stepping, stack/scopes/variables, and
a debug console that is effectively a REPL over the paused program (closures included). It debugs
the **production VM** — same bytecode, JIT unarmed. See [Debugging](Debugging).

## Profiling

`noeta profile` reports where a program spends its time — an exact per-function call-count/self-time
table (`--instrument`) or a wall-time **flamegraph** (folded / SVG / speedscope). Same production VM,
tier-0, and — like the debugger — outside the differential oracle. See [Profiling](Profiling).

## The agentic MCP surface (roadmap)

The remaining planned piece. The compiler already builds a queryable **reflection manifest** of a
program's declarations, their `#[…]` attributes, and their `@role(…)` semantic tags (see
[Attributes & Reflection](Attributes-and-Reflection)); the intent is an MCP server exposing that
labeled architectural graph to AI agents — which declarations are entry points, trust boundaries,
persistence boundaries, sinks, or layers, and how data flows between them. The manifest and the
semantic-role vocabulary exist; the protocol server does not yet. Until it lands, treat mentions of
"MCP tools" in design documents as forward-looking.

Also useful to an agent (or a human) today: `noeta dump <file>` prints the exact VM bytecode a
program compiles to — what actually runs, which fast paths fired, how names and constants are laid
out. See [The CLI](The-CLI#noeta-dump).
