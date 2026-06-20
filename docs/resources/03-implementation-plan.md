# Implementation Plan

*Working title: the language is referred to here as **the language**. Name TBD.*

This plan sequences the build to reach a *compelling demo* fast and defer the *mature product* until the demo has earned contributors, community, or funding. It is structured around milestones, each independently demoable, with the crates that do the heavy lifting and the irreducible work that is ours alone.

---

## 1. Honest scope

The mature vision (optimizing runtime, ecosystem compatibility, full tooling) is a **300K+ LOC, multi-year, needs-a-team** effort. That is the destination, not the starting line.

A **compelling, "this is real and interesting" version is ~40K–80K LOC and roughly a solo year** of focused work. The plan below treats reaching *that* as the goal. The mature-version LOC count should inform *sequencing*, not deter the *start* — Rust makes the start cheaper than it has ever been.

**LOC by milestone (rough):**

| Milestone | Added LOC (approx) | Solo effort |
|---|---|---|
| M0 Walking skeleton | 5K–10K | weeks–2 months |
| M1 Real language core | +25K–50K | several months |
| M2 Differentiators | +40K–80K | months, layered |
| M3 Long tail | +100K+ | multi-year, team |

**What no crate writes for us (the irreducible core):** the type checker and inference, the bytecode compiler, the VM dispatch loop and opcodes, the shape/inline-cache object model, the semantics (and any PHP-compat mode), generics/ADT checking, and the glue binding all borrowed crates into one coherent language. This is the ~60K–120K lines that *are* the project.

---

## 2. The "free from Rust" inventory

What the ecosystem hands us, by subsystem:

**Lexing & parsing**
- `logos` — derive-macro lexer generator; fast, declarative token definitions.
- `chumsky` (or `winnow`/`nom` for combinators, or `lalrpop` for an LR grammar) — turns "write a parser" into "write a grammar." `chumsky` has strong error-recovery ergonomics, which matters for the LSP.
- `ariadne` (or `codespan-reporting`) — publication-quality diagnostic rendering (spans, carets, "expected X found Y"). Weeks of polish for free; a large part of feeling professional.
- `rowan` — lossless concrete syntax tree (the library rust-analyzer is built on). Wanted anyway for the LSP and formatter.

**GC**
- `gc-arena` — generativity-branded, safe GC (proven by the `piccolo` Lua implementation) for the tracing path.
- `Rc`/`Arc` + a hand-written cycle collector for the refcount path. A moving generational collector, if ever wanted, is ours to build.

**Async, server, concurrency**
- `tokio` — async runtime, schedulers, I/O; the foundation for isolates and intra-isolate async.
- `hyper` / `axum` — production HTTP/1.1+2 servers; the bundled server is largely a thin layer over these.
- `tokio-tungstenite` — WebSockets for the reactivity transport.

**Tooling**
- `tower-lsp` — full LSP protocol plumbing (JSON-RPC, handlers); we implement only the language-specific analysis.
- `salsa` — incremental-computation framework (the engine `rust-analyzer` is built on). Express the compiler as a memoized query graph; powers incremental compilation, LSP responsiveness, and HMR blast-radius classification from one system (architecture §6, §9.14).
- Cargo — our build system and dependency manager; we write none of it.

**WASM target**
- Native Rust → WASM; `wasm-bindgen`, `wasm-pack`. `wasmtime` if embedding WASM is ever wanted.

**Desktop / GUI**
- `tauri` — native shell (window, webview, OS APIs, installer) around the runtime's existing server + reactivity; `wry` is the underlying webview layer if going below Tauri. Chosen over GTK+ bindings because Tauri's two-process (Rust backend + webview) split means sitting on a message channel rather than binding a C widget toolkit against the GC.

**Testing & quality**
- `criterion` (benchmarks), `insta` (snapshot tests — ideal for compiler output), `proptest` (property-based testing for the evaluator).

**Standard library**
- Largely **wrap excellent existing crates** with our binding layer rather than implement from zero: `regex`, `serde_json`, `chrono`/`time`, `reqwest`, DB drivers (`sqlx`), crypto (`ring`/`rustls`). This collapses what is normally a massive LOC sink.

---

## 3. Milestones

### M0 — Walking skeleton (weeks → ~2 months, 5K–10K LOC)

**Goal:** run simple programs; prove the syntax feels right.

