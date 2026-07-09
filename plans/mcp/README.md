# Agent-native tooling — `noeta mcp` Model Context Protocol server

**Status: M0–M3 done; M4 next.** `noeta mcp` runs over stdio (`rmcp` 2.1) with five tools — `check` (typed diagnostics for inline/on-disk Noeta) plus the Ground pillar `docs_search`/`docs_get`/`examples_find`/`explain_diagnostic` over the embedded docs+example corpus — and exposes docs as MCP resources. Verified by a real client⇄server round-trip. This is the third leg of the editor-tooling story —
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
(**format**) that wraps a formatter being built by a parallel arc.

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

**Consumed from a parallel arc (in-flight, not this arc's work):**

- **`noeta fmt` (source formatter)** and **`noeta check` (lint + static verification)** are being
  built by a parallel effort. The MCP does **not** reimplement them — its `format` and `check` tools
  are **thin wrappers over that arc's engine** (a library API where one is exposed; otherwise the CLI
  surface). MCP `check` therefore surfaces the *combined* lint + static-verify + type diagnostics
  the agent should see, exactly matching what `noeta check` reports, rather than a divergent
  MCP-only diagnostic set. §Decisions #6. Coordination point: align on the reusable entry the fmt/
  check arc exposes so agent and CLI never disagree.

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
| `check` | `source \| file`, `workspace?` | Combined lint + static-verify + type diagnostics `{code, severity, span, message, labels, rendered?}` — wraps the parallel `noeta check` arc — **the feedback loop** |
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

### Transform

