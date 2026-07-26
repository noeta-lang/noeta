# Arc — Package-manager public alpha

Status: burn-down complete 2026-07-26 (announcement next); two rows remain user-gated

## Where this stands

The full package story is live and proven end to end: noeta v0.2.1 released (v0.2.0 + the alpha
burn-down: toolchain-floor enforcement, unified routing, `noeta cache`, `[patch]`), all 7 para
packages published to registry.noeta.dev (keyless-signed, provenance-enforced, transparency log),
para/cli at 0.1.1, the registry hardened (rotation, rate limits, keyless-by-default claims,
browse page) and its ops documented, docs.noeta.dev refreshed and redeployed. Below, every row of
the original run-list with its resolution; strike-through = shipped.

## Blocking — resolved

| # | Item | Resolution |
|---|---|---|
| 1 | ~~Registry ops story~~ | DEPLOY.md + `scripts/backup-d1.sh` (per-table, data-only — whole-db D1 export refuses FTS5); pre-migration backup taken; migration 0017 applied; worker deployed |
| 2 | ~~Default routing for audit/log paths~~ | `http_base_for`/`default_http_base` unify every verb onto URL > DIR > hosted; claim audience derived from the registry host |
| 3 | Self-service scope claim proven on prod | **USER-GATED:** server path ready (worker verifies GitHub identity); awaiting the user's claim run + the GitHub OAuth app (client id) for the laptop device flow |
| 4 | ~~Native-consumer prerequisite UX~~ | Docs state cargo/rustc requirement + first-compose expectation (Using-Packages, Extension-Compatibility); compose errors surface cargo's own diagnostics |
| 5 | ~~Hermetic native-from-index round-trip test in CI~~ | `composed_toolchain_native_package_from_registry_index` (+ the git-dep sibling) now run in CI (`-- --ignored composed_toolchain`); found+fixed a real stale-shim-cache bug (compose key stamped into shim version) |
| 6 | ~~Docs refresh~~ | All packaging pages refreshed against source; new Using-Packages + Quickstart-Packages (every command verified live); stale claims fixed across 10+ pages |
| 7 | ~~Compose-cache lifecycle~~ | `noeta cache ls|clean [--all]` with identity-stamped compose entries; documented in The-CLI |
| 8 | ~~Extension-author compatibility statement~~ | docs/Extension-Compatibility.md — stable surface, unstable list, one-toolchain-wins model, pre-1.0 policy |

## Should-have — resolved

| # | Item | Resolution |
|---|---|---|
| 9 | ~~Windows story~~ | Linux/macOS-only stated in docs; install.sh names supported pairs on unsupported OS/arch |
| 10 | ~~macOS Gatekeeper~~ | Documented (Getting-Started + README) with the quarantine remedy |
| 11 | ~~Registry browse surface~~ | Was already live (docs-api-render merged 2026-07-14); home listing upgraded to description + keyword cards |
| 12 | ~~Token rotation + policy defaults~~ | `POST /v1/scopes/{scope}/rotate` (current-token or admin auth); new claims default to require-provenance keyless |
| 13 | ~~Worker abuse posture~~ | Per-IP sliding-window limits (publish 10/min, claim 3/hour, rotate 5/hour), hashed IPs, 429+retry-after; reads unlimited |
| 14 | ~~Dev-time path override~~ | Root-only `[patch]` table: graph-wide override, loud notices, lock omission, publish refusal |
| 15 | Stub READMEs | Content + 0.0.2 bumps committed in noeta-reservations; **USER-GATED:** `publish-all.sh` run (crates.io + npm auth) |
| 16 | ~~Alpha quickstart + announcement~~ | docs/Quickstart-Packages.md (live-verified, refreshed for para/cli 0.1.1); announcement draft delivered for review |

## Post-alpha follow-ups (new, from the burn-down)

| Item | Source / trigger |
|---|---|
| `noeta add` warning should state the full import path (`use para.cli.{…}`) and the key-vs-derived-key ergonomics | quickstart friction log #3 |
| `noeta rotate` client verb for the rotation endpoint (promote its wire shape into the canonical fixtures) | registry hardening note |
| Onboarding must mention new claims default to keyless require-provenance (publish from CI or relax policy first) | registry hardening note |
| `noeta init`'s 159 KB SYNTAX.md is a heavy first impression for humans | quickstart friction log #5 |
| Registry `GET /` home caching / pagination once package count grows | browse upgrade |
