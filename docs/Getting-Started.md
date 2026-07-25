# Getting Started

This page gets you from a clean checkout to running your own program in a few minutes.

> [!NOTE]
> Noeta is pre-alpha and not yet published as a released binary. You build the toolchain from source with a recent stable Rust. The binary is named `noeta`; source files use the `.noe` extension.

## 1 · Build the toolchain

You need a recent stable Rust toolchain (1.95+). Then, from the repository root:

```sh
cargo build                 # builds the whole workspace, including the `noeta` binary
```

The binary lands at `target/debug/noeta`. For a fast optimized build use `cargo build --release` (→ `target/release/noeta`). To put `noeta` on your `PATH`:

```sh
cargo install --path crates/noeta-cli
```

Everywhere below, `noeta` means "the built binary." If you have not installed it, substitute `cargo run -p noeta-cli --` for `noeta` — e.g. `cargo run -p noeta-cli -- run hello.noe`.

## 2 · Hello, world

Create `hello.noe`:

```noeta
echo "hello";
```

Run it:

```console
$ noeta run hello.noe
hello
```

That is a complete program. There is **no `main` function** and no boilerplate — top-level statements run top to bottom. `echo` prints a value followed by a newline.

Semicolons are optional; a newline ends a statement. This is equally valid:

```noeta
echo "hello"
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

- **`noeta.toml`** — the package identity, the `[dependencies]` table (add one with `noeta add`), and two build targets: `development` with the dev tiers (`@test`, `@bench`, `@doc`, `@debug`) live, and `production` as an explicit name for the tier-free baseline.
- **`src/main.noe`** — a small entry file that exercises all four tiers, so `run`, `test`, `bench`, and `doc` each have something to do immediately.
- **`.vscode/`, `AGENTS.md`, `SYNTAX.md`** — run/debug profiles for the [editor extension](Editor-and-AI-Tooling), and the docs an AI agent needs to drive the project.

`noeta init` never overwrites an existing file, so it is also safe to run in a directory that already has code. The full scaffold is documented at [The CLI](The-CLI#noeta-init); the manifest it writes on the [`noeta.toml` Manifest](Manifest) page.

## 6 · The rest of the toolchain

The `noeta` binary is more than a runner. In brief:

| Command | What it does |
|---|---|
| `noeta init [dir]` | Scaffold a new project: manifest, `src/main.noe`, editor run profiles, agent docs. See [The CLI](The-CLI#noeta-init). |
| `noeta run <file>` | Type-check and execute a program. |
| `noeta repl` | Interactive REPL. |
| `noeta test <file>` | Run the program's `@test` blocks. See [Testing](Testing). |
| `noeta bench <file>` | Run and measure its `@bench` blocks. See [Benchmarking](Benchmarking). |
| `noeta doc <file>` | Extract its `@doc { … }` prose to stdout. See [Documentation & Dev Tiers](Documentation-and-Tiers). |

Run `noeta <command> --help` for the flags of any command.

## Exit codes

`noeta run` exits `0` on success, `1` on a type error / runtime error (a program's own exit code passes through), and `2` if the file cannot be read.

## Where to go next

- **[Language Tour](Language-Tour)** — learn the whole language by example.
- **[The Type System](Type-System)** — how types, inference, unions, and `dyn` fit together.
- **[Standard Library](Standard-Library)** — the built-in types and modules you will reach for.
- **[The `noeta.toml` Manifest](Manifest)** and **[Package Registries](Package-Registries)** — dependencies, build targets, and where packages come from.
