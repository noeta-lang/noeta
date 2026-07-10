# Phase 4 — Hosted registry + supply-chain security model

*Parent: [`README.md`](README.md). Follows Phases 0–3 (modules → registry mechanism → packages →
native composition), all merged to local `main` (`3dc45bf4`, `v0.1.0`). This phase builds the **real
registry** (Cloudflare Workers + D1, a separate repo) and, more importantly, hardens the whole package
system against supply-chain attacks — learning from Deno/JSR/Go-modules rather than repeating NPM's
mistakes.*

## Why this phase exists

The package manager works (git/path/registry deps, transitive resolution, reproducible lockfile,
native composition), but its **trust posture is NPM-grade or worse**: everything a dependency can do,
it does *automatically*, transitively, with the app's full authority. Supply-chain attacks are now a
weekly occurrence; the ecosystem's safety is a design property we set *now*, before packages exist,
because it's unfixable later without breaking everyone.

## Threat model

Attacker goal: run code on a developer/CI machine or in production by getting malicious code into a
dependency (direct or, worse, transitive — the deep tree nobody audits).

Real-world vectors (event-stream, ua-parser-js, node-ipc, xz/liblzma, countless PyPI/npm typosquats):

1. **Compromised maintainer / malicious version** of an otherwise-trusted package.
2. **Typosquatting / dependency confusion** (`reqeust`, internal-name shadowing).
3. **Install-time code execution** (npm `postinstall`) — runs before you even import.
4. **Build-time code execution** (`build.rs`, proc-macros, node-gyp) — arbitrary code at build.
5. **Tag/artifact mutation** — the artifact differs from audited source, or a git tag is moved after audit.
6. **Registry compromise** — the index serves different code than the author published.
7. **Transitive ambient authority** — 10 vetted direct deps pull 500 transitive ones, all with full
   fs/net/env access; one reads `~/.aws/credentials`.
8. **Capability creep** — a "left-pad" string util that also opens a socket.

## What Noeta already defends (better than NPM — keep + name it)

- **Git + tagged source only, no separate build artifact** → what runs *is* the audited source
  (npm's tarball≠source gap doesn't exist). Mitigates 5 (partial-artifact class).
- **Lockfile pins commit SHA + content hash**; git deps refetch **by pinned SHA** with a moved-tag
  integrity error (`git.rs`), content-hash drift caught for immutable git sources (`graph.rs`).
  Mitigates 5 after first pin.
- **Registry is an index → git coords, not a code store** → a compromised index can at worst point at
  a different repo/tag; it can't serve code, and the SHA pin catches a swap. Mitigates 6.
- **Immutable published versions** (re-publish with different coords rejected). **Mandatory
  `company/package` namespacing** → constrains 1/2.
- **No `postinstall` hook for pure packages** → vector 3, the single biggest npm class, is absent by
  construction. Preserve this as an invariant.

## Confirmed current gaps (audited on `main` `3dc45bf4`)

| Vector | Today | Gate |
|---|---|---|
| **Native compose + arbitrary Rust build** (`build.rs`, proc-macros, `cargo build`) | **AUTOMATIC** — any `native` crate anywhere in the graph triggers compose + exec (`compose.rs:52`, `graph.rs:427`) | none (only a `NOETA_COMPOSED` re-entry guard) |
| **Dependency CLI subcommands** (`ExtCommand` → `noeta <cmd>`) | **AUTOMATIC** once composed (`lib.rs:335,362`) | none |
| **Runtime capabilities** (fs/env/net/p2p/telemetry) | **AMBIENT** — merged dep code shares the app's one `Box<dyn Host>` | none — no per-package scoping anywhere |
| **Publish provenance** | records **tag only**, not resolved SHA (`registry.rs` `GitCoords`) | lockfile SHA (TOFU: only after first pin) |

## The core principle

**Pulling a dependency grants it the least authority: sandboxed library code and nothing else. Every
escalation — native code, CLI commands, extra runtime capabilities — is an explicit, auditable opt-in
in the *consumer's* manifest, and authority flows only top-down from the human, never sideways or
upward from a dependency.** (Principle of Least Authority / capability security — the thing NPM lacks.)

The last clause is the anti-supply-chain crux: a transitive dependency can **never** authorize itself
or its own sub-dependencies. If the tree needs an escalation the human didn't grant at the root, the
build **fails loudly** and names it — informed consent, not silent power.

