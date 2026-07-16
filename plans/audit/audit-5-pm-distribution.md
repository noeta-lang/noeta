# Audit: package manager, caching, bundling, distribution & supply chain

Scope: `noeta-pm`, `noeta-cache`, `noeta-bundle`, `noeta-loader`, pm-facing `noeta-cli` (publish/add/claim/audit/compose), keyless/Sigstore code, `packages/`. All paths relative to `/home/niklas/Code/lang/.claude/worktrees/audit-main`.

---

## 1. `noeta.lock` does not pin registry version selection — every build re-solves against the live index

**Severity: high**

**Evidence:**
- `crates/noeta-pm/src/graph.rs:722-784` — `Walker::solve` gathers registry candidates via `index_for(...).releases(...)` and runs PubGrub. The lock is never consulted: of all `Walker` lock uses, only `git_pin` (graph.rs:563), `content_hash` (graph.rs:424), and `scope_trust` (graph.rs:620) exist. There is no "lock satisfies requirements → skip the index" fast path.
- `crates/noeta-pm/src/lock.rs:85-103` — the read model (`Lock`) holds `git_pins`, `hashes`, `shas`, trust pins. The per-package `version` is *written* (lock.rs:249) but never read back into anything selection consults.
- The code's own comments assume the opposite behavior:
  - `crates/noeta-pm/src/registry.rs:1190-1191`: *"A yanked release is never newly selected (**an existing lockfile pin bypasses the index entirely**, so it still resolves)"* — false; nothing bypasses the index.
  - `crates/noeta-pm/src/lib.rs:5`: *"the `noeta.lock` **reproducible pin**"*.
  - `crates/noeta-pm/src/manifest.rs:198-199` (publish_cooldown): *"**An existing lockfile pin is unaffected** (already your choice); only fresh selection is held back."*
- `noeta-registry/PROTOCOL.md` (companion repo): *"A yanked version is still returned (so an existing lockfile can still resolve it — Go's model)"* — but `HttpIndex::releases` filters yanked versions out of the candidate set entirely (registry.rs:1192), so a locked-but-yanked version cannot re-resolve; PubGrub will silently pick a *different* version.

**Why it matters:** For registry dependencies, `noeta run` is effectively `cargo update` on every invocation: publishing `1.3.0` upstream silently upgrades every consumer's next build (the publish-cooldown exists precisely to soften this, confirming versions float). Consequences: (a) builds are not reproducible from a committed `noeta.lock` (the whole point of a lock, per its own header comment); (b) builds with registry deps require network access every time — there is no offline mode even when everything is in the store; (c) a yanked or cooldown-covered version that a project already locked causes a silent version change or a hard failure (`apply_cooldown` fails closed, graph.rs:882-889) rather than "your pin still resolves"; (d) three separate documented behavioral claims are contradicted by the code. The git-SHA pin does hold for `git`/tag deps — the gap is specifically registry *version selection*.

**Proposed remedy (incremental):**
1. Record registry-resolved versions in the `Lock` read model (the field is already serialized).
2. In `Walker::solve`, first check whether the locked versions satisfy every requirement of the (unchanged) manifest graph; if so, adopt them as the solution and skip the index entirely (this also restores the documented yank/cooldown semantics for free).
3. Only fall through to the live index when the lock is absent, the manifest changed, or `noeta update` ran (which already deletes the lock, cli lib.rs:1087).

**Perf-regression risk:** none — this removes network round-trips from the hot path.

---

## 2. The dependency graph is fully re-resolved 2×+ per CLI invocation, and within one resolve the index and tree hashes are computed repeatedly

**Severity: medium**

**Evidence:**
- Every file-taking command calls `compose::maybe_delegate(file)` which runs `graph::resolve_graph(entry)` (compose.rs:95), then the command path resolves *again* via `compile_whole_file` → `manifest::dependency_packages` (crates/noeta-runner/src/compile.rs:151 → manifest.rs:514-516). `cmd_run` at cli lib.rs:2378 + 2398 is the canonical double.
- Within one resolve, registry releases are fetched **twice**: once in `solve` (graph.rs:761-764) and again in `materialize` (graph.rs:510-518) — the candidate sets gathered by `solve` are discarded before the walk.
- `GitForgeIndex::ensure_clone` refreshes tags from the network on **every** `releases()` call (git_forge.rs:79-92), so a forge-routed scope does `git fetch` per resolve — per invocation, twice.
- Tree hashing is repeated: `git::materialize_sha` computes `content_hash` (git.rs:73) that is dead (`Fetched.content_hash` is `#[allow(dead_code)]`, git.rs:29-30) because `Walker::walk` recomputes `hash_tree` itself (graph.rs:418); `gather` materializes path/git deps a second time before `walk` does (graph.rs:801 vs graph.rs:373). Net: a git dep's full tree is read and SHA-256'd on the order of 4-6× per `noeta run`.
- `assemble` re-reads every dep module's source from disk per resolve (graph.rs:1197).

