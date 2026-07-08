# Package manager — milestone scope

**Status: SCOPING (2026-07-08).** No code. This doc frames the milestone, records the user
decisions, decomposes it into phases, and marks the open decision points. Per the planning norm,
per-slice decomposition of each phase is a follow-on pass once the phase boundary is confirmed.

## What this milestone fundamentally is

The package manager is the **convergence point** for deferrals from at least six arcs — it is not one
feature but the machinery that lets *code and native capabilities enter a build only when the app
declares them*. The design's phrase for the packaging outcome:

> **first-party for trust, dependency-gated for whole-feature exclusion, tree-shaken for
> within-feature granularity.**

Three of those are already partly built (first-party = the in-tree `std` extension; tree-shaking =
the AOT DCE arc, `a7f4223`). The missing middle — **dependency-gating** — is this milestone, and it
rests on two pillars plus a gated third:

1. **A namespaced module system** — the *foundation* ("root module resolution"). Module identity
   becomes a full qualified path with a package root; today it's a bare name assumed to be `std`.
   Pure language/checker/loader/registry work; no network, no ABI freeze. **Prerequisite for
   everything below** and independently valuable (it fixes `use std.http.client`, enables the
   `std.http` split, and makes DCE precise). Design is captured in `plans/aot/dce.md` (carry-over §1).
2. **A manifest-driven package + dependency system** — `[package]`/`[dependencies]` in `noeta.toml`,
   git-tag sources, a resolver + lockfile + store + fetch, cross-package `use` resolution, and a
   *dynamic extension registry* that replaces the hardwired `static REGISTRY = &[&StdExtension]`.
3. **Third-party native extensions**, statically composed via `cargo` against a **versioned (not
   frozen)** `noeta-native`. In scope — we expand the ABI to close today's gaps rather than freeze it.

## Confirmed decisions (user, 2026-07-08)