## The `[trust]` block (manifest mechanism)

A single **central `[trust]` table** is the *only* place authority is granted, keyed by **package
identity** (`company/package` — what you audit), not the local dep-key (arbitrary/local):

```toml
[dependencies]
imageproc = { version = "^1", package = "acme/imageproc" }   # pulled as a library — sandboxed, no native, no commands

[trust]
# The complete, auditable list of every elevated grant. Empty = no dependency runs native code or adds a command.
native   = ["acme/imageproc"]     # may compile + run its native Rust crate (accepts its cargo build runs build.rs/proc-macros)
commands = ["acme/scaffold"]      # may add `noeta <subcommand>`
```

Chosen over an inline per-dep flag (`native = true`) because **auditability is the whole point**: a
reviewer or CI diff sees every escalation in one place, not buried in a long deps table. The identity
key (not dep-key) means the grant is about the *actual package*, survives key renames, and is what a
`noeta audit` report lists.

**Transitive rule.** The root app's `[trust]` is the sole authority. If any package in the *resolved
tree* declares `native`/`commands` and is not in the root's `[trust]`, resolution **errors**, naming
the package and the dependency path to it, and pointing at the `[trust]` line to add. A dependency's
own `[trust]` (if any) governs *its* build, never the consumer's — authority does not delegate down.

## Layered defenses (this phase's scope, in order)

### L2 — Native + commands are consumer opt-in *(the enforced core; build first)*
- `graph.rs`: a native crate enters `native_crates` **only if** the root `[trust].native` lists its
  identity; otherwise the package is still usable for its **pure-Noeta** surface, and if native is
  actually required (its Noeta code calls into the native unit) resolution errors with the `[trust]`
  pointer. Compose trigger unchanged (`native_crates.is_empty()`), but the set is now authority-gated.
- Commands: a composed dependency's `ExtCommand`s register **only if** its identity is in
  `[trust].commands`; else they're omitted from the CLI (not an error — a silent capability the user
  didn't ask for simply isn't granted).
- Build-time execution is *part of* the native grant (documented): authorizing native = accepting its
  `cargo build` runs. `noeta add` / resolve surfaces "requests native code" for informed consent.
- Manifest: extend the parser with the `[trust]` table (`native`/`commands` string-identity lists);
  thread the trust set through `resolve_graph` → `assemble`/compose.

### L0 — Provenance: registry pins the resolved SHA at publish
- `noeta publish` resolves the tag → commit SHA and records **`{url, tag, sha}`** (extend `GitCoords`
  + the D1 schema). The registry record becomes the authority on "v1.2.0 = this SHA"; a first-time
  registry resolve then pins the *registry's* SHA (closing the TOFU window a bare tag leaves open),
  and a moved tag is caught against the index, not just the lockfile.
- (Later phase, not v1) sigstore/OIDC-signed provenance linking version → commit → CI builder; v1
  records publisher identity + SHA only.

### L4 — Registry-side (in the Worker)
- **Scope ownership**: a publish token is bound to a scope; only `acme/*`'s owner publishes under it.
- **Immutable versions**; **yank marks, never deletes** (Go's model — a yanked version stops being
  *selected* but existing locks still resolve, so a yank can't break the world).
- Rate limits / abuse protection (Cloudflare-native).

### L3 — Capability transparency *(surface now; enforce later)*
- Packages declare the runtime capabilities they use (`[capabilities] net, fs`). `noeta add` and a new
  `noeta audit` surface the dependency tree's **aggregate** capability + trust footprint (native,
  commands, net/fs/env/…) — Android-permissions-style informed consent.
- **Deferred to a research phase:** *enforcing* per-dependency capabilities (a util gets no `net`).
  Hard in the merged-program model — needs the linker/checker to attribute each capability call to its
  originating package and gate it at the `Host` boundary per-package. Declaration + audit ship now;
  enforcement is out of v1 scope (surfaced, not silently dropped).

## The registry service (separate repo, Cloudflare Workers + D1)

Lives in its own git repo (`/home/niklas/Code/noeta-registry`), **not** in the language workspace —
different toolchain (TS/wrangler), separate deploy lifecycle (the crates.io-vs-rustc split). The HTTP
contract is the seam; the Rust `Index` client (in `noeta-pm`) owns the wire types, the Worker conforms.

