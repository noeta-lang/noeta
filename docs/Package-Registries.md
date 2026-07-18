# Package registries

A Noeta registry is an **index, not a store**: it maps a package identity + version
(`acme/greet 1.2.0`) to the **git coordinates** (URL + tag + pinned commit) where that release's
source lives, and the toolchain fetches the source over git. The registry never hosts code.

That design has a useful consequence: **a git host is already a registry.** A GitHub org, a GitLab
group, or a self-hosted Gitea/Forgejo server is a collection of repos with tags — which is exactly
what an index needs. So you can point a scope at a git forge and resolve packages straight from it,
public or private, with no separate registry service to run.

By default every dependency resolves from one registry (the hosted service at `NOETA_REGISTRY_URL`,
or the local file index offline). The `[registries]` table lets you route **per scope** instead.

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
  that exact commit — a moved tag is caught.

### Publishing

There is no publish command and no upload: **a release is a pushed tag.**

```sh
git tag v1.2.0
git push --tags
```

That's the whole publish flow for a git-forge registry. (`noeta publish` targets the hosted index and
is refused for a git forge.)

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
| `NOETA_REGISTRY_URL` | The default hosted registry for unmapped scopes (when no `[registries].default`) |
| `NOETA_GIT_FORGE_CACHE` | Where git-forge bare clones are cached (default: the toolchain cache dir) |
| `NOETA_GITHUB_TOKEN` | A token to authenticate `github.com` git access (CI) |
| `NOETA_GITHUB_AUTH_HOST` | Scope the token's auth header at a different host (self-hosted GitHub-compatible forge) |

## See also

- [Package Provenance](Package-Provenance) — signed attestations, transparency log, advisories
- [Modules & Visibility](Modules) — how `use` resolves the packages you depend on
- [The `noeta` CLI](The-CLI) — `noeta add`, `noeta publish`, and resolution commands
