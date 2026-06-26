# Architecture

This document is the technical overview of the **implementation**. The canonical *design* (the language's semantics, feature set, and rationale) lives in `docs/resources/01-architecture.md`; read that for the deep "why." This file describes the codebase as it actually exists and how its pieces fit together.

> [!NOTE]
> The implementation is at **M0 (walking skeleton)**. Many subsystems described in the design docs (bytecode VM, type checker, shape-based object model, GC, server, LSP) do not exist yet. This document is updated as each milestone lands; it never describes vaporware as if it were built.

## Compilation pipeline (M0)

```
source (.lang)
   │  lang-lexer (logos)
   ▼
tokens ──► lang-parser (chumsky) ──► AST (lang-ast) ──► lang-eval (tree-walker) ──► RunResult
   │                  │                                          │
   └──────────────────┴─── lang-diagnostics (ariadne) ──────────┘
        (every stage emits typed Diagnostics; one renderer)
```

Each stage is a separate crate with an explicit input and output type and no hidden shared mutable state, so a change to one stage is local to its crate and verifiable by that stage's snapshots. This staging is what makes the codebase tractable for agentic development (see `AGENTS.md`).

## Crate map (M0)

| Crate | Takes in | Emits |
|---|---|---|
| `lang-span` | — | `Span`, `SourceId`, `SourceMap`, offset ↔ line:col (shared vocabulary) |
| `lang-diagnostics` | `Diagnostic` values from any stage | the one error catalog (`DiagnosticCode`, stable `E0xxx`) + the single `ariadne` renderer |
| `lang-ast` | — | AST node types (pure data, every node carries a `Span`) + the `SyntaxKind` enum |
| `lang-lexer` | source `&str` + `SourceId` | token stream + lex diagnostics |
| `lang-parser` | token stream | `(Ast, Vec<Diagnostic>)` |
| `lang-eval` | `Ast` | `RunResult { stdout, exit_code, diagnostics }`, behind the `Backend` trait |
| `lang-builtins` | — | the M0 prelude (`echo`, `map`/`filter`/`sum`, `len`/`count`/`enumerate`, `next_id`, `Ok`/`Err`/`some`/`none`) |
| `lang-conformance` | the `.lang` corpus | the harness: `// expect:` runner, JSON output, `--stage`/`--file` partial runs, differential-mode hook |
| `lang-cli` | CLI args | the `lang` binary: `run`, `repl`, `test` |

Dependency edges form a strict DAG (no back-edges): `lang-span` is depended on by everyone; `lang-cli` depends on everything.

## Key implementation decisions

- **Errors as data, centralized.** Every diagnostic is a typed variant with a stable code in `lang-diagnostics`, rendered in exactly one place. No ad-hoc error strings in the stages.
- **The `Backend`/`RunResult` seam.** The evaluator runs programs through `trait Backend { fn run(&self, ast) -> RunResult }` and never writes stdout or exits the process directly. `TreeWalkBackend` is the only M0 backend; in M1 the bytecode VM becomes a second backend and the two are cross-checked (a free differential oracle).
- **Surface sugar stays in the AST.** Constructs like `?T`, `|>`, `~`, `?`, `??` are distinct AST nodes (not desugared in the parser) so later passes can produce precise diagnostics.
- **`SyntaxKind` defined early.** The lossless `rowan` CST (for the M2 LSP/formatter) is not built in M0, but its `SyntaxKind` enum is defined now so the concrete-syntax decisions remain recoverable.
- **No `unsafe` in M0.** Every crate is `#![forbid(unsafe_code)]`. The first `unsafe` arrives with the M1 `vm` (NaN-boxing) and `gc` crates, quarantined to those crates and checked with `miri`.
- **No salsa yet.** M0 is straight-line function calls. The compiler is reorganized into a `salsa` query graph starting with the M1 type checker; the sharp crate seams above are what make that reorganization mechanical.

## Testing architecture

See `docs/resources/03-implementation-plan.md` §6 for the full strategy. In short: each pipeline stage is snapshot-tested at its own boundary (`insta` — tokens, AST, rendered diagnostics), and end-to-end behavior is an executable conformance corpus under `tests/conformance/` (`.lang` files with `// expect:` headers). `proptest` covers invariants (parse→print→parse round-trips, evaluator-no-panic). The conformance suite runs through the **dev-only `lang-conformance` binary** (`cargo run -p lang-conformance`), kept out of the shipped `lang` CLI so the `lang test` verb is free for a user program's own `@test {}` blocks (object-model slice 6).
