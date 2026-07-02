# Getting Started

This page gets you from a clean checkout to running your own program in a few minutes.

> [!NOTE]
> `lang` is pre-alpha and not yet published as a released binary. You build the toolchain from source with a recent stable Rust. The binary is named `lang`; source files use the `.lang` extension. The name is a working title.

## 1 · Build the toolchain

You need a recent stable Rust toolchain (1.95+). Then, from the repository root:

```sh
cargo build                 # builds the whole workspace, including the `lang` binary
```

The binary lands at `target/debug/lang`. For a fast optimized build use `cargo build --release` (→ `target/release/lang`). To put `lang` on your `PATH`:

```sh
cargo install --path crates/lang-cli
```

Everywhere below, `lang` means "the built binary." If you have not installed it, substitute `cargo run -p lang-cli --` for `lang` — e.g. `cargo run -p lang-cli -- run hello.lang`.

## 2 · Hello, world

Create `hello.lang`:

```lang
echo "hello";
```

Run it:

```console
$ lang run hello.lang
hello
```

That is a complete program. There is **no `main` function** and no boilerplate — top-level statements run top to bottom. `echo` prints a value followed by a newline.

Semicolons are optional; a newline ends a statement. This is equally valid:

```lang
echo "hello"
```

## 3 · A first real program

`echo` and a couple of bindings already give you something useful:

```lang
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
$ lang run first.lang
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
$ lang repl
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
| `:reset` | Clear all bindings. |
| `:help` / `:h` / `:?` | Show help. |
| `:quit` / `:q` (or Ctrl-D) | Exit. |

Multi-line input is detected automatically — an unclosed `{`, `(`, or `[` continues onto the next line (`… ` prompt).

## 5 · The rest of the toolchain

The `lang` binary is more than a runner. In brief:

| Command | What it does |
|---|---|
| `lang run <file>` | Type-check and execute a program. |
| `lang repl` | Interactive REPL. |
| `lang test <file>` | Run the program's `@test` blocks. See [Testing](Testing). |
| `lang bench <file>` | Run and measure its `@bench` blocks. See [Benchmarking](Benchmarking). |
| `lang doc <file>` | Extract its `@doc { … }` prose to stdout. See [Documentation & Dev Tiers](Documentation-and-Tiers). |

Run `lang <command> --help` for the flags of any command.

## Exit codes

`lang run` exits `0` on success, `1` on a type error / runtime error (a program's own exit code passes through), and `2` if the file cannot be read.

## Where to go next

- **[Language Tour](Language-Tour)** — learn the whole language by example.
- **[The Type System](Type-System)** — how types, inference, unions, and `dyn` fit together.
- **[Standard Library](Standard-Library)** — the built-in types and modules you will reach for.