- **Store: D1** (SQLite at the edge). A row per `(name, version, url, tag, sha, published_by, published_at)`
  with `UNIQUE(name, version)` → atomic immutability + safe concurrent publish + queryable
  (list/yank). KV rejected (no atomic RMW → racy publishes). Free tier (5M row-reads/day, 5GB) is
  ample; read replication + edge Workers → low-latency global reads; rare publishes hit the primary.
- **API**: `GET /packages/{company}/{package}` → JSON `[{version, url, tag, sha, yanked}]` (maps onto
  `Index::versions`); `POST /packages/{company}/{package}` + scope token → publish (`Index::publish`);
  `POST …/yank`. `wrangler dev` for local iteration; Rust client tests run against an in-process mock
  HTTP server (no node/wrangler in the language workspace).
- **Not deployed by the agent** — deploy is the maintainer's Cloudflare creds + an outward action.

## Range resolution — ✅ DONE (S5, "build A properly")

The registry now serves **per-version dependency metadata** (the crates.io-index model), and version
selection runs a real PubGrub **backtracking** solve before materialization. `Walker::solve` gathers
the candidate graph — the path/git spine (materialized to learn identity/version/deps) plus every
reachable registry candidate (from the index, no cloning) — and resolves via a `Candidates` provider
where a **local/git source overrides the registry** for that identity (Cargo-style). This finds a
compatible set where the old greedy per-dep pick reported a false conflict (proof: the foo/bar/baz
diamond resolves to `foo 1.0.0 + bar 1.0.0`, in both a resolver unit test and an end-to-end CLI test
over real git repos + a local index).

**Deferred + surfaced (v-next):** git-deps-*of-published-packages* aren't expressible in the index
`Dep{package, req}` shape (published packages should depend via the registry); the index trusts that
a version's recorded deps match its source manifest (a real registry would verify at publish).

## Slice plan

1. **S1 — `[trust]` + native/commands gate (L2).** Manifest `[trust]` parse; authority-gate
   `native_crates`; gate command registration; transitive-native error with path + pointer. Self-
   contained in `noeta-pm`/`noeta-cli`, no network. Highest security value, testable immediately.
2. **S2 — publish pins SHA (L0).** `GitCoords{+sha}`, `noeta publish` resolves tag→SHA, lock/records.
3. **S3 — Worker + D1** (separate repo): schema, `GET`/`POST`/yank, scope tokens, `wrangler dev`.
4. **S4 — HTTP `Index` client** in `noeta-pm` (wire types, in-process mock-server tests) + wire into
   the graph walk behind the `Index` trait.
5. **S5 — drive PubGrub range resolution** over the real index.
6. **S6 — `noeta audit` + capability declaration (L3 transparency).**

Each slice commits green (workspace tests, differential/conformance, clippy). Deferred + surfaced:
per-dependency capability *enforcement* (research phase), signed provenance (v-next).

## Follow-ups #2 + #3 — ✅ DONE (post-milestone)

- **#3 — publish lints non-registry deps.** `noeta publish` rejects a package with a path/git
  dependency (a consumer resolving the release from the index would silently miss it).
- **#2 — provenance (Ed25519-signed attestations).** A scope registers an Ed25519 public key;
  `noeta publish` signs an attestation binding `version → commit` (`noeta key new` generates the
  keypair); the registry (LocalIndex, HttpIndex, and the Worker via Web-Crypto) stores + serves the
  signature and the scope key, and the Worker verifies at publish. On resolve, the consumer verifies
  the signature and **pins the scope key trust-on-first-use in `noeta.lock`** — a later registry
  serving a *different* key is rejected (the defense against a registry compromised after first use).
  Unsigned releases are allowed (unverified) for gradual adoption; `noeta audit` shows the pinned
  keys. Trust root = a registered key + TOFU pin; **the evolution is Sigstore-style keyless signing
  (OIDC → short-lived cert → public transparency log)**, which removes the long-lived secret and adds
  public detectability — same attestation shape, different trust root.

**Still deferred (v-next):** per-dependency capability *enforcement* (research; a static effect
analysis in the checker is the tractable first step, vs an object-capability language redesign).

## Phase 4 gate

A native/command dependency does nothing without a root `[trust]` grant (transitive included); the
Worker serves an immutable, SHA-pinned, scope-owned index; a range dep resolves through PubGrub
against it; a signed release is provenance-verified and its scope key TOFU-pinned; `noeta audit`
reports the tree's trust + provenance footprint. Full suite green.
