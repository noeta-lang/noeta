# Changelog

## 0.14.2

Fixes **`${...}` interpolation inside strings** rendering in the enclosing string's color. The
grammar did split each hole into tokens, but they kept `string.quoted.*` as an ancestor scope and
used bespoke scope names (`meta.interpolation.*`, `punctuation.section.interpolation.*`) that no
theme targets — so only tokens a theme styled more specifically than `string` (`self`, a call name)
punched through, while the `${` `}` delimiters and every bare identifier (`${self.id}`, `${i}`,
`${total(items)}`) fell back to the string color. The hole now uses the JavaScript/TypeScript
template-string convention every theme already ships rules for: `${`/`}` are
`punctuation.definition.template-expression.begin/end.noeta`, and the hole is
`meta.template.expression.noeta` (content `meta.embedded.line.noeta`), whose `meta.template.expression`
rule resets the region's foreground off the string color. Adds a theme-resolution regression suite
(`test/grammar-interpolation.test.js`) that pins the resolved colors, not just the scope names.

## 0.14.1

Fixes auto-indent after a **multi-line tier block** (`@doc { ... }`, `@sql { ... }`, ...): pressing
Enter after the closing `}` indented the next line to the block *body's* level. VS Code evaluates
its indentation rules against lines with brackets in comment/string tokens removed, so the
delimiter braces' `.comment.` scopes made the closing `}` invisible to `decreaseIndentPattern`.
The delimiter braces are now scoped `punctuation.section.tier.begin/end.noeta` (no `comment`), while
the body content -- and any braces *in* the prose -- stay comment-scoped (still excluded from
bracket matching). Adds a grammar+indent regression suite (`npm test`) that tokenizes with the real
grammars and drives a port of VS Code's indent algorithm.

## 0.7.1

Text tiers (text-tiers arc): `@doc { ... }` bodies now highlight as **embedded markdown** and are
bounded exactly like the compiler bounds them -- braces nest, `\{`/`\}`/`\\` are literal escapes --
fixing the long-standing leak where an apostrophe or backtick in doc prose opened a string scope
that corrupted highlighting for the rest of the file. Third-party declared text tiers
(`@tier(x, text: "...")`) plug in via a one-rule injection grammar (see README, "Text tiers and
embedded languages").

## 0.7.0

Adds **architecture navigation** over the compiler's static call graph (the same engine the
`noeta mcp` agent tools read):

- **Call hierarchy**: `Shift+Alt+H` on any function peeks its callers/callees in VS Code's native
  tree — cross-module, with each item's `@role` bindings in the detail and passed-as-value uses
  marked `reference`. External/dynamic callees are omitted here (no source location) and appear in
  the trace view instead.
- **Role CodeLenses**: declarations bearing a `@role` show `⚑ Enum.Variant · trace request path`;
  clicking opens the **trace view** — a read-only `noeta-trace:` document unfolding the full
  static path from that entry point, with a `boundaries reached` summary and clickable
  `path:line` links on every node. External/dynamic calls and cycles are labeled, never guessed.
- **Noeta: Trace Request Path** (palette) traces the active file's whole architectural surface
  (every role-bearing function).
- **Architecture sidebar** (new Noeta activity-bar view): the project's role surface as a tree —
  roles as groups, bearers beneath, each function's calls unfolding lazily with external/dynamic
  calls as labeled leaves. Click jumps to source; the context menu opens the full trace, the call
  hierarchy, or a **focused profile run**. Follows the active `.noe` editor; refreshes on save.

Adds **test integration** over VS Code's native Testing API:

- `@test` fns appear in the Test Explorer *and* get run arrows in the editor gutter — discovery is
  the compiler's own tier walk (`noeta/tests`), so the explorer and `noeta test` always agree.
  `#[Name]`, `#[Group]`, and `#[Skip]` metadata are honored.
- Runs shell out to the new `noeta test --json` (with `--name <fn>` for single tests) and map the
  machine-readable outcomes back: failures carry the assertion message and the test's captured
  output.

Adds **profile slices**: "Profile Focused on This Function" (Architecture view context menu)
profiles the run, then the flame view **re-roots every sample stack at that function** — you see
only the part of the run you asked about, with a bar reporting the slice's share of samples and a
one-click way back to the whole run.

Internal: one shared `noetaCommand()` toolchain resolver (`src/toolchain.js`); all commands use
the `Noeta` palette category.

## 0.6.0

Adds **run/build tasks**: a `noeta` task type (`run` / `build [--native|--exe]`, authorable in
`tasks.json`) with a TaskProvider for the active file — the native build is the default build task
(Ctrl+Shift+B) — plus **Noeta: Run File** and **Noeta: Build Native Executable** commands in the
editor title bar's run menu.

Adds the **profiler UI** (`noeta profile`):