| # | Decision |
|---|----------|
| **Sources** | **Git + tagged releases only — no tarball host.** A dep is a `path` dep or a `git` dep pinned to a **tag** (= a released version). Reproducibility comes from the lockfile pinning the resolved **commit SHA + content hash**, not an immutable tarball. (The Go-modules model with a thin resolver index.) |
| **Native code** | Consuming a package with native (Rust) code **statically composes** it via a `cargo` build → consumer needs a **Rust toolchain**. A **pure-Noeta** dependency needs **no Rust** — just a git checkout + `.noe` load. No cdylib, no C-ABI, no dynamic loading. |
| **Registry** | The registry is an **index** (name+constraint → git URL + tag + SHA + hash), *not* a code store. We **design the interface here**; it is **built & hosted separately on Cloudflare** (Workers + KV/D1). Small surface: `resolve(name, constraint)` + `publish(name, version, git-coords)`. |
| **Resolver** | **PubGrub** (the `pubgrub` crate) — explainable conflict errors match the project's diagnostic bar. SemVer, caret-default (`^1.2` → `>=1.2,<2`). |
| **Package identity** | Global identity `company/package` in `[package] name` (what the registry indexes / git coords map to). |
| **Import root** | A slash isn't an identifier, so identity ≠ import name. **Local import root = the dependency-table key**, chosen by the consumer (`http = { git = … }` → `use http.Client`). Kills cross-org name collisions; mirrors Rust `foo = { package = "real-name" }`. ✅ confirmed. |
| **ABI freeze** | **No formal C-ABI freeze.** Static cargo composition means `noeta-native` is consumed as a **normal versioned Rust crate** — Cargo's semver governs compatibility, there is no separate binary contract to freeze. Instead we **expand `noeta-native` to close today's known capability gaps** (so early third-party native packages don't force churn) and version it as a published crate. |
| **Module identity** | **Namespace-qualified, nested-path resolution binding the last segment**, and the **`std.http` client/server split** (from `plans/aot/dce.md` carry-over §1). Qualified call-site form (`http.client.get`) was considered and rejected — last-segment binding chosen. |

## Where the code stands today (verified on main `a7f4223`)

- **Registry is hardwired:** `noeta-stdlib/src/registry.rs:236` — `static REGISTRY: &[&(dyn Extension +
  Sync)] = &[&StdExtension];` (one element).
- **Module lookup is bare-name:** `find_module(name: &str)`; the checker/resolver assume the `std`
  root (`path == ["std"]` stripped). A third-party `guzzle.http` would collide with `std.http`.
- **`Extension` owns only `fn name(&self) -> &'static str`** (`registry.rs:392`) — no namespace root,
  no declared ring/feature.
- **`std.http` is unsplit:** `serve`/`response` live in the same `http` module as `get`/`post`
  (`noeta-stdlib/src/serve.rs`), which is why whole-module `use std.{http}` stays conservative in DCE.
- **`noeta.toml` exists** (`noeta-cli/src/manifest.rs`) with `[profiles.*]` and a *provider* concept
  that already parses but **rejects any provider except `"std"`** ("cross-package resolution lands
  with packages") — the manifest was shaped anticipating this milestone.
- **The AOT footprint scan** (`aot_ring_features`, `module_ring`/`fn_ring` tables in `noeta-cli`) is
  the **interim, bytecode-derived** selector this milestone replaces with manifest-driven selection.

## Phased decomposition

Each phase is independently shippable and green under the differential/conformance gates.

### Phase 0 — Namespaced module system (the foundation / "root module resolution")

*No network, no ABI freeze — language + checker + loader + registry.* Fully specified in
`plans/aot/dce.md` carry-over §1. Three coupled pieces:

- **Module identity = full qualified path incl. package root.** `find_module` matches a full path
  (`std.http.client` vs `guzzle.http.client` — distinct); each `Extension` owns a namespace **root**
  in the `REGISTRY`. Generalize `is_native_module` / `selective_import_module` / the checker off the
  hard-coded `std` to match any registered root.
- **Nested-path resolution, binding the last segment.** `use std.http.client` (today `E0005`) joins
  segments after the root, looks up `std.http.client`, binds **`client`** locally so `client.get(…)`
  works. `Const::NativeModule` carries the full path; the checker's `modules: HashSet<String>` → a
  `bound → full-path` map.
- **The `std.http` split.** `serve`/`response` → `std.http.server`; `get`/`post`/…/`_async` →
  `std.http.client`; `http` stops being a module; `Response`/`Request` types stay top-level. Makes
  whole-module DCE precise (`std.http.server` sheds reqwest). ~21 files + 3 docs migrate.

**Why first:** prerequisite for cross-package resolution (Phase 2) *and* for precise DCE; standalone
value (unbreaks nested imports, closes the last DCE conservatism). Zero external-facing risk.

### Phase 1 — Dynamic extension registry + manifest dependency-gating (build-time, in-tree)

Replace `static REGISTRY = &[&StdExtension]` with a registry **assembled from the manifest's declared
extensions at build time**. This is the *same machinery* as AOT DCE Axis B viewed from the other end:
"which extensions/rings join the `REGISTRY` slice + the link line" is one manifest-driven decision.

- Each `Extension`/`ExtModule` **declares its ring/feature** (retire the hand-maintained
  `module_ring`/`fn_ring` tables in `noeta-cli`; manifest becomes source of truth, footprint scan →
  cross-check fallback for source-only `noeta build --native`).
- Land `ring-http-server` gating (dce.md §2); make the remaining rings (`crypto`, `id`) uniform
  extension-units (dce.md §3).
- **In-tree first-party extensions become dependency-gated:** an app that doesn't declare a capability
  never links it or its transitive native deps.

**Unblocks without an ABI freeze:** `vec`/`quat` leaving `core`; **p2p P3** shipping as a
*first-party in-tree* gated extension (p2panda enters the build only when declared — no frozen ABI
needed because it's in-tree); precise manifest-driven DCE.

### Phase 2 — Package + dependency system (pure-Noeta packages, end to end)

- **Manifest:** `[package]` (`name = "company/package"`, `version`, edition?), `[dependencies]`
  (path / git-tag / registry-name forms), SemVer.
- **Resolver:** PubGrub over the dep graph → **`noeta.lock`** pinning name, version, git URL, commit
  SHA, content hash.
- **Sources & store:** git fetch (tag → SHA) + checksum-verify + a content-addressed **package
  store** (reuse the `~/.cache/noeta` pattern from `noeta-cache`); `noeta add` / `noeta update`.
- **Cross-package `use` resolution** built on Phase 0's qualified identity (dep-key → import root).
- **Registry interface** designed here (resolve/publish contract + a `noeta publish` client stub);
  the service itself is built & hosted separately (Cloudflare).
- **Cross-package tier providers** activate (the `noeta.toml` provider grammar already parses; lift
  the "only `std`" restriction).

**Delivers:** the full "declare it to get it" story for **pure-Noeta** third-party packages, plus
first-party native ones via Phase 1.

### Phase 3 — Third-party native packages (versioned `noeta-native`, no freeze)

Static `cargo` composition of separately-distributed native extensions that `impl Extension` against
`noeta-native`. Because composition is source + cargo (not a cdylib / C ABI), **there is nothing to
freeze** — a native package pins the `noeta-native` version it builds against, and Cargo's own semver
resolves compatibility in the consumer's build. Two pieces of work:

- **Expand `noeta-native` to close today's known gaps** so early third-party packages aren't forced to
  churn it: the **raw-buffer / columnar-kernel ABI** for third-party `@packed` types (P-SIMD tier-3,
  keyed `(module/type, operation)`) and **host-coupled finalizers**. These are additive now; they'd be
  breaking if demanded later.
- **Version + publish `noeta-native`** as a normal crate packages depend on; the `ExtCommand`
  external-binary (`cargo-<name>`-style) form joins here.

## Scope boundary

**This milestone = Phases 0–3.** The static-composition choice dissolves the ABI-freeze gate that
would otherwise have forced a deferral: `noeta-native` is just a versioned crate, so third-party
native packages compose in the consumer's cargo build with no binary-ABI commitment. Phase 1 already
gives p2p its dependency-gated packaging (first-party in-tree); Phase 3 extends the same "declare it
to get it" story to out-of-tree native crates.

## The convergence ledger (dependents — do not lose)

| Deferral | Source arc | Lands in |
|----------|-----------|----------|
| Namespace-qualified module identity + nested paths + `std.http` split | AOT DCE §1 | **Phase 0** |
| Manifest drives archive feature-set; footprint scan → fallback; per-`Extension` ring decl | AOT DCE §4 | **Phase 1** |
| `ring-http-server`, uniform ring gating (crypto/id) | AOT DCE §2–3 | **Phase 1** |
| Dynamic multi-extension registry (kill hardwired `&[&StdExtension]`) | higher-order-abi / native-extensions | **Phase 1** |
| `vec`/`quat` physically leaving `core` (dogfood exit) | native-extensions | **Phase 1** |
| p2p P3 real transport as dependency-gated first-party extension | p2p README §P3 | **Phase 1** (packaging) |
| Cross-package tier providers (lift "only `std`") | Documentation-and-Tiers | **Phase 2** |
| Version + publish `noeta-native` (no freeze) | higher-order-abi kickoff | **Phase 3** |
| Third-party native extensions composed out-of-tree via cargo | native-extensions | **Phase 3** |
| `ExtCommand` external-binary (`cargo-<name>`-style) form | noeta-native/command.rs | **Phase 3** |
| Raw-buffer/columnar kernel ABI + host-coupled finalizers (gap-fill) | p-simd-column-layout / deferred.md:128 | **Phase 3** (ABI expansion) |

**Adjacent (not strictly PM):** `roles_of::<RoleEnum>()` turbofish (reflection refinement, AOT DCE
Axis C) — a language-surface change that rides the reflection decision, noted here only because it's
tangled in the same DCE thread.

## Decisions resolved (2026-07-08)

1. **Scope boundary** — this milestone = **Phases 0–3** (Phase 3 folded in; no ABI freeze, expand
   gaps instead).
2. **Import root** — **local import root = dependency-table key**, decoupled from the `company/package`
   global identity.
3. **Phase 0 sequencing** — **same arc** (this milestone's first slices), not a preceding arc.

Per-phase slice files: [`phase-0-modules.md`](phase-0-modules.md) (others follow as each phase begins).

## Standing norms this milestone rides

- **Determinism / differential:** fetch + resolution are non-deterministic host IO → they live
  *outside* the differential oracle (like `RealHost`/`Network`); the *build* stays deterministic
  because it's lockfile-pinned. The oracle is untouched.
- **Build it right, not easy:** Phase 1 assembles the runtime from manifest-declared capabilities at
  the feature/crate seam — the AOT DCE arc explicitly wants its footprint scan to be the *interim*
  stand-in for this, not a parallel one-off.
- **Confirm before narrowing scope:** the Phase 3 deferral is surfaced as decision #1, not announced.
- Nothing pushed without authorization.
