# Editor tooling — syntax highlighting (done) + `noeta lsp` language server

**Status: syntax highlighting SHIPPED; LSP is planning (proposal for sign-off).** This track covers the
"editor grammars + VS Code ext" (M3 long tail) and "embedded LSP" (M2 differentiator) roadmap rows —
they are one continuous story, so they live in one plan. The static grammar half is done on branch
`editor-tooling`; this document scopes the language-server half.

## What already shipped (branch `editor-tooling`)

Three commits stood up **static, compiler-free** editor support — it colorizes without running the
compiler, so it works instantly and offline:

- `06b5352` — VS Code TextMate grammar for the full Noeta surface (`.noe`): keywords, the three string
  forms with `${…}` interpolation, every numeric form, directives/tier blocks (`@derive`, `@test`,
  `@bench`, `@doc`), metadata attributes (`#[…]`), and the operators (`|>`, `..`/`..=`, `...`, `??`/`??=`).
- `d5c69ee` — packaged the VS Code extension (`editors/vscode-noeta/`), scoped self + attributes.
- `9cb0773` — tree-sitter grammar (`editors/tree-sitter-noeta/`) for the editors outside the TextMate
  ecosystem (Neovim, Helix, Zed).

The VS Code extension's README already declares the next step, and it is deliberately "structured to host
that client when it lands":

> `noeta lsp` — a language server over the compiler's salsa query graph: live diagnostics first, then
> hover types, go-to-definition, and completion.

That is this plan.

## Why now / why this shape

**The load-bearing infrastructure already exists.** M1 built `noeta-db` — a salsa incremental query graph
(`SourceProgram` input → `tokens` → `ast` → `checked` → `bytecode`, plus the multi-file `Workspace` →
`linked` → `linked_checked`). Its own doc-comments call editing an input "a future incremental-edit / LSP
concern": mutating one source "invalidates exactly the queries that read it." That is the entire hard part
of a responsive language server, and it is done. An LSP is mostly a thin JSON-RPC adapter that:

1. holds each open document's text in its `SourceProgram` input, calling the salsa-generated `set_text`
   setter on `didChange`, and
2. re-reads the `checked` / `linked_checked` queries — salsa recomputes only what the edit touched — and
3. marshals the already-structured results (`Diagnostic`s, the `type_of_sites` map) into LSP wire types.

Two facts make the first two features nearly free:

- **Diagnostics** (`noeta-diagnostics::Diagnostic`) already carry a `Span`, a stable code (`E0037`…), a
  message, and secondary `Label`s — a near-1:1 map onto the LSP `Diagnostic` shape.
- **Hover types** are already computed: `Checked.type_of_sites: HashMap<Span, TypeRepr>` is carried on the
  memoized `checked` query (built for the redundant-passes dedup). Hover is a span lookup, not new work.
- **Position mapping** exists: `noeta_span::Source::line_col(offset)` converts a byte offset to a 1-based
  line/column (column in Unicode scalar values).

So we build the credible version — a correct, tested language server delivering the features developers
reach for first — on infrastructure that was designed for it from M1.

## Decisions proposed (confirm before L0)

