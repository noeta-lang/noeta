# Phase 2 — Package + dependency system (pure-Noeta packages, end to end)

*Parent: [`README.md`](README.md). Builds on Phase 0 (qualified module identity) + Phase 1 (extension
registry). This is the milestone's largest phase. Delivers the full "declare it to get it" story for
**pure-Noeta** third-party packages; first-party native ones already ship via Phase 1, out-of-tree
native rides Phase 3.*

## The enabling discovery

Noeta **already links user source across files** (`noeta-loader`, M1.9): an entry `.noe` file resolves
`use App.Models.User` against **sibling `.noe` files** that self-declare `namespace App.Models;`,
flat-merging the imported `pub` declarations into one `Program` (E0019 private/missing, E0020
collision, opaque-stub fallback). So **cross-package `use` is a resolution-layer extension, not a new
language capability**: feed a dependency package's source modules into the loader's candidate set,
rooted at the consumer's dep-key. The type-checker and both backends see one merged `Program` exactly
as today — the differential oracle is untouched.

## The determinism boundary (standing norm)

Resolution + fetch (git, network, the resolver walking manifests) are **real host IO**, done *before*
compilation, and produce two deterministic artifacts: a **materialized package store** (source trees
on disk) + **`noeta.lock`**. The existing loader/compiler then consume those on-disk sources
deterministically. So fetch/resolve live *outside* the differential oracle (like `RealHost`/Network);
the build stays reproducible because it's lockfile-pinned. The oracle sees only already-materialized
source.

## The namespace ↔ dep-key model — ✅ R1 CONFIRMED (2026-07-08)

**Re-root at the package boundary.** A package's modules declare namespaces rooted at the package's
own name segment (package `guzzle/http` → modules `namespace http.client;`, `namespace http.models;`,
internal `use http.client.X`). A consumer keys the dependency freely and the loader **replaces that
root segment with the key**: `webclient = { … }` + `use webclient.client.Client`. Convention: a
package's module namespaces must start with its package-name segment (like Rust's crate-rooted paths).
Chosen over R2 (consumer imports the canonical root, no rewrite) — R1 honors the Phase 0 "import root
= dep-key" decision and kills cross-org root collisions (two `*/http` packages coexist under distinct
keys because the key, not the shared root `http`, is what the consumer writes).

Phase 0 (user-confirmed) fixed **import root = the dependency-table key**, decoupled from the global
`company/package` identity (mirrors Rust `foo = { package = "real" }` → `use foo::X`). The mechanism:

- A package's modules declare namespaces under a **single root segment = the package-name part of
  `[package] name`** (`guzzle/http` → root `http`; modules declare `namespace http.client;`,
  `namespace http.models;`; intra-package `use http.client.X` resolves normally).
- A **consumer** keys the dependency freely (`webclient = { git = "…/guzzle/http", tag = "v1.2.0" }`)
  and imports via that key: `use webclient.client.Client`.
- At the package boundary the loader **re-roots** the dependency's module namespaces from its package
  root (`http.*`) to the consumer's key (`webclient.*`), then resolves normally. Kills cross-org
  collisions; the consumer never sees the dependency's internal root.

**Recommended** over the alternative (consumer imports the package's canonical root directly, no
rewrite) because it's what Phase 0 locked. Confirm before building 2.1's loader around it.

## Slices

Each commits green (`cargo test --workspace`, differential + conformance, fmt/clippy). Slices 2.0/2.2
are pure (no IO, unit-tested); 2.1 is deterministic (path deps, differential-covered); 2.3–2.5 are the
real-IO layer (CLI-only, outside the oracle).

### 2.0 — Manifest: `[package]` + `[dependencies]` (pure parse/validate)

Extend `Manifest` (`noeta-cli/manifest.rs`) with two tables, parse + validate only (no resolution):

- **`[package]`** — `name = "company/package"` (validated shape: one `/`, identifier segments),
  `version = "x.y.z"` (semver), optional `edition`. Derive the **package root** (the `package` part).
- **`[dependencies]`** — each key is the **import root** (dep-table key, an identifier); the value is
  a typed `Dependency`: `{ path = "…" }` | `{ git = "…", tag = "…" }` | `{ version = "^1.2" }`
  (registry form) | bare `"^1.2"` (registry shorthand). Caret-default semver.

A typed `Dependency` enum + `PackageMeta`; unit tests over each form + malformed cases. No network.

### 2.1 — Path dependencies + cross-package `use` (deterministic, differential-covered)

The load-bearing language slice, and the first that needs **no** network/resolver — a `path` dep is a
local source tree, so the whole cross-package linking mechanism is exercised deterministically.

