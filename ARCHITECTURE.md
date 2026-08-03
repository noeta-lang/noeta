# Architecture

This document is the technical overview of the **implementation**. The language's *design* — its semantics, feature set, and the "why" behind each technique — is documented in the [wiki](docs/Home.md), especially the [Concepts & design](docs/Architecture-and-Pipeline.md) section. This file describes the codebase as it actually exists and how its pieces fit together. `plans/roadmap.md` points at the current frontier and `plans/backlog.md` registers everything open.

> [!NOTE]
> **Status.** The language core ships: the register-based bytecode **VM** + a Cranelift Tier-1 **JIT**, NaN-boxed **value model** + shape-based **object model**, precise refcount + cycle **GC**, the inferred-static bidirectional **type checker**, a **salsa** incremental query graph, traits/derives/generics/attributes+reflection, multi-file **modules**, a layered **stdlib**, and a real **host IO** boundary (sandbox + real disk/network) over twelve capability traits. On top of that the toolchain has grown well past M2: native **AOT** builds (`noeta-aot-runtime`/`noeta-bundle`), a **package manager** with keyless signing (`noeta-pm`), editor & agent tooling (`noeta-lsp`/`noeta-dap`/`noeta-mcp`), a canonical **formatter** (`noeta-fmt`) and **profiler** (`noeta-prof`), **reactivity** (`noeta-reactive`), and OTLP **telemetry**; the `para` package family (LiveView HTML, api middleware, cli, aether, db, p2p/CRDTs) lives in its own repos under the noeta-lang org. The differential oracle is now **Core-IR interpreter ↔ VM** (the original M0 AST tree-walker was retired in the memory-management migration; `noeta-eval` survives as the IR interpreter). Open work is tracked in `plans/` (`roadmap.md` + `backlog.md`); completed arc ledgers live in git history.

## Compilation pipeline

```
source (.noe)
   │  noeta-lexer (logos)
   ▼
tokens ──► noeta-parser (chumsky) ──► AST (noeta-ast)
                                       │
                                       ▼
                             noeta-check (types + local inference)   ── a shared front-end:
                                       │                                a program with type errors
                                       ▼                                emits diagnostics and does not run
                             noeta-ir (ANF lowering)
                                       │
                                       ▼
                             noeta-ir-passes (liveness ─► drop insertion ─► in-place reuse)
                                       │
                        ┌──────────────┴───────────────────────────────┐
                        ▼                                               ▼
                   noeta-eval                              noeta-compiler ─► Chunk/Module (noeta-bytecode)
            (Core-IR interpreter, the oracle)                            ─► noeta-vm (register VM)
                        │                                               │
                        └───────────────────► RunResult ◄──────────────┘
                              (observable output; the two are asserted identical)
```

Every stage emits typed `Diagnostic`s rendered in exactly one place (`noeta-diagnostics` over `ariadne`). The whole pipeline is expressed as **salsa** queries (`noeta-db`: `tokens → ast → checked → linked → bytecode`), so editing one module recomputes only its dependents. Each stage is a separate crate with an explicit input and output type and no hidden shared mutable state, so a change to one stage is local to its crate and verifiable by that stage's snapshots. This staging is what makes the codebase tractable for agentic development (see `AGENTS.md`).

## Crate map

Dependency edges form a strict DAG (no back-edges): `noeta-span` is depended on by everyone; `noeta-cli` depends on (nearly) everything. The two backends are **siblings** — neither depends on the other; both meet at the `Backend`/`RunResult` seam.

**Name disambiguation:** the language *runtime* is `noeta-vm`; `noeta-runner` is the lean execution core the prod artifacts staple onto; `noeta-aot-runtime` is the AOT link archive for `--native` builds; `noeta-host-real` is only the CLI's real-IO `Host`; `noeta-ext-abi` is the extension ABI (nothing to do with AOT); and `noeta-compiler` is only IR→bytecode — the "compiler" a newcomer expects is check + compiler + loader together.

