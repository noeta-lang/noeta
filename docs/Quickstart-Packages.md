# Quickstart: Packages

From nothing to a running program with a registry dependency in five commands. Every command and every line of output on this page was run against the released `noeta` v0.2.0 on x86_64 Linux.

## 1 · Install the toolchain

```sh
curl -fsSL https://noeta.dev/install | sh
```

```
installing noeta v0.2.0 for x86_64-unknown-linux-gnu
installed …/bin/noeta
```

The script downloads the latest release, verifies its checksum, and installs to `~/.local/bin` by default (this run overrode the destination with `NOETA_INSTALL_DIR`; `--to <dir>` works too). If the destination is not on your `PATH`, the script prints the exact line to add for your shell. Later, `noeta upgrade` updates the binary in place.

```sh
noeta --version
```

```
noeta 0.2.0
```

## 2 · Create a project

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

`noeta.toml` is the manifest; `src/main.noe` is a runnable entry point with example tests and benchmarks. `AGENTS.md` and `SYNTAX.md` teach coding agents the language so they are productive in the project from the first prompt.

## 3 · Add a dependency

We'll use `para/cli`, the first-party command-line framework, from the hosted registry:

```sh
noeta add para --version "^0.1" --package para/cli
```

```
added `para` to …/hello-cli/noeta.toml
warning: `para` binds a package whose own module root is `cli` — imports resolve as `para.…`, not `cli.…`
```

The first argument (`para`) is the import root you will `use` in code; `--package` is the registry identity. The manifest now contains:

```toml
[dependencies]
para = { version = "^0.1", package = "para/cli" }
```

`noeta add` also resolves the dependency immediately and writes `noeta.lock`, pinning the exact release tag, commit sha, and content hash, plus the scope's signing identity (the GitHub Actions release workflow that produced the release)—every later install verifies against these.

## 4 · Write a program

Replace `src/main.noe` with a two-command CLI. In `para/cli`, the function signature is the spec: annotate a function with `#[Command]` and the framework derives the argument parser, help text, and exit codes from it.

```noeta
use para.cli.{Command, Arg, run}
use std.{io, os}

#[Command(about: "Greet someone")]
fn greet(name: string, #[Arg(short: "l", help: "shout it")] loud: bool = false): int {
    io.outln(if loud then "HELLO ${name}" else "hello ${name}")
    return 0
}

os.exit(run())
```

Check it—this typechecks your code *and* the dependency boundary:

```sh
noeta check .
```

```
checked 1 file: 0 error(s), 0 warning(s)
```

## 5 · Run it

Arguments for your program go after `--` (everything before it belongs to `noeta run`):

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
Commands:
  greet    Greet someone
```

> [!NOTE]
> With exactly one `#[Command]` in the program, arguments are parsed directly by that command—you do not type `greet` as a subcommand. Add a second `#[Command]` function and `run()` switches to subcommand dispatch.

## 6 · Build

```sh
noeta build src/main.noe
```

```
wrote src/main.noeb (15630 bytes)
```

The `.noeb` bundle is the compiled program; `noeta build --native` produces a standalone executable instead.

## Where to go next

- [Getting Started](Getting-Started) — the language itself, from hello world up
- [Manifest](Manifest) — everything `noeta.toml` can express
- [Package Registries](Package-Registries) — path, git, and registry dependencies in depth
- [Package Provenance](Package-Provenance) — how signing, the transparency log, and trust gates work
- Browse packages at [registry.noeta.dev](https://registry.noeta.dev)
