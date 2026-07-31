# Getting Started

This page gets you from nothing to running your own program in a few minutes.

> [!NOTE]
> Noeta is alpha software. The binary is named `noeta`; source files use the `.noe` extension. At alpha, prebuilt binaries cover **Linux and macOS only** (x86_64/aarch64) — other platforms build from source (below).

## 1 · Install the toolchain

One line — it downloads the latest [release](https://github.com/noeta-lang/noeta/releases) for your machine, verifies its checksum, and installs to `~/.local/bin`:

```sh
curl -fsSL https://noeta.dev/install | sh
```

`--version vX.Y.Z` pins a specific release; `--to <dir>` (or `NOETA_INSTALL_DIR`) changes the destination. Later releases are one `noeta upgrade` away — the installer is only needed once (and `noeta upgrade` never installs a prerelease; see [The CLI](The-CLI#noeta-upgrade)).

**macOS:** the release binaries are not Apple-notarized, so Gatekeeper may refuse to run a copy of `noeta` whose download it quarantined — typically one fetched with a browser rather than the installer (`curl` does not set the quarantine attribute). If macOS blocks it ("cannot be opened because the developer cannot be verified"), clear the quarantine flag and re-run:

```sh
xattr -d com.apple.quarantine <path-to>/noeta
```

### Building from source

On any other platform (musl-only Linux, Windows, *BSD) — or to hack on the toolchain — build with a recent stable Rust (1.95+) from a checkout of [the repository](https://github.com/noeta-lang/noeta):

```sh
cargo build                 # builds the whole workspace, including the `noeta` binary
```

The binary lands at `target/debug/noeta`; `cargo build --release` produces the optimized `target/release/noeta`. To put a source-built `noeta` on your `PATH`: `cargo install --path crates/noeta-cli`.

Everywhere below, `noeta` means "the installed binary."

## 2 · Hello, world

Create `hello.noe`:

```noeta
echo "hello"
```

Run it:

```console
$ noeta run hello.noe
hello
```

That is a complete program. There is **no `main` function** and no boilerplate — top-level statements run top to bottom. `echo` prints a value followed by a newline.

Semicolons are optional; a newline ends a statement. A `;` is still valid, and is what lets two statements share a line:

```noeta
echo "hello";
echo "one"; echo "two"
```

## 3 · A first real program

`echo` and a couple of bindings already give you something useful:

```noeta
name = "Ada"
greeting = "Hello, ${name}!"
echo greeting

mut total = 0
for n in 1..=5 {
    total += n
}
echo "sum 1..5 = ${total}"
```

```console
$ noeta run first.noe
Hello, Ada!
sum 1..5 = 15
```

Things to notice:

- `name = ...` binds an immutable variable; `mut total = ...` binds a mutable one.
- `"Hello, ${name}!"` is an **interpolated string** — `${expr}` embeds any expression.
- `1..=5` is an inclusive range; `for n in ...` iterates it.
- `total += n` is compound assignment (`total = total + n`).

The [Language Tour](Language-Tour) builds from here through functions, data modeling, pattern matching, collections, and error handling.

## 4 · The REPL

For quick experiments, start an interactive session:

```console
$ noeta repl
» 1 + 2 * 3
7
» name = "world"
» "hello ${name}"
hello world
» :quit
```

A bare expression prints its value. The REPL keeps your bindings alive between entries, and has a few meta-commands:

| Command | Effect |
|---|---|
| `:type <expr>` / `:t` | Evaluate an expression and print its runtime type. |
| `:bindings` / `:b` | List the live bindings. |
| `:drop <name>` / `:free` | Run a binding's destructor now and unbind it. |
| `:check on` / `:check off` | Toggle per-entry type-checking mid-session. |
| `:reset` | Clear all bindings. |
| `:help` / `:h` / `:?` | Show help. |
| `:quit` / `:q` (or Ctrl-D) | Exit. |

Multi-line input is detected automatically — an unclosed `{`, `(`, or `[` continues onto the next line (`… ` prompt).

## 5 · Start a real project

A single file is all `noeta run` needs. The moment you want dependencies, a test suite, or editor debugging, scaffold a package:

```console
$ noeta init hello
  created noeta.toml
  created src/main.noe
  created .gitignore
  created .vscode/launch.json
  created .vscode/extensions.json
  created AGENTS.md
  created SYNTAX.md
  created git repository
initialized Noeta package `local/hello` in hello
$ noeta test hello/src/main.noe
running 2 tests on 2 threads
  ok    greets
  ok    greets_noeta

2 passed, 0 failed, 2 total
```

The scaffold works before you edit a line:

- **`noeta.toml`** — the package identity, the `[dependencies]` table (add one with `noeta add`), and two build targets: `development` with the dev tiers live (`tiers = ["test", "bench", "doc", "debug"]` — a bare name turns a tier on, a `-name` turns one off) — the `@test`/`@bench`/`@doc`/`@debug` blocks that sit beside your code and are stripped from a production build (see [Dev Tiers](Dev-Tiers)) — and `production` as an explicit name for the tier-free baseline.
- **`src/main.noe`** — a small entry file that exercises all four tiers, so `run`, `test`, `bench`, and `doc` each have something to do immediately.
- **`.vscode/`, `AGENTS.md`, `SYNTAX.md`** — run/debug profiles for the [editor extension](Editor-and-AI-Tooling), and the docs an AI agent needs to drive the project.

`noeta init` never overwrites an existing file, so it is also safe to run in a directory that already has code. The full scaffold is documented at [The CLI](The-CLI#noeta-init); the manifest it writes on the [`noeta.toml` Manifest](Manifest) page.

> [!TIP]
> Those `.vscode/` files assume the Noeta VS Code extension — install it to get syntax highlighting, live diagnostics, hover types, and one-click debugging out of the scaffold. Setup instructions: [Editor & AI Tooling](Editor-and-AI-Tooling).

## 6 · The rest of the toolchain

The `noeta` binary is more than a runner. In brief:

| Command | What it does |
|---|---|
| `noeta init [dir]` | Scaffold a new project: manifest, `src/main.noe`, editor run profiles, agent docs. See [The CLI](The-CLI#noeta-init). |
| `noeta run <file>` | Type-check and execute a program. |
| `noeta check <file>` | Type-check without running or building — every diagnostic, no execution. See [The CLI](The-CLI#noeta-check). |
| `noeta build <file>` | Compile to a self-contained `.noeb` bundle, or a native executable / WebAssembly with `--native`/`--wasm`. See [The CLI](The-CLI#noeta-build). |
| `noeta repl` | Interactive REPL. |
| `noeta test <file>` | Run the program's `@test` blocks. See [Testing](Testing). |
| `noeta bench <file>` | Run and measure its `@bench` blocks. See [Benchmarking](Benchmarking). |
| `noeta doc <file>` | Extract its `@doc { … }` prose to stdout. See [Documentation](Documentation-and-Tiers). |
| `noeta add …` | Add a dependency to `noeta.toml` and resolve it. See [Using Packages](Using-Packages). |
| `noeta upgrade` | Self-update the toolchain to the latest release. See [The CLI](The-CLI#noeta-upgrade). |

Run `noeta <command> --help` for the flags of any command.

## Exit codes

`noeta run` exits `0` on success, `1` on a type error / runtime error (a program's own exit code passes through), and `2` if the file cannot be read.

## Where to go next

- **[Language Tour](Language-Tour)** — learn the whole language by example.
- **[Using Packages](Using-Packages)** — add your first dependency and run a project that uses it.
- **[The Type System](Type-System)** — how types, inference, unions, and `dyn` fit together.
- **[Diagnostics](Diagnostics)** — what any `E0xxx` the toolchain reports means.
- **[Built-ins](Standard-Library)** — strings, lists, maps, sets and iterators, available with no import.
- **[Standard library reference](Std)** — the `use std.{…}` modules: `math`, `json`, `fs`, and the rest.
- **[The `noeta.toml` Manifest](Manifest)** and **[Package Registries](Package-Registries)** — dependencies, build targets, and where packages come from.
