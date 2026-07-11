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

## 5 · The rest of the toolchain

The `noeta` binary is more than a runner. In brief:

| Command | What it does |
|---|---|
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
