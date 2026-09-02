# Using Packages

This walkthrough installs the toolchain, scaffolds a project, adds a dependency from the hosted registry, grants it what it needs, and runs the result. The reference pages behind each step are [the `noeta.toml` Manifest](Manifest), [Package Registries](Package-Registries), and [Package Provenance](Package-Provenance).

## 1 · Install `noeta`

```sh
curl -fsSL https://noeta.dev/install | sh
```

The script downloads the latest release, checks its SHA-256 against the release's `SHA256SUMS`, and installs to `~/.local/bin`. Pass `--to <dir>`, or set `NOETA_INSTALL_DIR`, to install somewhere else. If the destination is not on your `PATH`, the script prints the exact line to add for your shell. Later, `noeta upgrade` replaces the binary in place and never installs a prerelease.

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

`noeta.toml` is the manifest and `src/main.noe` is a runnable entry point with example tests and benchmarks. `AGENTS.md` and `SYNTAX.md` give a coding agent the project's conventions and the language's syntax; `noeta docs` fetches the rest of the wiki on demand.

`noeta init` writes only the files that are missing, so re-running it in a project that already has some of them adds the rest and leaves those alone. Inside an existing git repository it skips the `git init` step.

The project runs before you edit anything. The whole standard library is built in, so a program with no dependencies needs none of what follows; packages cover everything beyond it.

## 3 · Add a dependency

Dependencies live in the manifest's `[dependencies]` table, and `noeta add` makes the edit for you. The first-party `para/*` packages are published on the hosted registry at [registry.noeta.dev](https://registry.noeta.dev), where each package page lists its current version.

We will use `para/cli`, the command-line framework:

```sh
noeta add para --package para/cli
```

```
resolved `para/cli` to ^0.4 (the registry's current version)
added `para` to …/hello-cli/noeta.toml
  use para.cli
```

No version is written by hand: with no source given, `noeta add` asks the registry for the package's current version and writes a caret requirement for it, so a tutorial's command does not go stale when the package ships a minor. A **prerelease** or a **yanked** release is never picked that way; depend on one deliberately with `--version`. The last line lists the module paths the new dependency binds.

The first argument (`para`) is the **import root** you write after `use`; `--package` is the **registry identity**. Keeping them separate lets several packages of one scope sit under one import root. `noeta add para/cli` would instead read the identity as the positional and derive the import root from its second half, binding the package under `cli`. We want `para` here, so we name it. The manifest now contains:

```toml
[dependencies]
para = { version = "^0.4", package = "para/cli" }
```

`noeta add` then resolves the dependency and writes `noeta.lock`, pinning the release's tag, commit sha, content hash, language edition and signing identity (the CI workflow the release's certificate names). Every later build verifies against those pins. `noeta add` and `noeta update` also call out a new pin whose commits introduce a committer new to that repository.

The other source forms are covered in [the Manifest](Manifest): a local `{ path = "…" }` tree, a git repo pinned to a `{ git = "…", tag = "…" }` release or tracking a `branch`, and a **scope array** binding several packages of one scope under one key. Mix them freely during development, though a *published* package may depend only via the registry.

## 4 · `[trust]` — when a package is native or adds commands

Packages get no elevated capability from appearing in `[dependencies]`. A package that runs **native code** (Rust compiled into your toolchain, such as `para/db`'s database drivers or `para/p2p`'s networking) must be authorized explicitly, and so must one that contributes **`noeta <subcommand>` CLI commands** (like `para/db`'s `noeta migrate`):

```toml
[trust]
native = ["para/db"]

[trust.commands]
migrate = "para/db"          # `noeta migrate`
```

Native code is authorized per package. A command is authorized one at a time, and the same entry decides what you type: the key is the local name you will run (`noeta migrate`) and the value is the provider. Add `:exported` to bind a command under a different name, as `db = "para/db:migrate"` does for `noeta db`, which is also how two packages exporting the same command name coexist.

An unauthorized native package or command is refused with an error naming the grant to add. `noeta audit` reports the resulting trust footprint for the whole tree.

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

`noeta run` (and `build`/`check`/`test`) resolves the graph on demand, so nothing has to be installed first. It fetches each release's source, verifies its signed [provenance](Package-Provenance), and writes `noeta.lock`; commit that file. The registry serves *coordinates*, and the code comes from the package's own git repo. The lock pins every package's version, commit sha, content hash, and edition, plus each release's trust root on first use, so every later build reproduces the same tree on any machine, offline once cached.

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
> With exactly one `#[Command]` in the program, arguments are parsed directly by that command, so you do not type `greet` as a subcommand. Add a second `#[Command]` function and `run()` switches to subcommand dispatch.

## 7 · Build

```sh
noeta build src/main.noe
```

```
wrote src/main.noeb (18657 bytes)
```

The `.noeb` bundle is the compiled program; `noeta build --native` produces a standalone executable instead.

## 8 · What a package may implement

A dependency can add behavior to its **own** types, or to its own traits. `impl Trait for Type` must live in the package that declares the trait or the type, which is the [orphan rule](Generics-and-Traits#the-orphan-rule), E0070. A package you install can therefore never change what another package's types do, and no combination of dependencies produces a coherence conflict you have no way to resolve.

The same rule applies to *your* code. To give a dependency's type behavior of your own, wrap it in a type you own and delegate; `@derive(Trait, via: field)` writes the newtype for you.

## 9 · Keeping up

| Command | What it moves |
|---|---|
| `noeta update` | Re-resolves the whole graph: newer versions satisfying your requirements, moved branch tips, drifted pins. It is the only command that moves an existing pin. |
| `noeta upgrade` | Replaces the *toolchain* binary with the latest release, never a prerelease. |
| `noeta audit` | Reports what you are running: every package's source, the active `[trust]` grants, each pinned provenance identity, and any security-advisory hits. |

By default everything resolves from `registry.noeta.dev`. Private and per-scope registries (`[registries]`, `NOETA_REGISTRY_URL`, `NOETA_REGISTRY_DIR`) are on [Package Registries](Package-Registries).

## Where to go next

- [Language Tour](Language-Tour) — the language itself, example-driven, in one sitting
- [Manifest](Manifest) — everything `noeta.toml` can express
- [Package Registries](Package-Registries) — path, git, and registry dependencies in depth
- [Package Provenance](Package-Provenance) — how signing, the transparency log, and trust gates work
- [Writing Native Packages](Writing-Native-Packages) — the producer's side
- Browse packages at [registry.noeta.dev](https://registry.noeta.dev)