### Frontend
| Crate | Role |
|---|---|
| `noeta-span` | `Span`, `SourceId`, `SourceMap`, offset ↔ line:col (shared vocabulary). |
| `noeta-diagnostics` | The one error catalog (`DiagnosticCode`, stable `E0xxx`) + the single `ariadne` renderer. |
| `noeta-ast` | AST node types (pure data, every node carries a `Span`) + `SyntaxKind` + reflection/pretty helpers. `native_reflect` is reflection's other half: the ONE seam that answers for a declaration living in the extension registry rather than the AST, resolved lazily on a lookup miss instead of materialized into every artifact. |
| `noeta-lexer` | source `&str` + `SourceId` → token stream + lex diagnostics (logos). |
| `noeta-parser` | token stream → `(Ast, Vec<Diagnostic>)` (chumsky). |

### Types
| Crate | Role |
|---|---|
| `noeta-types` | The structural `Type` lattice + subtyping (one documented function); built-in trait enum. |
| `noeta-check` | Bidirectional type checker + local inference, run as a shared front-end upstream of both backends. Also collects codegen-hint site maps consumed during lowering. |

### IR & memory management
| Crate | Role |
|---|---|
| `noeta-ir` | ANF intermediate representation + AST→IR lowering (the RC-migration target). |
| `noeta-ir-passes` | Precise reference-counting analyses/transforms over the IR: liveness → drop insertion → in-place reuse. |

### Backends
| Crate | Role |
|---|---|
| `noeta-backend` | The execution-backend seam: `trait Backend { fn run(&Program) -> RunResult }`. Tiny shared vocabulary both backends depend on. |
| `noeta-eval` | Core-IR → `RunResult` (the **differential-oracle reference**, `Rc`-based value model). Began as the M0 AST tree-walker; that walk was retired in the RC migration and the crate now interprets the same RC-annotated IR the VM runs. |
| `noeta-compiler` | IR → `Chunk`/`Module` bytecode; register allocation (graph colouring). |
| `noeta-bytecode` | Opcode set, `Chunk`/`Module`, constant pool (pure data). |
| `noeta-vm` | `Module` → `RunResult` (the Tier-0 register VM, `VmBackend`) over NaN-boxed values + inline caches; owns the tier-promotion counters and the JIT runtime helpers. |
| `noeta-jit` | The Tier-1 method JIT (Cranelift, behind the `jit` cargo feature): compiles hot prototypes to native code holding VM registers in SSA (typed/unboxed where provable), with a bail-to-interpreter deopt contract and per-call-site inline caches. The sandbox/differential path never uses it. |
| `noeta-jit-abi` | The frozen calling-convention/ABI vocabulary shared between `noeta-vm` and `noeta-jit` (native ↔ interpreter frame contract). |
| `noeta-aot-runtime` / `noeta-bundle` | Ahead-of-time native builds (`noeta build --native`): the AOT runtime support crate and the self-contained artifact bundler (per-ring stdlib + DCE). |
| `noeta-builtins` | The prelude constructors (`Ok`/`Err`/`some`/`none`, `echo`, collection builtins). |

### VM value model
| Crate | Role |
|---|---|
| `noeta-object` | Shapes (hidden classes): the flat-slot layout descriptor for structs/classes/enums. Pure data below `noeta-value`. |
| `noeta-value` | The NaN-boxed `Value` + heap payloads (strings, closures, collections, shaped objects, cells, file handles). The NaN-boxing `unsafe` lives here, miri-gated (see the `unsafe` quarantine below). |
| `noeta-gc` | Refcount + `__destruct` policy + Bacon–Rajan cycle collector over `noeta-value`. |

