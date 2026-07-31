# Package provenance

When you resolve a dependency from the registry, what proves that `acme/imgfx 1.2.0` really is the code Acme released — and not something a compromised registry, or a thief with a stolen credential, slipped into the index? Noeta's answer is a signed **attestation**: a statement binding *"this name + version = this git commit"*, verified by the consumer **independently of trusting the registry**, and pinned in `noeta.lock` so later substitutions are rejected.

Two trust roots exist. Every release carries at most one.

| | **Keyless (Sigstore)** — recommended | **Key (Ed25519)** |
|---|---|---|
| Who signs | Your CI's OIDC identity (e.g. a GitHub Actions workflow) via an **ephemeral** key | A long-lived private key file you guard |
| What can be stolen | Nothing — the key is discarded seconds after signing | The key file (laptop, CI secrets) |
| Compromise visibility | **Public**: every signature lands in a transparency log anyone can monitor | Local: only consumers who already pinned your key notice a change |
| Requires | An ambient CI identity at publish time | `noeta key new` + registering the public key |
| Consumer pins | The signing **identity** (issuer + workflow) | The **public key** |

> [!TIP]
> **What you should do.** Publish from CI and provenance is automatic — keyless, zero-config. Publishing from a laptop, add `--interactive` (a browser sign-in). Then run `noeta scope require-provenance <scope>` once, so a leaked publish token alone can no longer push a release.