- Lexer with `logos`.
- Parser with `chumsky`, producing an AST.
- Diagnostics with `ariadne` from day one (good errors are a feature, not a finishing touch).
- Tree-walking evaluator (no bytecode yet) covering: variables (immutable-default + `mut`), functions/closures, the core types, `if`/`for`/`match`, classes with `.` access, string interpolation, `~` concatenation.
- `insta` snapshot tests over parser and evaluator output; `proptest` on the evaluator.
- **The test harness skeleton (§6) and crate seams (§7) are M0 deliverables, not later polish** — the conformance corpus, layered snapshots, and `lang test` loop must exist before M1, because every subsequent milestone is driven through them agentically.
- A REPL and a file runner.

**Demo:** the complete small program from the syntax doc runs.

### M1 — Real language core ("this is a real language", several months, +25K–50K LOC)

**Goal:** write non-trivial programs; show people.

- **Replace the tree-walker with a register-based bytecode compiler + VM** (Tier 0 baseline interpreter).
- **NaN-boxed value representation.** Decide the **boxed-object vs. packed-value-type distinction** here (architecture §3.1) even if packed value types and flat typed arrays are *implemented* later — it is representation-level (touches value layout, arrays, generics specialization, and the FFI) and the hardest thing to retrofit, so the design must leave room for it from the start.
- **Shape-based object model with inline caches.**
- **GC:** refcount + cycle collector floor (deterministic `__destruct`); tracing path via `gc-arena` as the internal optimization for destructor-free classes.
- **Type checker** with inference (the largest single irreducible piece). Structure the compiler as a **`salsa` query graph** from here onward (not straight-line passes) — this is the foundation for incremental compilation, LSP responsiveness, and HMR (architecture §6); the discipline is paid once and yields all three.
- **The powerful spine:** generics (erasure-for-storage / reification-for-identity via shapes), ADTs + exhaustive `match`, `Result`/`Option` + `?`, immutable-by-default with ownership analysis, and **traits as the unified mechanism for operators and built-in protocols** (replacing all of PHP's magic methods; see architecture §9.2) with `#[derive(...)]` for the common cases. Built-in derives are implemented in the compiler (no user-defined macros / no comptime, §9.13); the front end also keeps the **attribute manifest** that powers discovery/registration and LSP queries.
- **Modules / namespaces** and a **layered standard library** (architecture §11.1): the rich Ring 1 core types (List/Map/Set/string/numeric/Option/Result) and the thin Ring 2 always-shipped modules (file/IO, process/env, basic math/random/time, JSON) land in M1; Ring 3 first-party modules (regex, timezone date/time, crypto, HTTP client, extra formats, SIMD math) are built later as native first-party extensions reusing the §10 mechanism, not bundled. Mostly wrapping Rust crates throughout.

**Demo:** a real domain program with typed errors, generics, and pattern matching; clean compile errors.

### M2 — Differentiators (months, layered & individually demoable, +40K–80K LOC)

Each item rides on a mature crate, so each is far cheaper than from scratch. **Build the persistent runtime + bundled server first** — it is the keystone and the strongest demo.

- **Persistent process + isolates** on `tokio` (non-atomic per-isolate refcounts, in-process caching/pooling, background work).
- **Async / structured concurrency** (architecture §7.1–§7.2) — `async`/`await` over the tokio scheduler for I/O-bound work (the everyday concurrency case), scoped `concurrent { }` structured-concurrency blocks, `Result`-typed errors propagating through `await`, and the single **`TaskScope`** ownership primitive: `concurrent { }` is its block-lifetime form, and an injected app-lifetime `TaskScope` is the fire-and-forget/background primitive (drains on shutdown, owned-not-orphaned). The language ships only `TaskScope` + the scope rule; **DI, workers, durable queues, and schedulers are framework/first-party-extension patterns built on it** (queue/scheduler land as M3 extensions over the DB layer), not language features. Central to the server/web positioning, so the primitive lands in M2.
- **Bundled HTTP/WS server** on `hyper`/`axum` + `tokio-tungstenite`. `build` produces a single static binary that *is* the server.
- **Server-side signals** (`signal`/`computed`/`effect`) fused with the WS server → a correct-by-construction LiveView, no WASM.
- **Embedded LSP** on `tower-lsp`, built from the same parser/checker (zero drift). Surfaces the **attribute manifest** (architecture §9.13) — "show every `#[Route]`", who-consumes queries, jump-to-all-usages — from the index the front end already builds; no comptime or runtime reflection involved.
- **Native toolchain**: `init`/`add`/`build`/`test`/`fmt`/`lint`/`lsp` in the one binary. The **package manager** resolves *user libraries* from the language's own registry (not crates.io); it invokes **cargo as the build backend for native extensions** (which are Rust crates) and is itself built with cargo — three distinct dependency layers (architecture §11.2). The **user test runner** (`lang test` on a user project) reuses the conformance-harness infrastructure (§6) — one runner, exposed inward for conformance and outward for user tests (§11.3). `lang lint` runs core analyses + first-party + project rules over the salsa model (§9.17); expose the **analysis query API over salsa as a deliberate, stable public surface** — it is consumed by both the LSP and third-party (WASM-sandboxed) lint rules.
- **AOT binary mode** (CLI first — matches the single-binary aesthetic — then web-app-to-binary), including **dead-code elimination over the salsa reachability graph** (architecture §9.8.1): static-by-default (closed world, total tree-shaking), scoped dynamism via pinned roots (plugin/by-name cases stay shakeable), and full-`eval` as a deliberate manifest opt-out that forfeits the small-binary guarantee. This is the mechanism the "pay nothing for unused capabilities" cost model depends on, so it is part of the AOT path, not a later optimization.
- **Tier 1 specializing interpreter** (CPython-style self-rewriting opcodes) for performance.
- **Hot module replacement (dev mode)** (architecture §9.14) — change backend code, the running process picks it up without restart or state loss, and the change propagates to the live UI through the reactivity graph. Rides on persistence + isolates + bytecode VM + signals (all already built); the dev-mode face of the bundled server. Safe-change hot-swap with scoped isolate-restart fallback for shape-incompatible changes; production binary stays sealed.
- **Built-in observability + structured logging** (architecture §9.6, §12.1): OpenTelemetry-style spans, `/healthz`, and the standard structured logging API with pluggable store drivers.
- **Agentic DX surface** (architecture §12) — the differentiator for a training-data-less language, mostly *exposing existing models* through one interface:
  - **Built-in MCP server**: semantic tools (salsa/LSP), runtime/debug tools, profiling tools, project/build tools, `query_logs`; dev-only by default, production opt-in via explicit per-tool allowlist (capability-gated, compiled out otherwise).
  - **App/framework-registered tools**: free-form content via a standard typed registration mechanism (`#[agent_tool]`).
  - **Debug engine** over the VM (breakpoints, frame/state inspection, isolate live-inspect), exposed via **DAP** (editors) and **MCP** (agents) — same engine, two protocols.
  - **Profiling/flamegraphs** as structured data (agent-readable) plus human visual.
  - **Compiler-as-syntax-oracle**: `check_snippet`/`explain_syntax`/`show_example` MCP tools — propose→verify→commit against the real compiler, the antidote to no training data.
  - **`lang init` scaffolding**: `AGENTS.md` + a toolchain-generated, version-matched language primer.
- **Baseline data access** (architecture §11.4) — a typed query interface and driver-backed access to common databases (Postgres first, `sqlx`-style binding), with pooling across requests. This is table stakes for the server positioning and must ship *without* opting into anything reactive; the reactive ORM (§9.12) is a later R&D layer on top. Lands in M2.
- **Packed value types + flat typed arrays** (architecture §3.1) — the implementation of the layout decision made in M1: unboxed contiguous records, generics-specialized flat arrays for packed element types, and the SIMD-backed 3D-math module. Enables the numeric/game/data use cases; lands in M2 (the *decision* is M1, the *implementation* here).

**Demo:** a reactive web app deployed as one binary, with live IDE support and good errors.

### M3 — Long tail (multi-year, team, +100K+ LOC)

The work that needs more than one person:

- **WASM target** — Tier 1 shared contracts (validation/types emitted for client), then Tier 2 shared pure-logic kernels. Tier 3 (full client) deferred indefinitely.
- **Desktop / GUI via Tauri** — Depth A (Tauri as the native shell around the existing server + reactivity) can land as late as M2, near-free off that stack; Depth B (binding Tauri's command/plugin API into the language) is the optional later deepening. Crates: `tauri` (and `wry` if going below Tauri to the webview layer). Extends the deployment story to "one binary that is a desktop app" and widens the niche to an "any surface" positioning.
- ~~**PHP/Composer/Laravel compatibility layer**~~ — **dropped** (see §4: clean break decided). Not on the roadmap; the language does not target the PHP ecosystem.
- **Background-work first-party extensions** (architecture §7.2) — a durable job queue (persisted jobs over the DB layer §11.4, surviving restart, retried) and a scheduler (cron/interval), both patterns built on `TaskScope` and shipped as opt-in first-party extensions, not language features. The in-process hosting collapses the "web app + Horizon + Redis + cron + Supervisor" stack into one binary.
- **Optimizing JIT** via copy-and-patch (CPython 3.13 model).
- **Editor integration: grammars + VS Code extension.** Two highlighting layers, wired up together:
  - **Syntactic highlighting (grammar-based, instant, no server).** Two *separate* grammar artifacts are maintained: a **tree-sitter grammar** (fast, incremental, error-tolerant — the de-facto standard consumed *directly* by tree-sitter-native editors: Neovim, Helix, Zed, plus GitHub and other tools) and a **TextMate grammar** (for VS Code's classic highlighting path). Both are standalone deliverables — the tree-sitter grammar in particular is consumed by many editors *without* the VS Code extension, so it earns its place as an independent artifact. The **VS Code extension bundles and registers both grammars** (plus the language contribution: file extensions, language ID), so installing the extension lights up highlighting immediately when a file opens, before any server starts.
  - **Semantic highlighting (LSP, context-aware).** The LSP (§9.6) emits *semantic tokens* — type vs. variable, `mut` vs. immutable, unused symbols, function vs. method — refinement that grammar highlighting cannot produce because it needs name resolution and types. This layers on top of the grammar highlighting once the language server is ready.
  - **The VS Code extension** then auto-wires, with zero user config, the **LSP** (intelligence + semantic tokens), the **DAP** debug engine (§12.2, breakpoints/stepping/inspection), and the **MCP server** (§12.4, full agent introspection in-editor). One install gives a developer and their agent the complete tooling story. Because all of these already exist as toolchain/grammar artifacts, the extension is integration glue, not new capability. Sequenced in M3, but the tree-sitter and TextMate grammars can land earlier since they are independent of the runtime.
- **Reactive persistence / object mapping** (architecture §9.12) — ship the credible layer first (typed query builder, literal hydration, structural-update dirty-tracking; these use existing machinery and could land in late M2). The *reactive* layer (live signal-backed models/queries, DB-change-to-UI propagation) is a differentiating R&D bet layered on once signals are proven, gated on resolving the open problems: reactivity scope, change-storm control, and the DB-vs-signal-graph consistency model.
- **Collaborative / local-first / p2p state** (architecture §9.15) — reactive CRDTs (concurrent multi-user editing of shared reactive state) for the data-convergence layer, plus native p2p/local-first networking (peer discovery, NAT traversal, sync, identity, encryption) for which **p2panda** (Modal Collective) is the candidate supporting stack — data-type/CRDT-agnostic, transport-independent, with an existing Tauri integration. The language provides the glue: signals react to synced state, the persistent runtime hosts the embedded node, Tauri packages it as a single binary. R&D bet; opt-in per application; p2panda is pre-1.0 so watch-and-integrate, not a near-term dependency. Explicitly *not* the HMR mechanism. Same staging as reactive persistence.
- **Extension system** (architecture §10) — keyed to the trust boundary: WASM-sandboxed by default for registry/third-party code (reuses the WASM toolchain), native FFI against a stable host ABI for local/first-party bindings and external native libraries, with package-manager integration making both as routine as adding a dependency. **Design the stable host ABI early and keep it narrow** — it is the one effectively-irreversible decision here; everything else (tooling, WASM tier, package integration) can evolve, but the ABI cannot move once third-party extensions depend on it.
- **On-disk startup cache** (AppCDS-style) so cold process startup skips re-parsing (architecture §9.10) — the small, optional thing that exists *instead of* PHP's shared-memory opcache, not a descendant of it.
- **Editions machinery** formalized (front-end-only dialect selection) and faithful support for this language's own older editions.
- **Mobile** (architecture §11.5) — *not* a 1.0 target, but explicitly **not foreclosed**: Tauri has a mobile story and the runtime/reactivity/single-binary model is mobile-compatible in principle. No M-milestone work is committed, but architectural decisions through M1–M2 (deployment, UI hosting, runtime embedding, the thin stdlib core §11.1) must stay mobile-reachable so this remains a viable later direction rather than a corner the project built itself into.

---

## 4. Strategy (decided)

**Decision: clean break from PHP. New language, capability thesis, no compatibility layer.** PHP resemblance is incidental, not an adoption argument. The reason to exist stands on the *capability combination* — single-binary deployment + server-side reactivity + a real (ML-grade) type system + any-surface (CLI/web/desktop) from one persistent runtime — not on familiarity. This was chosen over the two alternatives below and removes the largest piece of risk from the roadmap: the PHP/Composer/Laravel compatibility layer is **off the critical path entirely** (it was the 300K-LOC, multi-year, possibly-needs-a-team item in M3).

For the record, the options considered:
1. **Compatible-runtime play** *(rejected)* — be a drop-in PHP runtime first, then layer new features. Highest ceiling, but the M3 compatibility layer becomes a precondition; enormous build, wrong fit for a solo start.
2. **Adjacent-language / clean-break play** *(chosen)* — a new language whose surface is incidentally PHP-like, winning on the capability combination above without carrying the legacy ecosystem. Much smaller surface; fits a solo, tooling-focused builder.
3. **"Better PHP" with no sharp niche** *(rejected)* — new semantics, no bridge, no distinct identity. The doomed middle.

**The wedge:** win a specific job where a fast, reactive, single-binary, any-surface, type-safe language is *obviously* the right tool and the incumbents (PHP-on-FPM, but also Go/Node for reactive apps) structurally are not — most likely a live web app shipped as one binary, because it is cheapest off the Rust stack and the most striking demo. Earn a community there first.

---

## 5. Sequencing discipline

The one trap to avoid: building the 300K-line version before shipping the 50K-line version that proves anyone cares.

1. Get M0 running; make the syntax feel good in real use.
2. Reach M1 so it is a real language, not a toy.
3. In M2, build **persistent runtime + bundled server first** and put *that* demo in front of people — it is the cheapest (rides on `tokio`/`hyper`) and the most differentiating.
4. Use the reception to decide how aggressively to deepen the chosen niche and which surface (web / desktop / CLI) to lead with — not whether to build a compatibility layer (that question is closed; see §4).

Reframe the whole effort away from "get PHP programmers to switch languages" toward "be the obviously-right tool for a job people already need to do, where the language being new is irrelevant to the person choosing it." Nail that job, and newness stops being the obstacle.

---

## 6. Test harness

A language implementation is unusually well-suited to a machine-checkable test strategy: every layer of the pipeline produces an artifact that can be captured and diffed, and the end-to-end behavior is "run a program, compare its output." This makes the feedback loop tight enough to drive almost entirely by automation — which is also what makes the codebase agent-friendly (§7). Design the harness first; it is leverage, not overhead.

### 6.1 The layered strategy

Test each pipeline stage at its own boundary so a failure localizes to one stage rather than surfacing only as wrong final output.

1. **Lexer** — golden-file snapshots of the token stream for representative source. `insta` snapshots; a token-stream change shows as a reviewable diff.
2. **Parser** — `insta` snapshots of the AST (and the `rowan` CST). Include deliberately malformed input to lock in error-recovery behavior, not just the happy path.
3. **Diagnostics** — snapshot the *rendered* `ariadne` output for known-bad programs. The quality of error messages is a product feature (see syntax doc); regressions in spans, suggestions, or wording must be caught. This is the test category most projects neglect and most worth having here.
4. **Type checker** — a corpus of programs each annotated with its expected outcome: should-typecheck, or should-fail-with-error-E-at-span-S. Cover generics, ADT exhaustiveness, `Result`/`?` propagation, immutability violations.
5. **Compiler** — `insta` snapshots of emitted bytecode (disassembled to a stable textual form, never raw bytes). Catches accidental codegen changes.
6. **VM / end-to-end** — the primary suite: a directory of `.lang` programs each paired with expected stdout / exit status / emitted error. Run, capture, compare. This is the **language test-suite** model (CPython's, Go's, Rust's `ui` tests) and should grow into the thousands of small files.

### 6.2 Snapshot tests as the backbone (`insta`)

Snapshot testing fits a compiler perfectly: the "assertion" is "this input still produces this artifact," and `insta` makes reviewing and accepting intended changes a one-command operation (`cargo insta review`). Use it for tokens, AST, bytecode, and rendered diagnostics. The discipline: a snapshot change in a PR must be *explained*, never blind-accepted.

### 6.3 Conformance corpus (the spec, executable)

Maintain a `tests/conformance/` tree of small programs, each a self-contained `.lang` file with an expectation header:

```
// expect: stdout "Order #1 awaiting payment"
// expect: exit 0
```

and negative cases:

```
// expect: error OrderError-unhandled at 12:5
// expect: exit 1
```

This corpus *is* the language specification in executable form. Every feature lands with corpus entries; every bug fixed lands with a regression entry. It is the single most valuable asset for agentic work because it lets an agent verify a change end-to-end without human judgment.

### 6.4 Property-based & differential testing (`proptest`)

- **Property tests** on invariants that must hold for all inputs: parse→print→parse round-trips to the same AST; the evaluator and the bytecode VM produce identical results for the same program (cross-check the tree-walker retained from M0 against the M1 VM — a free differential oracle); GC never frees a live object (assertions under a stress allocator).
- **Fuzzing** (`cargo-fuzz`/`libfuzzer`) the lexer and parser against panics and hangs once the grammar stabilizes. A parser that panics on malformed input is a correctness bug; fuzzing finds these cheaply.

### 6.5 GC and concurrency testing

- A **stress-allocation mode** (allocate aggressively, collect frequently) run over the whole conformance corpus to surface use-after-free and missed roots.
- **Deterministic destruction tests**: programs that assert `__destruct` ordering and timing, since that semantic is load-bearing and easy to regress when the tracing-optimization path is touched.
- **`loom`** for the isolate/channel concurrency primitives — exhaustively explores thread interleavings for the small concurrent core, catching races the runtime must never have.
- **`miri`** in CI over the `unsafe` core (NaN-boxing, the VM, GC internals) to catch undefined behavior the type system can't.

### 6.6 Performance regression (`criterion`)

Benchmark the hot paths (dispatch loop, property access through inline caches, allocation) with `criterion`, tracking results over time. Performance is a stated reason-to-adopt; a silent 2x dispatch regression must fail CI, not be discovered in a demo.

### 6.7 CI gates

Every change must pass, in rough order of speed: `fmt` check → `clippy` (deny warnings) → unit + snapshot tests → conformance corpus → property tests → `miri` on the unsafe core → benchmarks (regression threshold). Fast gates first so failures surface in seconds; expensive gates (`miri`, fuzz, bench) last or on a schedule.

---

## 7. Agent-friendly codebase design

The intent is to build this **entirely through agentic engineering**. That is viable here precisely because a compiler is verifiable at every layer (§6) — but only if the codebase is deliberately structured so an agent can make a small, correct, *independently verifiable* change without holding the whole system in context. The harness above is half of this; the structure below is the other half. These are not cosmetic conventions — they change how the code is organized.

### 7.1 Architecture for legibility

- **Sharp stage boundaries with typed interfaces.** Lexer → parser → checker → compiler → VM, each a crate (or module) with an explicit input and output type and no hidden shared mutable state. An agent tasked with "fix parsing of grouped imports" should be able to work entirely within the parser crate, verified by parser snapshots, without touching or understanding the VM. Clean seams are what make a task *local*.
- **Workspace of small crates** rather than one monolith: `lexer`, `parser`, `ast`, `checker`, `bytecode`, `vm`, `gc`, `runtime`, `server`, `lsp`, `cli`, `stdlib`. Smaller compile units mean faster agent feedback loops and smaller blast radius per change.
- **Single repository (monorepo), one version, one CI.** The whole language project lives in one repo: a Cargo workspace of the small crates above, plus the non-Rust artifacts as sibling directories — the **tree-sitter and TextMate grammars**, the **conformance corpus** (the executable spec, §6), the **VS Code extension**, the **first-party extensions** (p2p, queue, scheduler, Ring-3 stdlib modules), and the **design docs**. This is *many small crates in one repo*, not one big crate — the modularity above is preserved; only the repo is unified. The rationale is specific to this project, not generic preference:
  - It is what **enforces the zero-drift guarantees** the architecture promises. "The LSP can never disagree with the compiler," "static-analysis rules query the same model as the compiler" (§9.17), and the shared salsa spine are *build-time* properties — true only if those components build from the same source at the same version. Split repos with independent release cycles would reintroduce exactly the drift the design eliminates.
  - It makes the **version-matched promises automatic**: the `lang init` language primer, the MCP `check_snippet` oracle, and the grammars must all match the exact compiler the user runs. One repo = one version = these align by construction, rather than a release-coordination matrix.
  - It makes the **vertical-slice agentic change model possible** (§7.3): "implement `~` end-to-end" touches grammar → AST → checker → bytecode → VM → conformance corpus in *one reviewable commit*. In split repos that becomes a coordinated multi-repo PR dance — exactly the diffuse, hard-to-verify shape this section tells agents to avoid. Atomic cross-cutting commits are what the whole agentic strategy is built on.
  - It keeps **syntax changes atomic** across parser, grammars, primer, and conformance corpus, so docs/grammar/tests are never transiently inconsistent.

  Boundary: the monorepo is the *language project* — things that must version together with the language. First-party extensions live *in* the repo (versioned with the toolchain) but are *distributed out* via the package manager (in-repo ≠ bundled-in-every-binary, §10.3). Third-party ecosystem packages and user apps are *outside* the repo entirely. (This is the code-level counterpart of the cross-reference index, `04-cross-reference.md`, which enforces the same consistency at the documentation level.)
- **Errors as data, centralized.** One error catalog (each diagnostic a typed variant with a stable code, e.g. `E0102`), rendered in one place. An agent adds a diagnostic by adding a catalog entry and a conformance case — a mechanical, pattern-matched task.
- **No clever global state.** Pass context explicitly. Implicit statics and ambient singletons are exactly what make changes non-local and defeat an agent's ability to reason about a unit in isolation.

### 7.2 The feedback loop is the product

An agent is only as good as the signal it gets after a change. Invest in making that signal fast, precise, and machine-readable:

- **One command runs everything**: `lang test` (via the native toolchain, M2) runs the full layered suite. An agent's inner loop is edit → `lang test` → read structured failures → fix.
- **Machine-readable test output** (JSON mode): which conformance file failed, expected vs actual, the exact span. An agent parses this far more reliably than scraping human-formatted logs.
- **Fast partial runs**: `lang test --stage parser`, `lang test --file tests/conformance/orders/empty.lang` so an agent reruns only the relevant slice in seconds rather than the whole suite.
- **Deterministic everything.** No time-, hash-order-, or thread-scheduling-dependent test output. Nondeterministic tests poison an agent's feedback loop — it can't tell a real regression from flake. Seed RNGs, sort map iteration in test mode, pin the allocator in stress runs.

### 7.3 Tasks shaped for agents

- **A `CONTRIBUTING`/`AGENTS.md` at the root** stating the pipeline, the crate map, where each kind of change goes, and the iron rule: *every feature or fix lands with a conformance corpus entry.* This is the agent's orientation document — keep it current; it is load-bearing, not decoration.
- **Per-crate `README`s** with the one-paragraph "what this crate does, what it takes in, what it emits." An agent reads this before touching the crate.
- **A new-feature template**: the standard shape of a change is (1) grammar/AST, (2) checker rule, (3) bytecode, (4) VM op, (5) conformance cases, (6) snapshot update. Documenting this sequence turns "add a language feature" from open-ended into a checklist an agent can follow and you can review.
- **Vertical-slice issues.** Prefer "implement the `~` concat operator end-to-end" (one feature through all stages, fully testable) over "refactor the parser" (diffuse, hard to verify). Vertical slices have a clear done-condition: the conformance cases pass.

### 7.4 Guardrails that keep agents honest

- **`#![forbid(unsafe_code)]` everywhere except the few crates that genuinely need it** (`gc`, `vm` NaN-boxing). Quarantining `unsafe` to named crates lets `miri` focus there and stops an agent from reaching for `unsafe` as an escape hatch in safe code.
- **`clippy` with warnings-as-errors** so style/correctness lints are enforced mechanically, not in review.
- **Coverage of the conformance corpus over the feature set** tracked explicitly — an agent (or you) can see which language features lack tests and fill gaps as discrete tasks.
- **Snapshot acceptance is never automatic in CI.** An agent may *update* snapshots locally, but a changed snapshot in a PR is a flagged diff requiring justification — the guard against an agent "fixing" a test by accepting wrong output.

### 7.5 Why this compounds

The same properties that make the codebase agent-friendly — sharp boundaries, executable spec, fast deterministic feedback, local changes — are also what make it *correct* and *maintainable* by anyone, agent or human. There is no tradeoff between "good for agents" and "good engineering" for a project of this shape; the conformance corpus and staged architecture are simply the right way to build a language, and they happen to be exactly what an agent needs to work unsupervised. Build the harness and the crate seams first, in M0, and every later milestone is driven through that same loop.