- **2.1a — loader accepts dependency packages. ✅ DONE.** `noeta-loader` gains `DepPackage`
  (key + package root + modules), `reroot_program` (rewrite leading `namespace`/`use` segment
  root→key), and `link_with_deps`/`link_parsed_with_deps` — dependency modules are both resolution
  candidates *and* import drivers (closed unit), with origin-tracking dedup and std-import retention.
  `link`/`link_parsed` unchanged for existing callers; 4 unit tests; full corpus differential-green.
- **2.1b — CLI run/build path wired. ✅ DONE.** `manifest::dependency_packages` builds `DepPackage`s
  from the manifest's **path** deps (each dep's own `[package]` gives its root; sources read
  recursively — a package is a tree); `compile_whole_file` feeds them to `load_with_deps` **and** the
  startup-cache key + `SourceMap` (so a dep change never serves stale bytecode, and merged-dep spans
  render). A git/registry dep errors with a pointer to P2.3+. 2 CLI integration tests (re-root run +
  git-dep error); manual e2e confirmed (key ≠ root re-roots; package-internal cross-ref resolves).
- **2.1c — remaining consumers. ✅ DONE (bar conformance).** The package manager was **extracted into
  the `noeta-pm` library** (out of the `noeta-cli` binary) so `noeta-lsp`/`noeta-db` can resolve deps.
  `run_file` (`noeta serve`) and `noeta check` now feed deps (CLI side). The **salsa** `Workspace`
  gained `dep_modules` (re-rooted in the `linked` query via the now-public `reroot_program`), and the
  **LSP** resolves the entry's deps through `noeta-pm` and maps dep spans back to their files — so
  hover/goto/completion resolve **cross-package** in-editor (`goto_definition_jumps_into_a_dependency_package`).
  **Conformance** still uses the no-deps `workspace()` — its dep feed is meaningless until a
  cross-package *fixture* exists (needs a fixture-schema extension); tracked, surfaced, not built
  speculatively.

<details><summary>Original 2.1b consumer survey (all injection points)</summary>

- **2.1b — feed dependency sources at all three consumers** (survey-confirmed injection points; the
  sibling scan is duplicated, so all must learn about deps or a stale artifact diverges):
  - **CLI** — `noeta_loader::load` (main.rs:1333) + the startup-cache key `read_workspace`
    (`open_startup_cache`, main.rs:1426) **must include dep sources**, or a cache hit serves bytecode
    compiled without them. Keep `SourceId` assignment consistent across `link`/`read_workspace`.
  - **Salsa/LSP** — `noeta-db`'s `Workspace` input `modules` vec (lib.rs:264) + the LSP's own
    `discover_sources` scan (`noeta-lsp` lib.rs:282). Both feed `link_parsed` (noeta-db:305).
  - **Conformance** — mirrors both pipelines (`read_workspace` + its own db workspace); its
    differential/IR/leak oracles need the dep feed to stay faithful.
  - Cleanest seam: generalize the loader to accept an **extra rooted-module set** (dep sources as
    `Source`s carrying their dep-root), so all three consumers pass deps through one new parameter
    rather than each re-implementing out-of-directory discovery.
- Conformance: a path-dep package exporting a type/fn, imported and used; re-root collision cases;
  differential-green over the merged program.

</details>

### 2.2 — SemVer + PubGrub resolver (pure, in-memory)

