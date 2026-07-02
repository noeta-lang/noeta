# Architecture & Pipeline

This section is the "how it works under the hood" tour, for the curious and the systems-minded. It describes the implementation as it actually exists — and, where the design intends more than what has shipped, says so.

The implementation is a workspace of ~25 small Rust crates forming a strict dependency DAG. Two ideas organize everything: a **compilation pipeline** of sharp, single-purpose stages, and a **two-backend differential oracle** that keeps the whole thing honest.

## The pipeline

```
source (.lang)
   │  lang-lexer (logos)
   ▼
tokens ──► lang-parser (chumsky) ──► AST (lang-ast)
                                       │
                                       ▼
                             lang-check (types + local inference)   ── a shared front-end:
                                       │                                a program with type errors
                                       ▼                                is rejected before it runs
                             lang-ir (ANF lowering)
                                       │
                                       ▼
                             lang-ir-passes (liveness ─► drop insertion ─► in-place reuse)
                                       │
                        ┌──────────────┴───────────────────────────────┐
                        ▼                                               ▼
                   lang-eval                              lang-compiler ─► Chunk/Module
              (the reference interpreter)                                ─► lang-vm (register VM)
                        │                                               │
                        └───────────────────► RunResult ◄──────────────┘
                              (observable output; the two are asserted identical)
```

Each stage is a separate crate with an explicit input and output type and no hidden shared mutable state, so a change to one stage is local to its crate and verifiable by that stage's snapshot tests. This staging is what makes the codebase tractable to work on — including for AI agents.

## The two-backend differential oracle

The language is implemented **twice**:

- **`lang-eval`** — a tree/IR-walking interpreter with an `Rc`-based value model.
- **`lang-vm`** — a register-based bytecode VM over NaN-boxed 64-bit words.

Both implement one trait — `trait Backend { fn run(&self, program: &Program) -> RunResult }` — and the conformance harness runs every program through *both*, asserting their `RunResult` (stdout + exit code + diagnostics) is byte-for-byte identical, with a hard **`0 skipped`** gate.

Why build it twice? A second independent implementation is a continuously-running oracle: any divergence between the two is a bug in one of them, caught mechanically instead of by hand-written expected output. Crucially, the comparison is on *observable behavior*, not internal representation — which is exactly what frees the two backends to use completely different value models (an `Rc` enum vs. NaN-boxed words). This oracle is the spine of the whole test strategy, and it constrains the design: any shared semantics that both backends must agree on live *once*, in `lang-stdlib`, so they cannot drift.

> [!NOTE]
> **A precise nuance.** The differential's reference used to be the AST tree-walker. Since the memory-management migration, the reference is the **Core-IR interpreter** (the same `lang-eval` machinery, now interpreting the RC-annotated ANF IR) — because the AST walker fired destructors only at global teardown and so could no longer reproduce last-use destruction. The live differential is now *Core-IR interpreter ↔ VM*: two genuinely different memory machines (Rust `Rc` vs. a manual refcount heap) executing the *same* RC-annotated IR. The AST walk survives as a performance baseline and property-test helper.

## Incremental compilation (salsa)

The pipeline is not just a straight line of function calls — it is expressed as a graph of memoized **queries** using [salsa](https://github.com/salsa-rs/salsa) 0.27 (the framework rust-analyzer is built on), in the `lang-db` crate. The `SourceProgram` input feeds tracked queries `tokens → ast → checked → bytecode`; the module graph adds `Workspace → linked → linked_checked → linked_bytecode`. Editing one module recomputes only its transitive dependents.

This is not a separate feature bolted on — it is the *same* query graph that would power a responsive LSP, hot-module-reload blast-radius analysis, and agent-local change verification. The sharp crate seams are what make the queries mechanical. (salsa needs memoized returns to be `Update + PartialEq`; the three foreign artifacts are wrapped in local newtypes with conservative "always-changed" impls — the crate's only, miri-gated, `unsafe`.)

## Diagnostics as data

Every diagnostic is a typed variant with a stable `E0xxx` code, defined once in `lang-diagnostics` and rendered in exactly one place (over `ariadne`). Stages never format error strings themselves — they emit `Diagnostic` values that flow to the one renderer. This is what lets a negative conformance test assert with a `// expect: error E0xxx` header, and what lets the checker promote a would-be runtime error to a compile-time one while keeping the differential green: the diagnostic *is* the observable result, identical on both backends.

## Crate map

The dependency edges form a strict DAG (no back-edges). `lang-span` is depended on by everyone; `lang-cli` depends on nearly everything; the two backends are siblings that meet only at the `Backend`/`RunResult` seam.

| Layer | Crates |
|---|---|
| Shared vocabulary | `lang-span`, `lang-diagnostics`, `lang-ast` |
| Frontend | `lang-lexer` (logos), `lang-parser` (chumsky) |
| Types | `lang-types` (the `Type` lattice + trait registry), `lang-check` (the checker) |
| IR & memory | `lang-ir` (ANF), `lang-ir-passes` (liveness → drops → reuse) |
| Backends | `lang-backend` (the seam), `lang-eval` (reference), `lang-compiler` + `lang-bytecode` + `lang-vm` (the VM), `lang-builtins` |
| VM value model | `lang-object` (shapes), `lang-value` (NaN-boxed values — the one `unsafe` crate), `lang-gc` (cycle collector) |
| Runtime & host | `lang-stdlib` (shared semantics + `Host`), `lang-runtime` (the real host) |
| Tooling | `lang-loader` (modules), `lang-db` (salsa), `lang-conformance` (the harness), `lang-cli` (the binary) |

Each crate carries its own `README.md` as its primary documentation. The following pages go deep on the individual techniques:

- **[The Virtual Machine](The-Virtual-Machine)** — register bytecode, NaN-boxing, shapes, inline caches.
- **[Memory Management](Memory-Management)** — copy-on-write, in-place reuse, precise reference counting, cycle collection.
- **[The Type Checker](Type-Checker-Internals)** — bidirectional checking with local inference.
- **[Concurrency Internals](Concurrency-Internals)** — the stackless coroutine substrate, isolates, channels.
- **[Performance Techniques](Performance-Techniques)** — SIMD/layout, inline caches, and what was measured and dropped.
- **[Native Extensions](Native-Extensions)** — the registry seam for adding native modules.
