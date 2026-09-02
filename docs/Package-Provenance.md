# Package provenance

A release published to a Noeta registry carries a signed **attestation**: a statement binding *"this name + version = this git commit"*. The consumer verifies it without trusting the registry, and pins the signing root in `noeta.lock` so a later substitution is rejected. That is what proves `acme/imgfx 1.2.0` is the code Acme released.

Two trust roots exist. Every release carries at most one.

| | **Keyless (Sigstore)** — recommended | **Key (Ed25519)** |
|---|---|---|
| Who signs | Your CI's OIDC identity (a GitHub Actions workflow, say) through an **ephemeral** key | A long-lived private key file you guard |
| What can be stolen | Nothing; the key is discarded seconds after signing | The key file, from a laptop or from CI secrets |
| Compromise visibility | **Public**: every signature lands in a transparency log anyone can monitor | Local: only consumers who already pinned your key notice a change |
| Requires | An ambient CI identity at publish time | `noeta key new` plus registering the public key |
| Consumer pins | The signing **identity** (issuer + workflow), under the **package** | The **public key**, under the **scope** |

> [!TIP]
> **What to do.** Publish from CI and provenance is automatic, keyless and zero-config. Publishing from a laptop, add `--interactive` for a browser sign-in. Then run `noeta scope require-provenance <scope>` once, so a leaked publish token alone can no longer push a release.

