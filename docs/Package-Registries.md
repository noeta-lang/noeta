# Package registries

A Noeta registry is an **index, not a store**: it maps a package identity + version
(`acme/greet 1.2.0`) to the **git coordinates** (URL + tag + pinned commit) where that release's
source lives, and the toolchain fetches the source over git. The registry never hosts code.

That design has a useful consequence: **a git host is already a registry.** A GitHub org, a GitLab
group, or a self-hosted Gitea/Forgejo server is a collection of repos with tags — which is exactly
what an index needs. So you can point a scope at a git forge and resolve packages straight from it,
public or private, with no separate registry service to run.

By default every dependency resolves from one registry — the built-in hosted service at
`registry.noeta.dev`, which is live and already serves the first-party `para/*` packages
(see [Using Packages](Using-Packages)). Point the default elsewhere with `NOETA_REGISTRY_URL`
(another hosted registry) or `NOETA_REGISTRY_DIR` (a local file index, used offline and in
tests) — precedence when set together is `NOETA_REGISTRY_URL`, then `NOETA_REGISTRY_DIR`,
then the hosted default. The `[registries]` table lets you route **per scope** instead, and a
scope it maps never falls through to the environment default.

## The `[registries]` table

Map a scope — the `company` half of a `company/package` identity — to the registry its packages come
from. An optional `default` covers every other scope.

```toml
[package]
name = "me/app"
version = "0.1.0"

[registries]
# acme/* resolve from the acme GitHub org; everything else stays on the default registry.
acme = "github:acme"

[dependencies]
greet = { version = "^1.0", package = "acme/greet" }
```

You can mix public and private freely:

```toml
[registries]
default   = "https://registry.noeta.dev"   # override the fallback for unmapped scopes
acme      = "github:acme"                   # a GitHub org
widgets   = "gitlab:widgets-inc/oss"        # a GitLab group (nesting allowed)
internal  = "git:https://git.corp.example/tools"  # a self-hosted forge
```

The scope key is only a **routing alias** — it need not equal the forge owner. Mapping
`internal = "git:https://git.corp.example/tools"` and depending on `internal/logger` resolves
`https://git.corp.example/tools/logger`. Reserved namespaces (`std`, `noeta`, `core`) are never
resolved from any registry, so a `[registries]` entry can't shadow the toolchain.

### Source syntax

| Value | Meaning |
|---|---|
| `github:<owner>` | The git forge `https://github.com/<owner>` |
| `gitlab:<group>` | The git forge `https://gitlab.com/<group>` (nested groups allowed) |
| `git:<url>` | A git forge at `<url>` verbatim — self-hosted HTTPS, `ssh://…`, or `file://…` |
| `https://…` (bare) | A hosted **noeta-registry service** (the index API), *not* a git forge |

The three forge shorthands all normalize to one base URL; `github:`/`gitlab:` are just convenience
over `git:`.

## Publishing to the hosted registry

The hosted registry (`registry.noeta.dev`, or any deployment of the registry service) is still an
index — publishing uploads no code. `noeta publish` records *identity + version → git coordinates*
plus a signed attestation, and consumers keep fetching your source from your repo. Submitting a
package is three steps; the first happens once per scope.

### 1 · Claim your scope

A scope — the `company` half of `company/package` — is claimed **self-service and squat-proof**: you
can only claim the scope whose name matches an identity you prove.

```sh
NOETA_REGISTRY_URL=https://registry.noeta.dev noeta claim acme   # prove you are the GitHub org/user `acme`
```

`noeta claim` targets the hosted registry the scope routes to — a `[registries]` mapping, else
`NOETA_REGISTRY_URL` (there is no implicit default here: claiming binds a credential, so you name
the registry explicitly). In GitHub Actions (with `id-token: write` granted) the ambient OIDC token is the proof — zero-config.
On a laptop the command falls back to the GitHub **device flow**: it prints a URL and a code, and you
authorize in a browser. Alternatively, `noeta claim acme --domain acme.dev` proves control of a
domain whose first label is the scope, by serving `https://acme.dev/.well-known/noeta-registry.txt`
containing `noeta-scope=acme`.

On success a **publish token** is bound to the scope and printed **once** — save it:

```sh
export NOETA_REGISTRY_TOKEN=…       # what `noeta publish` authenticates with
```

Re-running `noeta claim` as the same proven identity rotates the token; any other identity is
refused — ownership never transfers implicitly. Full flag reference at
[The CLI](The-CLI#noeta-claim).

### 2 · Tag the release

A release is a `v<semver>` git tag on a repo consumers can reach; the `noeta.toml` at that tag is
the release's manifest (a published package may depend only via the registry — `path`/`git`
dependencies are rejected at publish).

```sh
git tag v1.2.0
git push --tags
```

### 3 · Publish

```sh
noeta publish --git https://github.com/acme/greet    # tag defaults to v<version>
```

