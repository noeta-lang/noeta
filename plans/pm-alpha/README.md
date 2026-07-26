# Arc — Package-manager public alpha

Status: in-progress (run-list compiled 2026-07-26, after the para extraction shipped)

## Where this stands

The full package story is live and proven end to end: noeta v0.2.0 released (binaries + install.sh +
`noeta upgrade`), all 7 para packages extracted to standalone repos under the noeta-lang org and
published to registry.noeta.dev (keyless-signed, provenance-enforced, transparency log at
tree_size 7), and a from-scratch consumer resolved the 3-package `para` scope from the hosted
registry, composed the native driver, and ran queries. This arc is everything package-manager
related still needed before announcing a **public alpha**.

## Blocking — correctness / operability gaps

| # | Item | Source |
|---|---|---|
| 1 | **Push + open the registry repo.** `/home/niklas/Code/noeta-registry` is ~6 commits ahead of its origin (org-mapping fix, FIRST_PARTY_SCOPES, deployed worker c9dd5a66 built from unpushed source). Decide visibility, push, and write down the deploy ritual (wrangler) and a D1 backup/export story — today the index exists only in production D1. | extraction arc |
| 2 | **Default routing for the audit/log paths.** `open_default()` falls back to registry.noeta.dev, but the `open_http()` audit/verify/transparency-log paths still require an explicit `NOETA_REGISTRY_URL`. Same precedence everywhere: env URL > env dir > hosted default. | deferred at the v0.2.0 routing flip (`crates/noeta-pm/src/registry.rs`) |
| 3 | **Self-service scope claim, proven on prod.** `para` was claimed via the admin bootstrap. A third party must be able to run `noeta claim` against registry.noeta.dev end to end (GitHub identity, token issuance, first publish). Verify on production with a throwaway scope, then document the flow. | namespace-protection arc |
| 4 | **Native-consumer prerequisite UX.** Composing a native package requires cargo + rustc on the consumer's machine. Missing-toolchain and compose failures need actionable errors (what to install, where the log is), and the docs need the requirement + first-compose time expectation stated up front. | extraction arc |
| 5 | **Hermetic native-from-index round-trip test in CI.** The e2e that now works in production (index → git fetch → compose → run → ExtCommand dispatch) has no in-repo guard; `doc.rs:229-231` deferred it. Template: the `_oot-proof` fixtures + a local registry dir. Language changes silently rot out-of-tree packages otherwise — this is the tripwire. | extraction arc / CI-repair gotcha |
| 6 | **Docs refresh to match reality.** `packages/README.md` is pre-extraction; wiki Package-Registries / Provenance / Manifest pages predate scope arrays, `[trust]` (native + commands), registry deps (`{ version, package }`), lockfile v2 `[[scope]]` TOFU identity pinning, and the `noeta update` re-pin flow. Add a getting-started "add a dependency" page. | extraction arc |
| 7 | **Compose-cache lifecycle.** Each toolchain tag leaves a 1.3–2 GB build under `~/.cache/noeta/compose` with no GC. Minimum: document location + safe deletion; better: `noeta cache ls|clean`. | observed during the cascade |
| 8 | **Extension-author compatibility statement.** Which crates a native package may depend on and what stability they get: `noeta-ext-abi` + `noeta-reactive-abi` stable, conformance harness usable as a dev-dep, `noeta-embed` unstable. Publish the statement; it is the API contract of the whole native ecosystem. | extraction arc gaps |

## Should-have — announcement quality

| # | Item | Source |
|---|---|---|
| 9 | **Windows story.** No Windows target, and install.sh is unix-only. Either add `x86_64-pc-windows-msvc` to the release matrix or state "Linux/macOS only at alpha" in the docs and installer error path. | release pipeline |
| 10 | **macOS Gatekeeper.** Release binaries are not Apple-signed/notarized; first run may be quarantined. Document the workaround now, evaluate signing later. | release pipeline |
| 11 | **Registry browse surface.** registry.noeta.dev has no human-facing package browse; the docs-browser registry branch (`docs-api-render`) is unmerged. Decide the minimum for alpha (even a static package list beats a bare API). | docs-browser arc |
| 12 | **Token rotation / revocation + scope-policy defaults.** Publish tokens are admin-issued and irrevocable today; decide rotation UX and whether `require-provenance --root keyless` should be the default for newly claimed scopes. | registry service |
| 13 | **Worker abuse posture.** Rate limits on publish/claim endpoints, transparency-log checkpoint monitoring, D1 growth. Cheap now, painful after abuse. | registry service |
| 14 | **Dev-time path override.** No Cargo-`[patch]` equivalent: developing a published package against a consumer app means hand-editing deps to `path`/`file://` and regenerating locks. Fine for us, rough for outside contributors — likely first post-alpha item if cut. | extraction arc gaps |
| 15 | **crates.io / npm stance.** The 0.0.1 squat-protection stubs are live; crates are distributed by git tag BY DESIGN. Write the one-paragraph explanation into the stub READMEs and docs so the empty crates don't read as abandonment. | squat-protection pass |
| 16 | **Alpha quickstart + announcement.** Install one-liner → `noeta init` → add a `para` dependency → run; blog post; landing-page copy that mentions the registry. | user directive |

**Discipline:** strike rows here as they close; anything cut at announcement time moves to
`plans/backlog.md` with a trigger, per `plans/README.md`.