| Tool | Input | Returns |
|------|-------|---------|
| `format` | `source \| file` | Formatted source — thin wrapper over the parallel `noeta fmt` arc (§Decisions #6) |

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

## Decisions (signed off — see per-item notes)

1. **`noeta-mcp` is a new crate + a `noeta mcp` CLI subcommand.** Mirrors `noeta-lsp`/`noeta-dap`:
   `cmd_mcp()` → `noeta_mcp::run_stdio()`. Depends on `noeta-db`, `noeta-check`, `noeta-compiler`,
   `noeta-bytecode`, `noeta-vm`, `noeta-runtime`, `noeta-loader`, `noeta-span`, `noeta-diagnostics`
   (the `run` set), plus the MCP SDK. Keeps MCP weight out of the CLI except at the entry point.
2. **Protocol = the official Rust MCP SDK (`rmcp`), over hand-rolled framing.** MCP has more surface
   than LSP/DAP (tools + JSON-Schema'd params + resources + prompts + capability negotiation);
   `rmcp` derives tool schemas and handles the envelope, where the LSP/DAP hand-rolls paid off for
   their smaller, fussier protocols. Reversible at M0 (nothing downstream depends on it beyond the
   adapter). **✅ Signed off: `rmcp`.**
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
   `pub` and depend `noeta-mcp → noeta-lsp`. **✅ Signed off: the extraction, deferred to the
   defs/refs slice (M5)** so it never blocks the headline value.
4. **Debugging drives the VM seam directly; extract the breakpoint/`Debugger` glue.** The MCP
   implements its own `noeta_vm::Debugger` (no DAP wire). The reusable seam is public, but
   `resolve_breakpoints` + a headless `Debugger` impl are private in `noeta-dap`; the debug slice
   promotes those to a shared spot (a small `noeta-debug`/shared module) so LSP-less, DAP-less
   drivers reuse them. Values are `!Send` and thread-local to the run worker, so everything crosses
   the boundary as rendered strings (as the DAP already does).
5. **Execution is sandboxed by default; real-host opt-in; liveness limits are always-on.**
   `run`/`eval`/`test` use the deterministic `SandboxHost` unless the caller passes `host: "real"`.
   Two points settled after a second pass:
   - **Real-host opt-in is load-bearing, not a luxury.** Under the sandbox, `fs` is an in-memory
     Vfs, `time` a logical clock, `random` seeded, `http`/network pure responders — so a program
     that reads a real config, calls a real API, or serves real HTTP runs against *stubs* and won't
     reflect production. Determinism is the right default for "why does this produce X"; real-host
     (with explicit limits, gating the IO capabilities) is required for "does this actually work
     end-to-end." The residual risk is only the *agent* triggering real effects silently — which the
     harness's per-call tool approval already gates, and which is no more than `noeta run` already
     does.
   - **Limits apply even in sandbox.** Determinism does not prevent an infinite loop or an output
     flood from hanging the server, so a wall-clock timeout + output cap (+ optional step budget)
     bound *every* execution; `host: "real"` additionally unlocks the IO capabilities.
   **✅ Signed off: sandbox default, real opt-in, always-on liveness limits.**
6. **`format` and `check` wrap the parallel `noeta fmt` / `noeta check` arc — not reimplemented
   here.** A separate in-flight effort is building the source formatter and a lint + static-verify
   `check` command. The MCP `format` tool is a thin wrapper over the formatter engine; the MCP
   `check` tool surfaces the *combined* lint + static-verify + type diagnostics that `noeta check`
   reports, so the agent and the CLI never disagree. Both land as MCP tools in this arc, gated only
   on the parallel arc exposing a reusable entry (library API preferred; CLI surface otherwise).
   **✅ Signed off: wrap the parallel arc; `format` is in scope as a wrapper, coordinate on the
   shared entry.**
7. **Auto-registration = VS Code MCP provider API + documented manual registration elsewhere.** In
   `editors/vscode-noeta/`, register via `vscode.lm.registerMcpServerDefinitionProvider` returning a
   stdio definition `{command: noetaCommand(), args: ["mcp"]}` — reusing the same `noeta.server.path`
   setting, so one binary serves `lsp`/`dap`/`mcp`. Needs an `engines.vscode` bump (the LM/MCP API is
   far newer than the current `^1.82.0` floor). For Claude Code / Cursor / other clients, ship a
   documented `claude mcp add noeta -- noeta mcp` snippet (no code, just docs). **✅ Signed off:
   both; the `engines.vscode` bump is accepted.**

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
| **M0** ✅ | Server skeleton + `check` + instructions | An agent can connect and get real diagnostics | `noeta-mcp` crate + `noeta mcp` subcommand; `rmcp` 2.1 over stdio; MCP handshake, tools capability + **instructions** advertisement; one tool, `check`, over the `linked_checked` query — returns `{ok, errors, warnings, diagnostics[]}` where each diagnostic is `noeta_diagnostics::JsonDiagnostic`, the **same canonical shape `noeta check --format json` emits** (verified byte-identical), for inline `source` or a `file` (sibling `.noe` modules resolved). `JsonDiagnostic` gained a feature-gated `JsonSchema` derive so the tool and CLI share one schema. Tested by direct-logic units + a client⇄server duplex round-trip fixture. **Protocol/SDK decision (#2) landed: `rmcp`.** The parallel `noeta check` arc is now merged to main; MCP consumes its `to_json` schema directly. |
| **M1** ✅ | Ground — docs + examples + explain | The agent writes idiomatic Noeta from real sources | `docs_search`/`docs_get`/`examples_find`/`explain_diagnostic` over an in-memory index of the **embedded** corpus (`docs/*.md` + `tests/conformance/**/*.noe`, baked in via `include_dir` so an installed binary is self-contained). Lexical section/example ranking; `explain_diagnostic` derives its title from the `DiagnosticCode` variant and surfaces real `// expect:`-tagged repros (diagnostics-dir first, capped). Docs also exposed as MCP **resources** (`noeta-doc://`, `noeta-example://`). |
| **M2** ✅ | Ground — stdlib surface | The agent stops inventing stdlib calls | `stdlib_api` renders the native registry (`registry::extensions()`) into surface-syntax signatures — every `use std.*` module + functions and every extern value type + methods, straight from the same `SigType`/`RetTy` data the checker maps onto `Type`. Filter by exact module identity (qualified `std.math` / bare `math`), family prefix (`http` → `http.client`/`server`), or extern type (`Uuid`); no filter lists the whole surface; an unknown filter returns the catalog as fallback. Unions render `A\|B`, optional params `T?`, higher-order `Fn(..) -> ..`, `Future<T>`. Note: string/list/map/set ops are **value methods**, not modules, so they are not in `stdlib_api` (they surface via `docs`/`examples`). Slice gate = an in-process MCP round-trip fixture over `stdlib_api`. |
| **M3** ✅ | Understand (cheap half) + Introspect | Semantic + artifact queries with zero refactor | `type_at` (tightest `expr_types` span at a `symbol`/position, over `linked_checked_ide`), `symbols` (AST outline), `ast` (`Pretty` S-expr), `bytecode` (`linked_bytecode`→`disassemble`), `module_graph` (`use`/`namespace` edges), `pipeline` (lex→parse→check→compile summary), `reflect` (`reflect::build` `@role`/attribute/type manifest — the same index `roles_of()` reads). All on public `noeta-db`/`noeta-ast` — **no LSP dep** (the `noeta-ide` extraction stays deferred to M5); `symbols` walks the AST directly for now. Shared `analyze.rs` = workspace builder + `LineIndex` (byte↔line/col) + `symbol_offset`. Slice gate = a `type_at` duplex fixture. |
| **M4** | Execute — run / eval / test + `format` | "Run this and tell me what happened"; clean it up | `run` (sandbox default, real-host opt-in, always-on liveness limits — decision #5), `eval` via `VmSession`, `test` over `@test` blocks (structured stdout/stderr/exit/traceback). `format` wraps the parallel `noeta fmt` arc (decision #6) — bundled here as the transform leg; if the fmt entry isn't ready, it slips to a one-commit follow-on. |
| **M5** | Understand (full) — defs / refs / completions / signature | Precise cross-symbol navigation | **Extract `noeta-ide`** from `noeta-lsp` (decision #3): `DocumentStore` + `offsets`/`resolve`/`completion`/`signature`/`symbols` become a shared crate; `noeta-lsp` re-adapts over it (its fixtures are the gate); `noeta-mcp` calls it for `definition`/`references`/`completions`/`signature`. The load-bearing refactor, deferred to exactly where it's needed. |
| **M6** | Execute — debug sessions | Programmatic breakpoint / inspect / eval / step | Headless `noeta_vm::Debugger` + shared breakpoint index (decision #4); `debug_start`/`debug_inspect`/`debug_eval`/`debug_step`/`debug_stop` over a `session` handle map. Heaviest slice, last. |
| **M7** | Wire auto-registration | The extension registers the MCP server; other clients documented | `vscode.lm.registerMcpServerDefinitionProvider` in `editors/vscode-noeta/` (reusing `noetaCommand()`), `engines.vscode` bump; `docs/Editor-and-AI-Tooling.md` updated with `claude mcp add` + the tool catalog (decision #7). |

`format` rides along in M4 as a thin wrapper over the parallel `noeta fmt` arc (decision #6); if that
arc's reusable entry isn't ready when M4 lands, `format` slips to a one-commit follow-on — it never
blocks the rest of the arc.

---

## Deferred (revisit after the arc)

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
`unsafe`. `format` (and `check`'s lint layer) wrap the parallel `noeta fmt` / `noeta check` arc and
land as soon as that arc exposes a reusable entry; a not-yet-ready `format` is the one gate-exempt
tool (one-commit follow-on).
