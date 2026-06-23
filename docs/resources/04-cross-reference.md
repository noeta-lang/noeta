# Cross-Reference Index

*A maintenance artifact. Maps each feature to its home in the architecture doc, the syntax doc, and the implementation-plan milestone, so that misalignment between the three is **mechanically visible** rather than dependent on memory.*

**How to read this:**
- **Arch** = section in `01-architecture.md`. **Syntax** = section in `02-syntax.md`. **Plan** = milestone in `03-implementation-plan.md` (M0–M3), or a plan section (§).
- **`—`** means "intentionally has no surface here," with the reason in Notes. An empty cell that *should* be filled is the drift signal to catch.
- Rules of thumb for when `—` is correct: a feature has **no Syntax** entry if it is internal/runtime-only (GC, VM, object model) or an application note (games); it has **no Plan** entry only if it is purely conceptual (e.g. "the coherence"). Every *user-facing language feature* should have all three.

---

## Core language & type system

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Guiding principles | §1 | — | — | Framing; no surface/build. |
| Why PHP's architecture limits it | §2 | §1 (departures table) | — | Analysis + the syntax-level departures it motivates. |
| Value representation (NaN-boxing) | §3 | — | M1 | Internal; no user syntax. |
| Packed value types & flat arrays (SIMD) | §3.1 | §5 | M1 (decision) / M2 (impl) | Layout decided M1, implemented M2. |
| Object model: shapes + inline caches | §4 | — | M1 | Internal. |
| Object creation (no privileged constructor) | §4.1 | §6 | M1 | Records + `new` as ordinary fn. |
| Structural update (`..` spread) | §4.2 | §6 | M1 | Clone-with-changes. |
| Memory management (refcount + cycle GC) | §5 | — | M1 | Internal; `__destruct`/`destruct` surfaces in Syntax §9.6 traits note. |
| Execution (tiered VM, specializing) | §6 | — | M1 (Tier 0) / M2 (Tier 1) | Internal. Incremental-compilation (salsa) note lives here. |
| Variables & mutability (`mut`) | §9.1 (immutable-default) | §2 | M1 | Immutable by default. |
| Type system (generics, inference) | §9.1 | §5 | M1 | Name-first types; generics via shapes. |
| ADTs & exhaustive `match` | §9.1 | §5, §9.2 | M1 | Sum types + destructuring match. |
| `Result`/`Option` + `?`, one error hierarchy | §9.1 | §9.1, §9.5 | M1 | Exceptions = panics only; `?T` = `Option` sugar. |
| Strings & interpolation | — | §3 | M1 | Surface-only; stdlib text is §11.1. |
| Functions | — | §4 | M0 | Core syntax. |
| Collections (List/Map/Set) | §11.1 (Ring 1) | §7 | M1 | Literals in Syntax; stdlib scope in Arch §11.1. |
| Control flow | — | §8 | M0 | Core syntax. |
| Pipeline operator | — | §9.3 | M1 | Surface feature. |

## Traits, operators, metaprogramming

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Traits & built-in protocols (operators) | §9.2 | §9.6 | M1 | Replaces all PHP magic methods. |
| `@derive(...)` (built-in derives) | §9.13 | §9.6, §9.7 | M1 | Compiler-implemented codegen; no user derives. |
| `#[...]` attributes (records + traits + manifest) | §9.13 | §9.7 | M1 | Data attributes, distinct from `@derive`; no bespoke construct, no comptime. |
| Reflection (compile-time + runtime, closed-world) | §9.13 | §9.7 | M1 / M3 (runtime tail) | Distinct from `eval`. |
| No comptime / no user macros (decision) | §9.13 | §9.7 | — | Stated non-goal. |