This resolves the tag to its commit SHA, pins *`acme/greet` 1.2.0 → URL + tag + commit* into the
index, and **signs an attestation** binding name + version to that commit — keyless and zero-config
in CI, via `--interactive` browser sign-in or a [`noeta key new`](The-CLI#noeta-key) key file on a
laptop (see [Package Provenance](Package-Provenance) for the trade-offs). A published version is
**immutable**. Your package then appears at `https://registry.noeta.dev/acme/greet` — readme,
versions, and API docs.

Once the scope's releases are signed, harden it so a leaked token alone can't publish:

```sh
noeta scope require-provenance acme
```

## Git-forge registries

The convention is Go-module-like:

- **A package is a repo.** Package `acme/greet` routed to base `https://github.com/acme` lives at
  `https://github.com/acme/greet`.
- **A version is a tag.** The published versions are the repo's `v<semver>` tags — `v1.2.0`,
  `v2.0.0-rc.1`. Tags that aren't `v<semver>` (a `nightly` or `latest`) are ignored.
- **Dependencies come from the tag.** Each version's `[dependencies]` are read from the `noeta.toml`
  at that tag, so the resolver backtracks over ranges exactly as it does with the hosted index. A tag
  with no manifest isn't a package release and is skipped.
- **A broken manifest is never silent.** If a tag's `noeta.toml` fails to parse — a malformed manifest,
  or a **future edition** this toolchain can't read — that version is skipped with a warning naming the
  tag and the parse error, so an older toolchain isn't stranded when *other* versions still satisfy the
  requirement. But if *every* candidate version's manifest is unparseable, resolution fails with an
  error listing each offending tag and its cause, rather than reporting a misleading "no versions found".
- **The commit is pinned.** The tag's commit SHA is recorded in `noeta.lock`, so a later build fetches
  that exact commit — a moved tag is caught. `noeta update` is the deliberate re-pin: it discards the
  lock and re-resolves every ref to its current commit.

### Publishing to a git forge

There is no publish command, no upload, and no scope to claim: **a release is a pushed tag.**

```sh
git tag v1.2.0
git push --tags
```

That's the whole publish flow for a git-forge registry. (`noeta publish` and `noeta claim` target the
hosted index and are refused for a git forge — see
[publishing to the hosted registry](#publishing-to-the-hosted-registry) above.)

## Authenticating private repos

Both halves that touch the network — discovering versions and fetching source — shell out to the
system `git`, so **git's own authentication is Noeta's authentication.** Two ways, in order:

1. **Ambient git credentials (the default, nothing to configure).** If you can already
   `git clone` the private repo — via a credential helper, `gh auth login`, `~/.git-credentials`, or
   an SSH key — then Noeta can resolve it. This is the normal laptop path.
2. **`NOETA_GITHUB_TOKEN` (a CI override).** When set, every git command Noeta runs is given a scoped
   `Authorization: Basic` header for `github.com`, so a CI job that has only a token (no credential
   helper) resolves private GitHub repos too. The token is passed per-invocation and is **never
   written to a repo config or to `noeta.lock`.** For a non-GitHub forge, configure git credentials
   (helper/SSH) as usual, or point the header at your host with `NOETA_GITHUB_AUTH_HOST`.

A private repo you can't access returns the git host's own "not found" — surfaced as a clear resolve
error, not a leak of whether the repo exists.

> The same token/credential mechanism also authenticates a plain private
> `dep = { git = "https://github.com/acme/util", tag = "v1.0.0" }` dependency.

## Trust: what you trade

A git-forge registry deliberately has **none** of the hosted registry's supply-chain machinery —
[signed provenance](Package-Provenance), the transparency log, the advisory feed, require-provenance,
or the publish cooldown. Those are server-side features of the hosted index.

For a private or internal registry that is the right trade: the trust model becomes your **git host's
access control** plus **git-native provenance** — signed commits and signed tags, which you can
require through your host's branch protections. Noeta's committer signal (which flags a new committer
appearing in a release's history) still applies, because it works over git directly. The
reserved-namespace guard also still applies everywhere.

If you need cryptographic, registry-independent provenance for public packages, publish them to the
hosted registry (or a mirror of it) rather than a bare git forge — see
[Package Provenance](Package-Provenance).

## Environment reference

| Variable | Effect |
|---|---|
| `NOETA_REGISTRY_URL` | Override the built-in default registry (`registry.noeta.dev`) for unmapped scopes (when no `[registries].default`) |
| `NOETA_REGISTRY_DIR` | Resolve unmapped scopes from a local file-backed index at this directory instead of a hosted registry (offline / tests) |
| `NOETA_GIT_FORGE_CACHE` | Where git-forge bare clones are cached (default: the toolchain cache dir) |
| `NOETA_GITHUB_TOKEN` | A token to authenticate `github.com` git access (CI) |
| `NOETA_GITHUB_AUTH_HOST` | Scope the token's auth header at a different host (self-hosted GitHub-compatible forge) |
| `NOETA_REGISTRY_TOKEN` | The scope's publish token (bound by `noeta claim`) — authenticates `noeta publish`, `noeta scope`, and publisher advisories |
| `NOETA_GITHUB_CLIENT_ID` | The registry's public GitHub OAuth client id — enables the laptop device flow for `noeta claim` |
| `NOETA_REGISTRY_AUDIENCE` | The OIDC audience the registry expects for claims (default `noeta-registry`) |

## See also

- [Package Provenance](Package-Provenance) — signed attestations, transparency log, advisories
- [Modules & Visibility](Modules) — how `use` resolves the packages you depend on
- [The `noeta` CLI](The-CLI) — `noeta add`, `noeta claim`, `noeta publish`, and resolution commands
