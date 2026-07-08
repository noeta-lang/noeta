# The `noeta` CLI

The `noeta` binary is the whole toolchain. It has eight subcommands:

| Command | Purpose |
|---|---|
| [`noeta run`](#noeta-run) | Type-check and execute a program. |
| [`noeta repl`](#noeta-repl) | Interactive REPL. |
| [`noeta dump`](#noeta-dump) | Disassemble a program to its VM bytecode (a debugging aid). |
| [`noeta test`](Testing) | Discover and run `@test` blocks. |
| [`noeta bench`](Benchmarking) | Discover and measure `@bench` blocks. |
| [`noeta doc`](Documentation-and-Tiers) | Extract `@doc { … }` prose to stdout. |
| [`noeta lsp`](Editor-and-AI-Tooling) | The language server, over stdio (started by your editor, not by hand). |
| [`noeta dap`](Debugging) | The debug adapter, over stdio (started by your editor's debug UI, not by hand). |
| [`noeta profile`](Profiling) | Profile a program — a hot-function table or a flamegraph. |

Run `noeta --help` or `noeta <command> --help` for the authoritative flag list.

> [!NOTE]
> There is intentionally no `build`, `fmt`, or `check` subcommand yet. The language-conformance/differential harness that developers use is a *separate* dev binary (`noeta-conformance`), deliberately kept out of the shipped CLI so the `test` verb stays free for your program's own tests. Editor tooling is on [Editor & AI Tooling](Editor-and-AI-Tooling); the debugger on [Debugging](Debugging).

---

## `noeta run`

```
noeta run [OPTIONS] <FILE>
```

Loads, type-checks, and executes a `.noe` file on the **real host** — real `env`/`args`, real-disk IO, a per-isolate async runtime — using the bytecode VM. Any sibling `.noe` modules the entry file `use`s are resolved and merged automatically (see [Modules](Modules)).

**Options**

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Activate a dev-tier for this run, e.g. `--tier debug` compiles in `@debug { … }` blocks. Repeatable. Without it, every tier block is stripped. |
| `--profile <NAME>` | Activate the tiers a `noeta.toml` build profile makes live. Unioned with any `--tier`. |

The active-tier set is the profile's live tiers ∪ any `--tier` flags, resolved *before* loading (a bad profile fails fast). With an empty active set — the default — every `@test`/`@bench`/`@doc`/`@debug` block strips away and the program runs as written. See [Documentation & Dev Tiers](Documentation-and-Tiers).

**Exit codes**

| Code | Meaning |
|---|---|
| `0` | Success (a program's own `exit` code passes through, clamped to a byte). |
| `1` | Diagnostics, a tier-activation error, or a runtime error. |
| `2` | The file could not be read. |

**Example**

```console
$ noeta run hello.noe
hello
```

---

## `noeta repl`

```
noeta repl [--no-check] [--load <FILE>]
```

Starts an interactive session. The prompt is `» `; a continuation line (inside an unclosed delimiter) shows `… `. Multi-line input is detected by counting unclosed `(`/`[`/`{` across lexer tokens, so braces inside strings and `${…}` never miscount.

`--load <FILE>` opens a **bootstrapped session**: the program runs to completion first — fully checked, imports resolved, output printed — and the prompt opens with everything it declared and bound live. This is the mechanism behind a framework "tinker" command: point it at your app's bootstrap script and explore the running app interactively, with entries type-checked against the app's real signatures. A bootstrap that fails to load, check, or run exits with its diagnostics instead of opening a broken prompt. (Isolates in a bootstrapped session run cooperatively.)

Entries **type-check before running**, against everything the session has accumulated: an entry with a type error prints its `E0xxx` diagnostics and is skipped — your bindings keep their values, and the skipped entry commits nothing. A fully-checked session also compiles entries with the checker's optimizations active (`type_of` full fidelity, packed lists), exactly like `noeta run`. `--no-check` (or `:check off` at the prompt) restores the permissive checkerless session where type errors surface at run time; note that once any entry runs unchecked, the session stays on conservative codegen even if checking is re-enabled (`:reset` earns it back).

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

## `noeta dump`

```
noeta dump [OPTIONS] <FILE>
```

Disassembles a program to the register-bytecode the VM executes and prints it to stdout — **a developer/debugging aid**, not part of the normal build/run flow. It runs the *same* front end and code generator as [`noeta run`](#noeta-run) (load → type-check → lower → compile), so what you see is exactly what would execute; a type error prints diagnostics and exits non-zero, just like `run`.

Use it to answer "how does this construct actually compile?" — which opcodes a loop or method call lowers to, whether an in-place/reuse fast path fired, or how names and constants are laid out. It is the first tool to reach for when working on codegen or interpreter performance.

**Options** — the same dev-tier activation as `run`:

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Disassemble with a dev-tier active (e.g. `--tier debug` compiles in `@debug { … }` blocks). Repeatable. |
| `--profile <NAME>` | Activate the tiers a `noeta.toml` build profile makes live. Unioned with any `--tier`. |

**Output.** The module's side tables first (shapes, packed schemas, method/destructor tables — only those that are non-empty), then `=== main ===` and each numbered function prototype (`=== proto N ===`). Each prototype lists its parameter/register counts, its constant pool, and its numbered instructions. The text is stable and human-readable — the same form the VM's disassembly snapshot tests assert — so it diffs cleanly across changes.

**Example.** The body of a recursive `fib` shows the two `LoadGlobal "fib"` that resolve the callee and the `Call` frames:

```console
$ noeta dump fib.noe
...
=== proto 1 ===
params: 1, registers: 4
constants:
  k0 = 2
  k1 = 1
  k2 = 2
code:
    0  LoadConst   r2 <- k0
    1  Binary      r1 <- r0 < r2
    2  RequireCondBool r1 (if)
    3  JumpIfFalse r1 -> 5
    4  Return      r0
    5  LoadConst   r2 <- k1
    6  Binary      r1 <- r0 - r2
    7  LoadGlobal  r3 <- "fib"
    8  Call        r2 <- r3(r1)
    9  LoadConst   r3 <- k2
   10  Binary      r1 <- r0 - r3
   11  Drop        r0
   12  LoadGlobal  r3 <- "fib"
   13  Call        r0 <- r3(r1)
   14  Binary      r1 <- r2 + r0
   15  Return      r1
   16  Halt
```

The opcode set and prototype/side-table layout are described in [The Virtual Machine](The-Virtual-Machine).

---

## The tier subcommands

`noeta test`, `noeta bench`, and `noeta doc` each operate on the dev-tier content co-located in a source file. They are documented on their own pages:

- **[Testing](Testing)** — `@test` blocks, `assert`, metadata attributes, isolation, and concurrency.
- **[Benchmarking](Benchmarking)** — `@bench` blocks and the timing method.
- **[Documentation & Dev Tiers](Documentation-and-Tiers)** — `@doc` extraction, the tier model, and `noeta.toml` build profiles.

All three accept `--profile <NAME>`, which acts as a **gate**: if the named `noeta.toml` profile does not make that tier live, the command prints a notice and no-ops with exit `0`. With no `--profile`, they always proceed.