**Why it matters:** The startup-cache arc got source runs to ~7.7 ms, but for any project with a manifest the *resolution* cost — subprocess git, full-tree hashing, HTTP metadata (twice), reading all dep sources — is paid before the cache is even consulted, and paid twice. It also creates a small TOCTOU window: provenance/coords are checked against the walk-time fetch, not the solve-time candidate the resolver actually chose.

**Proposed remedy:** (1) Thread the resolved `ResolvedGraph` from `maybe_delegate` into the command path (return it instead of discarding on the "no native crates" path). (2) Keep `solve`'s `registry: BTreeMap<String, Vec<Release>>` on the `Walker` and have `materialize` read from it instead of re-querying `index_for`. (3) Use `Fetched.content_hash` in `walk` instead of recomputing, and hash only on first materialization (store trees are immutable — the hash could be cached beside the tree). (4) With finding 1's lock fast path, the forge tag refresh disappears from locked builds.

**Perf-regression risk:** none — strictly removes repeated work.

---

## 3. `[targets.<name>.dependencies]` is parsed, validated, and documented — but no command ever resolves it

**Severity: medium** (functional gap in a shipped, documented manifest feature)

**Evidence:**
- `crates/noeta-pm/src/manifest.rs:751-765` (`active_dependencies`) and `graph.rs:143` (`resolve_graph_for(entry, target)`) implement target-scoped dependency resolution, with unit tests (graph.rs:1453).
- A workspace-wide search shows `resolve_graph_for(…, Some(target))` is called **only from graph.rs's own test**. Every production call site — `compile_whole_file` (compile.rs:151, via `dependency_packages`), all of compose.rs, `cmd_audit`, `cmd_add`, `cmd_update`, the LSP (`noeta-ide/src/lib.rs:476`) — resolves with `target = None`, i.e. globals only.
- `docs/Documentation-and-Tiers.md:157` advertises the feature: `[targets.dev.dependencies]  # layered on only when this target is selected`.
- `plans/dev-deps/README.md`: slice **D2 — resolution & lockfile** carries no ✅ while its siblings (D0, D3, D4c, D5, D6) do — this looks like an unfinished slice whose surface (manifest + docs) shipped ahead of its wiring.

**Why it matters:** A user who declares a dev-only dependency per the docs and runs `noeta run --target dev` gets an unknown-import error (or a tier whose provider package is validated at parse time, manifest.rs:672-682, but never linked). The manifest accepts and documents configuration that silently does nothing. Also, since deps aren't target-resolved, the startup-cache key can't be wrong for them — but only because the feature never engages.

**Proposed remedy:** Thread `target` from `compile_whole_file` (it already has it) into a `dependency_packages_for(entry, target)`; fold the target name into the startup-cache key material (the dep-source folding then covers content). Alternatively, if D2 is deliberately deferred, make `active_dependencies` under a target with scoped deps a hard "not yet supported" error and pull the docs line — a silent no-op is the worst of the three states.

**Perf-regression risk:** none.

---

## 4. The composed-toolchain cache key misses a path-dep native crate's source content — stale composed binaries in the dev loop

**Severity: medium**

**Evidence:** `crates/noeta-cli/src/compose.rs:374-421` — `compose_key` folds in: binary identity, shim kind, rings, trusted command roots, toolchain source form, and per entry `identity + dir path + Cargo.toml bytes`. The comment justifies omitting crate source: *"The entry crates' content is covered by re-resolution: **a path dep's tree hash changes on edit and the resolve step feeds fresh dirs**"* — but that is only true for store-materialized (git/registry) deps, whose `dir` is per-SHA. A **path** dependency's `dir` is a fixed local path that does not change on edit, and its content hash (which `resolve_graph` *does* compute, graph.rs:418) is not part of the key. `compose_binary` (compose.rs:249-260) skips `cargo build` entirely on a key hit.

