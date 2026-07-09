# Agent-native tooling — `noeta mcp` Model Context Protocol server

**Status: planning (proposal for sign-off).** This is the third leg of the editor-tooling story —
after `noeta lsp` (a *read* adapter over the salsa graph, shipped) and `noeta dap` (a *control*
adapter over the running VM, shipped). The MCP server is the adapter for a **different consumer**: an
AI coding agent. It is the feature that makes Noeta genuinely agent-native — the roadmap has always
named it (`docs/Editor-and-AI-Tooling.md:16-24`), and the reflection/`@role` machinery it was
designed around already exists and is checked (`crates/noeta-check/src/attributes.rs`, `tiers.rs`;
`roles_of()` reflection builtin, tests at `crates/noeta-check/src/tests.rs:588-657`).

This document is the reconnaissance-backed scope. It was written after mapping the LSP analysis
engine, the DAP/VM debug seam, the salsa query surface, the docs+examples corpus, and the VS Code
extension's registration wiring (findings cited inline).

---

## The thesis — an agent is not an editor

The LSP serves a human at a cursor: positions, squiggles, hovers. The DAP serves a human debug UI.
The MCP serves an **agent**, which has different ergonomics, and designing for them is the whole
point:

1. **Agents have ~zero Noeta in their training data.** Left ungrounded they write plausible-but-wrong
   Noeta (PHP-ish syntax, invented stdlib calls). The single highest-value thing the server does is
   **ground the agent in real docs, real examples, and real stdlib signatures** before it writes a
   line. This is not a "nice to have" bolted onto the LSP surface — it is the headline.
2. **Agents address code by name and by snippet, not by `(line, col)`.** A tool that only takes a
   cursor position is awkward for an agent that just generated a string. Tools accept **source text
   or a file path, plus a symbol name / code selection** where a position is needed — and fall back
   to `(line, col)` only when nothing else will do.
3. **The compiler is the agent's ground truth.** Every answer comes from the real salsa graph or the
   real VM — never a heuristic — so the agent can *trust* the result and stop guessing. `check`
   (does this typecheck, what exactly is wrong, what does `E0007` mean) is the tight feedback loop an
   agent lives in.
4. **Agents can do things a human debugger UI makes tedious.** Programmatically set breakpoints, run,
   read the paused state, evaluate an expression against it — a "run my program and tell me what
   actually happened / why" loop, driven from the agent's reasoning.
5. **Output is a token budget.** Results are compact, structured, ranked, and truncatable. A
   diagnostic is `{code, severity, span, message, labels}` plus an *optional* rendered caret block —
   not a screenful by default.

Four pillars follow from this: **Ground** (docs/examples/stdlib), **Understand** (semantic queries),
**Introspect** (compiler-artifact traversal), **Execute** (run / eval / debug). Plus a transform leg
(**format**) gated on a prerequisite that does not exist yet.

---

## What already exists vs. what we build

**Reused as-is (free / near-free):**

- **The salsa query graph** (`noeta-db`, `crates/noeta-db/src/lib.rs`). All the derived artifacts an
  agent wants are already public, memoized queries taking `&dyn salsa::Database`:
  `tokens` / `ast` / `checked` / `checked_ide` / `bytecode` and their `Workspace` analogues
  (`linked` / `linked_checked_ide` / `linked_bytecode`). `linked_checked_ide` carries both
  `.diagnostics` and `.expr_types: HashMap<Span, TypeRepr>` — diagnostics **and** hover types with no
  new work (`crates/noeta-db/src/lib.rs:332`).
- **The reflection manifest** — `@role(Semantic.EntryPoint / TrustBoundary / Sink / …)` +
  `@semantic` enums, validated in `noeta-check`, queryable in-language via `roles_of()`. This is the
  architectural-graph backbone the roadmap named (`docs/Editor-and-AI-Tooling.md:14`).
- **The doc + example corpus** — `docs/*.md` (33 wiki-style pages, ~4150 lines, flat + predictable
  titles) and `tests/conformance/<feature>/*.noe` (~40 feature dirs, 528 runnable snippets) — two
  clean retrieval axes that are already the source of truth the toolchain tests against
  (`crates/noeta-cli/tests/doc_samples.rs` compiles every fenced block).
- **`@doc` extraction** — `noeta_check::collect_docs(&Program)`, parse-only, reusable programmatic API.
- **AST pretty-printer** — `noeta_ast::Pretty` (S-expr with `@start..end` spans) for an `ast` tool.
- **The VM debug seam** — the reusable `noeta_vm::Debugger` trait + `DebugView` / `DebugAction` /
  `DebugEvalRequest` (`crates/noeta-vm/src/lib.rs:74-152`) and `noeta-dap::session::{compile_file,
  run_compiled}` (both `pub`). The MCP does **not** speak DAP wire — it implements its own headless
  `Debugger` and drives the VM directly, the same seam the DAP itself rides.
