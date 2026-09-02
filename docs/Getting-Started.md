# Getting Started

> [!NOTE]
> Noeta is alpha software. The binary is named `noeta`; source files use the `.noe` extension. Prebuilt binaries cover **Linux and macOS only** (x86_64/aarch64); other platforms build from source, [below](#building-from-source).

## 1 · Install the toolchain

This downloads the latest [release](https://github.com/noeta-lang/noeta/releases) for your machine, verifies its checksum, and installs to `~/.local/bin`:

```sh
curl -fsSL https://noeta.dev/install | sh
```

`--version vX.Y.Z` pins a specific release, and `--to <dir>` (or `NOETA_INSTALL_DIR`) changes the destination. Later releases are one `noeta upgrade` away, so the installer is only needed once. See [The CLI](The-CLI#noeta-upgrade).

**macOS:** the release binaries are not Apple-notarized, so Gatekeeper may refuse to run a copy of `noeta` whose download it quarantined. That happens to a binary fetched with a browser rather than the installer, since `curl` does not set the quarantine attribute. If macOS reports "cannot be opened because the developer cannot be verified", clear the flag and re-run:

```sh
xattr -d com.apple.quarantine <path-to>/noeta
```

### Building from source

On any other platform (musl-only Linux, Windows, \*BSD), or to work on the toolchain itself, build from a checkout of [the repository](https://github.com/noeta-lang/noeta) with Rust 1.95 or later:

```sh
cargo build                 # builds the whole workspace, including the `noeta` binary
```

The binary lands at `target/debug/noeta`, and `cargo build --release` produces the optimized `target/release/noeta`. To put a source-built `noeta` on your `PATH`, run `cargo install --path crates/noeta-cli`.

Everywhere below, `noeta` means the installed binary.

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

That is a complete program. Top-level statements run top to bottom, so there is no `main` function and no boilerplate. `echo` prints a value followed by a newline.

Semicolons are optional, and a newline ends a statement. A `;` is still valid, and is what lets two statements share a line:

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

- `name = ...` binds an immutable variable, and `mut total = ...` binds a mutable one.
- `"Hello, ${name}!"` is an **interpolated string**, where `${expr}` embeds any expression.
- `1..=5` is an inclusive range, and `for n in ...` iterates it.
- `total += n` is compound assignment, for `total = total + n`.

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

A bare expression prints its value, and the REPL keeps your bindings alive between entries. The meta-commands:

| Command | Effect |
|---|---|
| `:type <expr>` / `:t` | Evaluate an expression and print its runtime type. |
| `:bindings` / `:b` | List the live bindings. |
| `:drop <name>` / `:free` | Run a binding's destructor now and unbind it. |
| `:check on` / `:check off` | Toggle per-entry type-checking mid-session. |
| `:reset` | Clear all bindings. |
| `:help` / `:h` / `:?` | Show help. |
| `:quit` / `:q` (or Ctrl-D) | Exit. |

Multi-line input is detected automatically, so Enter inside an unclosed `{`, `(`, or `[` continues the entry instead of submitting it.

At a terminal you also get a real line editor: history that persists across sessions, arrow keys, syntax coloring as you type, and TAB completion. Completion is drawn from the same engine that powers the editor integration, so `x.` offers the receiver's methods and a type you declared earlier completes by name. See [The CLI](The-CLI#noeta-repl).

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

- **`noeta.toml`** carries the package identity, the `[dependencies]` table (add one with `noeta add`), and two build targets. `development` runs with the dev tiers live, as `tiers = ["test", "bench", "doc", "debug"]`, where a bare name turns a tier on and a `-name` turns one off. `production` is an explicit name for the tier-free baseline. Those tiers are the `@test`/`@bench`/`@doc`/`@debug` blocks that sit beside your code and are stripped from a production build; see [Dev Tiers](Dev-Tiers).
- **`src/main.noe`** is a small entry file that exercises all four tiers, so `run`, `test`, `bench`, and `doc` each have something to do immediately.
- **`.vscode/`, `AGENTS.md`, `SYNTAX.md`** are run/debug profiles for the [editor extension](Editor-and-AI-Tooling), plus the docs an AI agent needs to drive the project. `SYNTAX.md` is a short reference on purpose; [`noeta docs`](The-CLI#noeta-docs) searches the full guide, which is embedded in the binary.

`noeta init` never overwrites an existing file, so it is safe to run in a directory that already has code, and safe to re-run inside a project it already scaffolded, where it fills in whatever is missing. Delete `SYNTAX.md` after a toolchain upgrade and re-run to regenerate it. The full scaffold is documented at [The CLI](The-CLI#noeta-init), and the manifest it writes on the [`noeta.toml` Manifest](Manifest) page.

> [!TIP]
> Those `.vscode/` files assume the Noeta VS Code extension. Install it for syntax highlighting, live diagnostics, hover types, and one-click debugging straight out of the scaffold: [Editor & AI Tooling](Editor-and-AI-Tooling).

## 6 · The rest of the toolchain

The `noeta` binary is more than a runner:

| Command | What it does |
|---|---|
| `noeta init [dir]` | Scaffold a new project: manifest, `src/main.noe`, editor run profiles, agent docs. See [The CLI](The-CLI#noeta-init). |
| `noeta run <file>` | Type-check and execute a program. |
| `noeta check [path]` | Type-check a file or directory without running or building. See [The CLI](The-CLI#noeta-check). |
| `noeta build <file>` | Compile to a self-contained `.noeb` bundle, or to a native executable or WebAssembly with `--native`/`--exe`/`--wasm`/`--serve`. See [The CLI](The-CLI#noeta-build). |
| `noeta repl` | Interactive REPL. |
| `noeta test <file>` | Run the program's `@test` blocks. See [Testing](Testing). |
| `noeta bench <file>` | Run and measure its `@bench` blocks. See [Benchmarking](Benchmarking). |
| `noeta doc <file>` | Extract its `@doc { … }` prose to stdout, or generate a documentation artifact with `--out`. See [Documentation](Documentation-and-Tiers). |
| `noeta add …` | Add a dependency to `noeta.toml` and resolve it. See [Using Packages](Using-Packages). |
| `noeta upgrade` | Self-update the toolchain to the latest release. See [The CLI](The-CLI#noeta-upgrade). |

Run `noeta <command> --help` for the flags of any command.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | The program ran and succeeded. |
| `1` | A type error or a runtime error. A program's own exit code passes through here. |
| `2` | Setup failure: the command could not get as far as running the program, because a file could not be read or the arguments did not fit the input. |

## Where to go next

- **[Language Tour](Language-Tour)** — learn the whole language by example.
- **[Using Packages](Using-Packages)** — add your first dependency and run a project that uses it.
- **[The Type System](Type-System)** — how types, inference, unions, and `dyn` fit together.
- **[Diagnostics](Diagnostics)** — what any `E0xxx` the toolchain reports means.
- **[Built-ins](Standard-Library)** — strings, lists, maps, sets and iterators, available with no import.
- **[Standard library reference](Std)** — the `use std.{…}` modules: `math`, `json`, `fs`, and the rest.
- **[The `noeta.toml` Manifest](Manifest)** and **[Package Registries](Package-Registries)** — dependencies, build targets, and where packages come from.
