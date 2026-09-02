# Package registries

A Noeta registry is an **index**. It maps a package identity plus version (`acme/greet 1.2.0`) to the **git coordinates** where that release's source lives: URL, tag, and pinned commit. The toolchain then fetches the source over git. The registry hosts no code.

One consequence is that **a git host is already a registry.** A GitHub org, a GitLab group, or a self-hosted Gitea/Forgejo server is a collection of repos with tags, which is what an index needs. Point a scope at a git forge and packages resolve straight from it, public or private, with no separate registry service to run.

Every dependency resolves from the hosted service at `registry.noeta.dev` unless something says otherwise. The `[registries]` table routes **per scope**, and a scope it maps never falls through to the environment default; the [environment reference](#environment-reference) covers repointing the default itself.

## The `[registries]` table

Map a scope, the `company` half of a `company/package` identity, to the registry its packages come from. An optional `default` covers every other scope.

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

Public and private mix freely:

```toml
[registries]
default    = "https://registry.noeta.dev"  # override the fallback for unmapped scopes
acme       = "github:acme"                 # a GitHub org
my-company = "github:my-company"           # an org whose name has a hyphen
widgets    = "gitlab:widgets-inc/oss"      # a GitLab group (nesting allowed)
internal   = "git:https://git.corp.example/tools"  # a self-hosted forge
```

A scope takes what the `company` half of an identity takes: letters, digits, `_`, and `-` between them. Any org a forge accepts can therefore be mapped and depended on, as `my-company/logger` is.

The scope key is a **routing alias**, and need not equal the forge owner. Mapping `internal = "git:https://git.corp.example/tools"` and depending on `internal/logger` resolves `https://git.corp.example/tools/logger`. Reserved namespaces (`std`, `noeta`, `core`) resolve from no registry at all, so a `[registries]` entry cannot shadow the toolchain.

### Source syntax

| Value | Meaning |
|---|---|
| `github:<owner>` | The git forge `https://github.com/<owner>` |
| `gitlab:<group>` | The git forge `https://gitlab.com/<group>` (nested groups allowed) |
| `git:<url>` | A git forge at `<url>` verbatim: self-hosted HTTPS, `ssh://…`, or `file://…` |
| `https://…` (bare) | A hosted **noeta-registry service**, meaning the index API rather than a git forge |

The three forge shorthands all normalize to one base URL; `github:` and `gitlab:` are convenience over `git:`.

## Publishing to the hosted registry

The hosted registry at `registry.noeta.dev`, and any other deployment of the registry service, is an index: publishing uploads no code. `noeta publish` records *identity + version → git coordinates* plus a signed attestation, and consumers keep fetching your source from your repo. Submitting a package is three steps, the first of which happens once per scope.

### 1 · Claim your scope

A scope is claimed **self-service and squat-proof**: the scope you can claim is the one whose name matches an identity you prove.

```sh
noeta claim acme   # prove you are the GitHub org/user `acme`
```

Three proofs are accepted. In GitHub Actions with `id-token: write` granted, the ambient OIDC token proves it with no configuration. On a laptop the command falls back to the GitHub **device flow**, printing a URL and a code you authorize in a browser. `noeta claim acme --domain acme.dev` proves control of a domain whose first label is the scope, by serving `https://acme.dev/.well-known/noeta-registry.txt` containing `noeta-scope=acme`.

`noeta claim` targets the hosted registry the scope routes to: a `[registries]` mapping first, then `NOETA_REGISTRY_URL`, then the built-in default at `registry.noeta.dev`. Publishing follows the same resolution. A scope mapped to a git forge is refused, since a forge has no claim endpoint, and so is `NOETA_REGISTRY_DIR`'s file-backed local index.

On success a **publish token** is bound to the scope and printed **once**, so save it:

```sh
export NOETA_REGISTRY_TOKEN=…       # what `noeta publish` authenticates with
```

Re-running `noeta claim` as the same proven identity rotates the token; any other identity is refused, so ownership never transfers implicitly. Full flag reference at [The CLI](The-CLI#noeta-claim).

### 2 · Tag the release

A release is a `v<semver>` git tag on a repo consumers can reach. The `noeta.toml` at that tag is the release's manifest, and a published package may depend only via the registry: `path` and `git` dependencies are rejected at publish.

```sh
git tag v1.2.0
git push --tags
```

### 3 · Publish

```sh
noeta publish --git https://github.com/acme/greet    # tag defaults to v<version>
```

This resolves the tag to its commit SHA, pins *`acme/greet` 1.2.0 → URL + tag + commit* into the index, and **signs an attestation** binding name and version to that commit. Signing is keyless and zero-config in CI, and on a laptop runs through `--interactive` browser sign-in or a [`noeta key new`](The-CLI#noeta-key) key file; [Package Provenance](Package-Provenance) covers the trade-offs. A published version is **immutable**. Your package then appears at `https://registry.noeta.dev/acme/greet`, with its readme, versions, and API docs.

Once the scope's releases are signed, harden it so a leaked token alone cannot publish:

```sh
noeta scope require-provenance acme
```

## Git-forge registries

The convention is Go-module-like.

| Rule | What it means |
|---|---|
| **A package is a repo.** | Package `acme/greet` routed to base `https://github.com/acme` lives at `https://github.com/acme/greet`. |
| **A version is a tag.** | The published versions are the repo's `v<semver>` tags: `v1.2.0`, `v2.0.0-rc.1`. A tag that is not `v<semver>`, such as `nightly` or `latest`, is ignored. |
| **Dependencies come from the tag.** | Each version's `[dependencies]` are read from the `noeta.toml` at that tag, so the resolver backtracks over ranges exactly as it does with the hosted index. A tag with no manifest is not a package release and is skipped. |
| **The commit is pinned.** | The tag's commit SHA is recorded in `noeta.lock`, so a later build fetches that exact commit and a moved tag is caught. `noeta update` is the deliberate re-pin: it discards the lock and re-resolves every ref to its current commit. |

A broken manifest is reported rather than silent. When a tag's `noeta.toml` fails to parse, whether malformed or written for a **future edition** this toolchain cannot read, that version is skipped with a warning naming the tag and the parse error, which keeps an older toolchain working when other versions still satisfy the requirement. When *every* candidate version's manifest is unparseable, resolution fails with an error listing each offending tag and its cause, rather than reporting a misleading "no versions found".

### Publishing to a git forge

**A release is a pushed tag.** There is no publish command, no upload, and no scope to claim.

```sh
git tag v1.2.0
git push --tags
```

That is the whole publish flow for a git-forge registry. `noeta publish` and `noeta claim` target the hosted index and are refused for a git forge; see [publishing to the hosted registry](#publishing-to-the-hosted-registry) above.

## Authenticating private repos

Both halves that touch the network, discovering versions and fetching source, shell out to the system `git`, so **git's own authentication is Noeta's authentication.** Two ways, in order:

1. **Ambient git credentials**, the default, with nothing to configure. If you can already `git clone` the private repo, through a credential helper, `gh auth login`, `~/.git-credentials`, or an SSH key, then Noeta can resolve it. This is the normal laptop path.
2. **`NOETA_GITHUB_TOKEN`**, a CI override. When it is set, every git command Noeta runs is given a scoped `Authorization: Basic` header for `github.com`, so a CI job holding only a token resolves private GitHub repos too. The token is passed per invocation and is **never written to a repo config or to `noeta.lock`.** For a non-GitHub forge, configure git credentials as usual, or point the header at your host with `NOETA_GITHUB_AUTH_HOST`.

A private repo you cannot access returns the git host's own "not found", surfaced as a resolve error that says nothing about whether the repo exists.

The same token and credential mechanism authenticates a plain private `dep = { git = "https://github.com/acme/util", tag = "v1.0.0" }` dependency.

## Trust: what you trade

The hosted registry's supply-chain machinery is server-side: [signed provenance](Package-Provenance), the transparency log, the advisory feed, require-provenance, and the publish cooldown. A git-forge registry has none of it.

For a private or internal registry that is the right trade. The trust model becomes your **git host's access control** plus **git-native provenance**, meaning signed commits and signed tags, which your host's branch protections can require. Noeta's committer signal, which flags a new committer appearing in a release's history, works over git directly and still applies, as does the reserved-namespace guard.

Public packages that need cryptographic, registry-independent provenance belong on the hosted registry, or a mirror of it, rather than a bare git forge. See [Package Provenance](Package-Provenance).

## Environment reference

An unmapped scope resolves through `NOETA_REGISTRY_URL` first, then `NOETA_REGISTRY_DIR`, then the built-in hosted default.

| Variable | Effect |
|---|---|
| `NOETA_REGISTRY_URL` | Override the built-in default registry (`registry.noeta.dev`) for unmapped scopes, when there is no `[registries].default` |
| `NOETA_REGISTRY_DIR` | Resolve unmapped scopes from a local file-backed index at this directory instead of a hosted registry (offline, tests) |
| `NOETA_GIT_FORGE_CACHE` | Where git-forge bare clones are cached (default: the toolchain cache dir) |
| `NOETA_GITHUB_TOKEN` | A token to authenticate `github.com` git access (CI) |
| `NOETA_GITHUB_AUTH_HOST` | Scope the token's auth header at a different host, for a self-hosted GitHub-compatible forge |
| `NOETA_REGISTRY_TOKEN` | The scope's publish token, bound by `noeta claim`: authenticates `noeta publish`, `noeta scope`, and publisher advisories |
| `NOETA_GITHUB_CLIENT_ID` | Override the GitHub OAuth client id for `noeta claim`'s laptop device flow (the hosted registry's app is built in) |
| `NOETA_REGISTRY_AUDIENCE` | The OIDC audience the registry expects for claims (default: the host of the registry being claimed on, `registry.noeta.dev` for instance) |

## See also

- [Package Provenance](Package-Provenance) — signed attestations, transparency log, advisories
- [Modules & Visibility](Modules) — how `use` resolves the packages you depend on
- [The `noeta` CLI](The-CLI) — `noeta add`, `noeta claim`, `noeta publish`, and resolution commands
