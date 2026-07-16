# Cross-Cutting Architectural Audit — Noeta Workspace

*All paths below are relative to the audit root `/home/niklas/Code/lang/.claude/worktrees/audit-main`. Feature-shape claims were verified by actually building (`cargo check` under a scratch target dir); the tree was not modified.*

## Workspace layering as actually found (from all 52 Cargo.tomls)

| Layer | Crates |
|---|---|
| L0 — leaf vocabulary (no internal deps) | `noeta-span`, `noeta-reactive`, `noeta-crdt`, `noeta-object`, `noeta-cache`, `noeta-alloc-probe` |
| L1 | `noeta-edition`, `noeta-diagnostics`, `noeta-native` (extension ABI) |
| L2 | `noeta-ast`, **`noeta-stdlib`** (deps: native/reactive/reactive-abi/crdt + crypto batteries), `noeta-reactive-abi`, `noeta-html`, `noeta-css`, `noeta-para-p2p-net` |
| L3 — front-end | `noeta-lexer`, `noeta-parser`, `noeta-types`, `noeta-builtins`, `noeta-backend`, `noeta-check`, `noeta-ir`, `noeta-ir-passes`, `noeta-loader` |
| L4 — backends | `noeta-bytecode`, `noeta-value`, `noeta-gc`, `noeta-compiler`, `noeta-eval`, `noeta-vm`, `noeta-jit-abi`, `noeta-jit`, `noeta-bundle` |
| L5 — pipeline infra | `noeta-db`, `noeta-fmt`, `noeta-pm`, `noeta-runtime`, `noeta-runner`, `noeta-aot-runtime` |
| L6 — products/tooling | `noeta-ide`, `noeta-lsp`, `noeta-dap`, `noeta-mcp`, `noeta-prof`, `noeta-embed`, `noeta-conformance`, `noeta-playground`, `noeta-wasi-host`, `noeta-wasm-runner`, `noeta-wasm-serve`, `noeta-para-p2p`, `noeta-cli` |

The DAG is genuinely acyclic and the two backends are genuinely siblings, as documented. The surprise for a newcomer is **`noeta-stdlib` at L2** — beneath the type checker, the IR, the loader, and even the VM's value model — while ARCHITECTURE.md's crate map presents it in "Shared runtime & host" after the backends.

---

## Finding 1 — `noeta dap` / `noeta profile` / MCP `debug_*` have drifted off the production pipeline: no dependency packages, no tier activation

**Severity: high**

**Evidence:**
- `crates/noeta-dap/src/session.rs:65`, `crates/noeta-prof/src/session.rs:65`, `crates/noeta-mcp/src/debug.rs:472` all load via `noeta_loader::load(path, root_edition)` — the *sibling-modules-only* entry.
- `noeta run` goes through `crates/noeta-runner/src/compile.rs:125` (`compile_whole_file`), which resolves `dependency_packages`, calls `load_with_deps`, resolves the active tier set (`compile.rs:131-145`), and consults the startup cache.
- Both dap and prof doc-comments claim: *"This is the **same** production pipeline `noeta run` drives"* (`noeta-dap/src/session.rs:3`, `noeta-prof/src/session.rs:3`) — true when written, no longer true after the package-manager and tier-providers arcs.
- `noeta-prof/Cargo.toml` even documents the copy-paste: *"Mirrors the DAP crate's dependency set."* The `RunOutput`/`OutputChunk` structs and the entire load-error→rendered-text block are duplicated near-verbatim between dap and prof (`session.rs` in both).

**Why it matters:** A program that uses a package dependency (`use <dep>.…`) or `@tier` declarations runs fine under `noeta run` but fails to resolve under the debugger and the profiler — and inside one MCP server, the `run` tool resolves deps (salsa `workspace_with_deps`) while `debug_*` does not. This is the classic cost of pipeline assembly existing in ~5 places (runner, dap, prof, mcp-debug, salsa) — each new front-end concern (deps, tiers, editions — editions *did* get threaded to all of them) must be re-landed N times, and two were missed.

