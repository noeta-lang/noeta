# Backlog — everything open, in one place

The single registry of work we have said we will do and have not yet done. It merges the old
`deferred.md` (slice deferrals), the old design-proposal backlog, and the open tails of every
completed arc directory (deleted from `plans/` once shipped — their full slice histories and
design sketches live in git history).

**Discipline:** when an arc defers something non-gate, add a row here in the same commit. When a
slice or arc picks an item up, strike the row and point to the work that closed it. Each row names
its **source** and, where useful, a concrete **trigger** — the condition that should make us do it,
so we neither do it prematurely nor forget it. Items marked *(active)* are worth picking up on their
own; everything else is trigger-gated.

Nothing here is a correctness gap in shipped behavior unless explicitly marked **bug**.

## Language & type system

| Item | Source / trigger |
|---|---|
| **`obj.f(args)` on a closure-valued field** — parsed unconditionally as method dispatch (E0005); needs the field-access-then-call desugar. With it: the `Callable`/`Members` protocols | M1.8 tail + coroutines Track-I. Trigger: member-handles / user iterators holding a `next` closure |
| Nested `concurrent` inside a *spawned task's own body* runs atomically within that task's poll | A.7 residual |
| Prelude constructors (`Ok`/`Err`/`some`) and `panic` as first-class *values* | slice F2 residue. Exotic; needs hand-matched runtime arity/error text |
| **Editions S3/S4** — the first real edition-gated behavior (S3, pending a deliberate language divergence) and edition-aware diagnostics + a `noeta fix` migrator (S4, depends on S3) | editions arc. Trigger: a breaking language change we want to ship |
| **`@derive(FromJson)` — typed JSON deserialization** (the type declaration as the parsing spec; `Result<T, JsonError>` with path-carrying errors; missing `Option<T>` → `none`, missing `T` → error). Was gated on the inferred-static type system — **that landed, so this is now buildable.** Open decisions: Ring 2 vs Ring 3 placement; shape-only vs validating-constructor hook | design proposal (2026-06). *(active — the acceptance test for the type-system track)* |
| Const generics → explicit SIMD (`Simd<T, N>`) | bitwise Tier P. Trigger: user-code SIMD demand the columnar/autovectorization path can't meet |
| Range-*checked* narrowing conversions (`checked_to_u8(): u8?`) alongside the wrapping casts | bitwise W4 note. Trigger: first fallible-narrowing use case |
| E7 — editor injection for `${…}` holes inside foreign-language expr-tier bodies | expr-tiers, gated on the text-tiers editor grammar slices |

## Concurrency & runtime

| Item | Source / trigger |
|---|---|
| **Channel v1 limits:** capacity-0 rendezvous deadlocks; no auto-close when all senders drop (`close()` is explicit); a genuine cross-isolate deadlock on the *real* path hangs (spin-yield) rather than erroring — the sandbox is the oracle that catches it as E0010 | isolates I.4c |
| **Real-isolate worker environment limits:** workers snapshot only marshallable globals (a `class`-instance global is skipped → fails at use); worker teardown skips cycle collection | isolates I.4b |
| User-facing `h.cancel()` + a typed cancelled outcome (today cancellation is race/scope-internal) | A.8 scope decision |
| App-lifetime `TaskScope` patterns: DI-managed workers, durable queues, schedulers; overlaps the "background-work extensions" proposal below | §7.2 design. Framework/extension patterns, not language constructs |
| **In-run safepoint cycle collection** — both cycle reapers run only at clean exit; a program building cycles in a loop has unbounded peak residency | memory-management 6.x *(active — the main GC follow-up)* |
| Intrusive free-list registry — closes the trace collector's ~10% acyclic overhead on alloc-churn micro-benches | memory-management 6.4 |
| DAP: debug worker isolates (adapter reports a single hardcoded thread; workers run undebugged). Also: conditional/hit-count breakpoints, reverse debugging | debug-adapter deferred |