An unsigned release still resolves, unverified, so a scope can adopt provenance gradually. Once a scope or package is pinned, [downgrade protection](#trust-on-first-use-and-downgrade-protection) rejects the weaker forms.

## The attestation

Both roots sign the same canonical bytes:

```text
noeta-attestation-v1
<company/package>
<version>
<commit sha>
```

The name and the commit both come from the publisher, and nothing at publish time opens the tagged tree to confirm it holds the package being released. The consumer closes that gap on resolve: a fetched tree whose `[package] name` or `version` disagrees with the release it was resolved as is refused (see [`package` on a dependency](Manifest#dependencies--what-the-package-builds-against)). A signature proves *who* attested this name-version-commit triple; the resolve proves the commit actually holds it.

Keyless signing wraps those bytes in an [in-toto Statement](https://in-toto.io), with subject digest `sha256(canonical bytes)` and predicate type `https://noeta.dev/attestation/publish/v1` carrying `{name, version, sha, url, tag}`, inside a DSSE envelope. The registry stores the resulting **Sigstore bundle** next to the release. The key path stores a bare Ed25519 signature.

## Keyless signing (Sigstore)

### From CI (ambient identity)

Publishing from CI needs no configuration:

```sh
noeta publish --git https://github.com/acme/imgfx --tag v1.2.0
```

`noeta publish` detects the ambient OIDC identity (GitHub Actions, GitLab CI, Buildkite), generates an ephemeral P-256 key, gets a short-lived certificate for it from **Fulcio** (sigstore.dev's CA) binding the key to your CI identity, signs the attestation, records the signature in **Rekor** (a public, append-only transparency log), assembles the bundle, verifies its own bundle end to end, and publishes. The ephemeral key never touches disk and is gone when the command exits.

The success line names the package, version, git coordinates, commit sha, and the trust root it signed under (`[keyless: <identity>]`, `[signed]` for the key path, or `[UNSIGNED]`). Two links follow for a forge that has web URLs, one to the release tag and one to the commit, and then the docs and README uploads that ride along with the release.

### From a laptop (interactive browser login)

With no CI identity, sign in interactively. Sigstore runs a public OAuth frontend, so there is still nothing to register:

```sh
noeta publish --git … --tag … --interactive
```

Your browser opens Sigstore's login (GitHub, Google, or Microsoft, PKCE-protected), and the certified identity is your **email address**, which is what consumers pin. On a headless machine such as an SSH session or a container, add `--oob`: the CLI prints the sign-in URL to open anywhere and prompts for the verification code the page shows. Same ephemeral key, same transparency log, same guarantees as the CI flow.

### Verifying a release yourself

Resolution verifies every bundle, so a build that succeeds already means every signed release checked out. The consumer runs these checks **fully offline**, with no Rekor round trip:

1. the certificate chains to Fulcio's root, from a trust-root snapshot embedded in the toolchain,
2. its Certificate-Transparency SCT checks out,
3. the Rekor **inclusion proof and signed checkpoint** prove the signature is in the log,
4. the log's timestamp falls inside the certificate's short validity window,
5. the DSSE signature covers the attestation, whose digest is recomputed from the registry-served facts being trusted, and
6. the certified identity matches the pin (below).

### Why the transparency log matters

The log makes a compromise detectable. Every keyless signature is publicly recorded with the identity that made it, including one made by an attacker who hijacked your CI or stole a publish token. You, or anyone, can monitor Rekor for a release of your package signed by an identity that is not yours, even for packages nobody has installed yet. Fulcio and Rekor are operated by OpenSSF rather than by the Noeta registry, so this holds against a compromised registry operator too.

Two things stay trusted: the Sigstore root of trust (independently operated, TUF-rotated, witnessed) and the OIDC issuer, GitHub for instance, to authenticate identities. The toolchain embeds a pinned snapshot of Sigstore's `trusted_root.json`, refreshed with each toolchain release. `NOETA_SIGSTORE_TRUST_ROOT=<path>` points at your own copy when a root rotation lands ahead of one.

## Key-based signing

For publishers without a CI identity, such as a laptop release or air-gapped infrastructure:

```console
$ noeta key new
wrote private signing key to noeta-signing.key (keep it secret)
public key — register this with your registry scope:
  a2d6de57c3ec2f5ad091f1fbbcc00faf57add966d0064e792bf38b3ad6fea1ae
`noeta publish` reads the private key from NOETA_SIGNING_KEY (a path) or `noeta-signing.key`.
```

Registering the public key with your registry scope is a registry-operator step: send the printed key to the operator of the registry you publish to. The self-service [`noeta claim`](The-CLI#noeta-claim) flow binds the scope's *publish token*, not a signing key. Consumers then verify signatures against the registered key and pin it on first use. `--key` forces this path even when an ambient CI identity is present.

## Trust-on-first-use and downgrade protection

The first time a consumer resolves a release, its trust root is **pinned** in `noeta.lock`; commit that file. The lock (format `version = 2`) already pins each package's resolved version, git coordinates (`url`/`tag`/`sha`), content `hash`, and language `edition`. Provenance adds a `[[scope]]` entry per pinned root, plus the registry's transparency-log head (`[log]`) and advisory-feed head (`[advisory]`), all trust-on-first-use.

**What a pin is keyed by follows what the root identifies.**

| Root | Keyed by | Because |
|---|---|---|
| Keyless | The **package identity** (`acme/imgfx`) | The certificate names the CI workflow that published the release, which lives in that package's own repository. Two packages of one scope release from different repos, so no single identity matches both. |
| Key | The **scope** (`acme`) | A registry registers one signing key per scope. |

The two never collide, since a package identity always contains a `/` and a scope never does. Verifying a release looks up the package's own keyless pin first, then falls back to its scope's key pin.

```toml ignore
[[scope]]
name = "acme/imgfx"
issuer = "https://token.actions.githubusercontent.com"
identity = "https://github.com/acme/imgfx/.github/workflows/release.yml@refs/tags/v1.2.0"
```

From then on:

- a **different identity**, or a different key, is rejected;
- a pin on **keyless** rejects key-signed and unsigned releases, which is the downgrade a compromised registry would use to smuggle a forged release past the transparency log;
- a pinned **key** rejects a keyless-signed release, because key to keyless would let anyone with an OIDC identity take over a key-pinned scope.

A legitimate migration, where the maintainer really did move to keyless or rotated identity or key, is a consumer decision: reconcile with the maintainer, then `noeta update` re-resolves and re-pins.

`noeta audit` reports every pinned trust root alongside the tree's native and command trust footprint. Resolution *enforces* verification, so a passing build already means every signed release verified.

## Security advisories and intake tiers

The registry serves a signed, RUSTSEC-style **advisory feed** of known-bad releases. `noeta audit` cross-references every resolved dependency against it and reports any that a live advisory affects. The feed is signed with its own key, pinned trust-on-first-use in `noeta.lock`, and each advisory is bound into the registry's transparency log, so a compromised registry can neither fabricate an advisory nor silently drop one.

Every advisory carries an **intake tier**, recording how it entered the feed. The registry automates *provenance*, never *judgment*:

| Tier | Who issues it | Provenance |
|---|---|---|
| `operator` | The registry operator, curated | The feed signature, the anchor of trust |
| `publisher` | A scope's own owner, for a package in their scope | A **keyless Sigstore bundle** the consumer verifies offline against the pinned identity |
| `imported` | Mirrored from OSV / GHSA / RUSTSEC through an operator-curated name map | The upstream advisory id and link |

The tier is bound into the advisory's signed canonical bytes, so a client can trust which tier it was served. A **public report**, which anyone may file, is unauthenticated intake: never in the feed, and an advisory only once an operator or the scope owner promotes it.

### Severity, text or CVSS

An advisory's severity is one of `low` / `medium` / `high` / `critical`. A text severity is the fallback when no vector is present.

When an **imported** upstream record carries a **CVSS v3.x vector**, in OSV `severity[]` entries of type `CVSS_V3` or GHSA's `cvss.vectorString`, the registry computes the base score from that vector with the published CVSS v3.1 base-metric equations and derives the canonical band from it. The vector stays on the advisory as unsigned, informational metadata; the band is what is signed and what drives policy.

`noeta audit` re-derives the base score client-side from the vector and shows it beside the band, as `high (CVSS 7.8)`, so the number is visible and independently recomputed rather than taken on the registry's word.

### Issuing and reporting from the client

The whole lifecycle runs through `noeta advisory`.

| Verb | Who runs it | What it does |
|---|---|---|
| `noeta advisory publish` | A scope owner | Issues a **publisher**-tier advisory for their own package: keyless-signed with their OIDC identity (ambient CI, or an interactive browser login) and sent authenticated with the scope's publish token. A withdrawal stays in the log rather than being deleted. |
| `noeta advisory report` | Anyone | Files a **public report** against any package. Rate-limited, queued for triage, never published directly. |
| `noeta advisory reports` | An operator or the scope owner | Lists the promotable reports: the operator queue with `NOETA_REGISTRY_ADMIN_TOKEN`, or a scope's own with `--scope` and the scope token. |
| `noeta advisory promote` | An operator or the scope owner | Turns a report into a signed advisory, prefilled from the report. With the admin token it becomes an `operator`-tier advisory; by the package's scope owner it becomes a keyless-signed `publisher` advisory, the same Sigstore bundle a fresh `advisory publish` produces. |

Full flag reference at [The CLI](The-CLI#noeta-advisory).

### Per-tier policy: `[trust.advisories]`

By default every tier **warns**: `noeta audit` prints a matched advisory but does not fail. A project opts a tier up to `fail` for a CI gate, or down to `off`, per tier or all at once, in the manifest's `[trust.advisories]` table. The keys and syntax are on [the Manifest page](Manifest#trustadvisories--per-tier-advisory-policy). A common hardening is `fail` for the curated `operator` and owner-issued `publisher` tiers while leaving the broader `imported` tier at `warn`.

In the audit report a `fail`-level hit is marked `✗` and fails the run; a `warn`-level hit is marked `⚠`. A publisher advisory's line also shows the verified signing identity (`[publisher-verified: …]`).

### `noeta advisory watch` — suppression monitoring

A compromised registry's subtlest attack is to *withhold* an advisory from you specifically. `noeta advisory watch` defends against that over time. It pins the advisory feed head, the transparency-log checkpoint, and the set of advisory ids ever seen for a scope, then on each run verifies that the log is an **append-only extension** of the last checkpoint, that the feed key and log key are unchanged, and that **no previously-seen advisory has disappeared**. A rewrite, key change, feed rollback, or disappearance exits non-zero.

Where `noeta audit` proves the feed verifies *now*, this proves nothing has been rewritten *since*, which is why it is the one verb here that carries state between runs. It keeps one `<scope>.toml` per watched scope under `--state <dir>`, defaulting to `watch/` in the noeta cache. In CI, cache or commit that directory: a baseline that resets every run detects nothing.

```sh
noeta advisory watch                       # every scope `noeta.lock` pins — the CI cron form
noeta advisory watch acme                  # just one scope
noeta advisory watch --state .noeta-watch  # keep the baseline somewhere CI can cache
```

Watching the whole lockfile is the intended default, since the set worth monitoring is the set you depend on. The scope set is not filtered by source: an advisory names a *package*, so one against `acme/http` applies whether you resolved it from the registry or straight from git.

## Environment reference

| Variable | Effect |
|---|---|
| `NOETA_SIGNING_KEY` | Path to the Ed25519 private key (key path) |
| `NOETA_REGISTRY_TOKEN` | A scope's publish token: authenticates publishing, scope-owner triage, and scope-owner `advisory promote` |
| `NOETA_REGISTRY_ADMIN_TOKEN` | The registry admin token: authenticates the operator triage queue and operator `advisory promote` |
| `NOETA_SIGSTORE_TRUST_ROOT` | Path to a `trusted_root.json` overriding the embedded snapshot |
| `NOETA_FULCIO_URL` / `NOETA_REKOR_URL` | Override both signing endpoints together, for a private Sigstore deployment or staging (default: production sigstore.dev) |
| `NOETA_OIDC_URL` | Override the interactive login's OAuth provider (default: `oauth2.sigstore.dev/auth`) |