Add `semver` + `pubgrub` to the workspace. A resolver crate/module: given a root manifest and a way to
fetch a package's manifest + available versions (a trait, so it's testable with a synthetic in-memory
registry), run PubGrub → a resolved `name → version` map, or an explainable conflict error (matching
the project's diagnostic bar). Pure and deterministic — no network in the test path. This is the
algorithmic core, isolated from IO.

### 2.3 — Git sources + content-addressed package store + fetch (real IO, CLI-only) ✅ DONE

- **2.3a ✅** package store (`store.rs`): dir-tree CAS under `<cache>/pkg/<key>`, atomic dir-rename
  publish, `hash_tree` integrity via KeyBuilder, reuses noeta-cache root + 0700.
- **2.3b ✅** git fetch (`git.rs`): shell out to system `git`; `ls-remote` tag→SHA (peels annotated
  tags), shallow clone into the store, HEAD-vs-SHA integrity check, `.git` stripped; idempotent.
- **2.3c ✅** wired into `dependency_packages`: a git dep is fetched→stored→linked under its key;
  `dep_package_from_dir` shared with path deps. Registry deps still error (→ P2.5). E2e CLI test: an
  app pulls a local tagged repo and runs its code. **Flat only** (direct deps; no transitive) and does
  an `ls-remote` per run — both closed by P2.4 (the lockfile pins SHA + records the resolved graph).
- **`noeta add`/`update` — deferred to P2.4** (they write/refresh the lockfile; surfaced).

<details><summary>original 2.3 scope</summary>

- **Store:** reuse `noeta-cache`'s path-resolution (`NOETA_CACHE_DIR`/XDG/`~/.cache/noeta`), the
  `create_private_dir` 0700 discipline (security-critical — the store feeds source to a compiler), and
  the `open`/`open_at`/`locate` split. **New work:** `noeta-cache` stores single `.noeb` *blobs* with
  file `rename`; a package store holds whole **source trees**, so stage into a temp dir and do an
  atomic *directory* rename (`store/pkg/<sha>/`), plus recursive sizing and a **content-verify** of
  the fetched tree against its expected SHA before publishing (the blob cache trusts its key; a
  network-fed store must not).
- **Fetch:** git clone/fetch a tag → resolve to a commit SHA (**shell out to the system `git`** —
  dependency-light, Go-like; no libgit2/gix), checkout, checksum the tree, store. A `git` dep is a
  `url` + `tag`; the tag→SHA + tree hash are what the lockfile pins.
- **`noeta add` / `noeta update`** CLI verbs (via the extension-command seam or core verbs).

</details>

### 2.4 — Transitive resolution + `noeta.lock` (reproducible pins) ✅ DONE

- **2.4a ✅** loader `DepPackage.dep_renames` map + public `reroot_program`: a package's own
  `use <its-dep-key>.…` rewrites to the resolver-assigned **globally-unique segment**, so transitive
  packages link in the flat pool without key collisions. Empty for a leaf → single-level unchanged.
- **2.4b ✅** `graph.rs`: a fetch-driven **transitive walk** of the path+git graph, deduped by package
  identity, assigning each identity a global segment (direct dep keeps the consumer key;
  transitive-only synthesizes a unique one). PubGrub (2.2) runs as the selection/validation pass over
  the materialized versions; a same-identity-two-versions clash is an explainable conflict.
- **2.4c ✅** `lock.rs` `noeta.lock` (TOML): pins identity/version/source/**commit SHA**/content hash.
  A git dep is fetched **by its pinned SHA** (`git::fetch_pinned`) — an already-stored tree needs no
  network (**offline**, reproducible); a moved tag is caught; git content-hash drift is rejected. The
  lock is refreshed after resolve, rewritten only on change.
- **2.4d ✅** `noeta add` (format/comment-preserving manifest edit, re-parsed before an atomic write,
  then re-resolve) + `noeta update` (drop pins, re-resolve, re-pin SHAs).

### 2.5 — Registry interface (design + client stub) ✅ DONE

`registry.rs`: the `Index` trait (`versions(name) -> [(version, git coords)]` + `publish`), a
file-backed `LocalIndex` (offline/tests, `NOETA_REGISTRY_DIR`), and `resolve_coords` (highest match).
A registry dep now carries an optional `package = "company/pkg"` (the registry identity, decoupled
from the import-root key); the graph walk resolves it through the index into git coords and
materializes it via the shared git path. `noeta publish` records a release. The real service is built
& hosted separately (Cloudflare). **Scope note:** joint PubGrub range co-resolution across several
registry deps is wired-ready (`resolve.rs`) but not yet driven — git/path pins are exact, so the walk
selects greedily and reports a conflict rather than backtracking; that depth arrives with the hosted
registry (which serves per-version dep metadata cheaply enough to feed the whole candidate set).

### 2.6 — Cross-package tier providers ✅ DONE

Lifted the `manifest.rs` "only `std`" provider restriction: a tier's provider may now be a **declared
dependency** (named by its `[dependencies]` key), validated against the manifest; an undeclared
provider errors. `active_tier_providers` surfaces the resolved tier→provider map for a future
tier-execution layer.

## Deferred to Phase 3 (surfaced)

Third-party **native** packages (out-of-tree crates `impl Extension` against a versioned
`noeta-native`, statically cargo-composed) — Phase 2 is **pure-Noeta packages only**. First-party
native capabilities already ship gated via Phase 1.

## Phase 2 gate

`[package]`/`[dependencies]` parse; a path dep's `use <key>.mod.Name` resolves and runs
(differential-green); PubGrub resolves a multi-package graph with explainable conflicts; a git-tag dep
fetches into the store, pins in `noeta.lock`, and a re-build is reproduction-from-lock; the registry
contract + `noeta publish` stub exist; cross-package tier providers work. Full corpus + JIT
differential green; clippy clean.