**Why it matters:** Editing `src/lib.rs` of a path-dep native package (the exact workflow of `packages/para-p2p` development, and of any user iterating on a native extension) does not recompose — the app keeps running the stale extension until `Cargo.toml` or the toolchain binary changes. This is the "stale artifact hazard" class the startup cache goes to great lengths to prevent (`binary_identity` exists for precisely the analogous reason, noeta-cache/src/lib.rs:135-158). Note I am challenging a documented claim here because the claim's premise (fresh dirs on edit) is factually wrong for path sources.

**Proposed remedy:** Pass each entry's `content_hash` (already computed by the graph walk that produced the `NativeCrate` list — add it to `NativeCrate`) into `compose_key` in place of / alongside the `Cargo.toml` bytes. Cheap, and makes the key honest for all three source kinds.

**Perf-regression risk:** low — path-dep native edits will now trigger a recompose (correct behavior); cargo's own incrementality keeps that cheap.

---

## 5. `noeta publish` ignores `[registries]` per-scope routing — resolve and publish disagree about which registry owns a scope

**Severity: medium**

**Evidence:** Resolution routes each scope through the manifest's `[registries]` map (`Walker::index_for`, graph.rs:706-714 → `registry::open_source`, registry.rs:397-405). But `cmd_publish` opens `registry::open_default()` (cli lib.rs:1252) — the environment default (`NOETA_REGISTRY_URL` or the local index) — with no look at the manifest's routing. `cmd_claim` and `cmd_scope_require_provenance` similarly read only `NOETA_REGISTRY_URL` (cli lib.rs:779-785, 820-827).

**Why it matters:** A project with `[registries] acme = "https://registry.corp.example"` will *resolve* `acme/*` from the corporate registry but `noeta publish` inside that project writes to whatever `NOETA_REGISTRY_URL` happens to be — publishing a private package to the public default registry is exactly the leak the private-registries arc exists to prevent. A `github:` forge scope should get the forge's intentional "publish = push a tag" error (git_forge.rs:173-179), but only `open_source` knows that.

**Proposed remedy:** In `cmd_publish`, resolve the package's scope through `manifest.registries().source_for(scope)` and `registry::open_source(...)`; keep `open_default` as the unmapped fallback. Same one-liner for the docs/README uploads (they use the same `index`).

**Perf-regression risk:** none.

---

## 6. Resolution has a hidden write side effect — opening a file in the editor can rewrite `noeta.lock` (and, per finding 1, float versions)

**Severity: medium**

**Evidence:** `resolve_graph_for` unconditionally refreshes the lockfile as part of resolving (graph.rs:222-237, best-effort `lock::write`). The LSP/IDE calls the same entry point on every workspace (re)build where the file set changed: `noeta-ide/src/lib.rs:417` → `resolve_dep_modules` → `manifest::dependency_packages` (ide lib.rs:476).

**Why it matters:** A query API ("give me the dep packages so hover works") mutates project state on disk. Combined with finding 1, opening a file in VS Code after an upstream publish can silently re-pin the project to new versions — a state change no human asked for, attributed to no command. It also means the LSP performs network I/O (index queries, forge tag refreshes) on document-open. The `lock::write` no-churn guard (lock.rs:325) hides this most of the time, which makes the occasional surprise worse.

