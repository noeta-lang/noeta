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

## The sync rule (read this before editing)

**This directory is the canonical copy.** The registry repo carries a **verbatim copy** at
`noeta-registry/test/fixtures/wire/`. Two repos cannot share a file, so the copy is pinned by
`MANIFEST.sha256` (the same manifest lives in both copies) and each repo's test suite recomputes
the hashes — a stale or diverged copy fails that repo's tests.

To change the protocol:

1. Edit the fixtures **here**, and update both implementations + `PROTOCOL.md`.
2. Regenerate the manifest: `cd crates/noeta-pm/test_data/wire && sha256sum *.json > MANIFEST.sha256`.
3. Copy everything verbatim to the registry repo:
   `cp crates/noeta-pm/test_data/wire/* …/noeta-registry/test/fixtures/wire/`.
4. Run both suites: `cargo test -p noeta-pm --all-features` and (registry repo) `pnpm test`.

`MANIFEST.sha256` is `sha256sum` output — `sha256sum -c MANIFEST.sha256` also works from a shell.

## Conventions the fixtures pin

- Optional fields are **omitted** when absent — never `null` (both directions).
- `deps` defaults to `[]` on publish and is always present in a versions listing.
- A yanked version is still **served** (`yanked: true`) so an existing pin keeps resolving;
  a resolver must never *newly* select it.
- `published_at` (ISO-8601 UTC, the stored record) and `published_at_unix` (epoch ms, derived)
  describe the same instant; they are dynamic server-side, so the Worker tests compare listings
  modulo these two fields while the client tests assert the fixture values exactly.
- Errors are always `{ "error": "<message>" }` with a matching HTTP status.
