# The `lang` CLI

The `lang` binary is the whole toolchain. It has five subcommands:

| Command | Purpose |
|---|---|
| [`lang run`](#lang-run) | Type-check and execute a program. |
| [`lang repl`](#lang-repl) | Interactive REPL. |
| [`lang test`](Testing) | Discover and run `@test` blocks. |
| [`lang bench`](Benchmarking) | Discover and measure `@bench` blocks. |
| [`lang doc`](Documentation-and-Tiers) | Extract `@doc { … }` prose to stdout. |

Run `lang --help` or `lang <command> --help` for the authoritative flag list.

> [!NOTE]
> There is intentionally no `build`, `fmt`, `check`, `lsp`, or `serve` subcommand yet. The language-conformance/differential harness that developers use is a *separate* dev binary (`lang-conformance`), deliberately kept out of the shipped CLI so the `test` verb stays free for your program's own tests. Editor and AI tooling status is on [Editor & AI Tooling](Editor-and-AI-Tooling).

---

## `lang run`

```
lang run [OPTIONS] <FILE>
```

Loads, type-checks, and executes a `.lang` file on the **real host** — real `env`/`args`, real-disk IO, a per-isolate async runtime — using the bytecode VM. Any sibling `.lang` modules the entry file `use`s are resolved and merged automatically (see [Modules](Modules)).

**Options**

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Activate a dev-tier for this run, e.g. `--tier debug` compiles in `@debug { … }` blocks. Repeatable. Without it, every tier block is stripped. |
| `--profile <NAME>` | Activate the tiers a `lang.toml` build profile makes live. Unioned with any `--tier`. |

The active-tier set is the profile's live tiers ∪ any `--tier` flags, resolved *before* loading (a bad profile fails fast). With an empty active set — the default — every `@test`/`@bench`/`@doc`/`@debug` block strips away and the program runs as written. See [Documentation & Dev Tiers](Documentation-and-Tiers).

**Exit codes**

| Code | Meaning |
|---|---|
| `0` | Success (a program's own `exit` code passes through, clamped to a byte). |
| `1` | Diagnostics, a tier-activation error, or a runtime error. |
| `2` | The file could not be read. |

**Example**

```console
$ lang run hello.lang
hello
```

---

## `lang repl`

```
lang repl
```

Starts an interactive session. The prompt is `» `; a continuation line (inside an unclosed delimiter) shows `… `. Multi-line input is detected by counting unclosed `(`/`[`/`{` across lexer tokens, so braces inside strings and `${…}` never miscount.

A bare expression with no trailing `;` is retried with a `;` appended so its value prints:

```console
» 1 + 2 * 3
7
» xs = [1, 2, 3]
» xs.reverse()
[3, 2, 1]
```

Bindings persist across entries (unlike a compiled program, where a value is destroyed at its last use — the REPL keeps top-level bindings alive so you can keep using them).

**Meta-commands** (these are outside the language grammar):

| Command | Aliases | Effect |
|---|---|---|
| `:type <expr>` | `:t` | Evaluate the expression and print its runtime type. |
| `:drop <name>` | `:free` | Run a binding's destructor now and unbind it. |
| `:bindings` | `:b` | List the live bindings. |
| `:reset` | | Clear all bindings. |
| `:help` | `:h`, `:?` | Show help. |
| `:quit` | `:q` | Exit (also Ctrl-D). |

---

## The tier subcommands

`lang test`, `lang bench`, and `lang doc` each operate on the dev-tier content co-located in a source file. They are documented on their own pages:

- **[Testing](Testing)** — `@test` blocks, `assert`, metadata attributes, isolation, and concurrency.
- **[Benchmarking](Benchmarking)** — `@bench` blocks and the timing method.
- **[Documentation & Dev Tiers](Documentation-and-Tiers)** — `@doc` extraction, the tier model, and `lang.toml` build profiles.

All three accept `--profile <NAME>`, which acts as a **gate**: if the named `lang.toml` profile does not make that tier live, the command prints a notice and no-ops with exit `0`. With no `--profile`, they always proceed.
