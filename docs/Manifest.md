# The `noeta.toml` manifest

Every Noeta package is described by a `noeta.toml` at its root. It declares the package's identity, its dependencies, and how it builds. This page is the complete reference for the format: every table and key the toolchain reads, with the exact rules the parser enforces.

A manifest is TOML, and **unknown keys are ignored** everywhere. The parser looks only for the keys documented here, so a typo'd key name is skipped rather than reported. That is worth remembering when a setting seems to have no effect.

The smallest useful manifest is an identity:

```toml
[package]
name = "acme/imgfx"
version = "1.2.0"
```

A manifest with no `[package]` at all describes a **bare script**, which builds and runs. Publishing needs an identity, so a bare script cannot be published.

## `[package]` — identity and metadata

| Key | Required | Value | Notes |
|-----|----------|-------|-------|
| `name` | **yes** | `"company/package"` | Two registry names joined by a single `/`. |
| `root` | no | `"app"` | The segment this package's modules derive under. Defaults to the `package` half. |
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
edition = "2026"                             # optional: the language edition this package targets
toolchain = ">=0.2"                          # optional: the minimum noeta this package works with
license = "MIT OR Apache-2.0"                # optional: declared SPDX expression
keywords = ["image", "simd"]                 # optional: up to 5 discovery tags
description = "Fast image effects for Noeta" # optional: one-line search blurb
```

**`name`** is a global identity `company/package`, what a registry indexes and a git coordinate resolves to. Each half is a **registry name**: letters, digits and `_`, with `-` allowed between them. It is a coordinate rather than something spelled in source, so it may look like one. `noeta-lang/my-toolkit` is a legal identity, and the `company` half is the scope a forge already knows you by.

**`root`** is the segment this package's own modules derive under, and unlike the identity it is spelled in source. With `root = "app"`, `src/models.noe` is the module `app.models` and the package's own files import it as `use app.models`. It must therefore lex as one identifier.

The two keys are separate so that each can be what it is: rename the package and no import changes, and give the package a hyphenated name and its imports stay legal. **Omit `root` and the `package` half is used instead**, which then has to lex as an identifier. A hyphenated identity without a `root` is a manifest error saying exactly that. See [importing your own package's modules](Modules#importing-your-own-packages-modules).

**`toolchain`** declares the minimum `noeta` version the package works with, as a SemVer requirement the running binary's version must satisfy (`toolchain = ">=0.2"`). It is enforced at resolve time, for your own package and for every dependency, so a consumer on an older toolchain gets "requires noeta >=0.2 … run `noeta upgrade`" instead of a compile error deep inside a native build. Omit it and the package makes no claim.

The key is a courtesy floor; the compatibility contract itself is the extension ABI. Declare the oldest toolchain you test against, typically the release current when you publish.

The value is a full SemVer requirement, and range (`">=0.2, <0.4"`), tilde, exact and wildcard forms all work. Prefer `>=`: a bare `"0.2"` means caret (`>=0.2.0, <0.3.0`), which imposes an upper bound and would refuse noeta 0.3. A pre-release binary such as `0.3.0-rc.1` matches as its release triple `0.3.0`, so release candidates resolve.

**`license`** is checked for SPDX shape only, meaning letters, digits and `` .+()- `` up to 120 characters, rather than validated as real SPDX. The claim is the publisher's, since the registry never reads your source, and the SHA-pinned tree's `LICENSE` file is the ground truth. It is part of the immutable release record and bound into the transparency log, so a registry cannot equivocate about what a release declared.

**`keywords`** are a set of up to **5** tags, each 1–20 characters of lowercase `a–z`, `0–9` and `-`, starting with a letter or digit. They are stored deduplicated and sorted, so the order you write them, and any repeats, make no difference. One canonical form per tag is what lets a registry group everything tagged `aether` into one listing instead of scattering it across `Aether`, `aether_` and `AEther`. Keywords are discovery metadata and stay out of the transparency-log leaf, where `license` sits, since tampering with one mis-files a package in a listing.

**`description`** is a single-line blurb of up to 200 characters, with no line breaks, shown next to your package in search results and on its registry page. Like `keywords` it is discovery metadata, indexed for search and left out of the transparency log. A package without one is still searchable by name and keyword, and has no one-line summary in the results.

## `[dependencies]` — what the package builds against

Each entry's **key is the import root**, the name you write after `use`, decoupled from the package's real identity. The value names the source. A key may not shadow a built-in root (`std`, `noeta`, `core`).

```toml
[dependencies]
# A local source tree: no network, no resolver:
util  = { path = "../util" }
# A git dependency pinned to a released tag (the reproducible default):
http  = { git = "https://github.com/acme/http", tag = "v1.2.0" }
# A git dependency tracking a branch's tip (re-resolved by `noeta update`):
gfx   = { git = "https://github.com/acme/gfx", branch = "main" }
# A git dependency tracking the default branch's HEAD, with no tag or branch:
draft = { git = "https://github.com/acme/draft" }
# A registry dependency. `package` names the real identity when it differs from the key:
codec = { version = "^1.0", package = "acme/imgcodec" }
# A path dependency that also records which package that directory holds:
ai    = { path = "../para-ai", package = "para/ai" }
```

A dependency table names **exactly one** of `path`, `git`, or `version`.

| Form | Source |
|---|---|
| `{ path = "…" }` | A local directory, relative to the manifest. No network, no version resolution. |
| `{ git = "…", tag = "…" }` | A git repository pinned to a released tag, the reproducible form. A git dependency takes `tag` or `branch`, and a table with both is an error. |
| `{ git = "…", branch = "…" }` | A branch's tip, re-pinned by `noeta update`. |
| `{ git = "…" }` | The default branch's HEAD, for an in-development package not yet cut into releases. |
| `{ version = "…" }` | A registry dependency, where `version` is a SemVer requirement (`"^1.0"`, `"~2.3"`). |

A registry dependency needs a resolvable `package = "company/package"`. That key is also what lets the registry identity differ from the import-root key, as Cargo's `foo = { package = "real" }` does. The bare-string shorthand `dep = "^1.0"` leaves it unset and errors at resolve time.

**`package` on a `path` or `git` dependency is a checked claim.** On a `version` dependency the identity selects the package, since it is what the index is queried for. A `path` or `git` source has already selected its tree, so there the key records which package that source holds, and the resolver verifies it against that package's own `[package] name`.

A claim that disagrees is a manifest error naming both identities and the path. Nothing is inferred from a claim, so a wrong one fails the build rather than redirecting it. A dependency that names no `package` is the ordinary spelling and is unaffected. A dependency directory with no `[package]` table is refused separately, since a path or git dependency's identity and namespace root both come from that table.

The claim earns its place on a scope-array member, described below, where the path alone cannot say which package of the scope you meant.

**On a `version` dependency the same key is checked from the other side.** There the identity is the selector: the resolver asked the index for `acme/codec`, and the tree that came back declares itself something else.

That is a supply-chain event rather than a manifest mistake, whether a mis-published release whose coordinates name a commit holding a different package, a corrupted store, or a mirror serving someone else's tree. It is refused as a trust failure naming the release that was selected, the coordinates it was fetched from, and what the tree there says. When `noeta.lock` already pinned that release, the message says the tree changed under the pin.

The version is held to the same rule, because a release is the `(identity, version, commit)` triple a publish attestation signs. A tree declaring a version the index never served is refused rather than pinned in your lockfile. A `[patch]`ed identity is exempt, since it never reached the index; its local tree is checked against the identity it overrides.

**The key is the prefix its modules derive under.** A dependency's module paths are derived from where its files sit, under the key you wrote (see [Modules](Modules#where-a-modules-path-comes-from)). So `codec = { … }` puts a `parse.noe` at `codec.parse`, and renaming the key renames every import path, with nothing inside the package able to override it.

**A dependency's own internal imports are rewritten to match.** A package's files import each other by its `[package] root`, writing `use codec.parse` inside a package whose root is `codec`, which is what they derive under when the package is built on its own. A consumer's build rewrites that leading segment to whatever prefix the package derives under here. The key is therefore free to be anything, and the package's author never writes it. If you are writing a package, see [importing your own package's modules](Modules#importing-your-own-packages-modules).

**Scope dependencies.** An array value binds several packages that share one `company` scope under a single import root:

```toml
[dependencies]
acme = [
  { version = "^1.0", package = "acme/bytes" },
  { version = "^2.0", package = "acme/codec" },
]
```

Members may be any source form, mixed freely. Naming `package` pays off on a member sourced from a `path` or `git`: the members of a scope are siblings, and a bare `{ path = "../.." }` beside `{ path = "../../../para-api" }` says nothing about which package of the scope each one is. Write the identity and the manifest says which is which, checked so it stays true:

```toml
[dependencies]
para = [
  { path = "../..", package = "para/ai" },
  { path = "../../../para-api", package = "para/api" },
]
```

You do not have to write one by hand: [`noeta add`](The-CLI#noeta-add) under a key that already exists widens that entry into this form, keeping the existing member's text verbatim.

A scope array's members each get the package's own root segment appended to the key, so their modules derive **one segment deeper** than under a plain key. `acme/codec`'s `parse.noe` is `acme.codec.parse` here, where `codec = { package = "acme/codec" }` would make it `codec.parse`. That extra segment is the difference between the two forms, and it is why a family published to be addressed as `scope.package.module`, as the first-party `para/*` set is, is bound with the array form.

The relation runs one way. Several packages may share one root, and **one package may not be bound under two roots**. A package has one identity and its modules derive under one prefix, so a second key would have to be dropped, leaving `use <that key>.…` to fail later as "no module". Two keys naming one identity are refused at resolve time, naming both.

## `[patch]` — dev-time path overrides

Points a package identity at a local source tree while you develop it, leaving every `[dependencies]` entry and the lockfile alone. Keys are full identities, quoted because `"company/package"` contains a slash; values are `{ path = "…" }` tables relative to this manifest. `path` is the only override form.

```toml
[dependencies]
db = { version = "^1.4", package = "para/db" }

[patch]
# Every occurrence of para/db in the whole graph resolves from this tree instead:
"para/db" = { path = "../para-db" }
```

- **Root-only.** Patches are honored from the **root app's** manifest. A dependency's own `[patch]` table is never read, under the same top-down authority rule as `[trust]`.
- **Everywhere.** The override applies wherever the identity occurs, whether a direct dependency, a transitive one, or a scope member. A patched registry identity never touches the index, so the patched version need not be published.
- **The patched tree's version wins.** Its own `noeta.toml` is authoritative. When that version fails a requirement the graph imposes, such as a consumer declaring `^1` while your checkout says `2.0.0`, resolution warns and proceeds, since a development override means the developer knows best. The tree must declare exactly the identity it patches, and a mismatch is a hard error.
- **Loud.** Every resolve with an active patch prints a per-patch notice on stderr.
- **Never recorded.** `noeta.lock` omits patched identities, since the lock records reproducible state. While the patch is active the identity has no pin, and removing the patch re-pins it from its declared source on the next resolve.
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
db      = "para/db:migrate"      # `noeta db`, renamed from para/db's `migrate`
```

| Key | Value | Default |
|---|---|---|
| `native` | An array of `"company/package"` whose native crates may compile and run. An unlisted package's native crate is refused, and a typo'd identity is a hard error. | `[]` |
| `require_provenance` | A boolean, or an array of **scopes** (the `company` half rather than full package names). `true` requires provenance from every scope, `["acme"]` from the named scopes. | `false` |
| `require_transparency` | A boolean. | `false` |
| `publish_cooldown` | A duration string: an integer with an optional `s`, `m`, `h` or `d` suffix (`"24h"`, `"30m"`, `"7d"`), where a bare number is seconds. A raw TOML number is an error. | unset |

### `[trust.commands]` — contributed subcommands

A dependency that ships CLI commands, as `ExtCommand`s described in [Native Extensions](Native-Extensions#extension-commands), contributes them one binding at a time. Each entry binds one command: the key is the name you will type (`noeta <local>`), and the value is the providing package, optionally followed by `:` and the name that package exported it under.

```toml
[trust.commands]
migrate = "para/db"              # `noeta migrate`, with no rename, so the exported name is `migrate`
db      = "para/db:migrate"      # `noeta db`, which para/db exported as `migrate`
```

The binding is the grant. One entry both authorizes the provider to contribute this one command and fixes the name it appears under, so a package is named once, and only a command with an entry is registered.

Because the local name is yours, two packages exporting the same command name coexist: bind one of them under a different key. The first `:` splits the identity from the exported name, since a package identity always contains a `/` and never a `:`. The exported half may therefore contain any character, including a space (`remote-add = "acme/tools:remote add"`).

**A binding may take over one of `std`'s commands.** `test`, `bench`, `doc`, and `serve` are contributed through this same mechanism, registered by default because `std` ships with the toolchain, so binding a package under one of those names replaces it:

```toml
[trust.commands]
test = "thirdparty/ExcellentTesting"   # `noeta test` is now theirs: their flags, their --help
```

The replacement owns the verb completely, so its own arguments and exit codes apply, and `noeta test --json` means whatever the new provider says it means.

The **core toolchain verbs** are reserved. `run`, `build`, `check`, `fmt` and their siblings are the compiler, and a binding whose key names one is refused with exit `2`.

Note the difference from [`[directives]`](#directives--where-each-name-comes-from). Binding `test` there changes what the `@test` directive means at compile time, and therefore what `noeta check` verifies; binding it here changes which command runs. A framework that runs your existing `@test` blocks its own way needs only this table.

The two tables differ in scope as well. `[directives]` is per-package and may be keyed by the using package's own dependency keys. This table is **root-only** and keyed by full package **identity**, because a capability grant is the top-level project's to make. A dependency's own `[trust.commands]` is not read.

A bare `commands = ["company/package"]` array, granting every command a package ships at once, is refused with a message naming this table as its replacement.

### `[trust.advisories]` — per-tier advisory policy

Sets what a **security-advisory hit** does per intake tier, where the tiers are `operator`, `publisher` and `imported`. [Package Provenance](Package-Provenance#security-advisories-and-intake-tiers) covers what each one means. Each tier takes one of three actions:

| Action | Effect |
|---|---|
| `"warn"` | `noeta audit` prints the hit. The default for every tier. |
| `"fail"` | The hit fails the run, which is the CI gate. |
| `"off"` | The tier is ignored. |

```toml
[trust.advisories]
operator  = "fail"     # a curated advisory breaks the build
publisher = "fail"     # so does an owner-issued one
imported  = "warn"     # imported feeds are broader, so warn rather than fail
```

A bare string under `[trust]`, `advisories = "fail"`, sets every tier at once. A key outside the three tiers, or an action outside `"fail"`, `"warn"` and `"off"`, is a manifest error.

## `[directives]` — where each `@name` comes from

The `[directives]` table names who provides each `@name` your source writes, mapping a local `@name` to `"provider[:exported]"`. **Plain directives such as `@openapi` and tier directives, the ones that take a block like `@test { … }` and `@sql { … }`, are bound the same way.** A `@name` is one namespace, and source cannot tell the two apart until resolution, so the manifest does not ask you to classify them.

```toml
[dependencies]
para = [
    { version = "^0.2", package = "para/api" },
    { version = "^0.4", package = "para/db" },
]
criterion = { version = "^1.0", package = "acme/criterion" }

[directives]
test    = "std"             # std's `@test { … }`
openapi = "para/api"        # para/api's `@openapi` directive
sql     = "para/db"         # para/db's `@sql { … }` block tier
oapi    = "para/api:openapi"  # the same directive, written `@oapi` in this package's source
crit    = "acme/criterion:bench"  # a dependency's `bench` tier, written `@crit` locally
```

A provider is the built-in `"std"`, a package **identity** such as `para/db`, or a key of this package's `[dependencies]`, including a `[targets.<name>.dependencies]` key for a development-only tier. Naming a provider outside those three is a manifest error.

**Prefer the identity.** A key bound to a scope covers several member packages at once, so `para` above is `para/api` and `para/db` together and cannot say which one you meant.

Every `@name` a dependency provides resolves through an entry in this table, and that entry is also what pulls a pure-Noeta provider's handler into your program. An `import` neither substitutes for a binding nor is needed alongside one. The built-in tiers work the same way: `test`, `bench`, `doc` and `debug` are ordinary `std` tiers you name here like any other provider's.

Bindings are **per-package**. A `@name` resolves in the source that wrote it, so a dependency's `@openapi` means whatever its manifest said regardless of what you bind `@openapi` to, and two packages can name one provider's `@name` differently. `:exported` renames, which is how two providers' same-named entries coexist.

Which of the tier directives among these are live in a build is a separate axis, a target's `tiers` live-set ([below](#targets--build-recipes)), since only tier directives activate.

Binding a `@name` authorizes no native code. That grant stays root-only, in [`[trust].native`](#trust--grants-and-provenance-policy).


## `[targets]` — build recipes

A target is a named build recipe. It holds an **activation live-set** of the local tier names, drawn from `[directives]`, that are live in the build, plus its own development dependencies and an optional base to inherit from. The live-set names tiers, and `[directives]` names their providers. It is an array of tier names where a bare name is live and a `-`-prefixed name turns an inherited tier off.

```toml
[targets.test]
# dev-only dependencies, overlaid on the globals for this target:
dependencies = { check = { version = "^1.0", package = "acme/check" } }
# activation live-set: which local tier names are live:
tiers = ["test"]

[targets.bench]
extends = "test"                     # inherit test's live-set and dependencies
tiers = ["bench", "-test"]           # add bench, drop the inherited test
```

| Key | Value |
|---|---|
| `extends` | A base target to inherit the live-set and dependencies from. A nearer entry overrides the base's, and a `-name` turns an inherited tier off. Inheritance cycles are an error. |
| `dependencies` | Follows the same rules as the global `[dependencies]` table. |
| `tiers` | A live-set of local tier names, as the array form above (bare is live, `-name` is off) or the equivalent boolean sub-table `[targets.<name>.tiers]` with `name = true` or `false`. The provider each resolves to lives in the top-level `[directives]` table. |

The target to build is chosen at the command line with `--target`.

### A worked example

The pair `noeta init` scaffolds is the pattern to copy: a `development` target that makes the four std dev tiers live, and an explicit `production` name for the tier-free baseline that any command with no `--target` already builds.

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

With that manifest, `noeta run src/main.noe --target development` executes `@debug { … }` blocks. On the tier runners `--target` acts as a **gate**: a target that leaves the tier inactive runs nothing and exits `0`.

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

Routes a scope to the registry its packages resolve from, with an optional `default` for the rest. A scope's own mapping wins, then `[registries].default`, then `NOETA_REGISTRY_URL`, then `NOETA_REGISTRY_DIR`, and otherwise the built-in hosted registry at `registry.noeta.dev`. [Package Registries](Package-Registries) covers this in full.

```toml
[registries]
default = "https://registry.noeta.dev"   # the hosted service for unmapped scopes
acme = "github:acme"                      # acme/* resolve straight from a GitHub org
internal = "git:ssh://git.example.com/pkgs"
```

A registry source is an `http(s)://` URL for a hosted registry service, `github:<owner>`, `gitlab:<group>`, or `git:<url>` for any git remote, since a forge serves as an index. Every key other than `default` must be a bare scope (`company`) identifier.

## `[db]` — database connection

The project's database connection and migration layout, read by the [para/db](para-db) extension's [`noeta migrate`](The-CLI#commands-a-package-contributes) command and by an app migrating itself at boot. All three keys are optional strings, and a present key of the wrong type is a manifest error:

```toml
[db]
url = "sqlite:app.db"        # any dsn scheme db.connect accepts (sqlite:… / postgres://…)
migrations = "migrations"    # the migrations directory (default "migrations")
seeds = "seeds"              # the seeds directory (default "seeds")
```

The connection string is read from the `--db <dsn>` flag first, then the `DATABASE_URL` environment variable, then this `url`. The `--dir` and `--seeds-dir` flags override the two directories per invocation.

## Related tables

Two more tables live in a `noeta.toml` and are read by their own subsystems rather than the package model:

| Table | Purpose |
|---|---|
| `[native]` | A `rings` array configuring native capability rings. See [Native Extensions](Native-Extensions). |
| `[fmt]` | Formatter style. See [`noeta fmt`](The-CLI#noeta-fmt). |
