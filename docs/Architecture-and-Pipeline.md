# Architecture & Pipeline

The implementation is a workspace of about 50 small Rust crates forming a strict dependency DAG. Two ideas organize it: a **compilation pipeline** of sharp, single-purpose stages, and a **two-backend differential oracle** that keeps the whole thing honest.

The crate map below covers the pipeline core. The tooling and runtime subsystems (the JIT and AOT runtime, package manager, LSP/DAP/MCP, formatter, profiler, reactivity) add the rest.

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

Each stage is a separate crate with an explicit input and output type and no hidden shared mutable state. A change to one stage is therefore local to its crate, and verifiable by that stage's snapshot tests.

## The two-backend differential oracle

The language is implemented **twice**:

- **`noeta-eval`** interprets the RC-annotated ANF IR, over an `Rc`-based value model.
- **`noeta-vm`** is a register-based bytecode VM over NaN-boxed 64-bit words.

Both implement one trait, `trait Backend { fn run(&self, program: &Program) -> RunResult }`, and the conformance harness runs every program through *both*. It asserts their `RunResult` (stdout, stderr, exit code and diagnostics) is byte-for-byte identical, with a hard **`0 skipped`** gate.

A second independent implementation is a continuously running oracle: any divergence between the two is a bug in one of them, caught mechanically instead of by hand-written expected output.

The comparison is on *observable behavior* rather than internal representation, which is what frees the two backends to use completely different value models. It also constrains the design: any shared semantics both backends must agree on live once, in `noeta-stdlib`, so they cannot drift.

> [!NOTE]
> The reference backend is the **Core-IR interpreter**, meaning `noeta-eval` interpreting the RC-annotated ANF IR. Last-use destructor timing is a property of the IR, and only an interpreter of that IR can reproduce it exactly. The live differential is therefore Core-IR interpreter against VM: two genuinely different memory machines, Rust `Rc` and a manual refcount heap, executing the same RC-annotated IR.

## Incremental compilation (salsa)

The pipeline is a graph of memoized **queries** rather than a straight line of function calls, expressed with [salsa](https://github.com/salsa-rs/salsa) 0.27 (the framework rust-analyzer is built on) in the `noeta-db` crate.

The `SourceProgram` input feeds the tracked queries `tokens → ast → checked → bytecode`, and the module graph adds `Workspace → linked → linked_checked → linked_bytecode`. Editing one module recomputes only its transitive dependents.

This is the query graph the LSP and the MCP server already run on, through the shared `noeta-ide` engine, and the one hot-module-reload analyzes a swap's blast radius with. The sharp crate seams are what make the queries mechanical. (Salsa requires memoized returns to be `Update + PartialEq`, so the artifact types are wrapped in local newtypes with conservative always-changed impls.)

## Diagnostics as data

Every diagnostic is a typed variant with a stable `E0xxx` code, defined once in `noeta-diagnostics` and rendered in exactly one place, over `ariadne`. Stages never format error strings themselves; they emit `Diagnostic` values that flow to the one renderer.

That is what lets a negative conformance test assert with a `// expect: error E0xxx` header. It is also what lets the checker promote a would-be runtime error to a compile-time one while keeping the differential green, since the diagnostic *is* the observable result, identical on both backends.

## Crate map

The dependency edges form a strict DAG with no back-edges. `noeta-span` is depended on by everyone, `noeta-cli` depends on nearly everything, and the two backends are siblings that meet only at the `Backend`/`RunResult` seam.

| Layer | Crates |
|---|---|
| Shared vocabulary | `noeta-span`, `noeta-diagnostics`, `noeta-ast` |
| Frontend | `noeta-lexer` (logos), `noeta-parser` (chumsky) |
| Types | `noeta-types` (the `Type` lattice + trait registry), `noeta-check` (the checker) |
| IR & memory | `noeta-ir` (ANF), `noeta-ir-passes` (liveness → drops → reuse) |
| Backends | `noeta-backend` (the seam), `noeta-eval` (reference), `noeta-compiler` + `noeta-bytecode` + `noeta-vm` (the VM), `noeta-builtins` |
| Native code | `noeta-jit` + `noeta-jit-abi` (the Cranelift Tier-1 JIT), `noeta-aot-runtime` + `noeta-bundle` (`--native` output) |
| VM value model | `noeta-object` (shapes), `noeta-value` (NaN-boxed values), `noeta-gc` (cycle collector) |
| Runtime & host | `noeta-stdlib` (shared semantics + `Host`), `noeta-host-real` (the real host), `noeta-ext-abi` (the extension seam), `noeta-reactive` |
| Tooling | `noeta-loader` (modules), `noeta-db` (salsa), `noeta-project` (the project model: `project_check`, entry pools, the shared salsa workspace), `noeta-conformance` (the harness), `noeta-cli` (the binary) |
| Editor & agent | `noeta-ide` (the shared engine), `noeta-lsp`, `noeta-dap`, `noeta-mcp`, `noeta-fmt`, `noeta-prof`, `noeta-pm` |

The workspace lints `unsafe_code = "forbid"`, and a crate that needs `unsafe` opts out explicitly and is listed in `ARCHITECTURE.md`'s quarantine. The value model (`noeta-value`) and the VM's dispatch loop (`noeta-vm`) hold most of it.

Each crate carries its own `README.md` as its primary documentation. The following pages go deep on the individual techniques:

- **[The Virtual Machine](The-Virtual-Machine)** — register bytecode, NaN-boxing, shapes, inline caches.
- **[Memory Management](Memory-Management)** — copy-on-write, in-place reuse, precise reference counting, cycle collection.
- **[The Type Checker](Type-Checker-Internals)** — bidirectional checking with local inference.
- **[Concurrency Internals](Concurrency-Internals)** — the stackless coroutine substrate, isolates, channels.
- **[Performance Techniques](Performance-Techniques)** — SIMD and numeric layout, inline caches, and the JIT.
- **[Native Extensions](Native-Extensions)** — the registry seam for adding native modules.
- **[Extension Compatibility](Extension-Compatibility)** — the stable surface and versioning contract for native packages.
