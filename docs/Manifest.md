# The `noeta.toml` manifest

Every Noeta package is described by a `noeta.toml` at its root. It declares the package's identity,
its dependencies, and how it builds. This page is the complete reference for the format — every
table and key the toolchain reads, with the exact rules the parser enforces.

A manifest is TOML. **Unknown keys are ignored** everywhere — the parser only ever looks for the
keys documented here, so a typo'd key name is silently skipped rather than reported. Keep that in
mind when a setting seems to have no effect.

The smallest useful manifest is just an identity:

```toml
[package]
name = "acme/imgfx"
version = "1.2.0"
```

A manifest with no `[package]` at all is a **bare script** — valid to build and run, but it has no
identity, so it cannot be published.

## `[package]` — identity and metadata

| Key | Required | Value | Notes |
|-----|----------|-------|-------|
| `name` | **yes** | `"company/package"` | Two identifiers joined by a single `/`. |
| `version` | **yes** | SemVer string | A concrete version like `"1.2.0"`, not a range. |
| `edition` | no | `"2026"` | The language edition. Defaults to the current edition when omitted. |
| `native` | no | relative directory | Points at this package's native Rust entry crate. See [Native Extensions](Native-Extensions). |
| `license` | no | SPDX expression | Recorded with the release and bound into its transparency-log leaf. |
| `keywords` | no | array of tags | Discovery tags the registry indexes by. |
| `description` | no | one-line string | The blurb package search shows. |

```toml
[package]
name = "acme/imgfx"                          # the global identity the registry indexes
version = "1.2.0"                            # SemVer
edition = "2026"                             # optional — the language edition this package targets
license = "MIT OR Apache-2.0"                # optional — declared SPDX expression
keywords = ["image", "simd"]                 # optional — up to 5 discovery tags
description = "Fast image effects for Noeta" # optional — one-line search blurb
```

**`name`** is a global identity `company/package`. Each half is an identifier — a letter or `_`,
then letters, digits, or `_` (no leading digit, no hyphens). The `package` half is the *import root*
a consumer re-binds the dependency under; see `[dependencies]` below.

**`license`** is checked for SPDX *shape* only — letters, digits, and `` .+()- ``, up to 120
characters — not validated as real SPDX. It is publisher-asserted: the registry never reads your
source, so the claim is yours, and the SHA-pinned tree's `LICENSE` file is the ground truth. It is
part of the immutable release record and bound into the transparency log, so a registry cannot
equivocate about what a release declared.

**`keywords`** are a *set* of up to **5** tags, each 1–20 characters of lowercase `a–z`, `0–9`, and
`-`, starting with a letter or digit. They are stored deduplicated and sorted, so the order you
write them — and any repeats — never matters. The narrow spelling is deliberate: one canonical form
per tag is what lets a registry group everything tagged `aether` into one listing instead of
scattering it across `Aether`, `aether_`, and `AEther`. Unlike `license`, keywords are **not** bound
into the transparency-log leaf — tampering with one only mis-files a package in a listing, it can't
redirect a build.

**`description`** is a single-line blurb of up to 200 characters — no line breaks — shown next to
your package in search results and on its registry page. Like `keywords` it is discovery metadata
(indexed for search, not bound into the transparency log). Leave it off and a package is still
searchable by name and keyword; it just has no one-line summary in the results.

## `[dependencies]` — what the package builds against

Each entry's **key is the import root** — the name you write after `use` — and is decoupled from the
package's real identity. The value names the source. A key may not shadow a built-in root (`std`,
`noeta`, `core`).

```toml
[dependencies]
# A local source tree — no network, no resolver:
util  = { path = "../util" }
# A git dependency pinned to a released tag (the reproducible default):
http  = { git = "https://github.com/acme/http", tag = "v1.2.0" }
# A git dependency tracking a branch's tip (re-resolved by `noeta update`):
gfx   = { git = "https://github.com/acme/gfx", branch = "main" }
# A git dependency tracking the default branch's HEAD — no tag or branch needed:
draft = { git = "https://github.com/acme/draft" }
# A registry dependency. `package` names the real identity when it differs from the key:
codec = { version = "^1.0", package = "acme/imgcodec" }
```

The source forms:

- **`{ path = "…" }`** — a local directory, relative to the manifest. No network, no version
  resolution.
- **`{ git = "…", tag = "…" }`** — a git repository pinned to a released tag. This is the
  reproducible default. A git dependency takes `tag` **or** `branch`, never both.
