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
| **`Members`/`DynamicCall` protocol dispatch** — registered in the trait table (`get(name)` / `call(name, args)`) but with no behavior: no language construct consumes dynamic member access today (`obj.f(args)` field-calls, `Callable`, and `next`-driven iterators all shipped without it). Deliberately deferred rather than shipping speculative machinery | M1.8b tail (the last unwired protocol names). Trigger: a concrete consumer — a dynamic-proxy/ORM-style use case, or reflection-driven member access |
| **Generic-method call-site-typed forwarding** — a generic METHOD's own type parameter cannot forward into a call-site-typed native position (`json.try_parse::<U>` inside a method body): method dispatch has no hidden-slot channel, so it is a clean `E0058` (poly-deferrals D3 pinned this — see `tests/conformance/generic_methods/method_forwarding_pinned.noe`). Forwarding stays a top-level-generic-fn capability; wiring the hidden-arg machinery (`TypeArgInfo` table + `$ty` params) through method dispatch is the additive extension | poly-deferrals arc D3 (2026-07). Trigger: a real generic service type wanting per-`U` decode in a method |
| **Reflection-intrinsic forwarding** — `roles_of::<T>`/`from_bytes::<T>` with a forwarded type parameter stay clean checker errors (composite `List<T>`, nested-`fn`, `attributes_of::<T>`, and forwarding-fn-as-value forwarding all shipped in poly-values D2b/D2c). Each remaining case is an additive extension of the same hidden-slot machinery | poly-values arc F2b (2026-07). Trigger: a reflection-driven generic wanting per-`T` role/decode lookup |
| **Trait-method generics** — a trait's required-method set stays monomorphic: a per-method `<U>` on a trait method is a clean `E0058` (poly-deferrals D3 pinned this — the trait is dispatched dynamically, so each `impl` would have to agree on the method's own parameters). Generic methods live on concrete `class`/`struct`/`enum` types. Supporting them needs per-impl method-parameter unification at the dispatch seam | poly-deferrals arc D3 (2026-07). Trigger: a trait abstraction wanting a method-scoped type parameter |
| **Editions S3/S4** — the first real edition-gated behavior (S3, pending a deliberate language divergence) and edition-aware diagnostics + a `noeta fix` migrator (S4, depends on S3) | editions arc. Trigger: a breaking language change we want to ship |
| ~~Validating-constructor hook for typed JSON decode~~ **SHIPPED (validation arc, 2026-07)** — the `Validate` built-in trait (`validate(): Result<void, E>`, `E` a `string` or an `Error`), auto-run **bottom-up** at every recipe door: `json.parse::<T>` (aborts, E0007), `json.try_parse::<T>`/`decode_typed` (recoverable `Result.Err(JsonError)` with `field[i]: <msg>` path), and `from_bytes::<T>` (packed, aborts at `[i]`). Plus `@validated` static construction channeling — outside-the-impl literal/record-update construction is E0060, forcing a validating constructor. Docs: `docs/Validation.md`. Conformance: `tests/conformance/validation/`. **Deferred within-arc:** `@validated` on an `enum`/`trait` is silently dropped rather than a misplacement diagnostic (structs/classes are the only literal-constructed types); dev-tier (`in_dev_tier`) does not bypass E0060 (a white-box test builds through the constructor). Trigger for the enum/trait diagnostic: a user hitting the silent drop |
| **Multi-source `From` impls on one target** — `impl From<JsonError>` + `impl From<IoError>` on the same `AppError`. Barred today by construction: impl-block methods flatten into the type's method table by NAME (no overloading), so a second `From` is a coherence conflict (E0027); supporting it needs source-typed dispatch for `from` (overloading or mangled per-source methods) — a new dispatch category. The `?`-conversion checker side (`try_conversion_sites` keyed by resolved source) is already shaped for it | error-ergonomics arc (2026-07). Trigger: a real pipeline funneling ≥2 error types into one wrapper |
| **`From<Source>` as a generic bound** (`<T: From<int>>`) — `From` impls are recorded with their instantiation, but the built-in bound checker (`satisfies`) is name-only; an instantiated *built-in* bound needs the user-trait bound machinery (`TraitBound` args) extended to built-ins. Explicit `Target.from(x)` calls and the `?` position work today | error-ergonomics arc (2026-07). Trigger: first generic helper wanting to construct over a conversion |
| Const generics → explicit SIMD (`Simd<T, N>`) | bitwise Tier P. Trigger: user-code SIMD demand the columnar/autovectorization path can't meet |
| Range-*checked* narrowing conversions (`checked_to_u8(): u8?`) alongside the wrapping casts | bitwise W4 note. Trigger: first fallible-narrowing use case |
| E7 — editor injection for `${…}` holes inside foreign-language expr-tier bodies | expr-tiers, gated on the text-tiers editor grammar slices |