**Proposed remedy:** Extract one `compile_project(path, CompileOpts { debug_info, session, tiers, no_cache, … })` — most of it already exists as `noeta-runner`'s `compile_whole_file`; either dap/prof/mcp depend on `noeta-runner` (it is deliberately lean), or the function moves down next to the loader. The dap/prof-specific choices (debug info on/off, session checker kept alive) are two booleans it already almost has.

**Perf-regression risk:** none.

---

## Finding 2 — `noeta-stdlib` is the keystone of the DAG: ~24 crates rebuild on any stdlib edit, and the VM value model sits on top of the battery tree

**Severity: medium (challenging a partially documented decision)**

**Evidence:**
- `noeta-check`, `noeta-ir`, `noeta-loader`, `noeta-db`, `noeta-value`, `noeta-vm`, `noeta-compiler` all depend on `noeta-stdlib` (their Cargo.tomls), which transitively puts it under essentially the whole workspace (~24 crates).
- `crates/noeta-value/Cargo.toml:1493-1494` justifies its edge with a **stale comment**: *"noeta-stdlib has no internal deps, so this adds no dependency cycle"* — noeta-stdlib now has four internal deps plus sha1/sha2/md-5/hmac/bcrypt/uuid/serde_json/bytemuck/jiff.
- The actual imports in `noeta-value` are almost all `noeta-native` re-exports (`MapKey`, `ExternValue`, `NativeValue`, `Scalar`, `ExternBox`, `Executor`, `SandboxExecutor`, `mask_to_width` — verified by grep); only `FileHandle` (`noeta-stdlib/src/handle.rs:100`), `format_float`, and `json::stringify` genuinely live in stdlib.
- The "shared semantics live once in noeta-stdlib" decision is documented (ARCHITECTURE.md:115) — the *DAG position* consequence is not.

**Why it matters:** Editing any stdlib method body (e.g. `datetime.rs`) recompiles the checker, IR, compiler, value model, GC, VM, loader, db, and all tooling — the worst possible incremental-build blast radius for the crate that changes most often as the language grows. It also means the NaN-boxed value model nominally sits *above* bcrypt in the build graph.

**Proposed remedy (incremental):** (a) Re-point `noeta-value`'s imports at `noeta-native` directly and move `FileHandle` + float formatting down into the ABI crate (the Cargo.toml comment says FileHandle sharing was the only reason for the edge) — this alone frees value/gc from stdlib. (b) Longer-term: split registration *metadata* (module/function/type signatures the checker and loader consult) from dispatch *bodies*, so the front-end depends on a data-only crate and stdlib method-body edits stop rebuilding the front-end.

**Perf-regression risk:** none (build-graph only).

---

## Finding 3 — Toolchain-side error handling is stringly-typed at crate boundaries (`noeta-pm`: 148 `Result<_, String>`s)

**Severity: medium**

**Evidence:** The user-program diagnostics story is exemplary (typed `DiagnosticCode` E-codes, one `ariadne` renderer, one `JsonDiagnostic` schema shared by CLI and MCP). But `noeta-pm` — a *library* consumed by `noeta-ide`, `noeta-db`, `noeta-dap`, `noeta-prof`, and the CLI — exposes `Result<_, String>` on essentially its whole surface (148 occurrences across `crates/noeta-pm/src`; e.g. `manifest.rs:384`, `resolve`/`registry`/`keyless` throughout). No `anyhow`/`thiserror` anywhere (consistent, at least). `noeta-cli` has 36 more; consumers cannot distinguish "no manifest" from "network failure" from "signature verification failed" except by substring.

**Why it matters:** The LSP/IDE consume pm results programmatically; string errors force presentation decisions into the producer and preclude structured handling (retry vs. hard-fail, machine-readable `noeta check --format json` parity for toolchain failures). It's the one crate boundary where the "errors as data, centralized" principle (ARCHITECTURE.md:116) is not honored.