Unsigned releases remain allowed (they resolve, unverified) so provenance can be adopted gradually — but see [downgrade protection](#trust-on-first-use-and-downgrade-protection): once a scope is pinned, weaker releases are rejected.

## The attestation

Both roots sign the same canonical bytes:

```text
noeta-attestation-v1
<company/package>
<version>
<commit sha>
```

Keyless signing wraps them in an [in-toto Statement](https://in-toto.io) — subject digest = `sha256(canonical bytes)`, predicate type `https://noeta.dev/attestation/publish/v1` carrying `{name, version, sha, url, tag}` — inside a DSSE envelope. The registry stores the resulting **Sigstore bundle** next to the release; the raw key path stores a bare Ed25519 signature.

## Keyless signing (Sigstore)

### From CI (ambient identity)

Publishing from CI, there is nothing to configure:

```console
$ noeta publish --git https://github.com/acme/imgfx --tag v1.2.0
published `acme/imgfx` 1.2.0 → …#v1.2.0 (a3f9c2d1…) [keyless: https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main]
```

`noeta publish` detects the ambient OIDC identity (GitHub Actions, GitLab CI, Buildkite), generates an ephemeral P-256 key, gets a ~10-minute certificate for it from **Fulcio** (sigstore.dev's CA) binding the key to your CI identity, signs the attestation, records the signature in **Rekor** (a public, append-only transparency log), assembles the bundle, verifies its own bundle end-to-end, and publishes. The ephemeral key never touches disk and is gone when the command exits.

### From a laptop (interactive browser login)

No CI identity? Sign in interactively — Sigstore runs a public OAuth frontend, so there is still nothing to register:

```console
$ noeta publish --git … --tag … --interactive
Opening browser for authentication...
```

Your browser opens Sigstore's login (GitHub, Google, or Microsoft; PKCE-protected), and the certified identity is your **email address** — that's what consumers pin. On a headless machine (SSH, container) add `--oob`: the CLI prints the sign-in URL to open anywhere and prompts for the verification code the page shows. Same ephemeral key, same transparency log, same guarantees as the CI flow.

### Verifying a release yourself

You never have to do any of this — **resolution verifies every bundle automatically**, and a build that succeeds already means every signed release checked out. But the checks are worth knowing, because the consumer runs them **fully offline** — no Rekor round-trip:

1. the certificate chains to Fulcio's root (from a trust-root snapshot embedded in the toolchain),
2. its Certificate-Transparency SCT checks out,
3. the Rekor **inclusion proof and signed checkpoint** prove the signature is in the log,
4. the log's timestamp falls inside the certificate's short validity window,
5. the DSSE signature covers the attestation, whose digest is recomputed from the registry-served facts being trusted, and
6. the certified identity matches the pin (below).

### Why the transparency log matters

The log is what turns a compromise from *silent* to *detectable*. Every keyless signature — including one made by an attacker who hijacked your CI or stole a publish token — is publicly recorded with the identity that made it. You (or anyone) can monitor Rekor for *"a release of my package signed by an identity that isn't mine"*, even for packages nobody has installed yet. Because Fulcio and Rekor are operated by OpenSSF — not by the Noeta registry — this holds **even against a compromised registry operator**.

**Residual trust, stated honestly:** you still trust the Sigstore root of trust (independently operated, TUF-rotated, witnessed) and the OIDC issuer (e.g. GitHub) to authenticate identities. The toolchain embeds a pinned snapshot of Sigstore's `trusted_root.json`; `NOETA_SIGSTORE_TRUST_ROOT=<path>` overrides it if a root rotation lands before a toolchain update ships. Automatic TUF-based root refresh is planned.

## Key-based signing

For publishers without a CI identity (a laptop release, air-gapped infrastructure):

```console
$ noeta key new                  # writes noeta-signing.key, prints the public key
$ noeta publish --git … --tag …  # signs if NOETA_SIGNING_KEY or ./noeta-signing.key exists
```

Register the public key with your registry scope — today a registry-operator step (the self-service [`noeta claim`](The-CLI#noeta-claim) flow binds the scope's *publish token*, not a signing key; send the printed public key to the operator of the registry you publish to). Consumers verify signatures against it and pin it on first use. `--key` forces this path even when an ambient CI identity is present.

## Trust-on-first-use and downgrade protection

The first time a consumer resolves a release from a scope, whichever trust root it carries is **pinned** in `noeta.lock` (commit this file). The lock (format `version = 2`) already pins each package's resolved version, git coordinates (`url`/`tag`/`sha`), content `hash`, and language `edition`; provenance adds a `[[scope]]` entry per scope — the keyless identity, or for the key root a `public_key` — plus the registry's transparency-log head (`[log]`) and advisory-feed head (`[advisory]`), all trust-on-first-use:

```toml
[[scope]]
name = "acme"
issuer = "https://token.actions.githubusercontent.com"
identity = "https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main"
```

From then on, for that scope:

- a **different identity** (or different key) is rejected,
- a scope pinned **keyless** rejects key-signed *and* unsigned releases — the downgrade a compromised registry would use to smuggle a forged release past the transparency log,
- a **trust-root switch is never implicit in either direction**: key→keyless would let *anyone* with an OIDC identity take over a key-pinned scope, so it also fails closed.

A legitimate migration (the maintainer really did move to keyless, or rotated identity/key) is a consumer decision: reconcile with the maintainer, then `noeta update` re-resolves and re-pins.

`noeta audit` reports every scope's pinned trust root alongside the tree's native/command trust footprint — resolution *enforces* verification, so a passing build already means every signed release verified.

## Security advisories and intake tiers

The registry serves a signed, RUSTSEC-style **advisory feed** of known-bad releases. `noeta audit` cross-references every resolved dependency against it and reports any that a live advisory affects. The feed is signed with its own key (pinned trust-on-first-use in `noeta.lock`), and each advisory is bound into the registry's transparency log, so a compromised registry can neither fabricate an advisory nor silently drop one.

Every advisory carries an **intake tier** — how it entered the feed. The registry automates *provenance*, never *judgment*:

| Tier | Who issues it | Provenance |
|---|---|---|
| `operator` | The registry operator (curated) | The feed signature (the anchor of trust) |
| `publisher` | A scope's own owner, for a package in their scope | A **keyless Sigstore bundle** the consumer verifies offline against the scope's pinned identity |
| `imported` | Mirrored from OSV / GHSA / RUSTSEC via an operator-curated name map | The upstream advisory id + link |

The tier is bound into the advisory's signed canonical bytes, so a client can trust which tier it was served. A **public report** (anyone may file one) is *not* an advisory — it is unauthenticated intake, never in the feed, and only becomes an advisory when an operator or the scope owner promotes it.

### Severity — text or CVSS

An advisory's severity is one of `low` / `medium` / `high` / `critical`. When an **imported** upstream record carries a **CVSS v3.x vector** (OSV `severity[]` entries of type `CVSS_V3`, or GHSA's `cvss.vectorString`), the registry computes the base score from that vector honestly — the published CVSS v3.1 base-metric equations — and derives the canonical band from it, keeping the vector on the advisory as unsigned, informational metadata. (The band is what's signed and what drives policy; the vector is display only.) `noeta audit` re-derives the base score client-side from that vector and shows it beside the band — `high (CVSS 7.8)` — so the number behind the band is visible and independently recomputed, never taken on the registry's word. A text severity remains the fallback when no vector is present.

### Issuing and reporting from the client

The whole lifecycle runs through `noeta advisory`. A scope owner **issues** a publisher advisory for their own package with `noeta advisory publish` — keyless-signed with their OIDC identity (ambient CI, or an interactive browser login) and sent authenticated with the scope's publish token; a withdrawal stays in the log, never deleted. Anyone may **file a public report** against any package with `noeta advisory report` — rate-limited, queued for triage, never published directly. From there, an operator or the scope owner **reviews the queue** (`noeta advisory reports`) and **promotes** a report into a signed advisory (`noeta advisory promote`), prefilled from the report: promoted with the admin token it becomes an `operator`-tier advisory; promoted by the package's scope owner it becomes a keyless-signed `publisher` advisory — the *same* Sigstore bundle a fresh `advisory publish` produces, so a consumer verifies it identically. Full flag reference at [The CLI](The-CLI#noeta-advisory).

### Per-tier policy — `[trust.advisories]`

By default every tier **warns**: `noeta audit` prints a matched advisory but does not fail. A project opts a tier up to `fail` (a CI gate) or down to `off`, per tier or all at once, in the manifest's `[trust.advisories]` table — the keys and syntax are on [the Manifest page](Manifest#trustadvisories--per-tier-advisory-policy). A sensible hardening is `fail` for the `operator` and `publisher` tiers (curated or owner-issued — high confidence) while leaving the broader `imported` tier at `warn`.

In the audit report a `fail`-level hit is marked `✗` (and fails the run); a `warn`-level hit is marked `⚠`. A publisher advisory's line also shows the verified signing identity (`[publisher-verified: …]`).

### `noeta watch-scope <scope>` — suppression monitoring

A compromised registry's subtlest attack is to *withhold* an advisory from you specifically. `noeta watch-scope` defends against it over time: it pins the advisory feed head, the transparency-log checkpoint, and the set of advisory ids ever seen for a scope, then on each run verifies the log is an **append-only extension** of the last checkpoint (no history rewrite), that the feed key and log key are unchanged, and that **no previously-seen advisory has disappeared**. A rewrite, key change, feed rollback, or disappearance exits non-zero. State is kept in a small file (`--state <path>`, else under the noeta cache), so it is ideal as a CI cron:

```sh
noeta watch-scope acme            # first run pins the baseline; later runs detect drift
```

## Environment reference

| Variable | Effect |
|---|---|
| `NOETA_SIGNING_KEY` | Path to the Ed25519 private key (key path) |
| `NOETA_REGISTRY_TOKEN` | A scope's publish token — authenticates publishing, scope-owner triage, and scope-owner `advisory promote` |
| `NOETA_REGISTRY_ADMIN_TOKEN` | The registry admin token — authenticates the operator triage queue and operator `advisory promote` |
| `NOETA_SIGSTORE_TRUST_ROOT` | Path to a `trusted_root.json` overriding the embedded snapshot |
| `NOETA_FULCIO_URL` / `NOETA_REKOR_URL` | Override both signing endpoints together (private Sigstore deployment or staging; default: production sigstore.dev) |
| `NOETA_OIDC_URL` | Override the interactive login's OAuth provider (default: `oauth2.sigstore.dev/auth`) |