### Shared runtime & host
| Crate | Role |
|---|---|
| `noeta-ext-abi` | The extension **ABI**: the `Host` supertrait (twelve capability traits — filesystem, rng, clock, env, console, os, entropy, ids, network, tracing, metrics, logging — plus the `P2pProvider` seam that replaced the in-union p2p capability, para-namespace F2b), `ExtModule`/`ExtType`/`ExtFn` registration, the assembled `Registry` + the process-default slot (`install`/`single_registry_process`), and the neutral `NativeValue`/`PackedView` marshalling seam. What third-party native packages link against — and the **only** extension crate the front-end (`noeta-check`/`noeta-loader`/`noeta-compiler`/`noeta-ir`/`noeta-db`) links: they consume the registry as *data* seeded by the assembling driver, so a stdlib method-body edit rebuilds none of them (audit-6 F2). |
| `noeta-stdlib` | The **shared semantics layer**: Ring 1/Ring 2 stdlib + `SandboxHost` (deterministic) implemented over the `noeta-ext-abi` ABI. Both backends route through it so behaviour cannot drift. Linked by the runtimes and drivers (vm/eval/runtime/runner/embed/tooling/cli) — the driver seeds the process-default registry (`registry::default_seeded()`/`install_with_extras`) before the front-end's first lookup. |
| `noeta-host-real` | `RealHost`: per-isolate tokio, real disk async + real `env`/`args`/network + telemetry export. CLI/REPL only; never differential-tested. |
| `noeta-reactive` | Signals/computed/effects: the reactive graph, topological flush, and the E0045 runaway guard. |

### Modules, incremental, tooling
| Crate | Role |
|---|---|
| `noeta-loader` | Multi-file module loading + linking: walks the package, derives each module's path from where its file sits (`derive.rs`; E0072/E0073/E0074), resolves `use` to module declarations honoring `pub`, merges into one `Program`. |
| `noeta-db` | The salsa 0.27 query graph tying the pipeline together (`Workspace`/`linked`/`checked`/`bytecode`). |
| `noeta-cache` | Default-on bytecode cache (`~/.cache/noeta/*.noeb`, build-identity-keyed): the `compile_whole_file` seam for run/dump/build. |
| `noeta-conformance` | The dev-only harness: `// expect:` corpus runner, `--differential` oracle, JSON output, partial runs. |
| `noeta-alloc-probe` | Test-only global-allocator probe for heap-residency assertions. |
| `noeta-test-temp` | Test-only, and the one place the machine-shared resources a test needs are built, so no two test processes or checkouts collide: per-process fixture directories, free loopback ports, and the readiness waits (`wait_until_listening_or_child_exits`, `wait_until_closed`) that used to be ten hand-written poll loops with a fixed four-second budget. |
| `noeta-cli` | The `noeta` binary. Core verbs `run`/`build`/`check`/`repl`, `test`/`bench`/`doc`, `dump`/`fmt`/`profile`/`cache`, the editor/agent servers `lsp`/`dap`/`mcp`, and the package-manager verbs `add`/`update`/`publish`/`audit`/`key` (plus the dynamically-wired `serve`). |

### Editor, agent & dev tooling
| Crate | Role |
|---|---|
| `noeta-ide` | Shared IDE engine (hover, go-to-def, outline, references, call/role graph) over the salsa db, reused by both the LSP and the MCP server. |
| `noeta-lsp` | `noeta lsp`: tower-lsp language server (diagnostics/hover/def/refs/rename/completion/semantic-tokens/inlay-hints/formatting). |
| `noeta-dap` | `noeta dap`: debug adapter driving the production VM (breakpoints/stepping/scopes/variables) via a per-op debug hook. |
| `noeta-mcp` | `noeta mcp`: agent-native MCP server serving the reflection manifest + ~27 tools over stdio. |
| `noeta-fmt` | `noeta fmt`: the canonical formatter (lex+trivia → AST → Doc → text), also driving LSP formatting. |
| `noeta-prof` | `noeta profile`: the dev profiler/flamegraph (tier-0 VM), folded/inferno-SVG/speedscope output. |
| `noeta-pm` | The package manager: manifest/lockfile, dependency resolution (path/git/registry), keyless Sigstore signing + provenance verification, native-package toolchain composition. |

## Key implementation decisions