- **The compiled-session engine** — `noeta_vm::VmSession` (`eval` / `type_of` / `binding_names`,
  `crates/noeta-vm/src/session.rs:179`) for `eval`, and the `run` pipeline for `run` / `test`.
- **The single-binary CLI shape** — a `noeta mcp` subcommand mirroring `cmd_lsp`/`cmd_dap`
  (`crates/noeta-cli/src/main.rs:208-217`), one thin arm → `noeta_mcp::run_stdio()`.
- **The VS Code extension** — `editors/vscode-noeta/`, whose `noetaCommand()` + `noeta.server.path`
  already parameterize one binary across `lsp`/`dap`; MCP registration mirrors the existing DAP
  factory (`src/extension.js:42-51`).

**New plumbing (the real work):**

- The **`noeta-mcp` crate + `noeta mcp` subcommand** and the MCP protocol layer (tools + resources +
  prompts + server instructions).
- A **retrieval index** over `docs/` and `tests/conformance/` (in-memory, built at startup — the
  corpus is small) with a lexical/section ranker.
- A **stdlib-surface enumerator** — render the native registry + reflection manifest into
  agent-readable module/function signatures.
- A **diagnostic-explanation registry** — prose for each `E0xxx` (what it means, why it fires, how to
  fix), keyed off the existing typed diagnostic variants.
