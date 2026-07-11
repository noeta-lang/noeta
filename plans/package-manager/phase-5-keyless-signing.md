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

- **K0 — spike + scaffolding.** Add `keyless` feature; evaluate candidates on (a) offline
  bundle verification incl. inclusion proof + checkpoint, (b) build weight (clean-build delta,
  C deps), (c) API fit for a synchronous CLI. Record the decision + numbers in this doc.
  *Exit: a fixture Sigstore bundle verifies in a hermetic unit test against a test trust root.*
- **K1 — DSSE + bundle through the registry.** `canonical_bytes` as DSSE payload
  (payloadType `application/vnd.noeta.attestation.v1`); `Release.bundle` through LocalIndex,
  HttpIndex, mock-server tests; exactly-one-of-signature/bundle enforced; Worker contract delta
  documented. *Exit: a bundle round-trips publish → index → releases().*
- **K2 — offline verification seam.** `provenance::verify_keyless(att, bundle, trust_root,
  policy) -> VerifiedIdentity` — DSSE sig over canonical bytes, chain to Fulcio root, identity
  extension extraction, SCT, Rekor inclusion + checkpoint signature, time-in-validity. Hermetic
  fixtures: test trust root + test CA + test log generated in-repo (deterministic).
  *Exit: good bundle verifies; each single-property tamper (payload, cert, proof, checkpoint,
  identity, expired-time) fails with a distinct error.*
- **K3 — trust model in lock + graph.** Three-way `check_provenance`; lockfile schema for
  keyless pins; TOFU-on-identity; **downgrade rejection**; identity-changed error with
  re-pin guidance (`noeta update` parity with the key path). *Exit: graph tests cover
  first-pin, match, mismatch, downgrade-attack, unsigned-scope-unchanged.*
- **K4 — keyless publish.** `noeta publish` detects ambient OIDC (GitHub Actions env) →
  Fulcio cert for ephemeral key → DSSE sign → Rekor entry → assemble bundle → publish.
  Key-file path unchanged when no OIDC ambient; both present → keyless wins, `--key` forces.
  Fulcio/Rekor mocked via the existing in-process `mock_server` harness.
  *Exit: end-to-end publish-then-resolve over LocalIndex + mocked services, keyless-pinned lock.*
- **K5 — audit, docs, staging smoke.** `noeta audit` reports per-scope trust root
  (key / keyless identity / unsigned); embedded `trusted_root.json` snapshot + provenance docs
  incl. the honest trust statement; a manual (not CI) smoke script against Sigstore **staging**.
  *Exit: audit output + docs; smoke script exists and is documented as maintainer-run.*

Each slice commits green (workspace tests + clippy + fmt).

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