## Concurrency & async

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Isolates + message passing | §7 | §9.4 (channels/workers) | M2 | Parallelism model. |
| Async / structured concurrency | §7.1 | §9.4 | M2 | `async`/`await`, `concurrent { }`. |
| `TaskScope` (one ownership primitive) + fire-and-forget | §7.2 | §9.4 | M2 | `concurrent { }` = block-lifetime; injected app-lifetime scope = background. |
| Workers / durable queue / scheduler | §7.2 | — | M3 (extensions) | Framework/first-party patterns over `TaskScope`, not language features. |
| DI container | §7.2 (boundary note) | — | — (framework, not language) | Language has no DI; framework provides injection. |

## Runtime, deployment, tooling

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Persistence (keystone) | §1.3, throughout | — | M2 | Runtime model. |
| Editions (front-end only) | §8 | §15 (design rules) | M3 | This language's own evolution. |
| Reactivity / signals | §9.4 | §11 | M2 | Server-side signals. |
| Bundled HTTP/WS server | §9.5 | — | M2 | Library/runtime surface. |
| Tooling (LSP, fmt, lint, observability) | §9.6 | — | M2 | `fmt` opinionated; lint config declarative. |
| Formatter (`fmt`, non-configurable) | §9.6 | — | M2 | gofmt-style. |
| Lint configuration (declarative manifest) | §9.6 | — | M2 | Config declarative; rule bodies programmatic (§9.17). |
| Isomorphic logic (WASM target) | §9.7 | — | M3 | Shared contracts/logic. |
| Compile mode + DCE / tree-shaking | §9.8, §9.8.1 | — | M2 (AOT + DCE) | Static-default, `eval` opt-out. |
| Desktop / Tauri | §9.9 | — | M2 (Depth A) / M3 (Depth B) | Webview shell. |
| No shared-memory bytecode cache | §9.10 | — | M3 (startup cache) | The opcache role, eliminated. |
| Deployment targets (overview) | §9.11 | — | — | Summary table. |
| HMR | §9.14 | — | M2 | Dev-mode; rides on persistence+isolates+signals. |
| Static analysis (rule-based) | §9.17 | — (rules are advanced) | M2 (`lang lint`) | Rules programmatic; config declarative. |
| Extension system (trust-boundary) | §10 | — | M3 | WASM-sandboxed default / native FFI. |
| Stable host ABI | §10.2 | — | M3 | Design early, irreversible. |

## Completeness (general-purpose surface)

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Standard library (layered Ring 1/2/3) | §11.1 | §3, §5, §7 (core types) | M1 (Ring 1/2) / M3 (Ring 3) | Ring 3 = first-party extensions. |
| Packaging & dependencies (3 layers) | §11.2 | — | M2 (toolchain) | Own registry / cargo backend / compiler. |
| Testing (user-facing) | §11.3 | §12 | M2 | Reuses conformance harness (Plan §6). |
| Baseline data access (non-reactive DB) | §11.4 | — | M2 | Table stakes; reactive ORM is the bet. |
| Scope (in / later / out) | §11.5 | — | M3 (mobile note) | Mobile later-not-foreclosed; embedded out. |

## R&D directions (flagged bets, not finalized)

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Reactive persistence / reactive ORM | §9.12 | — | M3 | Credible layer can land late M2. |
| Collaborative / local-first / p2p (p2panda) | §9.15 | — | M3 | CRDTs + p2panda; opt-in. |
| Checked semantic-edit MCP tool | §12.8 (R&D half) | — | M3 | Node-targeted, hash-guarded edits computed onto authoritative text; prototype vs. `check_snippet` baseline. Gated on stable node-identity-over-text-edits. |