## Reactivity & web

| Item | Source / trigger |
|---|---|
| **Keyed-list structural changes** — `keyed()` captures the key set at first render; add/remove/reorder still re-renders the parent region (per-row incrementality covers in-place mutation only) | LiveView. Trigger: large mutable lists with churn *(active — the known LiveView gap)* |
| Nested reactive owner tree (SolidJS-style: a rerunning effect/computed owns and disposes child nodes) | reactivity S4b. Trigger: reactive nodes created inside a repeatedly-running body |
| Opt-in value-equality suppression on `set` (an equal value need not re-fire dependents) | reactivity S0 note. Must stay opt-in |
| OTEL: **observable/async instruments** (`ObservableCounter`/`ObservableGauge` with pull callbacks) — sync `counter`/`up_down_counter`/`histogram`/`gauge` shipped; callback-driven instruments deferred | native-otel metrics-logs, §deferred |
| OTEL: **histogram views / custom buckets / delta temporality** — every histogram uses the OTel default explicit bounds and only cumulative is emitted (the `Temporality::Delta` variant exists but is unused); custom bucket config, per-instrument views, and delta export deferred | native-otel metrics-logs, §deferred |
| OTEL: **metrics cardinality limits** — no per-metric attribute-set cap today (unbounded `BTreeMap` per instrument); a hard cardinality-limit policy slice deferred | native-otel metrics-logs, §deferred |
| OTEL: **stdout/structured-logging bridge** — `std.log` records go to the OTLP sink only; mirroring them to stdout (a `print`-bridge) is a separate decision, deferred | native-otel metrics-logs, §decision 3 |
| Synced **store** (a whole reactive dataset, the §9.12 merge point) and `.history()`/time-travel over the p2p append log | p2p "later/open" R&D |

## Packages, registry & distribution

