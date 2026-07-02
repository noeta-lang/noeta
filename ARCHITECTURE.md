# Architecture

This document is the technical overview of the **implementation**. The canonical *design* (the language's semantics, feature set, and rationale) lives in `docs/resources/01-architecture.md`; read that for the deep "why." This file describes the codebase as it actually exists and how its pieces fit together. `plans/roadmap.md` is the authoritative "what's done / what's next."

> [!NOTE]
> **Milestone status: M1 complete, M2 cluster 1 complete.** The register-based bytecode **VM**, NaN-boxed **value model** + shape-based **object model**, refcount + cycle **GC**, the bidirectional **type checker**, a **salsa** incremental query graph, traits/derives/generics, multi-file **modules**, a layered **stdlib**, and a real **host IO** boundary (sandbox + real disk) all exist and ship. The M0 tree-walker is **retained forever** as the differential oracle, never deleted. See `plans/roadmap.md` for the per-slice ledger; several later tracks (object-model redesign, reflection, isolates, an inferred-static type track) live on their own branches.

## Compilation pipeline

```
source (.lang)
   │  lang-lexer (logos)
   ▼
tokens ──► lang-parser (chumsky) ──► AST (lang-ast)
                                       │
                                       ▼
                             lang-check (types + local inference)   ── a shared front-end:
                                       │                                a program with type errors
                                       ▼                                emits diagnostics and does not run
                             lang-ir (ANF lowering)
                                       │
                                       ▼
                             lang-ir-passes (liveness ─► drop insertion ─► in-place reuse)
                                       │
                        ┌──────────────┴───────────────────────────────┐
                        ▼                                               ▼
                   lang-eval                              lang-compiler ─► Chunk/Module (lang-bytecode)
              (tree-walker, the oracle)                                  ─► lang-vm (register VM)
                        │                                               │
                        └───────────────────► RunResult ◄──────────────┘
                              (observable output; the two are asserted identical)
```

Every stage emits typed `Diagnostic`s rendered in exactly one place (`lang-diagnostics` over `ariadne`). The whole pipeline is expressed as **salsa** queries (`lang-db`: `tokens → ast → checked → linked → bytecode`), so editing one module recomputes only its dependents. Each stage is a separate crate with an explicit input and output type and no hidden shared mutable state, so a change to one stage is local to its crate and verifiable by that stage's snapshots. This staging is what makes the codebase tractable for agentic development (see `AGENTS.md`).

## Crate map

Dependency edges form a strict DAG (no back-edges): `lang-span` is depended on by everyone; `lang-cli` depends on (nearly) everything. The two backends are **siblings** — neither depends on the other; both meet at the `Backend`/`RunResult` seam.

### Frontend
| Crate | Role |
|---|---|
| `lang-span` | `Span`, `SourceId`, `SourceMap`, offset ↔ line:col (shared vocabulary). |
| `lang-diagnostics` | The one error catalog (`DiagnosticCode`, stable `E0xxx`) + the single `ariadne` renderer. |
| `lang-ast` | AST node types (pure data, every node carries a `Span`) + `SyntaxKind` + reflection/pretty helpers. |
| `lang-lexer` | source `&str` + `SourceId` → token stream + lex diagnostics (logos). |
| `lang-parser` | token stream → `(Ast, Vec<Diagnostic>)` (chumsky). |

### Types
| Crate | Role |
|---|---|
| `lang-types` | The structural `Type` lattice + subtyping (one documented function); built-in trait enum. |
| `lang-check` | Bidirectional type checker + local inference, run as a shared front-end upstream of both backends. Also collects codegen-hint site maps consumed during lowering. |

### IR & memory management
| Crate | Role |
|---|---|
| `lang-ir` | ANF intermediate representation + AST→IR lowering (the RC-migration target). |
| `lang-ir-passes` | Precise reference-counting analyses/transforms over the IR: liveness → drop insertion → in-place reuse. |

### Backends
| Crate | Role |
|---|---|
| `lang-backend` | The execution-backend seam: `trait Backend { fn run(&Program) -> RunResult }`. Tiny shared vocabulary both backends depend on. |
| `lang-eval` | AST/IR → `RunResult` (the M0 tree-walker, **frozen as the differential oracle**). `Rc`-based value model. |
| `lang-compiler` | IR → `Chunk`/`Module` bytecode; register allocation (graph colouring). |
| `lang-bytecode` | Opcode set, `Chunk`/`Module`, constant pool (pure data). |
| `lang-vm` | `Module` → `RunResult` (the register VM, `VmBackend`) over NaN-boxed values + inline caches. |
| `lang-builtins` | The prelude constructors (`Ok`/`Err`/`some`/`none`, `echo`, collection builtins). |

### VM value model
| Crate | Role |
|---|---|
| `lang-object` | Shapes (hidden classes): the flat-slot layout descriptor for structs/classes/enums. Pure data below `lang-value`. |
| `lang-value` | The NaN-boxed `Value` + heap payloads (strings, closures, collections, shaped objects, cells, file handles). **The one crate whose source uses `unsafe`** (NaN-boxing), miri-gated. |
| `lang-gc` | Refcount + `__destruct` policy + Bacon–Rajan cycle collector over `lang-value`. |

### Shared runtime & host
| Crate | Role |
|---|---|
| `lang-stdlib` | The **shared semantics layer**: Ring 1/Ring 2 stdlib, the `Host` capability trait + `SandboxHost` (deterministic), the neutral `NativeValue` marshalling seam. Both backends route through it so behaviour cannot drift. |
| `lang-runtime` | `RealHost`: per-isolate tokio, real disk async + real `env`/`args`. CLI/REPL only; never differential-tested. |

### Modules, incremental, tooling
| Crate | Role |
|---|---|
| `lang-loader` | Multi-file module loading + linking: resolves `use` to sibling-module declarations honouring `pub`, merges into one `Program`. |
| `lang-db` | The salsa 0.27 query graph tying the pipeline together (`Workspace`/`linked`/`checked`/`bytecode`). |
| `lang-conformance` | The dev-only harness: `// expect:` corpus runner, `--differential` oracle, JSON output, partial runs. |
| `lang-alloc-probe` | Test-only global-allocator probe for heap-residency assertions. |
| `lang-cli` | The `lang` binary: `run`, `repl`, `test`, `bench`, `doc`. |

## Key implementation decisions

- **Two backends, one differential oracle.** The frozen tree-walker (`lang-eval`) and the bytecode VM (`lang-vm`) are run through `trait Backend` against the same programs, and their `RunResult`s (observable output, not internal representation) are asserted identical. Comparing output — not value layout — is exactly what lets the two backends use completely different value models (the tree-walker's `Rc`-based enum vs. the VM's NaN-boxed words). This oracle is the spine of the test strategy.
- **Shared semantics live once, in `lang-stdlib`.** Anything both backends must agree on (stdlib method bodies, host IO, float formatting, the neutral marshalling seam) is factored into `lang-stdlib` and dispatched through exhaustive enums (`ListMethod`/`MapMethod`/…) rather than per-backend string matching, so the compiler catches a missing case. (Routing/dispatch that is still mirrored between the backends is a known debt tracked in `plans/`.)
- **Errors as data, centralized.** Every diagnostic is a typed variant with a stable code in `lang-diagnostics`, rendered in exactly one place. No ad-hoc error strings in the stages.
- **Precise reference counting via an ANF IR.** Memory management is compiled, not traced: `lang-ir` lowers to ANF and `lang-ir-passes` inserts drops at last-use and rewrites unique-owner mutations into in-place reuse. A cycle collector backstops reference cycles. See `docs/resources/05-memory-management.md`.
- **Surface sugar stays in the AST.** Constructs like `?T`, `|>`, `~`, `?`, `??` are distinct AST nodes (not desugared in the parser) so later passes can produce precise diagnostics.
- **Incremental by construction (salsa).** The compiler is a `salsa` query graph (`lang-db`); the sharp crate seams are what keep the queries mechanical.
- **`unsafe` is quarantined.** The workspace forbids `unsafe` via a `[workspace.lints]` table (`unsafe_code = "forbid"`); a short list of crates opt out with justification — the NaN-boxing in `lang-value`, the cycle collector's touch points in `lang-gc`, a trivially-sound salsa `Update` impl in `lang-db`, a SIMD reinterpret in `lang-stdlib`, and the test-only `lang-alloc-probe`. The VM and compiler themselves are `unsafe`-free. All `unsafe` is miri-checked.

## Testing architecture

See `docs/resources/03-implementation-plan.md` §6 for the full strategy. In short: each pipeline stage is snapshot-tested at its own boundary (`insta` — tokens, AST, rendered diagnostics), and end-to-end behavior is an executable conformance corpus under `tests/conformance/` (`.lang` files with `// expect:` headers), run through both backends and asserted identical (`--differential`, `0 skipped`). `proptest` covers invariants (parse→print→parse round-trips, evaluator-no-panic); `miri` covers the quarantined `unsafe`. The conformance suite runs through the **dev-only `lang-conformance` binary** (`cargo run -p lang-conformance`), kept out of the shipped `lang` CLI so the `lang test` verb is free for a user program's own `@test {}` blocks.