- **Noeta: Profile File (Sampling)** and **(Instrumenting)** commands (run-button dropdown +
  palette) profile the active `.noe` file and open the result in a new **Noeta Profile** view —
  an interactive flame graph (zoom, breadcrumbs, hover details, double/ctrl+click to jump to
  source) with a sortable per-function table, rendered in the editor's own theme colors
  (light/dark/high-contrast).
- **Hot-line annotations**: after a sampling run, profiled sources show each hot line's share of
  samples inline; cleared on edit or with **Noeta: Clear Profile Line Annotations**
  (`noeta.profile.lineAnnotations` to disable, `noeta.profile.hz` for the sampling rate).
- The view is a custom editor for `*.noeprof.json` and reads the standard artifacts (speedscope
  JSON with structured `file`/`line`/`col` frames, or the instrumenting JSON), so CLI-made
  profiles open in it too. The profiled program's own output streams to the **Noeta Profile**
  output channel.

## 0.5.0

Registers the **`noeta mcp` server** with the editor's language-model API (VS Code 1.101+), so AI
agents running in the editor (Copilot agent mode and friends) discover the compiler's tools
automatically — no manual MCP configuration:

- ~26 tools of compiler ground truth: documentation/example search, `check` diagnostics,
  type-at-position, definition/references/completions/signature (the same engine the language
  server uses), AST/bytecode/reflection introspection, sandboxed `run`/`eval`/`test`, interactive
  `debug_*` sessions, and `format`.
- Uses the existing `noeta.server.path` setting (the same `noeta` binary serves `lsp`, `dap`, and
  `mcp`). Hosts without the MCP API skip the registration quietly; everything else keeps working.
- Requires VS Code **1.101** or later (the MCP provider API); this is the new minimum for the
  extension.

## 0.4.0

Adds **formatting** via the `noeta lsp` server (the same engine as `noeta fmt`):

- **Format Document** and **format-on-save** — reformats the whole file into the canonical style.
  A safety check re-parses the result, so formatting can never change what a program means.
- **Format-on-type** — reformatting a block the moment you type its closing `}` (quiet while the
  code is still mid-typed).
- Both are **on by default** for `.noe` files (`editor.formatOnSave` / `editor.formatOnType`), along
  with 4-space indentation.

## 0.3.0

Adds **debugging** via the `noeta dap` Debug Adapter Protocol server:

- Press **F5** on a `.noe` file to run it under the production bytecode VM (JIT off) through VS Code's
  debug UI — no `launch.json` required. New `noeta` debug type with a `program`/`stopOnEntry` launch
  configuration.
- **Breakpoints** on any executable line (including bare `return` lines), **step over/into/out**, the
  **call stack**, and a **Variables** view with each frame's locals, values, and types — read live
  from the VM. Program `echo` output goes to the Debug Console.
- The debugger uses the existing `noeta.server.path` setting (the same `noeta` binary serves `lsp` and
  `dap`).

Language-server changes since 0.2.0 (all served automatically by `noeta lsp` — no client change):

- **Find references** and **rename** for value *and* member symbols (type-aware, cross-module), with
  `prepareRename` validation.
- **Signature help** for function and method calls, with the active argument highlighted.
- **Semantic highlighting** — compiler-accurate identifier colouring overlaid on the grammar.
- Richer **completion**: member completion on a bare `.` trigger, in-scope locals in whitespace, and
  type names in annotation position.
- Faster editing: `didChange` no longer re-reads sibling files from disk on every keystroke.

## 0.2.0

Wires in the `noeta lsp` language server, adding semantic features on top of the static grammar:

- **Live diagnostics**, **hover types**, **go-to-definition** (locals, parameters, functions, types,
  fields, methods — across modules), a **document outline**, and **completion** (keywords, in-scope
  names, and a receiver type's members after `.`).
- New settings: `noeta.server.path` (where to find the `noeta` executable) and `noeta.trace.server`
  (JSON-RPC tracing). New command: **Noeta: Restart Language Server**.
- Requires the `noeta` toolchain (the server is its `noeta lsp` subcommand) and a one-time
  `npm install` of the client dependency; see the README.

## 0.1.0

First release. Static TextMate grammar and editor configuration for `.noe`:

- Syntax highlighting for the full Noeta surface — keywords, the three string
  forms with `${…}` interpolation, every numeric literal form (decimal, hex,
  octal, binary, floats, `f32` and `i8`…`u64` suffixes), primitive and container
  types, PascalCase user types, `@directive`/tier blocks, `#[attribute]`s, and
  the full operator set (`|>`, `..`/`..=`, `...`, `??`/`??=`).
- Comment toggling, bracket matching, auto-closing pairs, and indentation rules.

No language server yet — semantic features arrive with `noeta lsp`.