- A **headless debug driver** — an MCP-owned `noeta_vm::Debugger` impl + a breakpoint-resolution
  reverse index (the DAP has one, but `resolve_breakpoints`/`DapDebugger` are private —
  §Decisions #4).
- **Shared analysis engine access** for defs/refs/completions/signature — the logic exists in
  `noeta-lsp` but is **private** (`DocumentStore` and the `resolve`/`completion`/`signature` modules
  are not `pub`). §Decisions #3 covers how we reach it without duplicating.
- **`format`** — a source formatter does **not exist** (no `noeta fmt`, no formatter crate; the
  `Pretty` trait emits S-expr debug output, not source). §Decisions #6.

---

## The tool surface (four pillars + transform)

Names are grouped by pillar; each is a stateless request/response unless noted. Inputs take
`{file}` **or** `{source}` (inline), and a workspace is an optional sibling-file set.

### Pillar 1 — Ground (the headline)

| Tool | Input | Returns |
|------|-------|---------|
| `docs_search` | `query`, `limit?` | Ranked doc sections (title, anchor, snippet) from `docs/*.md` |
| `docs_get` | `page` | Full markdown of one wiki page |
| `examples_find` | `feature \| query`, `limit?` | Matching `.noe` snippets (source + one-line description) from `tests/conformance/<feature>/` and `examples/` |
| `stdlib_api` | `module?` | Module/function/method signatures from the native registry + reflection manifest (e.g. `std.string`, `std.http`) |
| `explain_diagnostic` | `code` (`E0007`) | Prose: meaning, common causes, canonical fix, doc links |

The server's **instructions** field (below) is the always-on orientation; these tools are the
on-demand deep-dives.

### Pillar 2 — Understand (the compiler answers)

| Tool | Input | Returns |
|------|-------|---------|
| `check` | `source \| file`, `workspace?` | Diagnostics `{code, severity, span, message, labels, rendered?}` — **the feedback loop** |
| `type_at` | `file \| source`, `symbol \| selection \| position` | The `TypeRepr` at that site (hover) |
| `symbols` | `file \| source` | Outline: `fn`/`struct`/`class`/`enum`/`impl` tree with spans |
| `definition` | `file \| source`, `symbol \| position` | Defining location + snippet (needs the shared engine, §Decisions #3) |
| `references` | `file \| source`, `symbol` | All use sites |
| `completions` | `file \| source`, `position` | Candidates (lowest agentic priority; for interactive editing) |
| `signature` | `file \| source`, `call \| position` | Signature + active parameter |

### Pillar 3 — Introspect (traverse the graph)

| Tool | Input | Returns |
|------|-------|---------|
| `ast` | `file \| source` | The `Pretty` S-expr AST (spans included) |
| `bytecode` | `file \| source` | Disassembly (reuse `noeta dump`) — *what actually runs*, whether a reuse/in-place fast path fired |
| `module_graph` | `entry \| workspace` | Module dependency edges from the salsa `linked` graph |
| `reflect` | `file \| source`, `role?` | The `@role`/`@semantic` architectural graph: entry points, trust/persistence boundaries, sinks, layers — the roadmap's labeled graph |
| `pipeline` | `file \| source` | Per-stage summary (tokens → ast → checked → bytecode): where it breaks, sizes, counts |

### Pillar 4 — Execute (run and observe)

| Tool | Input | Returns |
|------|-------|---------|
| `run` | `source \| file`, `args?`, `host?`, `limits?` | stdout / stderr / exit / traceback. **Sandbox host by default** (deterministic), real host opt-in with limits (§Decisions #5) |
| `eval` | `expr`, `context?` | Value + type via `VmSession` (one-shot REPL) |
| `test` | `file`, `filter?` | Structured `@test` results (pass/fail per case) |

Debug session — **stateful**, addressed by an opaque `session` handle (an MCP-owned `Debugger`
driving the VM directly, JIT-off with debug info):

| Tool | Input | Returns |
|------|-------|---------|
| `debug_start` | `file`, `breakpoints[]` | `session` + first stop state |
| `debug_inspect` | `session` | Stack + scopes + variables at the current stop |
| `debug_eval` | `session`, `expr`, `frame?` | Evaluate against the paused frame (REPL-over-paused-program) |
| `debug_step` | `session`, `over\|into\|out\|continue` | Next stop state |
| `debug_stop` | `session` | Teardown |

### Transform (gated)

| Tool | Input | Returns |
|------|-------|---------|
| `format` | `source \| file` | Formatted source — **blocked: no formatter exists** (§Decisions #6) |

### Resources & prompts (complementary to tools)

- **Resources** — the docs and examples exposed as URI-addressable MCP resources
  (`noeta-doc://Type-System`, `noeta-example://generics/bounded`) so a client can browse/pin them
  directly, alongside the search tools.
- **Server instructions** — a tight, always-loaded orientation shipped in the MCP `instructions`
  field: *Noeta is inferred-static typed, ext `.noe`; run `check` before claiming code compiles;
  ground syntax with `docs_search` / `examples_find`; the stdlib is `stdlib_api`, not guessed.* This
  is cheap and disproportionately raises first-shot correctness.
- **Prompts** *(optional, low priority)* — `scaffold-module`, `explain-and-fix-diagnostics`,
  `review-noeta`.

---

## Decisions proposed (confirm before M0)

1. **`noeta-mcp` is a new crate + a `noeta mcp` CLI subcommand.** Mirrors `noeta-lsp`/`noeta-dap`:
   `cmd_mcp()` → `noeta_mcp::run_stdio()`. Depends on `noeta-db`, `noeta-check`, `noeta-compiler`,
   `noeta-bytecode`, `noeta-vm`, `noeta-runtime`, `noeta-loader`, `noeta-span`, `noeta-diagnostics`
   (the `run` set), plus the MCP SDK. Keeps MCP weight out of the CLI except at the entry point.
2. **Protocol = the official Rust MCP SDK (`rmcp`), over hand-rolled framing.** MCP has more surface
   than LSP/DAP (tools + JSON-Schema'd params + resources + prompts + capability negotiation);
   `rmcp` derives tool schemas and handles the envelope, where the LSP/DAP hand-rolls paid off for
   their smaller, fussier protocols. *Trade-off flagged:* `rmcp` is a newer dependency; if you'd
   rather hand-roll (zero new dep, full control) as we did for DAP, M0 is where we'd choose —
   nothing downstream depends on it beyond the M0 adapter. **Recommend `rmcp`.**
3. **Reuse the analysis engine by staging, not by upfront refactor.** The high-value pillars —
   `check`, `type_at`, `symbols`, `ast`, `bytecode`, `module_graph`, `reflect`, `pipeline`, `run`,
   `eval`, `test` — need only **public** crates (`noeta-db`, `noeta-vm`, `noeta-check`), *no* private
   LSP code. Only `definition`/`references`/`completions`/`signature` need `noeta-lsp`'s private
   `DocumentStore` + `resolve`/`completion`/`signature` modules. So we build pillars 1/3/4 and the
   cheap half of pillar 2 first (zero refactor), and when we reach defs/refs we **extract a shared
   `noeta-ide` engine crate** (move `DocumentStore` + `offsets`/`resolve`/`completion`/`signature`/
   `symbols` out of `noeta-lsp`; both `noeta-lsp` and `noeta-mcp` become thin protocol adapters over
   it). This is the architecturally correct boundary once there are two consumers, it is
   behavior-neutral (guarded by the LSP's existing request/response fixtures), and it avoids dragging
   `tower-lsp-server` into the MCP dep tree. *Alternative (cheaper, worse):* just make `DocumentStore`
   `pub` and depend `noeta-mcp → noeta-lsp`. **Recommend the extraction, deferred to the defs/refs
   slice** so it never blocks the headline value.
4. **Debugging drives the VM seam directly; extract the breakpoint/`Debugger` glue.** The MCP
   implements its own `noeta_vm::Debugger` (no DAP wire). The reusable seam is public, but
   `resolve_breakpoints` + a headless `Debugger` impl are private in `noeta-dap`; the debug slice
   promotes those to a shared spot (a small `noeta-debug`/shared module) so LSP-less, DAP-less
   drivers reuse them. Values are `!Send` and thread-local to the run worker, so everything crosses
   the boundary as rendered strings (as the DAP already does).
5. **Execution is sandboxed by default, real-host opt-in with limits.** `run`/`eval`/`test` use the
   deterministic `SandboxHost` unless the caller passes `host: "real"`, which requires explicit
   resource limits (wall-clock, output cap) — an agent should not touch the real disk/network/env
   unless the user's tool call says so. **Recommend sandbox default.**
6. **`format` is gated on a formatter that does not exist — spin it as a prerequisite arc, not
   inside this one.** A real source formatter (AST→source with trivia/comment preservation) is its
   own milestone; the `Pretty` S-expr printer is not it, and there is no `noeta fmt`. The MCP
   `format` tool ships as a **thin wrapper** that lands when a `noeta fmt` / formatter crate does; a
   `format` slice here is a stub that reports "unavailable" until then. *This is a scope deferral —
   confirm.* **Recommend: defer `format` to a dedicated formatter arc; wire the MCP tool in when it
   exists.**
7. **Auto-registration = VS Code MCP provider API + documented manual registration elsewhere.** In
   `editors/vscode-noeta/`, register via `vscode.lm.registerMcpServerDefinitionProvider` returning a
   stdio definition `{command: noetaCommand(), args: ["mcp"]}` — reusing the same `noeta.server.path`
   setting, so one binary serves `lsp`/`dap`/`mcp`. This needs an `engines.vscode` bump (the LM/MCP
   API is far newer than the current `^1.82.0` floor) — *confirm the bump is acceptable.* For
   Claude Code / Cursor / other clients, ship a documented `claude mcp add noeta -- noeta mcp`
   snippet (no code, just docs). **Recommend both.**

---

## Architecture — where the pieces live

```
AI agent (Claude Code / VS Code LM / Cursor)
        │  MCP / stdio (JSON-RPC: tools · resources · prompts)
        ▼
   noeta mcp   (noeta-cli Command::Mcp → noeta_mcp::run_stdio)
        │
        ▼
   noeta-mcp crate
   ┌───────────────────────────────────────────────────────────────┐
   │ Server (rmcp): tool dispatch · resource provider · instructions │
   ├───────────────┬───────────────┬───────────────┬────────────────┤
   │  Ground       │  Understand   │  Introspect   │  Execute        │
   │  retrieval    │  salsa reads  │  salsa reads  │  VM run/session │
   │  index over   │  + shared     │  + Pretty AST │  + headless     │
   │  docs/,       │  noeta-ide    │  + reflect    │  Debugger seam  │
   │  conformance/ │  engine       │  manifest     │                 │
   └───────┬───────┴───────┬───────┴───────┬───────┴────────┬────────┘
           ▼               ▼               ▼                ▼
   docs/*.md +      noeta-db (salsa)  noeta-ast Pretty   noeta-vm
   tests/conformance  linked_checked  noeta-db bytecode  VmSession +
   (in-mem index)     _ide · expr_types  module graph    Debugger/DebugView
                      noeta-ide*      noeta-check reflect  (JIT-off debug)
                      (extracted from
                       noeta-lsp)
```

New state the server owns: **one `LangDatabase`** (reused across analysis calls), the **retrieval
index** (built once at startup), a **diagnostic-explanation table**, and a **map of live debug
sessions** (`session` handle → worker thread + channel). `noeta-ide*` is the shared engine extracted
from `noeta-lsp` at the defs/refs slice (Decisions #3).

---

## Slices

One agent-visible capability per slice, each independently demonstrable. Like the LSP/DAP, this is
I/O-facing, so the differential/leak oracles don't apply directly; each slice is tested with
**in-process MCP tool-call fixtures** (drive the server, call a tool with a program + args, assert
the structured result), plus unit tests for the retrieval ranker, the position/marshalling helpers,
and the debug driver.

| # | Slice | Delivers | Notes |
|---|-------|----------|-------|
| **M0** | Server skeleton + `check` + instructions | An agent can connect and get real diagnostics | `noeta-mcp` crate + `noeta mcp` subcommand; MCP handshake, capability + **instructions** advertisement; one tool, `check`, over `linked_checked_ide` (`{code,severity,span,message,labels}`). **Protocol/SDK decision (#2) lands here.** The single highest-value tool, first. |
| **M1** | Ground — docs + examples + explain | The agent writes idiomatic Noeta from real sources | `docs_search`/`docs_get`/`examples_find` over the in-memory corpus index; `explain_diagnostic`; docs+examples also exposed as MCP **resources**. No compiler state beyond the index. |
| **M2** | Ground — stdlib surface | The agent stops inventing stdlib calls | `stdlib_api` renders the native registry + reflection manifest into module/function/method signatures. |
| **M3** | Understand (cheap half) + Introspect | Semantic + artifact queries with zero refactor | `type_at`, `symbols`, `ast`, `bytecode`, `module_graph`, `pipeline`, `reflect` — all on public `noeta-db`/`noeta-ast`/`noeta-check`. Ships the byte-offset↔position helper (astral-plane unit tests). |
| **M4** | Execute — run / eval / test | "Run this and tell me what happened" | `run` (sandbox default, real-host opt-in + limits — decision #5), `eval` via `VmSession`, `test` over `@test` blocks. Structured stdout/stderr/exit/traceback. |
| **M5** | Understand (full) — defs / refs / completions / signature | Precise cross-symbol navigation | **Extract `noeta-ide`** from `noeta-lsp` (decision #3): `DocumentStore` + `offsets`/`resolve`/`completion`/`signature`/`symbols` become a shared crate; `noeta-lsp` re-adapts over it (its fixtures are the gate); `noeta-mcp` calls it for `definition`/`references`/`completions`/`signature`. The load-bearing refactor, deferred to exactly where it's needed. |
| **M6** | Execute — debug sessions | Programmatic breakpoint / inspect / eval / step | Headless `noeta_vm::Debugger` + shared breakpoint index (decision #4); `debug_start`/`debug_inspect`/`debug_eval`/`debug_step`/`debug_stop` over a `session` handle map. Heaviest slice, last. |
| **M7** | Wire auto-registration | The extension registers the MCP server; other clients documented | `vscode.lm.registerMcpServerDefinitionProvider` in `editors/vscode-noeta/` (reusing `noetaCommand()`), `engines.vscode` bump; `docs/Editor-and-AI-Tooling.md` updated with `claude mcp add` + the tool catalog (decision #7). |

`format` is intentionally **not** a slice here — it is gated on a formatter arc that doesn't exist
(decision #6); the tool is wired in a one-commit follow-on when that lands.

---

## Deferred (revisit after the arc)

- **`format`** — blocked on a net-new source-formatter arc (decision #6). Wire the tool in when
  `noeta fmt` exists.
- **MCP prompts** (`scaffold-module`, `explain-and-fix`, `review-noeta`) — nice-to-have; the tools +
  instructions carry the value first.
- **Semantic/embedding retrieval** — M1 ships lexical/section ranking over the small corpus; an
  embedding index is a later refinement if lexical proves insufficient.
- **Incremental / long-lived analysis sessions** — the server holds one `LangDatabase` and re-sets
  inputs per call; a persistent per-agent workspace with watched files is a later add.
- **Debugging `isolate` parallelism**, **conditional/logpoint breakpoints**, **`setVariable`** — the
  debug slice starts with the sequential main isolate + line-granular breakpoints (mirrors the DAP's
  own deferrals).
- **TCP transport** — stdio only for the milestone (as LSP/DAP).
- **Marketplace publishing** of the extension with the MCP contribution.

---

## Gate — this milestone is done when

`noeta mcp` starts over stdio, an MCP client (Claude Code / VS Code LM) connects and negotiates, and
an agent can: get typed diagnostics for a program (`check`); ground itself in real docs, examples,
and stdlib signatures (`docs_search`/`examples_find`/`stdlib_api`/`explain_diagnostic`); query
types, symbols, AST, bytecode, the module graph, and the `@role` architectural manifest
(`type_at`/`symbols`/`ast`/`bytecode`/`module_graph`/`reflect`); run and evaluate code in the
sandbox (`run`/`eval`/`test`); navigate defs/refs (`definition`/`references`); and set a breakpoint,
inspect the paused state, and step (`debug_*`) — each covered by in-process tool-call fixtures, with
the VS Code extension auto-registering the server, the workspace clean under fmt/clippy, and zero new
`unsafe`. `format` is explicitly out (a follow-on when the formatter exists).