**Proposed remedy:** Split read from write: `resolve_graph` returns the graph + would-be pins; only the CLI command paths (`run`/`build`/`add`/`update`) call the lock writer. The IDE path additionally passes an "offline / lock-only" mode (with finding 1's fast path this is nearly free).

**Perf-regression risk:** none.

---

## 7. `Result<_, String>` is the error type of the entire pm layer; the IDE consequently swallows every resolution failure

**Severity: medium**

**Evidence:** Every public API in `noeta-pm` — the `Index` trait (registry.rs:91-151), `resolve_graph`, `Manifest::parse`, `keyless::verify_bundle`, `transparency::*` — returns `Result<T, String>`. The one consumer that needs to *distinguish* failures can't and therefore drops them wholesale: `noeta-ide/src/lib.rs:476` — `let Ok(packages) = noeta_pm::manifest::dependency_packages(&entry_path) else { return deps; }` — a version conflict, a provenance downgrade rejection, or a trust refusal all degrade to "no dependencies", surfacing to the user as spurious unknown-import diagnostics with the real cause invisible.

**Why it matters:** ARCHITECTURE.md:116 states the codebase ethos: *"Errors as data, centralized… No ad-hoc error strings in the stages."* The pm layer is entirely ad-hoc strings. The messages themselves are excellent (arguably best-in-class), so this is not about message quality — it's that no caller can branch on kind (retryable network vs. hard trust violation vs. conflict), and the IDE demonstrably needs to (show trust failures prominently; quietly skip network failures).

**Proposed remedy:** Incremental: introduce one `enum PmError { Network(String), Trust(String), Conflict(String), Manifest(String), Io(String) }` with `Display` preserving today's strings, migrate the `Index` trait and `resolve_graph` signatures first (mechanical `map_err`), and have the IDE surface `Trust`/`Conflict` as a workspace diagnostic instead of silence. Full typed-diagnostics integration can come later.

**Perf-regression risk:** none.

---

## 8. Cache-hit `SourceMap` reconstruction duplicates the loader's SourceId-assignment invariant across crates, unguarded

**Severity: low-medium**

**Evidence:** `crates/noeta-runner/src/compile.rs:283-300` rebuilds "the exact Source sequence `load_with_deps` assigns SourceIds to" by hand (entry, sorted siblings, then dep modules in package order), with only a comment binding it to `noeta-loader`'s behavior (loader: lib.rs:232-241 entry=0/siblings, and dep ordering inside `link_with_deps`). No test asserts the two stay in lockstep.

**Why it matters:** A cached module's spans (panic tracebacks, DAP breakpoints, diagnostics on a hit) resolve against this reconstructed map. If the loader's ordering ever changes (e.g., dep modules interleaved differently), cache *hits* silently attribute spans to the wrong files — the startup-cache key cannot catch it because the key doesn't encode ordering logic, only content. This is exactly the mirrored-invariant class the codebase elsewhere eliminates ("shared semantics live once").

**Proposed remedy:** Export one function from `noeta-loader` (`workspace_sources(entry, deps) -> Vec<Source>`) used by both `link_with_deps` and the cache slot; or add a test that compiles a multi-dep workspace, takes a cache hit, and asserts a dep-file span renders against the right file.

**Perf-regression risk:** none.

---

## 9. Registry wire schema is duplicated by hand across two repos, and has already drifted from its spec

**Severity: low-medium**

**Evidence:** The Rust client defines the wire shape ad hoc (`WireVersion`/`WireDep`/`ScopeResponse`, registry.rs:449-489; checkpoint/proof structs, registry.rs:693-720) and the Cloudflare Worker (`/home/niklas/Code/noeta-registry/src/*.ts`) re-implements it in TypeScript, coordinated only by prose (`PROTOCOL.md`: *"the client is the source of truth for the shape, the server conforms"*). The drift is already observable: PROTOCOL.md specifies yanked versions are returned so lockfiles keep resolving (Go's model), while the client deletes them from the candidate set (registry.rs:1192) and — per finding 1 — has no lock path that could honor the stated model.

**Why it matters:** Two hand-maintained encoders/decoders with no machine-checked contract is the classic cross-repo drift setup; the yanked case shows it isn't hypothetical. The hermetic mock-server tests in registry.rs test the client against *the client's own understanding*, not against the Worker.

**Proposed remedy:** Cheapest first: commit a set of golden JSON fixtures (one file per endpoint, in the registry repo) and have both repos' test suites deserialize them — the noeta-pm side via its wire structs, the Worker side via its response builders. A shared OpenAPI/JSON-schema is optional beyond that.

**Perf-regression risk:** none.

---

## 10. `LocalIndex::publish` rewrites the whole index file from lossy parses — partially-corrupt entries are silently deleted

**Severity: low**

**Evidence:** `LocalIndex::releases` skips entries with missing fields or unparseable versions via `continue` (registry.rs:249-257). `publish` then does read-modify-**rewrite of the entire file** from those parsed releases (registry.rs:306-342): any entry that failed the lossy parse — e.g. written by a newer toolchain with a shape this one half-understands — is dropped from disk on the next publish, without a warning.

**Why it matters:** The local index is the offline/test registry and the default when no `NOETA_REGISTRY_URL` is set; silently destroying records on write is a data-loss trap, and it diverges from the crate's own posture elsewhere (the lockfile treats an unknown *version* as "ignore the whole file", never "keep the parts I understand and rewrite").

**Proposed remedy:** Either fail `publish` when any entry in the existing file didn't fully parse ("index entry is corrupt/newer — refusing to rewrite"), or append-only the new `[[version]]` block textually like `manifest::add_dependency` does.

**Perf-regression risk:** none.

---

## 11. `DepPackage.edition` is a raw `String` on a stale premise

**Severity: low**

**Evidence:** `crates/noeta-loader/src/lib.rs:163-168`: *"A string (not the `Edition` enum) because the loader sits below the manifest layer that owns it."* But `Edition` was relocated to leaf crate `noeta-edition` (noeta-pm/src/lib.rs:15-18), and the loader's own signatures already take it: `load(entry, root_edition: noeta_lexer::Edition)` (loader lib.rs:65; `noeta_lexer::Edition` *is* `noeta_edition::Edition`, lexer lib.rs:18). So the justification no longer holds; the enum round-trips through a string across graph.rs:1208 → loader → ide (SourceProgram edition string) with re-parsing/defaulting at each consumer.

**Why it matters:** The editions arc's whole point is that an unknown edition is a hard error, "never a silently-accepted free string" (noeta-edition lib.rs:33-35); the `DepPackage` seam reintroduces a free string on the highest-traffic path. A typo'd/garbage edition string arriving at the checker degrades to `DEFAULT` silently.

**Proposed remedy:** Change `DepPackage.edition: Edition` (loader already depends on the type transitively; make the dep on `noeta-edition` direct). Mechanical.

**Perf-regression risk:** none.

---

## 12. Minor DRY: two git subprocess runners, four TOML-quote helpers, duplicated error prose

**Severity: low**

**Evidence:**
- Two near-identical `git` runners, both prepending auth: `git.rs:290-311` (`run_git`) and `git_forge.rs:222-234` (`git`) — the forge one loses the "is git installed?" hint on failure and untrimmed-stderr differences.
- Four hand-rolled TOML string quoters: `lock.rs:335`, `registry.rs:375`, `compose.rs:874` (`toml_quote`), cli lib.rs:1777 (`toml_string`) — identical bodies.
- The "registry dependency names no package" error is written twice with drifting wording (graph.rs:482-488 vs graph.rs:831-835).

**Why it matters:** All small, but the git-runner pair is the one worth unifying: git_auth.rs's doc (lines 14-15) itself names them "the two git choke points" — one choke point is strictly better for a security-relevant credential-injection path.

**Proposed remedy:** Move the runner into `git_auth` (or a `git_cmd` module) and a `toml_quote` into one shared spot in `noeta-pm`; fold the duplicated error message into a helper on `Dependency`.

**Perf-regression risk:** none.

---

## What's already good

- **One bytecode serialization, one artifact envelope.** `Module::encode` (postcard) lives in `noeta-bytecode`; the `.noeb` container (magic/format-version/runtime-version/flags, stapling, wasm slot) lives once in `noeta-bundle` and is shared byte-for-byte by the startup cache, `noeta build`, `--exe`, and `--wasm` — `run` and `build` even warm each other's cache entries (cli lib.rs:3653-3656). The suspected "three duplicated formats" do not exist.
- **Cache-key discipline is exemplary.** Domain-tagged, length-prefixed, order-independent hashing (noeta-cache lib.rs:314-320); the key covers entry identity, all sources, dep identities/renames/sources, root *and per-dependency* edition, runtime version, tier set, tier→provider selection, and the load-bearing local-rebuild `binary_identity` — each with a regression test naming the bug it prevents.
- **Version selection is genuinely one algorithm.** Path/git pins and registry ranges all flow into one pure PubGrub resolver (`resolve.rs`), with the semver→interval conversion validated against `semver::matches` on a dense grid (resolve.rs:177-223), and a test proving backtracking beats greedy (resolve.rs:297-327).
- **Trust evaluation is centralized and testable.** The crypto-free trust matrix is one pure function (`provenance_decision`, graph.rs:1067-1151) with the full pinned×release table documented and unit-tested; crypto is thin feature-gated wrappers; downgrade/switch rules are explicit in both directions; secrets are redacted in `Debug` (registry.rs:1114-1123, keyless.rs:138-146).
- **Hermetic supply-chain testing is real.** An in-process CA/CT/Rekor (`keyless_fixtures.rs`) mints bundles that verify under the default policy; transparency Merkle math, advisory feed heads, and the HTTP client (via an in-process mock server) are all covered without network.
- **Authority is top-down by construction.** `[trust].native`/`commands`/`require_provenance` are read only from the root manifest (graph.rs:167-175); reserved scopes (`std`/`noeta`/`core`) are refused at manifest parse, solve frontier, and walk (defense in depth), mirrored server-side.
- **Atomicity everywhere it matters.** Temp-file+rename for cache blobs, store trees (with lost-race tolerance), lockfile, and `noeta add`'s format-preserving, re-parse-before-write manifest edit.
- **The compose split is principled.** Toolchain vs. runner vs. AOT-staticlib shims off one machinery; dev features (`fmt`) only in dev compositions; footprint rings gated per artifact kind — each rule pinned by a unit test.