- **`{ git = "…", branch = "…" }`** — tracks a branch's tip; `noeta update` re-pins it.
- **`{ git = "…" }`** — no `tag` or `branch` tracks the default branch's HEAD, handy for an
  in-development package not yet cut into releases.
- **`{ version = "…" }`** — a registry dependency, where `version` is a SemVer requirement (`"^1.0"`,
  `"~2.3"`). Add `package = "company/package"` when the registry identity differs from the import-root
  key (like Cargo's `foo = { package = "real" }`). A registry dependency needs a resolvable
  `package`; the bare-string shorthand `dep = "^1.0"` leaves it unset and errors at resolve time.

A dependency table must name **exactly one** of `path`, `git`, or `version`.

**Scope dependencies.** An array value binds several packages that share one `company` scope under a
single import root:

```toml
[dependencies]
acme = [
  { version = "^1.0", package = "acme/bytes" },
  { version = "^2.0", package = "acme/codec" },
]
```

## `[trust]` — grants and provenance policy

Dependencies get no elevated capability by default. `[trust]` is where you grant specific ones and
set provenance policy for the whole project. See [Package Provenance](Package-Provenance) for the
trust model.

```toml
[trust]
# Packages allowed to compile and run their native Rust crate (runs cargo/build.rs/proc-macros):
native = ["acme/imgfx"]
# Packages allowed to contribute `noeta <subcommand>` CLI commands:
commands = ["acme/tools"]
# Require signed provenance: true (every scope), false (default), or a list of scopes:
require_provenance = ["acme", "para"]
# Require every registry dependency to appear in the transparency log:
require_transparency = true
# Refuse a release published within this window (defends against a compromised-token rush):
publish_cooldown = "24h"
```

- **`native`** and **`commands`** are arrays of `"company/package"` — an unlisted package's native
  crate is refused and its CLI commands are ignored. A typo'd identity is a hard error.
- **`require_provenance`** is a boolean or an array of **scopes** (the `company` half, not full
  package names): `true` requires provenance from every scope, `false` (the default) from none,
  `["acme"]` only from the named scopes.
- **`require_transparency`** is a boolean (default `false`).
- **`publish_cooldown`** is a duration string — an integer with an optional `s`/`m`/`h`/`d` suffix
  (`"24h"`, `"30m"`, `"7d"`; a bare number is seconds), not a raw number.

## `[targets]` — build recipes

A target is a named build recipe: it maps a *tier* to the package that *provides* it, and can carry
its own dependencies (dev-dependencies) and inherit from another target. A tier's provider must be
the built-in `"std"` or a dependency declared in `[dependencies]` or the target's own
`[targets.<name>.dependencies]`.

```toml
[targets.test]
# dev-only dependencies, overlaid on the globals for this target:
dependencies = { check = { version = "^1.0", package = "acme/check" } }
# tier → provider (a bare string, or a { package = "…" } table with target-level options):
tiers = { test = "check" }

[targets.bench]
extends = "test"                     # inherit test's tiers and dependencies
tiers = { bench = { package = "std", samples = 100 } }
```

- **`extends`** names a base target to inherit tiers and dependencies from; inheritance cycles are an
  error.
- **`dependencies`** follows the same rules as the global `[dependencies]` table.
- **`tiers`** maps a tier name to a provider — either a bare string (`test = "std"`) or a
  `{ package = "…" }` table (extra keys in the table are reserved for future options and ignored).

The target to build is chosen at the command line (`--target`), not in the manifest.

## `[registries]` — routing scopes to registries

Route a scope to the registry its packages resolve from, with an optional `default` for the rest.
Unmapped scopes resolve from the built-in default — the hosted registry at `registry.noeta.dev` —
unless `[registries].default` or the `NOETA_REGISTRY_URL`/`NOETA_REGISTRY_DIR` environment
overrides say otherwise. Fully covered in [Package Registries](Package-Registries).

```toml
[registries]
default = "https://registry.noeta.dev"   # the hosted service for unmapped scopes
acme = "github:acme"                      # acme/* resolve straight from a GitHub org
internal = "git:ssh://git.example.com/pkgs"
```

A registry source is one of: an `http(s)://` URL (a hosted registry service), `github:<owner>`,
`gitlab:<group>`, or `git:<url>` (any git remote — a forge *is* an index). Every key other than
`default` must be a bare scope (`company`) identifier.

## Related tables

Two more tables live in a `noeta.toml` but are read by their own subsystems rather than the package
model:

- **`[native]`** with a `rings` array configures native capability rings — see
  [Native Extensions](Native-Extensions).
- **`[fmt]`** configures the formatter — see the `noeta fmt` documentation.