**Proposed remedy:** A small `PmError { kind: PmErrorKind, message: String }` at the crate boundary (kinds: Manifest, Resolve, Network, Auth, Provenance, Io); interior code can keep formatting strings into it. Incremental — start with the surfaces `noeta-ide` consumes.

**Perf-regression risk:** none.

---

## Finding 4 — Edition round-trips typed → `String` → lenient re-parse, contradicting its own hard-error policy

**Severity: medium**

**Evidence:**
- `noeta-edition/src/lib.rs:34-36`: *"an unknown value in a manifest is a **hard error** …, never a silently-accepted free string"*; `noeta-pm`'s `Manifest` carries a validated `Option<Edition>` (`manifest.rs:237`).
- But `noeta_loader::DepPackage.edition` is a `String` (`noeta-loader/src/lib.rs:163`), re-parsed downstream with **silent defaulting**: `Edition::parse(&dep.edition).unwrap_or_default()` at `noeta-loader/src/lib.rs:377`, `noeta-db/src/lib.rs:101` and `:403`. `noeta-pm/src/manifest.rs:534` also maps an unreadable manifest to `Edition::DEFAULT`.

**Why it matters:** Any path where a dep's edition string arrives without going through `Manifest::parse` (lockfile, registry index, a future host) silently compiles under 2026 instead of erroring — exactly the failure mode the crate's doc promises can't happen. It's also duplicate plumbing: the same value is validated once, erased, and re-validated three times.

**Proposed remedy:** Carry `Edition` (the type) through `DepPackage` and `DepSources` — the loader already names the type via the `noeta_lexer` re-export, and `noeta-db` can store the `u8`-sized enum in its salsa input as easily as a `String`. Where a string genuinely must cross (salsa/serde), make the re-parse a diagnostic, not `unwrap_or_default()`.

**Perf-regression risk:** none.

---

## Finding 5 — Extension registry reaches leaf code by two routes: per-session parameter *and* process-global fallback

**Severity: medium (challenging a documented decision)**

**Evidence:** The instance-registry arc threads `Option<&'static Registry>` through `CheckOptions` (`noeta-check/src/lib.rs:203`), `compile_with_sites_session_with_registry` (`noeta-compiler/src/lib.rs:255`), and the VM. But `noeta_stdlib::registry::default_seeded()` is still read directly at `noeta-loader/src/lib.rs:581`, `noeta-ir/src/lower.rs:163`, `noeta-ide/src/completion.rs:146,165`, `noeta-ide/src/lib.rs:624`. The compiler documents the global as the default entry's choice (`noeta-compiler/src/lib.rs:253`), and leaving LSP/MCP/IDE on the global was a recorded decision.

**Why it matters:** A session assembled with extra extensions (the `noeta-embed` path, MCP with a composed toolchain) type-checks and runs against its private registry while loader tier-seeding and every IDE answer (completion, namespace children) consult the std-only global — hover/completion can silently disagree with the checker for the same buffer. The split is invisible at call sites; nothing marks a `default_seeded()` call as "single-registry assumption here."

**Proposed remedy:** Funnel the global fallback through one named accessor per layer (e.g. `Registries::for_session(None)`), so the assumption is greppable and the IDE can be upgraded session-aware later without a hunt. No behavior change today.

**Perf-regression risk:** none.

---

## Finding 6 — Per-session registries are `Box::leak`ed; the `&'static Registry` assumption is smeared through checker/compiler/VM

**Severity: medium**

**Evidence:** `crates/noeta-embed/src/lib.rs:348`: each `Session` with custom extensions leaks its assembled registry (*"leaked to `'static`, matching the `'static` extension-data model the whole pipeline assumes"*). `CheckOptions.registry`, `check_all_with_registry`, and the compiler/VM all take `&'static`.