- **Two backends, one differential oracle.** The Core-IR interpreter (`noeta-eval`) and the bytecode VM (`noeta-vm`) are run through `trait Backend` against the same programs, and their `RunResult`s (observable output, not internal representation) are asserted identical. Comparing output — not value layout — is exactly what lets the two backends use completely different memory machines (the interpreter's `Rc`-based enum vs. the VM's NaN-boxed words on a manual refcount heap) while executing the *same* RC-annotated IR. (Historically the reference was the M0 AST tree-walker; it was retired in the memory-management migration because it fired destructors only at teardown and so couldn't reproduce last-use destruction.) This oracle is the spine of the test strategy.
- **Tiered execution, oracle-gated.** The VM is Tier 0; a Cranelift method JIT (`noeta-jit`, `jit` feature) is Tier 1, compiling hot prototypes with registers held in SSA and a bail-before-mutate deopt contract onto the interpreter's own register stack. Tier 1 has its own differential gate (`--jit-differential`: forced-JIT vs interpreter, byte-identical output, zero leaks, zero refcount anomalies) so native code can never silently disagree with the interpreter. See [The Virtual Machine → Tier 1](docs/The-Virtual-Machine.md).
- **Shared semantics live once, in `noeta-stdlib`.** Anything both backends must agree on (stdlib method bodies, host IO, float formatting, the neutral marshalling seam) is factored into `noeta-stdlib` and dispatched through exhaustive enums (`ListMethod`/`MapMethod`/…) rather than per-backend string matching, so the compiler catches a missing case. (Routing/dispatch that is still mirrored between the backends is inventoried — with a keep/lift decision per piece — in `plans/backend-mirror.md`.)
- **Errors as data, centralized.** Every diagnostic is a typed variant with a stable code in `noeta-diagnostics`, rendered in exactly one place. No ad-hoc error strings in the stages.
- **Precise reference counting via an ANF IR.** Memory management is compiled, not traced: `noeta-ir` lowers to ANF and `noeta-ir-passes` inserts drops at last-use and rewrites unique-owner mutations into in-place reuse. A cycle collector backstops reference cycles — at clean exit *and* mid-run at interpreter safepoints (loop back-edges, frame transfers, scheduler drive rounds), so a cycle-building loop's peak residency stays bounded. A safepoint collection never runs a destructor: destructor-bearing dead components are deferred to the exit collection (unobservable reclamation is what lets the two backends collect at unsynchronized points while the differential stays byte-identical). See the [Memory Management](docs/Memory-Management.md) wiki page.
- **Surface sugar stays in the AST.** Constructs like `?T`, `|>`, `~`, `?`, `??` are distinct AST nodes (not desugared in the parser) so later passes can produce precise diagnostics.
- **Incremental by construction (salsa).** The compiler is a `salsa` query graph (`noeta-db`); the sharp crate seams are what keep the queries mechanical.
- **`unsafe` is quarantined.** The workspace forbids `unsafe` via a `[workspace.lints]` table (`unsafe_code = "forbid"`); a short list of crates opt out with justification — the NaN-boxing and the cycle collector's refcount/graph touch points in `noeta-value`'s heap module (`noeta-gc` itself stays `unsafe`-free — it holds only the collector *policy*), a trivially-sound salsa `Update` impl in `noeta-db`, a SIMD reinterpret in `noeta-stdlib`, the test-only `noeta-alloc-probe`, and the Tier-1 seam: `noeta-jit` (finalizing a code pointer, reading the baked frame template) and `noeta-vm`'s `jit`-feature helpers (`deny` + per-function `#[allow]`: reconstituting the VM from the native ABI's pointers, and the fast call convention's `set_len` window reserve). The compiler and every default-feature interpreter path stay `unsafe`-free. `unsafe` that can execute under miri is miri-checked; JIT-generated native code cannot be, so its contracts are gated by the `--jit-differential` oracle (byte-identity + zero leaks + zero refcount anomalies) instead.

## Testing architecture

See the [Contributing guide](docs/Contributing.md) for the full strategy. In short: each pipeline stage is snapshot-tested at its own boundary (`insta` — tokens, AST, rendered diagnostics), and end-to-end behavior is an executable conformance corpus under `tests/conformance/` (`.noe` files with `// expect:` headers), run through both backends and asserted identical (`--differential`, `0 skipped`). `proptest` covers invariants (parse→print→parse round-trips, evaluator-no-panic); `miri` covers the quarantined `unsafe`. The conformance suite runs through the **dev-only `noeta-conformance` binary** (`cargo run -p noeta-conformance`), kept out of the shipped `noeta` CLI so the `noeta test` verb is free for a user program's own `@test {}` blocks.
