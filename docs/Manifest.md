# The `noeta.toml` manifest

Every Noeta package is described by a `noeta.toml` at its root. It declares the package's identity, its dependencies, and how it builds. This page is the complete reference for the format — every table and key the toolchain reads, with the exact rules the parser enforces.

A manifest is TOML. **Unknown keys are ignored** everywhere — the parser only ever looks for the keys documented here, so a typo'd key name is silently skipped rather than reported. Keep that in mind when a setting seems to have no effect.

The smallest useful manifest is just an identity:

```toml
[package]
name = "acme/imgfx"
version = "1.2.0"
```

A manifest with no `[package]` at all is a **bare script** — valid to build and run, but it has no identity, so it cannot be published.

## `[package]` — identity and metadata

| Key | Required | Value | Notes |
|-----|----------|-------|-------|
| `name` | **yes** | `"company/package"` | Two identifiers joined by a single `/`. |
| `version` | **yes** | SemVer string | A concrete version like `"1.2.0"`, not a range. |
| `edition` | no | `"2026"` | The language edition. Defaults to the current edition when omitted. |
| `toolchain` | no | SemVer requirement | The minimum `noeta` this package works with, e.g. `">=0.2"`. |
| `native` | no | relative directory | Points at this package's native Rust entry crate. See [Native Extensions](Native-Extensions). |
| `license` | no | SPDX expression | Recorded with the release and bound into its [transparency-log](Package-Provenance) leaf. |
| `keywords` | no | array of tags | Discovery tags the registry indexes by. |
| `description` | no | one-line string | The blurb package search shows. |

```toml
[package]
name = "acme/imgfx"                          # the global identity the registry indexes
version = "1.2.0"                            # SemVer
edition = "2026"                             # optional — the language edition this package targets
toolchain = ">=0.2"                          # optional — the minimum noeta this package works with
license = "MIT OR Apache-2.0"                # optional — declared SPDX expression
keywords = ["image", "simd"]                 # optional — up to 5 discovery tags
description = "Fast image effects for Noeta" # optional — one-line search blurb
```

**`name`** is a global identity `company/package`. Each half is an identifier — a letter or `_`, then letters, digits, or `_` (no leading digit, no hyphens). The `package` half is the *import root* a consumer re-binds the dependency under; see `[dependencies]` below.

**`toolchain`** declares the minimum `noeta` version the package works with, as a SemVer requirement the *running binary's* version must satisfy (`toolchain = ">=0.2"`). It is enforced at resolve time — for your own package and for every dependency — so a consumer on an older toolchain gets "requires noeta >=0.2 … run `noeta upgrade`" instead of a compile error deep inside a native build. Omit it and the package makes no claim. It is a courtesy floor, not the compatibility contract itself (that is the extension ABI): declare the oldest toolchain you actually test against, typically the release current when you publish.

The value is a full SemVer requirement — ranges (`">=0.2, <0.4"`), tilde, exact, and wildcard forms all work. Prefer `>=`: a bare `"0.2"` means caret (`>=0.2.0, <0.3.0`), which *also imposes an upper bound* and would refuse noeta 0.3 — rarely what a compatibility floor intends. A pre-release binary (say `0.3.0-rc.1`) matches as its release triple `0.3.0`, so release candidates are not spuriously refused.

**`license`** is checked for SPDX *shape* only — letters, digits, and `` .+()- ``, up to 120 characters — not validated as real SPDX. It is publisher-asserted: the registry never reads your source, so the claim is yours, and the SHA-pinned tree's `LICENSE` file is the ground truth. It is part of the immutable release record and bound into the transparency log, so a registry cannot equivocate about what a release declared.

**`keywords`** are a *set* of up to **5** tags, each 1–20 characters of lowercase `a–z`, `0–9`, and `-`, starting with a letter or digit. They are stored deduplicated and sorted, so the order you write them — and any repeats — never matters. The narrow spelling is deliberate: one canonical form per tag is what lets a registry group everything tagged `aether` into one listing instead of scattering it across `Aether`, `aether_`, and `AEther`. Unlike `license`, keywords are **not** bound into the transparency-log leaf — tampering with one only mis-files a package in a listing, it can't redirect a build.

**`description`** is a single-line blurb of up to 200 characters — no line breaks — shown next to your package in search results and on its registry page. Like `keywords` it is discovery metadata (indexed for search, not bound into the transparency log). Leave it off and a package is still searchable by name and keyword; it just has no one-line summary in the results.