| Item | Source / trigger |
|---|---|
| **Publish the toolchain + registry repos** — the step that makes out-of-tree packages truly portable (in-tree copies stay path deps until a committed git-dep can resolve against a public repo). Note: local `main` is hundreds of commits ahead of `origin/main`; publishing is a deliberate user decision | para out-of-tree follow-on *(the keystone item)* |
| Dynamic extension loading (the dyn dispatch tables exist; every compiled-in extension monomorphizes past them) | higher-order-abi. Trigger: a plugin that can't be compiled in |
| TUF-based Sigstore trust-root refresh (today: build-time-embedded root + env override); registry-side keyless *requirement* is a policy flag away | keyless-signing v-next |
| Per-dependency **capability enforcement** (a dep's `[trust]` grant actually bounding what it can reach) — research phase; static effect analysis is the tractable first step | package-manager phase-4 L3 |
| Git-deps of *published* packages aren't expressible in the index `Dep` shape | package-manager v-next |
| **Advisory intake beyond operator-curated:** self-service scope-owner advisories; a public report/triage queue; OSV/GHSA/RUSTSEC import with name mapping; a transparency-log suppression monitor (`noeta watch-scope`). Decide the trust model (who publishes vs who reports) first | namespace-protection arc (2026-07-15) |
| Hosted edge-platform proof (Fastly/Fermyon Spin) + an edge deployment docs page | wasm W4.2. Needs an account; stays a user action |
| Desktop packaging (Tauri); with it the p2p packaging polish (within-feature DCE pruning, capability-gating) | M3 roadmap item; the one roadmap entry the README carries |

## Tooling

| Item | Source / trigger |
|---|---|
| Profiler: continuous / attach-to-running-`serve` profiling; differential A/B flamegraph compare; column-precise attribution | profiler deferred tail |
| tree-sitter: per-project generated grammar for third-party text tiers (a static grammar can't know which `@name` opens a verbatim body; the TextMate side ships a generator) | text-tiers / Documentation-and-Tiers |
| MCP: prompts, semantic/embedding retrieval, long-lived analysis sessions, TCP transport | mcp deferred |
| `noeta fmt`: width-wrapping of long binary/method chains and unions; `--diff`; `// fmt: off`; broader `[fmt]` config | fmt deferred (optional by design) |
| REPL: JIT at the prompt | repl-on-vm follow-on. Trigger: demand |
| Salsa: a deleted file's **fixed-size input struct** cannot be freed — salsa 0.27 has no public input-delete API (append-only input table; `evict_lru`/revision-GC act only on LRU tracked functions). Mitigated, not fully closed: on deletion `noeta_db::release_source` reclaims all *unbounded* content (input text + downstream `ast`/`Sites`/`Module` memos, overwritten with empty-program equivalents) and `WorkspaceCache` reuses tombstoned slots for the next new file, so the input table is bounded by the *concurrent* file high-water mark. Remaining residual: N distinct files created-then-deleted with no later adds leaves N small empty input slots. Closes if salsa gains real input deletion (upstream) or we switch to an interned-path input keyed by a reusable id. | audit F9 residual a (partial) |

## Performance

| Item | Source / trigger |
|---|---|
| **Kernels in Noeta** — JIT/AOT loop-shape recognition over packed lists, so `for p in ps { total += p.r }` compiles to the tight buffer loop a native kernel is; demotes the native-kernel ABI to an escape hatch | kernel design discussion (2026-07-09). Trigger: packed bulk math in *user* code as a demonstrated bottleneck |
| **P-MONO** — monomorphic shape specialization (generics are erased-for-storage) + reflection cross-`dyn` element recovery (`List<int>`'s `int` after a `dyn` boundary) | M1.8 tail / perf ledger. Trigger: a workload demonstrating its value |
| `TypeId` interning in `noeta-types` (`Type` is a plain owned tree) | Trigger: a checker profile showing clone cost (the audit's env-borrow work already cut checker allocs ~39%) |
| Inline caches for extern-type methods; borrowed-arg projection for registry dispatch (owned `NativeValue::Str` clones per call) | extern-types follow-ons. Trigger: a hot extern-method workload |
| Zero-alloc map-key probe adapter (`Equivalent<MapKey>` + canonical hash off object slots) + reuse-analysis recycling of per-iteration key temps | packed-keys follow-up. Trigger: keyed-lookup profile |
| VM-throughput next-gap investigation — the disassembly-driven inventory of remaining PHP-gap slices (post-P-VMT) | standing investigation, `scratch-bench/xlang/RESULTS.md` for the current numbers |

## Stdlib follow-ons (all trigger-gated)

| Item | Source / trigger |
|---|---|
| Crypto: blake3; argon2/scrypt (needs the algorithm-tagged verify design); AEAD encryption (AES-GCM / chacha20-poly1305 — only WITH a key-management story) | crypto arc scope cuts |
| HTTP client: per-request timeout; streaming bodies (rides the `ReadSource` model); cookie jar / redirect / retry config; HMAC request-signing helpers | http arc scope cuts |
| Raw TCP/UDP (`std.net`) for non-HTTP protocols — needs a scripted-peer deterministic sandbox model (harder than the HTTP responder) | http-server design question. YAGNI until a concrete protocol |
| CRDTs: last-write-wins register; add/remove OR-Set | p2p arc |
| Kernel bundles: `IVec2`/`IVec3`, `f64` vectors, `Transform` (TRS), `Color` | kernel-methods deferred |

## Design proposals (no slice yet)

- **Vulnerability-intake trust model** — see the advisory row above; the design decision (operator / scope-owner / promote-from-report) precedes any code.
- **Packed-field-kind enum dedup** — four phase-appropriate `PackedKind` encodings; revisit only if a shared public layout vocabulary earns its keep across the package boundary (the `PackedView` ABI may already be that vocabulary — check before building).
- **WASM revisit conditions** (recorded, not planned): direct wasm codegen only on perf data; wasm-threads isolates only on multi-core edge demand; p2p-in-browser is its own arc.
