# Architecture & Pipeline

This section is the "how it works under the hood" tour, for the curious and the systems-minded. It describes the implementation as it actually exists — and, where the design intends more than what has shipped, says so.

The implementation is a workspace of ~50 small Rust crates forming a strict dependency DAG (the crate map below covers the pipeline core; the tooling and runtime subsystems — JIT, package manager, LSP/DAP/MCP, formatter, profiler, reactivity — add the rest). Two ideas organize everything: a **compilation pipeline** of sharp, single-purpose stages, and a **two-backend differential oracle** that keeps the whole thing honest.

## The pipeline

```text
source (.noe)
   │  noeta-lexer (logos)
   ▼
tokens ──► noeta-parser (chumsky) ──► AST (noeta-ast)
                                       │
                                       ▼
                             noeta-check (types + local inference)   ── a shared front-end:
                                       │                                a program with type errors
                                       ▼                                is rejected before it runs
                             noeta-ir (ANF lowering)
                                       │
                                       ▼
                             noeta-ir-passes (liveness ─► drop insertion ─► in-place reuse)
                                       │
                        ┌──────────────┴───────────────────────────────┐
                        ▼                                               ▼
                   noeta-eval                              noeta-compiler ─► Chunk/Module
              (the reference interpreter)                                ─► noeta-vm (register VM)
                        │                                               │
                        └───────────────────► RunResult ◄──────────────┘
                              (observable output; the two are asserted identical)
```

Each stage is a separate crate with an explicit input and output type and no hidden shared mutable state, so a change to one stage is local to its crate and verifiable by that stage's snapshot tests. This staging is what makes the codebase tractable to work on.

## The two-backend differential oracle

The language is implemented **twice**:

- **`noeta-eval`** — a tree/IR-walking interpreter with an `Rc`-based value model.
- **`noeta-vm`** — a register-based bytecode VM over NaN-boxed 64-bit words.

Both implement one trait — `trait Backend { fn run(&self, program: &Program) -> RunResult }` — and the conformance harness runs every program through *both*, asserting their `RunResult` (stdout + exit code + diagnostics) is byte-for-byte identical, with a hard **`0 skipped`** gate.

Why build it twice? A second independent implementation is a continuously-running oracle: any divergence between the two is a bug in one of them, caught mechanically instead of by hand-written expected output. Crucially, the comparison is on *observable behavior*, not internal representation — which is exactly what frees the two backends to use completely different value models (an `Rc` enum vs. NaN-boxed words). This oracle is the spine of the whole test strategy, and it constrains the design: any shared semantics that both backends must agree on live *once*, in `noeta-stdlib`, so they cannot drift.

> [!NOTE]
> **A precise nuance.** The reference backend is the **Core-IR interpreter** — `noeta-eval` interpreting the RC-annotated ANF IR — because last-use destructor timing is a property of the IR, and only an interpreter of that IR can reproduce it exactly. The live differential is therefore *Core-IR interpreter ↔ VM*: two genuinely different memory machines (Rust `Rc` vs. a manual refcount heap) executing the *same* RC-annotated IR. An AST-level tree-walker cannot serve as the reference here: it would fire destructors only at global teardown, so it could not witness the last-use timing the IR encodes.

## Incremental compilation (salsa)

The pipeline is not just a straight line of function calls — it is expressed as a graph of memoized **queries** using [salsa](https://github.com/salsa-rs/salsa) 0.27 (the framework rust-analyzer is built on), in the `noeta-db` crate. The `SourceProgram` input feeds tracked queries `tokens → ast → checked → bytecode`; the module graph adds `Workspace → linked → linked_checked → linked_bytecode`. Editing one module recomputes only its transitive dependents.

This is not a separate feature bolted on — it is the *same* query graph that would power a responsive LSP, hot-module-reload blast-radius analysis, and agent-local change verification. The sharp crate seams are what make the queries mechanical. (salsa requires memoized returns to be `Update + PartialEq`, so three foreign artifact types are wrapped in local newtypes with conservative "always-changed" impls.)

## Diagnostics as data

Every diagnostic is a typed variant with a stable `E0xxx` code, defined once in `noeta-diagnostics` and rendered in exactly one place (over `ariadne`). Stages never format error strings themselves — they emit `Diagnostic` values that flow to the one renderer. This is what lets a negative conformance test assert with a `// expect: error E0xxx` header, and what lets the checker promote a would-be runtime error to a compile-time one while keeping the differential green: the diagnostic *is* the observable result, identical on both backends.

## Crate map

The dependency edges form a strict DAG (no back-edges). `noeta-span` is depended on by everyone; `noeta-cli` depends on nearly everything; the two backends are siblings that meet only at the `Backend`/`RunResult` seam.

| Layer | Crates |
|---|---|
| Shared vocabulary | `noeta-span`, `noeta-diagnostics`, `noeta-ast` |
| Frontend | `noeta-lexer` (logos), `noeta-parser` (chumsky) |
| Types | `noeta-types` (the `Type` lattice + trait registry), `noeta-check` (the checker) |
| IR & memory | `noeta-ir` (ANF), `noeta-ir-passes` (liveness → drops → reuse) |
| Backends | `noeta-backend` (the seam), `noeta-eval` (reference), `noeta-compiler` + `noeta-bytecode` + `noeta-vm` (the VM), `noeta-builtins` |
| VM value model | `noeta-object` (shapes), `noeta-value` (NaN-boxed values — the one `unsafe` crate), `noeta-gc` (cycle collector) |
| Runtime & host | `noeta-stdlib` (shared semantics + `Host`), `noeta-host-real` (the real host) |
| Tooling | `noeta-loader` (modules), `noeta-db` (salsa), `noeta-project` (the project model — `project_check`, entry pools, the shared salsa workspace), `noeta-conformance` (the harness), `noeta-cli` (the binary) |

Each crate carries its own `README.md` as its primary documentation. The following pages go deep on the individual techniques:

- **[The Virtual Machine](The-Virtual-Machine)** — register bytecode, NaN-boxing, shapes, inline caches.
- **[Memory Management](Memory-Management)** — copy-on-write, in-place reuse, precise reference counting, cycle collection.
- **[The Type Checker](Type-Checker-Internals)** — bidirectional checking with local inference.
- **[Concurrency Internals](Concurrency-Internals)** — the stackless coroutine substrate, isolates, channels.
- **[Performance Techniques](Performance-Techniques)** — SIMD/layout, inline caches, and what was measured and dropped.
- **[Native Extensions](Native-Extensions)** — the registry seam for adding native modules.
- **[Extension Compatibility](Extension-Compatibility)** — the stable surface and versioning contract for native packages.