## Developer experience & agentic tooling

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Structured logging API + store drivers | §12.1 | §13 | M2 | Standard, queryable; pluggable backends. |
| Debug engine (DAP + MCP) | §12.2 | — | M2/M3 | One engine, two protocols; isolate live-inspect. |
| Profiling / flamegraphs (structured) | §12.3 | — | M2/M3 | Agent-readable tree + human visual. |
| Built-in MCP server | §12.4 | §13 (tool registration) | M2 | Dev-default, prod allowlist; the agent's surface. |
| App/framework-registered tools (free-form, typed) | §12.4 | §13 | M2 | Mechanism standard, tool set free-form. |
| Compiler-as-syntax-oracle (`check_snippet`) | §12.5 | — | M2 | Antidote to no-training-data. |
| Text-authoritative authoring (decided) | §12.8 (decided half) | — | M2 | Source text *is* the program; the semantic graph is a queryable derivation. Realized by the §12.5 oracle. Contrast: graph-as-source. |
| `lang init` scaffolding (AGENTS.md + primer) | §12.6 | — | M2 | Toolchain-generated, version-matched. |
| Semantic role tags (`SemanticRole` → typed `Role` → MCP graph queries) | §12.7 | §9.7 | M2 | Attribute-conferred roles label the call graph; `list_roles`/`trace_from`/`flows_between`; composition over manifest §9.13 + call graph §9.17 + MCP §12.4. |
| tree-sitter grammar (standalone artifact) | — (tooling artifact) | — | M3 (can land early) | Syntactic highlighting; consumed directly by Neovim/Helix/Zed/GitHub, and bundled by the VS Code extension. |
| TextMate grammar (standalone artifact) | — (tooling artifact) | — | M3 (can land early) | VS Code classic syntactic highlighting; bundled by the extension. |
| Semantic highlighting (LSP semantic tokens) | §9.6 | — | M2 | Context-aware (type vs var, `mut`, unused); layers on grammar highlighting. |
| VS Code extension (bundles grammars; auto-wires LSP+DAP+MCP) | integrates §9.6, §12.2, §12.4 | — | M3 | One install = highlighting + full tooling + agent surface. |

## Application notes & framing

| Feature | Arch | Syntax | Plan | Notes |
|---|---|---|---|---|
| Game development (scripting layer) | §9.16 | — | — | Application note; uses §3.1, HMR, FFI. |
| The coherence (synthesis) | §13 | — | — | Closing synthesis. |
| Positioning / identity | (see `00-positioning.md`) | — | §4 (strategy) | Clean break; capability thesis. |

## Build/process (plan-only, no architecture/syntax surface)

| Concern | Plan | Notes |
|---|---|---|
| Milestones M0–M3 | §3 | Walking skeleton → long tail. |
| Strategy (decided: clean break) | §4 | Capability thesis, no PHP bridge. |
| Test harness (for building the language) | §6 | Conformance corpus; distinct from user testing (Arch §11.3). |
| Agent-friendly codebase design | §7 | Salsa seams, deterministic feedback. |
| Repository structure (monorepo) | §7.1 | One repo, Cargo workspace + sibling artifacts; enforces zero-drift/version-match. |
| "Free from Rust" crate inventory | §2 | logos, salsa, tokio, hyper, tower-lsp, etc. |

---

## Known intentional asymmetries (not drift)

These `—`s are deliberate; listing them so they are not mistaken for gaps:
- **Internal runtime mechanisms** (value representation, object model, memory management, execution) have **no Syntax** surface — they are not user-visible language constructs.
- **Library/runtime surfaces** (bundled server, observability, WASM target) have **no Syntax** entry — they are APIs/build targets, not language syntax.
- **R&D directions** (§9.12, §9.15, §12.8) have **no Syntax** — their surfaces (e.g. `synced_signal`, the reactive query builder, the semantic-edit MCP tool) are sketched in the architecture brainstorms but not finalized into the syntax spec, deliberately, until the designs settle. The semantic-edit tool is an MCP surface, not language syntax, so its `—` is permanent rather than provisional.
- **Application notes** (games §9.16) and **synthesis** (coherence §12) have no Syntax/Plan — they are framing, not features.
- **`fmt`, `lint`, server, observability, extensions** have no Syntax entry because they are toolchain/runtime/config surfaces, not language syntax. Lint *rule authoring* is ordinary code (so it uses general syntax), and lint *config* is declarative manifest (not language syntax).

## Maintenance protocol

When adding or changing a feature, update **all three** docs and this index in the same pass. If a row would have an empty cell that is not a known intentional asymmetry above, that is the drift to fix. This index is the single place where the three documents' consistency is checkable at a glance.
