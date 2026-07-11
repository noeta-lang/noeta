# Phase 5 — Keyless signing (Sigstore / public trust root)

*Parent: [`README.md`](README.md). Follows Phase 4 follow-up #2 (Ed25519 key-based provenance,
merged). This phase migrates the provenance trust root from "registered key + TOFU pin" to
Sigstore keyless — OIDC identity → short-lived Fulcio certificate → Rekor transparency log —
against the **public sigstore.dev infrastructure**. The attestation payload
(`canonical_bytes`, version → commit binding) is unchanged; only the trust root moves.*

## Decisions (user-confirmed, 2026-07-11)

| # | Decision |
|---|----------|
| **Trust root** | **Public sigstore.dev** (OpenSSF-operated Fulcio CA + Rekor log + Sigstore OAuth). The self-operated "Sigstore-shaped" CA/log variant is **dropped entirely, not built as a stepping stone** — independent operation of the CA/log *is* the security property (a registry-operated log can't detect a registry-operator attack). Precedent: npm provenance, PyPI attestations, conda/prefix.dev all sit on public sigstore.dev. |
| **Formats** | **Sigstore's actual formats verbatim** — DSSE envelope wrapping our unchanged `Attestation::canonical_bytes()` as payload, Fulcio certificate identity extensions, Sigstore **bundle** (v0.3) for storage/verification. No homegrown dialect: the formats are the seam. |
| **Keys stay** | Keyless is a **second trust root, not a replacement**. Per-scope trust is `Unsigned \| Key(pinned pubkey) \| Keyless(identity policy)`. `noeta key new` and the Ed25519 path remain fully supported (publishers without an OIDC identity: laptops, air-gapped CI). |
| **Downgrade protection** | Once a consumer pins a scope as keyless (identity X), a later key-signed or unsigned release of that scope is **rejected** — otherwise keyless adds nothing against a registry serving a key-signed malicious release instead. TOFU moves from key to **identity**. |
| **Publish flow** | **CI-ambient OIDC first** (GitHub Actions `ACTIONS_ID_TOKEN_REQUEST_URL` token — the flow where keyless matters most, and it works against public Fulcio with zero registration; this is how `npm publish --provenance` works). Interactive browser OAuth **deferred** (Sigstore operates its own OAuth endpoint, so it slots in later behind the same seam). |
| **Verification** | **Fully offline from the bundle.** The registry stores the bundle next to the release (index-not-code-store preserved: a bundle is provenance metadata). Consumers verify cert chain → identity policy → SCT → Rekor inclusion proof **+ signed checkpoint** → integrated-time-within-cert-validity, without contacting Rekor. Checkpoint/consistency verification is mandatory, not decorative — it's what makes the log meaningful. |
| **Trust-root distribution** | Embed a **pinned snapshot of Sigstore's `trusted_root.json`** in the CLI. TUF-based rotation is **deferred + surfaced** (v-next). |

## Honest trust statement (goes in docs)

Keyless removes the long-lived secret (nothing to steal after publish) and makes every
release publicly attributable and monitorable: the real maintainer — or anyone — can watch
Rekor for "a release of my package signed by an identity that isn't mine," even for packages
they never installed. Because the CA and log are operated by OpenSSF, not by the Noeta
registry, this detectability holds **even against a compromised registry operator** — the
property a self-operated log cannot provide. Residual trust: the Sigstore root (rotated via
TUF, independently witnessed) and the OIDC issuer (GitHub) for identity binding.

## Wire/schema deltas

- `Release` gains `bundle: Option<String>` (JSON Sigstore bundle) alongside the existing
  `signature: Option<String>` (hex Ed25519). Exactly one may be set. LocalIndex TOML +
  HttpIndex wire (`WireVersion`) + Worker `POST` body extend accordingly (Worker = separate
  repo; contract documented here, conformance is its own change).
- `noeta.lock` `[[scope]]` grows a trust discriminator:
  `name + public_key` (existing, = Key) **or** `name + issuer + identity` (= Keyless).
  Presence of `issuer`/`identity` ⇒ keyless-pinned ⇒ downgrade rejection.
- Identity policy = `{issuer, identity}` with exact match on issuer and
  exact-or-explicit-pattern match on identity (SAN); for GitHub Actions the identity is the
  workflow ref — pin semantics documented (repo-level match, workflow path allowed to move? →
  decided in K3, surfaced to user if narrowing).

## Crate strategy (K0 spike decides, constraints fixed here)

Candidates, in preference order pending build-weight measurement:
1. **prefix-dev `sigstore-rust`** modular workspace (`sigstore-verify`, `sigstore-bundle`,
   `sigstore-merkle`, `sigstore-trust-root` for consume; `sigstore-sign`, `sigstore-fulcio`,
   `sigstore-rekor`, `sigstore-oidc` for publish). Used by the conda ecosystem — a package
   manager in exactly our position. AWS-LC crypto backend (C build dep — the main concern).
2. **`jdx/sigstore-verification`** — verification-only, rustls-capable; would cover consume,
   leaving publish to (1) or hand-rolled Fulcio/Rekor HTTP (both are small JSON APIs).
3. **Official `sigstore` (sigstore-rs)** — requires tokio + aws-lc-rs, attestation
   verification explicitly incomplete. Least preferred.

Constraints regardless of pick: everything behind a new `keyless` cargo feature in `noeta-pm`
(pattern of `provenance`/`registry-http`), workspace-pinned with rationale comments,
`default-features = false` where possible, CLI-only linkage (LSP and interpreter core never
pull an X.509/TLS stack). The chosen crate hides behind our own verify/sign seam so it stays
swappable.

## Slices

- **K0 — spike + scaffolding. ✅ DONE.** Decision: **prefix-dev `sigstore-rust` 0.11**
  (`sigstore-verify`/`-trust-root`/`-types`, `default-features = false`). Measured/verified:
  - Official `sigstore` 0.14 rejected (tokio-bound, attestation verification explicitly
    incomplete); `jdx/sigstore-verification` rejected (verify-only yet 438-dep tree vs 191,
    pulls oauth2 + a second reqwest + ring).
  - Build weight: whole stack ~13s wall / 105s CPU clean debug; **aws-lc-sys builds with `cc`
    alone (no cmake)** — the sole C build dep this adds.
  - API fit: verification is **sync and fully offline**, implements the client-spec steps 0–8
    (chain → SCT → policy → inclusion proof + **signed checkpoint** → integrated-time →
    signature → CVE-2022-36056 consistency). Identity policy built in
    (`require_identity`/`require_issuer`). `SIGSTORE_PRODUCTION_TRUSTED_ROOT` ships embedded
    in `sigstore-trust-root` (no `tuf` feature needed) — K5's snapshot requirement comes free.
  - **DSSE binding is in-toto-only (fails closed on custom payload types)** → K1 wraps
    `canonical_bytes` in an in-toto Statement: subject digest = `sha256(canonical_bytes)`,
    predicate carries `{name, version, sha, url, tag}`, predicateType
    `https://noeta.dev/attestation/publish/v1`. `canonical_bytes` stays the single
    cross-format truth; consumers recompute its digest from registry-served facts.
  - Shipped: `noeta-pm` `keyless` feature + `keyless.rs` seam (`verify_bundle[_with_root]`,
    `IdentityPolicy`, `VerifiedIdentity`); 7 hermetic tests over a vendored **real**
    GitHub-Actions DSSE bundle (verify + identity/issuer mismatch + wrong-artifact +
    tampered-sig + malformed inputs). LSP/core link zero sigstore crates.
- **K1 — DSSE + bundle through the registry. ✅ DONE.** Payload settled by K0's fail-closed
  finding: an **in-toto Statement v1** (`application/vnd.in-toto+json`), subject digest =
  `sha256(canonical_bytes)` (pinned by test against an out-of-band-computed hex), subject name
  `name@version`, predicateType `https://noeta.dev/attestation/publish/v1`, predicate
  `{name, version, sha, url, tag}` (`keyless::publish_statement`/`attested_digest`; the
  `keyless` feature now includes `provenance` — both roots sign the same `Attestation`).
  `Release.bundle: Option<String>` through LocalIndex TOML + HttpIndex wire + publish body;
  at-most-one-of signature/bundle enforced at both publish impls
  (`Release::check_provenance_shape`). Worker contract delta: `WireVersion.bundle` +
  `POST` body `bundle` key (nullable string), stored verbatim, served back verbatim.
- **K2 — offline verification seam. ✅ DONE.** Seam shipped in K0
  (`keyless::verify_bundle[_with_root]`); K2 completed the **adversarial tamper matrix** by
  structural mutation of the real GHA bundle — stronger than a synthetic CA for this purpose,
  since each mutant isolates exactly one verification property against the *production* root:
  tampered certificate / inclusion-proof hash / checkpoint signature / DSSE signature /
  integrated time, missing checkpoint, empty tlogEntries, wrong artifact digest, identity and
  issuer pin mismatch, malformed inputs — every one rejected with a distinct error (asserted).
  *(Re-sequenced, not cut: the in-repo test CA + test log generator lands in K4, where the
  mocked Fulcio/Rekor structurally require it to mint bundles that genuinely verify.)*
- **K3 — trust model in lock + graph. ✅ DONE.** `lock::ScopeTrust` enum
  (`Key(hex) | Keyless{issuer, identity}`, crypto-free by design — the LSP reasons about trust
  *shapes* without linking a verification stack); `[[scope]]` entries discriminate by field
  (`public_key` vs `issuer`+`identity`), lock v1 format backward-compatible. The trust logic is
  a **pure decision function** (`graph::provenance_decision`) with the full matrix unit-tested;
  `check_provenance` is a thin feature-gated crypto wrapper (pins established only *after*
  verification). Rules: keyless pin → bundle required (else **downgrade rejection**) + identity
  must match; key pin → key-change rejection as before, and a bundle = **root-switch rejection**
  (never implicit in either direction — key→keyless would let *any* OIDC identity take over a
  scope); first use pins whichever root the release carries; unsigned stays allowed for
  unpinned/key-pinned scopes (gradual adoption; the strictness asymmetry is deliberate — a
  keyless pin is an explicit strong-trust statement). CLI now builds with `keyless` (consumers
  verify as of this slice). E2E (negative paths, no valid bundle needed): lock-driven downgrade
  rejection, live CLI bundle verification, switch rejection; the Phase-4 key e2e passes
  untouched. Positive keyless resolve e2e = K4 (needs minted bundles).
- **K4 — keyless publish. ✅ DONE.** `noeta publish` prefers the ambient OIDC identity
  (GitHub Actions/GitLab/Buildkite via `ambient-id` through `sigstore-oidc`) → ephemeral P-256
  key → Fulcio cert → DSSE over the K1 statement → Rekor v1 entry → bundle
  (`keyless::publish_bundle[_at]`, sync-wrapped `sigstore-sign`; publisher **verifies its own
  bundle before upload**). Key path unchanged without ambient identity; `--key` forces it; no
  key + no identity = unsigned. Endpoints: production, or `NOETA_FULCIO_URL`+`NOETA_REKOR_URL`;
  trust root override `NOETA_SIGSTORE_TRUST_ROOT` (path) = rotation escape hatch + test seam.
  **Hermetic fixtures** (`keyless-test-fixtures`, `keyless_fixtures.rs`): a real in-process
  test CA + CT log + Rekor log — Fulcio-profile certs w/ **embedded SCTs** (RFC 6962 precert
  signing), size-1 inclusion proofs, **signed checkpoints**, **SETs** (JCS payload; required —
  a v1 bundle's integrated time is only trusted with its SET), stored-entry canonicalization
  matching the CVE-2022-36056 consistency check. Mint mirrors verify's own crates/versions so
  they can't drift. E2E through the real CLI: ambient-token mock → keyless publish → consumer
  resolves, verifies offline under the **default policy**, TOFU-pins identity in `noeta.lock`,
  audit names it, identity change rejected. pm round-trip test additionally proves the
  production root rejects fixture-minted bundles.
- **K5 — audit, docs, staging smoke. ✅ DONE.** Audit trust-root reporting landed in K3
  (key prefix / keyless identity+issuer / none) and is e2e-asserted in K4; the embedded
  `trusted_root.json` snapshot came free with `sigstore-trust-root` (K0) with the
  `NOETA_SIGSTORE_TRUST_ROOT` override (K4). New here: **`docs/Package-Provenance.md`**
  (wiki page, sidebar-linked) — both trust roots, the attestation format, offline
  verification steps, the honest trust statement (what the log detects, residual trust in
  the Sigstore root + OIDC issuer), TOFU/downgrade/switch rules, `noeta update` re-pin,
  env reference; **`scripts/keyless-staging-smoke.sh`** — maintainer-run (real ambient OIDC
  required, CI-dispatched) against `fulcio.sigstage.dev`/`rekor.sigstage.dev`, catching
  client↔service wire drift that hermetic fixtures can't.

Each slice commits green (workspace tests + clippy + fmt).

## Arc status: ✅ K0–K5 COMPLETE (2026-07-11, branch `keyless-signing`)

## Deferred + surfaced (v-next)

- Interactive browser OAuth publish (Sigstore's own OAuth endpoint; same seam).
- TUF-based trust-root rotation (embedded snapshot until then).
- Worker-side bundle storage/validation (separate repo; contract shipped here).
- Log monitoring tooling (`noeta watch-scope`?) — the ecosystem-side detectability story.
- Requiring keyless for a scope registry-side (publish policy, not consumer policy).

## Phase 5 gate

A GitHub-Actions publish produces a Sigstore bundle stored in the index; a consumer verifies
it fully offline (chain, identity, SCT, inclusion + checkpoint) and TOFU-pins the identity in
`noeta.lock`; a later release signed by a different identity — or downgraded to key/unsigned —
is rejected; key-based scopes keep working untouched; `noeta audit` names each scope's trust
root. Full suite green; LSP/core link no new crypto.
