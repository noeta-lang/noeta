# Package provenance

When you resolve a dependency from the registry, what proves that `acme/imgfx 1.2.0` really is
the code Acme released — and not something a compromised registry, or a thief with a stolen
credential, slipped into the index? Noeta's answer is a signed **attestation**: a statement
binding *"this name + version = this git commit"*, verified by the consumer **independently of
trusting the registry**, and pinned in `noeta.lock` so later substitutions are rejected.

Two trust roots exist. Every release carries at most one.

| | **Keyless (Sigstore)** — recommended | **Key (Ed25519)** |
|---|---|---|
| Who signs | Your CI's OIDC identity (e.g. a GitHub Actions workflow) via an **ephemeral** key | A long-lived private key file you guard |
| What can be stolen | Nothing — the key is discarded seconds after signing | The key file (laptop, CI secrets) |
| Compromise visibility | **Public**: every signature lands in a transparency log anyone can monitor | Local: only consumers who already pinned your key notice a change |
| Requires | An ambient CI identity at publish time | `noeta key new` + registering the public key |
| Consumer pins | The signing **identity** (issuer + workflow) | The **public key** |

Unsigned releases remain allowed (they resolve, unverified) so provenance can be adopted
gradually — but see [downgrade protection](#trust-on-first-use-and-downgrade-protection): once a
scope is pinned, weaker releases are rejected.

## The attestation

Both roots sign the same canonical bytes:

```text
noeta-attestation-v1
<company/package>
<version>
<commit sha>
```

Keyless signing wraps them in an [in-toto Statement](https://in-toto.io) — subject digest =
`sha256(canonical bytes)`, predicate type `https://noeta.dev/attestation/publish/v1` carrying
`{name, version, sha, url, tag}` — inside a DSSE envelope. The registry stores the resulting
**Sigstore bundle** next to the release; the raw key path stores a bare Ed25519 signature.

## Keyless signing (Sigstore)

### From CI (ambient identity)

Publishing from CI, there is nothing to configure:

```console
$ noeta publish --git https://github.com/acme/imgfx --tag v1.2.0
published `acme/imgfx` 1.2.0 → …#v1.2.0 (a3f9c2d1…) [keyless: https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main]
```

`noeta publish` detects the ambient OIDC identity (GitHub Actions, GitLab CI, Buildkite),
generates an ephemeral P-256 key, gets a ~10-minute certificate for it from **Fulcio**
(sigstore.dev's CA) binding the key to your CI identity, signs the attestation, records the
signature in **Rekor** (a public, append-only transparency log), assembles the bundle, verifies
its own bundle end-to-end, and publishes. The ephemeral key never touches disk and is gone when
the command exits.

### From a laptop (interactive browser login)

No CI identity? Sign in interactively — Sigstore runs a public OAuth frontend, so there is
still nothing to register:

```console
$ noeta publish --git … --tag … --interactive
Opening browser for authentication...
```

Your browser opens Sigstore's login (GitHub, Google, or Microsoft; PKCE-protected), and the
certified identity is your **email address** — that's what consumers pin. On a headless machine
(SSH, container) add `--oob`: the CLI prints the sign-in URL to open anywhere and prompts for
the verification code the page shows. Same ephemeral key, same transparency log, same
guarantees as the CI flow.

Consumers verify the bundle **fully offline** — no Rekor round-trip:

1. the certificate chains to Fulcio's root (from a trust-root snapshot embedded in the
   toolchain),
2. its Certificate-Transparency SCT checks out,
3. the Rekor **inclusion proof and signed checkpoint** prove the signature is in the log,
4. the log's timestamp falls inside the certificate's short validity window,
5. the DSSE signature covers the attestation, whose digest is recomputed from the
   registry-served facts being trusted, and
6. the certified identity matches the pin (below).

### Why the transparency log matters

The log is what turns a compromise from *silent* to *detectable*. Every keyless signature —
including one made by an attacker who hijacked your CI or stole a publish token — is publicly
recorded with the identity that made it. You (or anyone) can monitor Rekor for *"a release of my
package signed by an identity that isn't mine"*, even for packages nobody has installed yet.
Because Fulcio and Rekor are operated by OpenSSF — not by the Noeta registry — this holds **even
against a compromised registry operator**.

**Residual trust, stated honestly:** you still trust the Sigstore root of trust (independently
operated, TUF-rotated, witnessed) and the OIDC issuer (e.g. GitHub) to authenticate identities.
The toolchain embeds a pinned snapshot of Sigstore's `trusted_root.json`;
`NOETA_SIGSTORE_TRUST_ROOT=<path>` overrides it if a root rotation lands before a toolchain
update ships. Automatic TUF-based root refresh is planned.

## Key-based signing

For publishers without a CI identity (a laptop release, air-gapped infrastructure):

```console
$ noeta key new                  # writes noeta-signing.key, prints the public key
$ noeta publish --git … --tag …  # signs if NOETA_SIGNING_KEY or ./noeta-signing.key exists
```

Register the public key with your registry scope. Consumers verify signatures against it and pin
it on first use. `--key` forces this path even when an ambient CI identity is present.

## Trust-on-first-use and downgrade protection

The first time a consumer resolves a release from a scope, whichever trust root it carries is
**pinned** in `noeta.lock` (commit this file):

```toml
[[scope]]
name = "acme"
issuer = "https://token.actions.githubusercontent.com"
identity = "https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main"
```

From then on, for that scope:

- a **different identity** (or different key) is rejected,
- a scope pinned **keyless** rejects key-signed *and* unsigned releases — the downgrade a
  compromised registry would use to smuggle a forged release past the transparency log,
- a **trust-root switch is never implicit in either direction**: key→keyless would let *anyone*
  with an OIDC identity take over a key-pinned scope, so it also fails closed.

A legitimate migration (the maintainer really did move to keyless, or rotated identity/key) is a
consumer decision: reconcile with the maintainer, then `noeta update` re-resolves and re-pins.

`noeta audit` reports every scope's pinned trust root alongside the tree's native/command trust
footprint — resolution *enforces* verification, so a passing build already means every signed
release verified.

## Environment reference

| Variable | Effect |
|---|---|
| `NOETA_SIGNING_KEY` | Path to the Ed25519 private key (key path) |
| `NOETA_SIGSTORE_TRUST_ROOT` | Path to a `trusted_root.json` overriding the embedded snapshot |
| `NOETA_FULCIO_URL` / `NOETA_REKOR_URL` | Override both signing endpoints together (private Sigstore deployment or staging; default: production sigstore.dev) |
| `NOETA_OIDC_URL` | Override the interactive login's OAuth provider (default: `oauth2.sigstore.dev/auth`) |