1. **Framework = `tower-lsp-server`** (the maintained fork of `tower-lsp`), over the alternatives.
   - *Why not `tower-lsp`:* effectively unmaintained; the community fork `tower-lsp-server` is the live
     continuation with the same async/`tokio` ergonomics.
   - *Why not raw `lsp-server` + `lsp-types` (rust-analyzer's stack):* lower-level, sync, more boilerplate
     (hand-rolled dispatch loop). We do not need its control over the main loop; the batteries-included
     async model composes with the `tokio` already in the tree (`noeta-runtime`).
   - Trade-off flagged for the user: `tower-lsp-server` is a newer crate name; if you'd rather take the
     rust-analyzer stack for its longevity/control, L0 is where we'd swap — nothing downstream depends on
     the choice beyond the L0 adapter.
2. **Transport = stdio only** for this milestone. `noeta lsp` speaks JSON-RPC over stdin/stdout — what
   every editor client spawns by default. (TCP/socket transport is a trivial later add if needed.)
3. **Text sync = full-document to start.** `didChange` replaces the whole buffer and calls `set_text`.
   Salsa still only recomputes affected queries, so full sync is not a performance problem at this stage;
   incremental (range) sync is a later refinement (deferred list).
4. **Position encoding = negotiate UTF-8, fall back to UTF-16.** LSP defaults to UTF-16 offsets; 3.17 lets
   the server advertise `positionEncoding: ["utf-8", "utf-16"]`. Preferring UTF-8 lets us use the compiler's
   native byte offsets directly (no re-encoding); when a client only supports UTF-16 we convert via a
   per-line code-unit index. `Source::line_col` counts *chars*, which equals neither directly, so the
   conversion helper is real work — it lands in L1 and is unit-tested against astral-plane fixtures.
5. **Scope = single-file semantics first, workspace second.** L1–L2 (diagnostics, hover) run the per-file
   `checked` query — instant value, no module resolution. Cross-file features (go-to-def across modules)
   switch to the `Workspace`/`linked_checked` queries in L3+, reusing `noeta_loader::read_workspace` for
   sibling-module discovery.
6. **`noeta-lsp` is a new crate + a `noeta lsp` CLI subcommand.** The server logic lives in `noeta-lsp`
   (depends on `noeta-db`, `noeta-diagnostics`, `noeta-span`, `noeta-loader`, `tokio`, `tower-lsp-server`);
   `noeta-cli` gains a thin `Command::Lsp` arm that starts it. Keeps the CLI crate free of the LSP
   dependency weight except at the entry point, mirroring how `noeta-runtime` is CLI-only.

## Architecture — where the pieces live

```
editor  ──JSON-RPC/stdio──►  noeta lsp  (noeta-cli Command::Lsp)
                                  │
                                  ▼
                         noeta-lsp crate
              ┌──────────────────────────────────────┐
              │ Backend (tower-lsp-server LanguageServer)│
              │  • one LangDatabase                    │
              │  • URI → SourceProgram input map       │
              │  • position ⇄ offset (utf-8/utf-16)    │
              └───────────────┬──────────────────────┘
                              │ set_text / query
                              ▼
                        noeta-db (salsa)
              checked · linked_checked · ast   (already built)
                              │
        ┌─────────────────────┼─────────────────────┐
   Diagnostic (L1)   type_of_sites (L2)      def index (L3)
```

The server owns exactly two pieces of new state: **one `LangDatabase`** and a **`HashMap<Url, SourceProgram>`**
mapping each open document to its salsa input. Everything else is a query read. This is why the feature
staging is so clean — each slice adds one request handler and one small marshaller, never new compiler state.

## Slices

Deliver one editor-visible feature per slice, each independently useful. Because the server is I/O-facing,
the differential/leak oracles don't apply directly; each slice is tested with **LSP request/response
fixtures** (drive the `Backend` in-process, feed a document + a request, assert the response) plus unit
tests for the position/marshalling helpers.

| # | Slice | Delivers | Notes |
|---|-------|----------|-------|
| **L0** | Server skeleton + lifecycle | A server an editor can connect to | `noeta-lsp` crate + `noeta lsp` subcommand; stdio JSON-RPC; `initialize`/`initialized`/`shutdown`; capability advertisement; `didOpen`/`didChange`/`didClose` maintaining the URI→`SourceProgram` map (full sync). No language features yet. **Framework decision lands here.** |
| **L1** | Live diagnostics *(the headline)* | Red squiggles as you type | On open/change, read `checked`, map each `Diagnostic`→LSP (severity, `E0xxx` code, `line_col` range, related-info from `Label`s), `publishDiagnostics`. Debounce changes. Ships the position-encoding helper (decision 4) with astral-plane unit tests. |
| **L2** | Hover types | Type-on-hover | `textDocument/hover`: position→byte offset→smallest enclosing span in `type_of_sites`→render `TypeRepr`. Pure read of an existing map. |
| **L3** | Go-to-definition | Jump to declaration | Needs a def/use index. First audit what the checker/binder already resolves; add a lightweight symbol-index query if not. Single-file first, then cross-module via `linked` (decision 5). |
| **L4** | Document symbols / outline | Breadcrumbs + symbol search | Walk the AST for `fn`/`struct`/`class`/`enum`/`impl` → `DocumentSymbol` tree. Cheap, low-risk, no new compiler state. |
| **L5** | Completion | Autocomplete | Keywords + in-scope symbols + member/method completion. Largest slice; scope tightly (a follow-on plan may split it). |
| **L6** | Wire the VS Code client | The extension launches the server | Add the language-client half to `editors/vscode-noeta/` so opening a `.noe` file spawns `noeta lsp`. The extension is already structured to host it. |

## Deferred (revisit after the arc)

- **Incremental (range) text sync** — full-document sync first (decision 3).
- **Semantic tokens** — could eventually *supersede* the TextMate grammar with compiler-accurate coloring;
  a natural sequel once L2's type info flows.
- **Find references, rename, code actions** (quick-fixes materialized from diagnostic labels), **signature
  help**, **formatting** — the second tier of features, planned after L1–L5 prove the adapter shape.
- **Multi-root workspaces** and **watched-file / on-disk change** handling beyond open editors.
- **Marketplace / registry publishing** of the VS Code and tree-sitter grammars.

## Gate — this milestone is done when

`noeta lsp` starts over stdio, a VS Code (or any LSP) client connects, and editing a `.noe` file produces
live diagnostics, hover types, go-to-definition, document outline, and completion, each covered by
in-process request/response fixtures, with the workspace clean under fmt/clippy and zero new `unsafe`.
