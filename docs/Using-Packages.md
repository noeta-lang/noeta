# Using Packages

The practical walkthrough: install the toolchain, scaffold a project, add a dependency from the hosted registry, grant it what it needs, and run. The reference pages behind each step are [the `noeta.toml` Manifest](Manifest), [Package Registries](Package-Registries), and [Package Provenance](Package-Provenance).

Every command and every line of output below was run live against the current release.

## 1 · Install `noeta`

```sh
curl -fsSL https://noeta.dev/install | sh
```

The script downloads the latest release, verifies its checksum, and installs to `~/.local/bin` by default — pass `--to <dir>` (or set `NOETA_INSTALL_DIR`) to install elsewhere. If the destination is not on your `PATH`, it prints the exact line to add for your shell. Later, `noeta upgrade` updates the binary in place (never to a prerelease).

Pinning a version, macOS Gatekeeper, and building from source on other platforms are covered in [Getting Started](Getting-Started#1--install-the-toolchain).

## 2 · Scaffold a project

```sh
mkdir hello-cli && cd hello-cli
noeta init
```

```
  created noeta.toml
  created src/main.noe
  created .gitignore
  created .vscode/launch.json
  created .vscode/extensions.json
  created AGENTS.md
  created SYNTAX.md
  created git repository
initialized Noeta package `local/hello_cli` in .
```

`noeta.toml` is the manifest and `src/main.noe` is a runnable entry point with example tests and benchmarks. `AGENTS.md` and `SYNTAX.md` teach coding agents the language, so they are productive in the project from the first prompt. `noeta init` never overwrites an existing file, so it is safe to re-run in a project that already has some of them.

The project runs before you edit anything, and a program with no dependencies needs none of what follows — the whole standard library is built in. Packages are for everything beyond it.

## 3 · Add a dependency

Dependencies live in the manifest's `[dependencies]` table, and `noeta add` makes the edit for you. The first-party `para/*` packages — `para/aether` (web framework), `para/db` (database drivers), `para/html` (LiveView), `para/api`, `para/cli`, `para/aether_db`, `para/p2p` — are published on the hosted registry at [registry.noeta.dev](https://registry.noeta.dev), where each package page lists its current version.

We will use `para/cli`, the command-line framework:

```sh
noeta add para --version "^0.2" --package para/cli
```

```
added `para` to …/hello-cli/noeta.toml
```

The first argument (`para`) is the **import root** — the name you write after `use` — and `--package` is the **registry identity**. They are separate so that several packages of one scope can sit under one import root. The manifest now contains:

```toml
[dependencies]
para = { version = "^0.2", package = "para/cli" }
```

`noeta add` resolves the dependency immediately and writes `noeta.lock`, pinning the exact release tag, commit sha, and content hash, plus the scope's signing identity — the release workflow that produced it. Every later install verifies against these, and a release introducing commits from a committer new to that repo is called out for review before you trust it.

The other source forms — a local `{ path = "…" }` tree, a git repo pinned to a `{ git = "…", tag = "…" }` release or tracking a `branch`, and binding several packages of one scope under a single key (a **scope array**) — are covered in [the Manifest](Manifest). Mix them freely during development, but a *published* package may depend only via the registry.

## 4 · `[trust]` — when a package is native or adds commands

Packages get no elevated capability by default. A package that runs **native code** (Rust compiled into your toolchain — `para/db`'s database drivers, `para/p2p`'s networking) must be authorized explicitly, and so must one that contributes **`noeta <subcommand>` CLI commands** (like `para/db`'s `noeta migrate`):

```toml
[trust]
native = ["para/db"]

[trust.commands]
migrate = "para/db"          # `noeta migrate`
```

Native code is authorized per package. A command is authorized **one at a time**, and the same entry decides what you type: the key is the local name (`noeta migrate`), the value the provider — add `:exported` to rename one (`undo = "para/db:rollback"`), which is also how two packages exporting the same command name coexist.

An unauthorized native package or command is refused with an error naming the grant to add — nothing runs on the strength of appearing in `[dependencies]` alone. `noeta audit` reports the resulting trust footprint for the whole tree.

## 5 · Write a program

In `para/cli` the function signature is the spec: annotate a function with `#[Command]` and the framework derives the argument parser, help text, and exit codes from it. Replace `src/main.noe` with a small greeter:

```noeta ignore
use para.cli.{Command, Arg, run}
use std.{io, os}

#[Command(about: "Greet someone")]
fn greet(name: string, #[Arg(short: "l", help: "shout it")] loud: bool = false): int {
    io.outln(if loud then "HELLO ${name}" else "hello ${name}")
    return 0
}

os.exit(run())
```

`noeta check` typechecks your code *and* the dependency boundary:

```sh
noeta check .
```

```
checked 1 file: 0 error(s), 0 warning(s)
```

## 6 · Run

There is no install step. `noeta run` (and `build`/`check`/`test`) resolves the graph on demand: it fetches each release's source (the registry serves *coordinates*; code comes from the package's own git repo), verifies its signed [provenance](Package-Provenance), and writes `noeta.lock` — commit that file. The lock pins every package's version, commit sha, content hash, and edition, plus each scope's trust root (trust-on-first-use), so every later build reproduces the same tree on any machine, offline once cached.

Arguments for your program go after `--`; everything before it belongs to `noeta run`:

```sh
noeta run src/main.noe -- world
```

```
hello world
```

```sh
noeta run src/main.noe -- world --loud
```

```
HELLO world
```

The short flag from the `#[Arg]` attribute works too (`-l`), and `--help` is generated for free:

```sh
noeta run src/main.noe -- --help
```

```
usage: main.noe <name> [--loud]

Arguments:
  name: string
  loud: bool (-l) [optional] - shout it
```

> [!NOTE]
> With exactly one `#[Command]` in the program, arguments are parsed directly by that command — you do not type `greet` as a subcommand. Add a second `#[Command]` function and `run()` switches to subcommand dispatch.

## 7 · Build

```sh
noeta build src/main.noe
```

```
wrote src/main.noeb (17376 bytes)
```

The `.noeb` bundle is the compiled program; `noeta build --native` produces a standalone executable instead.

## 8 · What a package may implement

A dependency can only add behavior to its **own** types, or to its own traits. `impl Trait for Type` must live in the package that declares the trait or the type — the [orphan rule](Generics-and-Traits#the-orphan-rule), E0070. So no package you install can quietly change what another package's types do, and no combination of dependencies can produce a coherence conflict you have no way to resolve.

The same rule applies to *your* code: to give a dependency's type behavior of your own, wrap it in a type you own and delegate — `@derive(Trait, via: field)` writes the newtype for you.

## 9 · Keeping up

- **`noeta update`** re-resolves everything deliberately: newer versions satisfying your requirements, moved branch tips, drifted pins. Nothing else ever moves an existing pin.
- **`noeta upgrade`** updates the *toolchain* itself (never to a prerelease).
- **`noeta audit`** shows what you are actually running: every package's source, the active `[trust]` grants, each scope's pinned provenance identity, and any security-advisory hits.

By default everything resolves from `registry.noeta.dev`; private and per-scope registries (`[registries]`, `NOETA_REGISTRY_URL`, `NOETA_REGISTRY_DIR`) are on [Package Registries](Package-Registries).

## Where to go next

- [Language Tour](Language-Tour) — the language itself, example-driven, in one sitting
- [Manifest](Manifest) — everything `noeta.toml` can express
- [Package Registries](Package-Registries) — path, git, and registry dependencies in depth
- [Package Provenance](Package-Provenance) — how signing, the transparency log, and trust gates work
- [Writing Native Packages](Writing-Native-Packages) — the producer's side
- Browse packages at [registry.noeta.dev](https://registry.noeta.dev)