## `[dependencies]` — what the package builds against

Each entry's **key is the import root** — the name you write after `use` — and is decoupled from the package's real identity. The value names the source. A key may not shadow a built-in root (`std`, `noeta`, `core`).

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
# A path dependency that also records which package that directory holds:
ai    = { path = "../para-ai", package = "para/ai" }
```

The source forms:

- **`{ path = "…" }`** — a local directory, relative to the manifest. No network, no version resolution.
- **`{ git = "…", tag = "…" }`** — a git repository pinned to a released tag. This is the reproducible default. A git dependency takes `tag` **or** `branch`, never both.
- **`{ git = "…", branch = "…" }`** — tracks a branch's tip; `noeta update` re-pins it.
- **`{ git = "…" }`** — no `tag` or `branch` tracks the default branch's HEAD, handy for an in-development package not yet cut into releases.
- **`{ version = "…" }`** — a registry dependency, where `version` is a SemVer requirement (`"^1.0"`, `"~2.3"`). Add `package = "company/package"` when the registry identity differs from the import-root key (like Cargo's `foo = { package = "real" }`). A registry dependency needs a resolvable `package`; the bare-string shorthand `dep = "^1.0"` leaves it unset and errors at resolve time.

A dependency table must name **exactly one** of `path`, `git`, or `version`.

**`package` on a `path` or `git` dependency is a checked claim.** On a `version` dependency the identity *selects* the package — it is what the index is queried for. A `path` or `git` source has already selected its tree, so there the same key means something different: it records **which package that source holds**, and the resolver verifies it against that package's own `[package] name`. A claim that disagrees is a manifest error naming both identities and the path; a dependency that names no `package` at all is unchanged, and is still the ordinary spelling. Nothing is inferred from a claim — a wrong one cannot redirect a build, only fail it. (A dependency directory with no `[package]` table is already refused for a different reason: a path/git dependency's identity and namespace root both come from that table, so it is rejected as a missing table whether or not a claim was written.)

Optional keys never carry their weight unless something reads them, and this one earns its place on a scope-array member, below — where the path alone cannot say which package of the scope you meant.

**On a `version` dependency the same key is checked from the other side.** There the identity is the selector, so a disagreement is not yours to correct: the resolver asked the index for `acme/codec`, and the tree that came back declares itself something else. That is a supply-chain event rather than a manifest mistake — a mis-published release whose coordinates name a commit holding a different package, a corrupted store, or a mirror serving someone else's tree — so it is refused as a trust failure naming the release that was selected, the coordinates it was fetched from, and what the tree there actually says (and, when `noeta.lock` already pinned that release, that the tree changed *under* the pin). The version is held to the same rule, because a release is the `(identity, version, commit)` triple a publish attestation signs: a tree declaring a version the index never served is refused too, rather than silently pinning that version in your lockfile. A `[patch]`ed identity is exempt — it never reached the index, and its local tree is checked against the identity it overrides instead.

**The key is the prefix its modules derive under.** A dependency's module paths are not declared by the dependency — they are derived from where its files sit, under the key *you* wrote (see [Modules](Modules#where-a-modules-path-comes-from)). So `codec = { … }` puts a `parse.noe` at `codec.parse`, and renaming the key renames every import path, with nothing inside the package able to override it.

**A dependency's own internal imports are rewritten to match.** A package's files import each other by the `package` half of its identity (`use codec.parse` inside `acme/codec`), which is what they derive under when the package is built on its own; a consumer's build rewrites that leading segment to whatever prefix the package derives under here. So the key is free to be anything, and the package's author never writes it. If you are *writing* a package, see [importing your own package's modules](Modules#importing-your-own-packages-modules).

**Scope dependencies.** An array value binds several packages that share one `company` scope under a single import root:

```toml
[dependencies]
acme = [
  { version = "^1.0", package = "acme/bytes" },
  { version = "^2.0", package = "acme/codec" },
]
```

Members may be any source form, mixed freely. A member sourced from a `path` or `git` is where naming its `package` pays off: the members of a scope are siblings, and a bare `{ path = "../.." }` beside `{ path = "../../../para-api" }` says nothing about which package of the scope each one is. Write the identity and the manifest reads as what it is — checked, so it stays true:

```toml
[dependencies]
para = [
  { path = "../..", package = "para/ai" },
  { path = "../../../para-api", package = "para/api" },
]
```

You do not have to write one by hand: [`noeta add`](The-CLI#noeta-add) under a key that already exists widens that entry into this form, keeping the existing member's text verbatim.

A scope array's members each get the package's own root segment appended to the key, so their modules derive **one segment deeper** than under a plain key: `acme/codec`'s `parse.noe` is `acme.codec.parse` here, where `codec = { package = "acme/codec" }` would make it `codec.parse`. That is the difference between the two forms, and it is why a family of packages published to be addressed as `scope.package.module` — the first-party `para/*` set is the standard case — must be bound with the array form to keep those addresses.

That relation only runs one way: several packages may share one root, but **one package may not be bound under two roots**. A package has one identity and its modules derive under one prefix, so a second key could only be dropped — and a dropped key is a manifest that lies, with `use <that key>.…` failing later as "no module". Two keys naming one identity are refused at resolve time, naming both.

## `[patch]` — dev-time path overrides

Point a package identity at a local source tree while you develop it, without editing any `[dependencies]` entry or regenerating locks by hand. Keys are full identities (`"company/package"` — quoted, they contain a slash); values are `{ path = "…" }` tables, relative to this manifest. `path` is the only override form — there are no git or version-selective patches.

```toml
[dependencies]
db = { version = "^1.4", package = "para/db" }

[patch]
# Every occurrence of para/db in the whole graph resolves from this tree instead:
"para/db" = { path = "../para-db" }
```

- **Root-only.** Patches are honored only from the **root app's** manifest — a dependency's own `[patch]` table is never read (no inheritance, the same top-down authority rule as `[trust]`).
- **Everywhere.** The override applies wherever the identity occurs — a direct dependency, a transitive one, or a scope member — and a patched registry identity never touches the index, so the patched version doesn't need to be published at all.
- **The patched tree's version wins.** Its own `noeta.toml` is authoritative; if that version fails a requirement the graph imposes (a consumer still declaring `^1` while your checkout says `2.0.0`), resolution **warns and proceeds** — a dev override means the developer knows best, and it never becomes an error. The tree must declare exactly the identity it patches; a mismatch is a hard error.
- **Loud.** Every resolve with an active patch prints a per-patch notice on stderr — an override is never silent.
- **Never recorded.** `noeta.lock` omits patched identities entirely: the lock records only reproducible state, so while the patch is active the identity simply has no pin, and removing the patch re-pins it from its declared source on the next resolve — no stale entries either way.
- **Not publishable.** `noeta publish` refuses a manifest with a non-empty `[patch]` table.

## `[trust]` — grants and provenance policy

Dependencies get no elevated capability by default. `[trust]` is where you grant specific ones and set provenance policy for the whole project. See [Package Provenance](Package-Provenance) for the trust model.

```toml
[trust]
# Packages allowed to compile and run their native Rust crate (runs cargo/build.rs/proc-macros):
native = ["acme/imgfx"]
# Require signed provenance: true (every scope), false (default), or a list of scopes:
require_provenance = ["acme", "para"]
# Require every registry dependency to appear in the transparency log:
require_transparency = true
# Refuse a release published within this window (defends against a compromised-token rush):
publish_cooldown = "24h"

# Each `noeta <subcommand>` a dependency may contribute, under the name you choose for it:
[trust.commands]
migrate = "para/db"              # `noeta migrate`
undo    = "para/db:rollback"     # `noeta undo`, renamed from para/db's `rollback`
```

- **`native`** is an array of `"company/package"` — an unlisted package's native crate is refused. A typo'd identity is a hard error.
- **`require_provenance`** is a boolean or an array of **scopes** (the `company` half, not full package names): `true` requires provenance from every scope, `false` (the default) from none, `["acme"]` only from the named scopes.
- **`require_transparency`** is a boolean (default `false`).
- **`publish_cooldown`** is a duration string — an integer with an optional `s`/`m`/`h`/`d` suffix (`"24h"`, `"30m"`, `"7d"`; a bare number is seconds), not a raw number.

### `[trust.commands]` — contributed subcommands

A dependency that ships CLI commands (`ExtCommand` — see [Native Extensions](Native-Extensions#extension-commands)) contributes none of them by default. Each entry binds one command: the **key** is the name you will type (`noeta <local>`), the value is the providing package, optionally followed by `:` and the name that package exported it under.

```toml
[trust.commands]
migrate = "para/db"              # `noeta migrate` — no rename, so the exported name is `migrate`
undo    = "para/db:rollback"     # `noeta undo` — para/db exported it as `rollback`
```

The binding *is* the grant. One entry both authorizes the provider to contribute this one command and fixes the name it appears under, so a package is never named twice to be trusted and bound, and a command from a package with no entry is never registered — a capability you never asked for.

Because the local name is yours, two packages exporting the same command name coexist: bind one of them under a different key. The first `:` splits the identity from the exported name — a package identity always contains a `/` and never a `:` — so the exported half may contain any character, including a space (`remote-add = "acme/tools:remote add"`).

**A binding may take over one of `std`'s commands.** `test`, `bench`, `doc`, and `serve` are contributed through this same mechanism — `std` ships with the toolchain, so they are registered by default rather than by a grant — which means binding a package under one of those names *replaces* it:

```toml
[trust.commands]
test = "thirdparty/ExcellentTesting"   # `noeta test` is now theirs — their flags, their --help
```

That is the whole point of std holding no privilege here: you get the batteries out of the box, and swapping one out is one line rather than a fork. The replacement owns the verb completely, so its own arguments and exit codes apply — `noeta test --json` means whatever the new provider says it means.

The **core toolchain verbs** are still reserved: `run`, `build`, `check`, `fmt` and their siblings are the compiler, and a binding that names one is refused. Note the difference from [`[directives]`](#directives--where-each-name-comes-from): binding `test` there changes what the `@test` *directive* means at compile time (and therefore what `noeta check` verifies); binding it here changes which *command* runs. A framework that runs your existing `@test` blocks its own way needs only this table.

Unlike [`[directives]`](#directives--where-each-name-comes-from), which is per-package and may be keyed by the using package's own dependency keys, this table is **root-only** and keyed by full package **identity**: it is a capability grant, and a grant is the top-level project's alone to make. A dependency's own `[trust.commands]` is not read.

A bare `commands = ["company/package"]` array — granting every command a package ships, rather than binding them one at a time — is refused with a message naming this table as its replacement.

### `[trust.advisories]` — per-tier advisory policy

Sets what a **security-advisory hit** does per intake tier (`operator` / `publisher` / `imported` — the concept and what each tier means are on [Package Provenance](Package-Provenance#security-advisories-and-intake-tiers)). Each tier takes one of three actions: `"warn"` (the default — `noeta audit` prints the hit), `"fail"` (the hit fails the run, a CI gate), or `"off"` (the tier is ignored).

```toml
[trust.advisories]
operator  = "fail"     # a curated advisory breaks the build
publisher = "fail"     # so does an owner-issued one
imported  = "warn"     # imported feeds are broader — warn, don't fail
```

A bare string under `[trust]` — `advisories = "fail"` — sets every tier at once. A key that is not one of the three tiers, or an action that is not `"fail"`/`"warn"`/`"off"`, is a manifest error.

## `[directives]` — where each `@name` comes from

The `[directives]` table names who provides each `@name` your source writes — a local `@name` → `"provider[:exported]"`. **Plain directives (`@openapi`) and *tier directives* — the ones that take a block, `@test { … }`, `@sql { … }` — are bound the same way.** A `@name` is one namespace, and source cannot tell the two apart until resolution, so the manifest does not ask you to classify them.

```toml
[dependencies]
para = [
    { version = "^0.2", package = "para/api" },
    { version = "^0.4", package = "para/db" },
]

[directives]
test    = "std"             # std's `@test { … }`
openapi = "para/api"        # para/api's `@openapi` directive
sql     = "para/db"         # para/db's `@sql { … }` block tier
oapi    = "para/api:openapi"  # the same directive, written `@oapi` in this package's source
crit    = "acme/criterion:bench"  # a dependency's `bench` tier, written `@crit` locally
```

A provider is the built-in `"std"`, a package **identity** (`para/db`), or a key of this package's `[dependencies]` (including a `[targets.<name>.dependencies]` key, for a dev-only tier). **Prefer the identity.** A key bound to a *scope* covers several member packages at once — `para` above is `para/api` *and* `para/db` — so it cannot say which one you meant. Naming a provider that is neither `"std"` nor a declared dependency is a manifest error.

Nothing here is ambient. A `@name` a dependency provides resolves **only** through an entry in this table, and the entry is also what pulls a pure-Noeta provider's handler into your program — an `import` neither substitutes for a binding nor is needed alongside one. There are no ambient built-in tiers either: `test`/`bench`/`doc`/`debug` are ordinary `std` tiers you name here like any other provider's.

Bindings are **per-package**: a `@name` resolves in the source that wrote it, so a dependency's `@openapi` means whatever *its* manifest said regardless of what you bind `@openapi` to, and two packages can name one provider's `@name` differently. `:exported` renames — which is how two providers' same-named entries coexist.

Which of the *tier directives* among these are live in a build is a separate axis — a target's `tiers` live-set ([below](#targets--build-recipes)) — because only tier directives activate.

Binding a `@name` does **not** authorize the provider's native code. That stays root-only, in [`[trust].native`](#trust--grants-and-provenance-policy).


## `[targets]` — build recipes

A target is a named build recipe: an **activation live-set** of the local tier names (from `[directives]`) that are live in the build, plus its own dependencies (dev-dependencies) and an optional base to inherit from. The live-set no longer names a provider (that is `[directives]`'s job) — it is an array of tier names where a bare name is live and a `-`-prefixed name turns an inherited tier off.

```toml
[targets.test]
# dev-only dependencies, overlaid on the globals for this target:
dependencies = { check = { version = "^1.0", package = "acme/check" } }
# activation live-set — which local tier names are live:
tiers = ["test"]

[targets.bench]
extends = "test"                     # inherit test's live-set and dependencies
tiers = ["bench", "-test"]           # add bench, drop the inherited test
```

- **`extends`** names a base target to inherit the live-set and dependencies from; a nearer entry overrides the base's (a `-name` turns an inherited tier off). Inheritance cycles are an error.
- **`dependencies`** follows the same rules as the global `[dependencies]` table.
- **`tiers`** is a live-set of local tier names — the array form above (bare = live, `-name` = off), or the equivalent boolean sub-table `[targets.<name>.tiers]` with `name = true`/`false`; the provider each resolves to lives in the top-level `[directives]` table.

The target to build is chosen at the command line (`--target`), not in the manifest.

### A worked example

The pair `noeta init` scaffolds is the pattern to copy: a `development` target that makes the four std dev tiers live, and an explicit `production` name for the tier-free baseline (any command with no `--target` already builds that shape):

```toml
[package]
name = "acme/app"
version = "0.1.0"

[directives]
test = "std"
bench = "std"
doc = "std"
debug = "std"

[targets.development]
tiers = ["test", "bench", "doc", "debug"]

[targets.production]
```

With that manifest, `noeta run src/main.noe --target development` executes `@debug { … }` blocks, and `--target` acts as a **gate** on the tier runners — a target that doesn't make the tier live no-ops with exit `0`:

```console
$ noeta test src/main.noe --target production
tier `test` is not active in target `production`
$ noeta test src/main.noe --target development
running 2 tests on 2 threads
  ok    greets
  ok    greets_noeta

2 passed, 0 failed, 2 total
```

## `[registries]` — routing scopes to registries

Route a scope to the registry its packages resolve from, with an optional `default` for the rest. Unmapped scopes resolve from the built-in default — the hosted registry at `registry.noeta.dev` — unless `[registries].default` or the `NOETA_REGISTRY_URL`/`NOETA_REGISTRY_DIR` environment overrides say otherwise. Fully covered in [Package Registries](Package-Registries).

```toml
[registries]
default = "https://registry.noeta.dev"   # the hosted service for unmapped scopes
acme = "github:acme"                      # acme/* resolve straight from a GitHub org
internal = "git:ssh://git.example.com/pkgs"
```

A registry source is one of: an `http(s)://` URL (a hosted registry service), `github:<owner>`, `gitlab:<group>`, or `git:<url>` (any git remote — a forge *is* an index). Every key other than `default` must be a bare scope (`company`) identifier.

## `[db]` — database connection

The project's database connection and migration layout, read by the [para/db](para-db) extension's [`noeta migrate`](The-CLI#noeta-migrate) command (and by an app migrating itself at boot). All three keys are optional strings — but a present key of the wrong type is a manifest error:

```toml
[db]
url = "sqlite:app.db"        # any dsn scheme db.connect accepts (sqlite:… / postgres://…)
migrations = "migrations"    # the migrations directory (default "migrations")
seeds = "seeds"              # the seeds directory (default "seeds")
```

`url` is the **lowest-priority** source of the connection string — the `--db <dsn>` flag, then the `DATABASE_URL` environment variable, win over it. The `--dir`/`--seeds-dir` flags override the two directories per-invocation.

## Related tables

Two more tables live in a `noeta.toml` but are read by their own subsystems rather than the package model:

- **`[native]`** with a `rings` array configures native capability rings — see [Native Extensions](Native-Extensions).
- **`[fmt]`** configures the formatter — see the `noeta fmt` documentation.