## Concurrency & runtime

| Item | Source / trigger |
|---|---|
| App-lifetime `TaskScope` patterns: DI-managed workers, durable queues, schedulers; overlaps the "background-work extensions" proposal below | §7.2 design. Framework/extension patterns, not language constructs |
| Safepoint GC inside **re-entrant runs**: mid-run collection is gated to the outermost interpreter loop (a nested run's outer register stacks live in Rust locals the poll cannot enumerate), so cycles built inside `map`-applied closures, `NativeCtx`-driven loops (the HTTP serve loop), or a single mammoth un-awaited task body still accumulate until the next outermost safepoint / exit. Rooting the outer stacks (an active-stack registry) would lift the gate | memory-management 6.x follow-up (safepoint GC shipped; this is the residual scope) |
| Intrusive free-list registry — closes the trace collector's ~10% acyclic overhead on alloc-churn micro-benches | memory-management 6.4 |

## Reactivity & web

| Item | Source / trigger |
|---|---|
| OTEL: **observable/async instruments** (`ObservableCounter`/`ObservableGauge` with pull callbacks) — sync `counter`/`up_down_counter`/`histogram`/`gauge` shipped; callback-driven instruments deferred | native-otel metrics-logs, §deferred |
| OTEL: **histogram views / custom buckets / delta temporality** — every histogram uses the OTel default explicit bounds and only cumulative is emitted (the `Temporality::Delta` variant exists but is unused); custom bucket config, per-instrument views, and delta export deferred | native-otel metrics-logs, §deferred |
| OTEL: **metrics cardinality limits** — no per-metric attribute-set cap today (unbounded `BTreeMap` per instrument); a hard cardinality-limit policy slice deferred | native-otel metrics-logs, §deferred |
| OTEL: **stdout/structured-logging bridge** — `std.log` records go to the OTLP sink only; mirroring them to stdout (a `print`-bridge) is a separate decision, deferred | native-otel metrics-logs, §decision 3 |
| Synced **store** (a whole reactive dataset, the §9.12 merge point) and `.history()`/time-travel over the p2p append log | p2p "later/open" R&D |

## Database (para/db)

| Item | Source / trigger |
| --- | --- |
| **Per-dialect migration overrides** — v1 runs each migration body verbatim in the target dialect's native SQL (write portable SQL). A `migrations/postgres/` (or `sqlite/`) sub-directory shadowing a base file for a divergent backend (e.g. `SERIAL` vs `AUTOINCREMENT`), selected by the connected driver's dialect. `load_dir` already ignores sub-directories, leaving room for this | migration engine (aether DB6, 2026-07-19). Trigger: a project genuinely needing to target both SQLite and Postgres from one migration set |
| **Out-of-order migration detection** — the runner applies pending files in filename-sort order and gates history integrity via checksum + deleted-file checks; it does not warn when a *newly added* migration sorts before an already-applied one (a rebase inserting an earlier prefix). Timestamps make this rare; a warning (not a hard error, to keep legitimate merges unblocked) could flag it | migration engine (aether DB6, 2026-07-19). Trigger: an out-of-order incident in practice |

## Packages, registry & distribution

| Item | Source / trigger |
|---|---|
| **Publish the toolchain + registry repos** — the step that makes out-of-tree packages truly portable (in-tree copies stay path deps until a committed git-dep can resolve against a public repo). Note: local `main` is hundreds of commits ahead of `origin/main`; publishing is a deliberate user decision | para out-of-tree follow-on *(the keystone item)* |
| Dynamic extension loading (the dyn dispatch tables exist; every compiled-in extension monomorphizes past them) | higher-order-abi. Trigger: a plugin that can't be compiled in |
| TUF-based Sigstore trust-root refresh (today: build-time-embedded root + env override); registry-side keyless *requirement* is a policy flag away | keyless-signing v-next |
| Per-dependency **capability enforcement** (a dep's `[trust]` grant actually bounding what it can reach) — research phase; static effect analysis is the tractable first step | package-manager phase-4 L3 |
| Git-deps of *published* packages aren't expressible in the index `Dep` shape | package-manager v-next |
| **Advisory-intake residuals** (arc landed 2026-07-19: three-tier feed — operator/publisher/imported — + public report queue + `noeta watch-scope`; merged both repos, unpushed). ~~(a) client verb to **promote a report**~~ ✅ DONE — `noeta advisory promote <report-id>` (operator via `NOETA_REGISTRY_ADMIN_TOKEN` → operator advisory; scope owner → keyless publisher advisory, prefilled from the report) + `noeta advisory reports [--scope S]`; new `GET /v1/reports/{id}`. ~~(b) **CVSS-vector scoring**~~ ✅ DONE — CVSS v3.1 base-metric equations both sides (`src/cvss.ts` / `noeta-pm::cvss`, unit-tested vs published vectors); import derives the band from an OSV `CVSS_V3` / GHSA vector, `noeta audit` shows band + re-derived score; text severity is the fallback. ~~(c) per-ecosystem source adapters + pagination~~ ✅ DONE — `src/sources.ts`: OSV api.osv.dev (per mapped package), GHSA GraphQL (`GITHUB_TOKEN`), RUSTSEC OSV feed, paginated, per-source env-gated, dedup by upstream id; `OSV_IMPORT_URL` kept as manual override. **(d) OPEN — design only:** `watch-scope` state is a local file; a shared/attested watch ledger — see the sketch in [`plans/attested-watch-ledger.md`](attested-watch-ledger.md) (recommendation: near-term opt-in committed signed-state file; long-term witness cosigning à la CT/Sigsum; trigger = a second registry operator or a cross-party compliance need) | advisory-intake arc (2026-07-19) |
| Hosted edge-platform proof (Fastly/Fermyon Spin) — **guide + script pre-written** (`docs/Edge-Deployment.md`, `examples/edge-hello/`; the `--serve` component verified serving on local Spin 4.0.2 — all routes + real entropy/wall-clock). **Remaining:** an account + one `spin deploy` + screenshot/notes; and Fastly Compute confirmation (unverified — its SDK world differs from the `wasi:http` proxy world we emit) | wasm W4.2. Needs an account; stays a user action |
| **Package the prebuilt `noeta-wasm-serve` component with the toolchain** — the bridge to offline one-click `noeta build --serve`. Today it resolves via `NOETA_WASM_SERVE` → sibling `noeta-wasm-serve.wasm` → on-demand `cargo build` (`wasm32-wasip2`); a binary-only released toolchain has neither of the first two, so `--serve` can't build offline there. Same distribution decision already pending for the AOT runtime + wasip1 runner | wasm W4.2 follow-on (surfaced writing `docs/Edge-Deployment.md`) |
| Desktop packaging (Tauri); with it the p2p packaging polish (within-feature DCE pruning, capability-gating) | M3 roadmap item; the one roadmap entry the README carries |

## Tooling

| Item | Source / trigger |
|---|---|
| Profiler: continuous / attach-to-running-`serve` profiling; differential A/B flamegraph compare; column-precise attribution | profiler deferred tail |
| MCP: prompts, semantic/embedding retrieval, long-lived analysis sessions, TCP transport | mcp deferred |
| REPL: JIT at the prompt | repl-on-vm follow-on. Trigger: demand |
| Salsa: a deleted file's **fixed-size input struct** cannot be freed — salsa 0.27 has no public input-delete API (append-only input table; `evict_lru`/revision-GC act only on LRU tracked functions). Mitigated, not fully closed: on deletion `noeta_db::release_source` reclaims all *unbounded* content (input text + downstream `ast`/`Sites`/`Module` memos, overwritten with empty-program equivalents) and `WorkspaceCache` reuses tombstoned slots for the next new file, so the input table is bounded by the *concurrent* file high-water mark. Remaining residual: N distinct files created-then-deleted with no later adds leaves N small empty input slots. Closes if salsa gains real input deletion (upstream) or we switch to an interned-path input keyed by a reusable id. | audit F9 residual a (partial) |
| IDE hover asymmetry for `@validated`: `hover_directive` (token-level hover, `noeta-ide/src/lib.rs`) has no descriptor for `@validated` (and none for `@tier`, which hovers via `hover_tier`), while completion's `decorator_detail` does. Now *explicit* after the `BuiltinDirective` refactor (the `Validated \| Tier => None` arm), so it is a visible decision, not a silent miss. Trigger: decide whether `@validated` (and the other no-hover directives) deserve a token-level hover descriptor; if yes, add the arm — behavior change, hence deferred out of the behavior-preserving refactor. | w8 directive-enum refactor (2026-07-19) |

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
| ~~**`std.session`** — the stateless signed-cookie codec + `Session` request/response pair~~ ✅ DONE — shipped as S2/S3 of the cookie arc: `keyring`/`encode`/`decode` + `open`/`attach`, verify-before-parse, mandatory `exp`, rotation keyring, 4096-byte hard error, required `secure` argument | cookie arc S1 tail |
| Manifest `optional` / `[suggests]` dependencies — a package declaring a dependency an app opts into. Rings are the wrong layer (native-payload DCE, unknown to `noeta-pm`); `Dependency::Scope` covers the `para/aether` + `para/db` case today, and the diagnostic is the better discoverability surface | http-sessions S4 design question. Trigger: a second use case that `Scope` does not cover |
| Raw TCP/UDP (`std.net`) for non-HTTP protocols — needs a scripted-peer deterministic sandbox model (harder than the HTTP responder) | http-server design question. YAGNI until a concrete protocol |
| CRDTs: last-write-wins register; add/remove OR-Set | p2p arc |
| Kernel bundles: `IVec2`/`IVec3`, `f64` vectors, `Transform` (TRS), `Color` | kernel-methods deferred |

## Design proposals (no slice yet)

- ~~**Vulnerability-intake trust model**~~ — DECIDED + built (advisory-intake arc, 2026-07-19): three tiers (operator-issued / scope-owner self-service with keyless provenance / OSV-GHSA-RUSTSEC import via operator-curated name map), a public report queue that only becomes an advisory on operator/owner promote, and `noeta watch-scope`. See the advisory-intake residuals row above for what's left.
- **Packed-field-kind enum dedup** — four phase-appropriate `PackedKind` encodings; revisit only if a shared public layout vocabulary earns its keep across the package boundary (the `PackedView` ABI may already be that vocabulary — check before building).
- **WASM revisit conditions** (recorded, not planned): direct wasm codegen only on perf data; wasm-threads isolates only on multi-core edge demand; p2p-in-browser is its own arc.
