# Golden wire fixtures — the registry protocol's single source of truth

These JSON files pin the **wire schema** shared by two repos:

- the Rust client — `noeta-pm`'s `HttpIndex` (this repo, `crates/noeta-pm/src/registry.rs`), and
- the hosted registry — the `noeta-registry` Cloudflare Worker (separate repo,
  `/…/noeta-registry`, which also carries the prose spec `PROTOCOL.md`).

One fixture per request/response shape, named `<endpoint>-<request|response>[-variant].json`.
Values are **deterministic**, including the Ed25519 signatures: the release signature is signed
with the fixture scope key (seed `0x42 × 32`, public key in `scope-key-response.json`), and the
transparency-log / advisory signatures with the two fixed **test** keys from the Worker's
`vitest.config.ts` — so both suites can verify real crypto against these exact bytes.

Two files break the naming convention because they pin something other than a shape, and belong to
the same set for the same reason — both repos must agree on them, byte for byte:

- `publish-request-description-*.json` pin the **publish limits' boundary**, not a schema. One
  description is exactly `MAX_DESCRIPTION` *code points* of astral-plane characters and must be
  accepted by both sides; one contains U+0085 and must be rejected by both. They exist because the
  two implementations disagreed about what "200 characters" counts (Unicode scalar values vs UTF-16
  code units) and about which characters are control characters (`Cc` vs ASCII-only) — a limit that
  agrees only on ASCII is not a limit that was checked.
- `semver-vectors.json` is **generated** — `cargo run -p noeta-pm --example semver_vectors` — and
  holds every answer `semver::VersionReq::matches` gives over a case list. `noeta audit` calls that
  function; `noeta-registry/src/semver.ts` is a hand port of it. Both suites replay the file, so
  "the port agrees with the crate" is a test rather than a claim in a comment. Never edit it by
  hand; add cases to the example.

## The sync rule (read this before editing)

**This directory is the canonical copy.** The registry repo carries a **verbatim copy** at
`noeta-registry/test/fixtures/wire/`. Two repos cannot share a file, so the copy is pinned twice:

1. `MANIFEST.sha256` hashes every fixture, and each repo's suite recomputes it — that catches a
   local hand-edit.
2. The manifest's own SHA-256 is a **source constant on each side**, outside the copied directory:
   `noeta_pm::registry::WIRE_MANIFEST_SHA256` here, `WIRE_MANIFEST_SHA256` in the registry's
   `src/wire-manifest.ts`. This is the one that catches a *stale copy*. Without it the pin is
   self-referential — the manifest is copied along with the fixtures, so each repo hashes its own
   fixtures against its own manifest and "regenerated here, never copied there" is green on both
   sides while the protocol diverges.

To change the protocol:

1. Edit the fixtures **here**, and update both implementations + `PROTOCOL.md`.
2. Run **`scripts/sync-wire-fixtures.sh`**. It regenerates the manifest, rewrites both stamps, and
   mirrors the directory into the registry checkout (`$NOETA_REGISTRY_DIR`, else the sibling clone).
   That one command replaces the three hand steps this list used to spell out — including the one
   nothing could catch.
3. Run both suites: `cargo test -p noeta-pm --all-features` and (registry repo) `pnpm test`.
4. Commit in **both** repos. The registry's changes do not appear in this repo's `git status`.

`scripts/sync-wire-fixtures.sh --check` is the read-only assertion (CI and `scripts/gate.sh` run
it). `MANIFEST.sha256` is plain `sha256sum` output, so `sha256sum -c MANIFEST.sha256` still works
from a shell.

## Conventions the fixtures pin

- Optional fields are **omitted** when absent — never `null` (both directions).
- `deps` defaults to `[]` on publish and is always present in a versions listing.
- A yanked version is still **served** (`yanked: true`) so an existing pin keeps resolving;
  a resolver must never *newly* select it.
- `published_at` (ISO-8601 UTC, the stored record) and `published_at_unix` (epoch ms, derived)
  describe the same instant; they are dynamic server-side, so the Worker tests compare listings
  modulo these two fields while the client tests assert the fixture values exactly.
- Errors are always `{ "error": "<message>" }` with a matching HTTP status.