**Why it matters:** The embed API's own header names a game engine's scripting layer as the canonical consumer — a host that creates sessions repeatedly (level loads, hot restarts) leaks a registry per session, unboundedly. The documented rationale (extension *data* is `'static`) is fine for the one-shot CLI; it is the wrong default for the one consumer whose lifecycle is many-sessions-per-process.

**Proposed remedy:** Intern assembled registries keyed by the extension-unit set (same units → same leaked registry, so the leak is bounded by distinct configurations) — a ~20-line `OnceLock<Mutex<HashMap<…>>>` in `noeta-stdlib::registry`, no signature changes anywhere. Full `Arc<Registry>` conversion can wait.

**Perf-regression risk:** none (session construction is cold).

---

## Finding 7 — `SourceId` assignment order is an implicit cross-crate contract, reconstructed by hand in the cache-hit path

**Severity: medium**

**Evidence:** `crates/noeta-runner/src/compile.rs:283-296`: *"Rebuild the exact Source sequence `load_with_deps` assigns SourceIds to, so a cached module's spans resolve … dependency modules continue the ids in the same order the loader parses them"* — followed by a hand-rolled loop re-deriving ids positionally. The authority (the loader's iteration order) lives in a different crate with nothing tying the two together but the comment.

**Why it matters:** If `load_with_deps` ever changes its iteration order (e.g. sorts deps, parallelizes parsing), cached-bytecode spans silently resolve to the wrong files — a wrong-line-numbers bug that only manifests on warm cache hits, the least-tested path.

**Proposed remedy:** Make the loader export the ordering as an artifact — `noeta_loader::source_order(workspace, deps) -> SourceMap` — and have both the compile path and the cache-hit path call it. One function, one place the invariant can break loudly.

**Perf-regression risk:** none.

---

## Finding 8 — CLI error prefix says `lang:`, the rest of the toolchain says `noeta:` (128 vs 4 sites)

**Severity: medium (user-facing, trivial to fix)**

**Evidence:** `grep -c '"lang:'` in `crates/noeta-cli/src` → 128 (e.g. `lib.rs:772,776,781`); `"noeta:"` → 4. Meanwhile `noeta-dap/src/session.rs:68` and `noeta-prof/src/session.rs` emit `noeta: …`. The language rename (lang → Noeta) reached the binary name and the docs but not the CLI's own stderr brand.

**Why it matters:** Every toolchain error message a user sees leads with the pre-rename name; scripts/tests grepping for the prefix will encode it.

**Proposed remedy:** One `const PREFIX` / `cli_err!` macro; mechanical sweep.

**Perf-regression risk:** none.

---

## Finding 9 — Four hosts hand-write ~12 capability impls each; the deterministic trio copy-pastes state-threading verbatim

**Severity: low-medium**

**Evidence:** Exactly four full `Host`s exist: `SandboxHost` (`noeta-stdlib/src/host.rs`, 1139 lines), `RealHost` (`noeta-runtime/src/lib.rs`, 2062), `WasiHost` (`noeta-wasi-host/src/lib.rs`, 767), `BrowserHost` (`noeta-playground/src/browser_host.rs`, 618). The `Rng`/`Clock`/`Ids` impls of Sandbox/Wasi/Browser are line-identical (`stdlib/host.rs:307-357` ≡ `wasi-host/lib.rs:277-321` ≡ `browser_host.rs:227-267`); the pure kernels are properly shared (`noeta_stdlib::random::*`, the telemetry `SpanTable`), so what's duplicated is the state-threading boilerplate — but that's still 3× per capability, with per-host divergence hiding in it (each host's `clock_unix_ms` is legitimately different, which makes the identical-looking blocks around it easy to mis-edit).

**13th-capability touch-set, measured against the telemetry precedent:** 1 trait file in `noeta-native` + the `Host` supertrait bound (`host.rs:545-559`) + 4 host impls + the stdlib dispatch module + checker signature registration ≈ **7 files, O(#hosts)**. Acceptable today; each *new host* re-pays ~12 impls.

**Why it matters:** The ISP question itself is well-answered — narrow traits, a blanket `impl Host`, and default methods for optional surfaces (websockets `host.rs:181-211`, p2p groups `:397-449`) mean partial consumers depend on one trait and optional capabilities degrade honestly. The residual cost is purely the 4-way boilerplate.

**Proposed remedy:** Embeddable component structs (`DeterministicClock`, `SeededRng`, `CounterIds`, `EnvOverlay`) + a small delegation macro; Wasi/Browser/Sandbox then differ only where they genuinely differ. Do it the next time a capability or host is added, not speculatively.

**Perf-regression risk:** none (same monomorphized calls).

---

## Finding 10 — CI has no cheap check for the lean feature shapes it depends on

**Severity: low**

**Evidence:** CI (`.github/workflows/ci.yml`) covers default, `noeta-cli --no-default-features`, and the `jit` feature — but the shapes the AOT/native-size story depends on (`noeta-vm --no-default-features --features aot`, `noeta-runtime --no-default-features`, `noeta-para-p2p --features ring-p2p`) are compiled only inside the slow `build --native` e2e (jit job) or never (`ring-p2p`). `noeta-runtime/Cargo.toml:1346-1351` documents exactly this class of bug biting before (*"a ring-less `noeta build --native` … failed to compile tokio's private `sync` module — a latent gap"*). I verified all three shapes compile today.

**Proposed remedy:** One CI step: `cargo check` the ~5 lean shapes (`vm/aot`, `vm/jit,aot`, `runtime no-default`, `stdlib no-default`, `para-p2p/ring-p2p`). ~2 minutes with the cache.

**Perf-regression risk:** none.

---

## Finding 11 — Stale intent comments at load-bearing seams

**Severity: low**

**Evidence (three concrete rots):**
- `noeta-para-p2p/Cargo.toml:1000`: *"Needs the `iroh-gossip` git `[patch]` in the workspace Cargo.toml (present)"* — no `[patch]` exists in the root manifest; `Cargo.lock:3045` resolves iroh-gossip from crates.io and the build passes (verified). The comment instructs future maintainers to look for machinery that's gone.
- `noeta-value/Cargo.toml:1493`: *"noeta-stdlib has no internal deps"* — false since the reactive/crdt/native split (see Finding 2).
- `ARCHITECTURE.md:84` still lists `p2p` among the Host's twelve capability traits, while `noeta-native/src/host.rs:484-491` documents that P2p left the union for `P2pProvider` (F2b).

**Why it matters:** This codebase's greatest strength is that comments *are* the architecture documentation; agents and humans both trust them. Stale ones at seams are worse than absent ones.

**Proposed remedy:** A sweep of Cargo.toml/seam comments against reality; consider a lightweight convention of dating cross-crate claims.

**Perf-regression risk:** none.

---

## Finding 12 — `NativeCtx` is a ~40-method god-trait; the extraction pattern exists but stopped

**Severity: low (challenging a partially executed documented decision)**

**Evidence:** `noeta-native/src/ctx.rs:134-379` — marshalling, list ops, async scheduling, arena/retained cells, capability broker, packed-buffer ABI, all on one trait each backend implements. The crate is self-aware: `ctx.rs:361-369` explains that `TaskContext`/`FutureTracing`/`HotReload` were split out precisely because they *"used to grow NativeCtx one method at a time."*

**Why it matters:** Two backend impls today, so cost is bounded — but this is the third-party extension ABI; every method is a compatibility commitment, and the packed-buffer + arena groups (`ctx.rs:250-358`) are exactly the shape of the already-extracted sub-traits.

**Proposed remedy:** Continue the established pattern: `fn arena(&mut self) -> &mut dyn RetainedArena`, `fn packed(&mut self) -> &mut dyn PackedBuffers` — same zero-cost `self`-returning accessors the existing sub-traits use.

**Perf-regression risk:** none (per the crate's own accessor-cost note).

---

## Finding 13 — Naming: three "runtime-ish" crates and an ABI crate named `native`

**Severity: low**

**Evidence:** `noeta-runtime` is only `RealHost` (the CLI's real-IO host — `noeta-runtime/src/lib.rs`); the actual language runtime is `noeta-vm`; `noeta-runner` is the lean execution core; `noeta-aot-runtime` is the AOT link archive. `noeta-native` is the *extension ABI* (nothing to do with `--native` AOT builds, which are `noeta-aot-runtime`/`noeta-jit`). `noeta-compiler` is only IR→bytecode (the "compiler" a newcomer expects is check+compiler+loader).

**Why it matters:** ARCHITECTURE.md's crate map corrects all of this on first read, and renames are cheap now and expensive after 1.0/publication. Where a newcomer is most likely misled in practice: reaching for `noeta-runtime` when they want the VM, and `noeta-native` when they want AOT.

**Proposed remedy:** If any rename is taken pre-publication: `noeta-runtime` → `noeta-host-real`, `noeta-native` → `noeta-ext-abi` (or re-export shims). Otherwise: a one-line "name disambiguation" note atop the crate map.

**Perf-regression risk:** none.

---

## Finding 14 — Minor DRY residue: manifest discovery ×2, `MANIFEST_NAME` ×2

**Severity: low**

**Evidence:** The ancestor-walk for `noeta.toml` exists in `noeta-pm/src/manifest.rs:372` (canonical, used by the CLI) and again in `noeta-fmt/src/config.rs:154`, with duplicate `MANIFEST_NAME` consts (`fmt/config.rs:9`, `pm/manifest.rs:37`). The duplication is *cycle-forced* — `noeta-pm` optionally depends on `noeta-fmt` (`fmt-config`), so fmt can't call pm.

**Proposed remedy:** Move the constant + walk into a crate both already depend on (`noeta-span` or a 30-line `noeta-project` leaf), or accept it with a cross-reference comment. Low priority; the two copies agree today.

**Perf-regression risk:** none.

---

## What's already good

- **Seam crates with paid-for rationale.** `noeta-jit-abi` (AOT links runtime support without Cranelift), `noeta-reactive-abi` (extensions never see engine internals), `noeta-edition` (bottom-of-DAG vocabulary), `noeta-backend` (the two backends stay siblings). None is a gratuitous pass-through; each has a documented consumer that needs exactly that split.
- **Host ISP is genuinely solved**, not just asserted: narrow capability traits + blanket `impl<T: …> Host for T` + default methods for optional surfaces means test doubles implement one trait, minimal hosts get honest capability errors, and adding an optional surface (websockets, encrypted p2p groups) touched only hosts that support it.
- **`CheckOptions` (`noeta-check/src/lib.rs:194`)** explicitly pre-empted the `_with_x_and_y` combinatorial-entry-point family — and its doc-comment says so. The compiler crate should copy it, but the pattern exists.
- **One diagnostics renderer, typed E-codes, one `JsonDiagnostic` schema** shared by CLI and MCP (`noeta-diagnostics` `schema` feature) — agent and human output cannot drift.
- **Feature architecture is footprint-driven with measured rationale** (the wasm-release profile comment even records a *rejected* alternative with numbers). All unusual shapes I tried compile: `vm/aot`, `vm/jit,aot`, `runtime --no-default-features`, `para-p2p/ring-p2p`.
- **Global state is scarce and documented**: interned `&'static Shape`s, the default registry `OnceLock`, thread-locals confined to the heap/oracle machinery.
- **CI's oracle architecture** (differential, leak, JIT-differential, wasm-differential, miri on exactly the `unsafe` quarantine) tests invariants, not implementations.
- **The intent-comment culture** is the workspace's real cross-cutting asset — nearly every odd dependency edge carries its justification inline (which is precisely why the three stale ones in Finding 11 are worth fixing).
