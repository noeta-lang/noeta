# The `noeta` CLI

The `noeta` binary is the whole toolchain. Its main subcommands:

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
| [`noeta fmt`](#noeta-fmt) | Format `.noe` source into the canonical style (files/dirs, `--check`, `--stdin`). |

Run `noeta --help` or `noeta <command> --help` for the authoritative flag list.

> [!NOTE]
> **Observability.** There is no telemetry subcommand or flag — production tracing rides `noeta run`
> and the server, configured by the standard `OTEL_EXPORTER_OTLP_ENDPOINT` env var and off until you
> set it. See [Observability](Observability). (The dev-time flamegraph tool is [`noeta profile`](Profiling).)

> [!NOTE]
> The language-conformance/differential harness that developers use is a *separate* dev binary (`noeta-conformance`), deliberately kept out of the shipped CLI so the `test` verb stays free for your program's own tests. Editor tooling is on [Editor & AI Tooling](Editor-and-AI-Tooling); the debugger on [Debugging](Debugging).

## `noeta fmt`

Formats `.noe` source into the canonical style — the same layout no matter how the code was written (like `gofmt`/`rustfmt`). It is a **canonical reformatter** guarded by a safety check: the formatted output is re-parsed and compared to the original, so formatting can never change what a program means; if anything looks off, the file is left untouched.

```
noeta fmt [PATHS...]   # format files, or every .noe under a directory, in place (atomic)
noeta fmt --check ...  # write nothing; list any file that is not already formatted, exit 1 (CI)
noeta fmt --stdin      # read source on stdin, write the formatted result to stdout (format-on-save)
```

Style is read from a `[fmt]` table in the nearest `noeta.toml`, or built-in defaults:

```toml
[fmt]
wrap             = false      # false (default) keeps your line breaks; true = width-driven wrapping
line_width       = 100        # column budget, used only when wrap = true
match_arm_arrows = "compact"  # "compact" (default) or "align" (column-align match `=>`)
sort_imports     = false      # false (default); true alphabetizes each comment-free run of `use`
```

With `wrap = false` (the default) the formatter preserves the line breaks you wrote and only normalizes indentation, spacing, and blank lines — so a tidy file is left essentially as-is. Trailing `;` and comments are always preserved; when `wrap = true`, wrapped lists get a trailing comma. Editors format with the same engine: the VS Code extension turns on **format-on-save** and **format-on-type** (reformatting a block when you type its closing `}`) for `.noe` files by default.

---

## `noeta run`

```
noeta run [OPTIONS] <FILE> [-- <ARGS>...]
```

Loads, type-checks, and executes a `.noe` file on the **real host** — real `env`/`args`, real-disk IO, a per-isolate async runtime — using the bytecode VM. Any sibling `.noe` modules the entry file `use`s are resolved and merged automatically (see [Modules](Modules)).

**Options**

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Activate a dev-tier for this run, e.g. `--tier debug` compiles in `@debug { … }` blocks. Repeatable. Without it, every tier block is stripped. |
| `--profile <NAME>` | Activate the tiers a `noeta.toml` build profile makes live. Unioned with any `--tier`. |
| `--no-cache` | Bypass the [startup cache](#the-startup-cache) for this run — don't read a cached compile and don't write one. Same effect as `NOETA_NO_CACHE`. |

The active-tier set is the profile's live tiers ∪ any `--tier` flags, resolved *before* loading (a bad profile fails fast). With an empty active set — the default — every `@test`/`@bench`/`@doc`/`@debug` block strips away and the program runs as written. See [Documentation & Dev Tiers](Documentation-and-Tiers).

**Passing arguments to the program**

Everything after a `--` separator is passed straight through to the program, which reads it with `args.all()`:

```console
$ noeta run app.noe -- --verbose input.txt
```

The `--` protects hyphen-prefixed values from being parsed as `noeta`'s own options. `args.all()` reports the program path as the first element (argv[0]) followed by these arguments — the **same** vector the program sees when shipped as a `noeta build --exe` binary and invoked directly (`./app --verbose input.txt`), so no code changes between running from source and running as an executable.

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

## `noeta serve` and `--watch`

`noeta serve app.noe --port 8080` serves the file's top-level `fn fetch(req: Request): Response`
handler (see the `http.server` section of [Standard Library Modules](Standard-Library-Modules));
the app defines the handler and must **not** call `server.serve(...)` itself — the command runs
the file's top-level setup, then drives the handler on the given port.

`--watch` works on **any** command (`noeta run --watch`, `noeta test --watch`, …): a file watcher
restarts the command on change — with the startup cache, a restart is a few milliseconds.

`noeta serve --watch` upgrades from restarts to **in-process hot reload**. On each save of the
entry file the watcher parses, type-checks (**transactionally** — red code never swaps; the old
version keeps serving and the diagnostics go to the terminal *and* to connected LiveView clients
as an error overlay), diffs against the running version, and swaps the changed definitions into
the live server. The state rule is the language behavior to know:

- **Reactive state survives edits** — an unchanged `signal(...)`/`cell`/`synced_signal` binding
  keeps its value across the swap; effects are disposed and re-created by the new version.
- **Plain state re-initializes** — ordinary top-level bindings are re-run from the new source.
  State that must survive belongs in a signal.

Connected LiveView clients (the bundled `server.liveview_js()` shim) are told over their own
websocket: a landed swap pushes `{"type":"reload"}` and closes the socket — the page reloads and
its fresh session snapshots the *preserved* signal state, so the browser view carries the same
counter through the edit; a rejected edit pushes `{"type":"error",…}`, which the shim renders as
a full-screen diagnostics overlay, cleared by the next good frame. Swaps apply immediately even
when the server is idle (the watcher wakes the blocked executor).

Changes the live process cannot absorb — a type-layout or signature change, an edit to another
project file, a namespaced entry — fall back to a **full restart**, automatically, with the
reason printed (`[hot] restart needed: the layout of type \`P\` changed`). After a restart, an
open browser page reconnects and re-syncs state but keeps its old markup until refreshed (the
reload push needs a live server to send it).

**The retention model.** A hot-swapping process deliberately retains superseded artifacts for
soundness: old code versions (in-flight requests finish on the code they started on), replaced
reactive nodes an alias might still read, and — when the JIT is armed — retired native code that
a live frame may still return into. Growth is **bounded per swap** and reclaimed when the process
exits; an edit marathon costs memory proportional to the number of swaps, never correctness. The
dev server hot-swaps at tier 1 (the JIT re-warms after each swap); `--no-jit` is unnecessary.

---

## The startup cache

`noeta run`, `dump`, and `build` re-lex, parse, type-check, and compile the source on every invocation. For a large program that front-end work dominates startup (≈120 ms on a 6000-line file — around 95 % of wall time). So the toolchain **caches the compiled bytecode**: the first run of a file compiles and stores it; subsequent runs of unchanged sources load the stored bytecode and skip the whole front-end (a ~17× startup win on that same file). It is **on by default** and requires no build step — a plain `noeta run app.noe` populates and reuses it.

The cache is **transparent and safe**: a cached run is byte-identical to an uncached one (verified in the test suite). An entry is keyed by everything that can change the output — the entry file *and* every sibling module's content, the toolchain version, the running binary's build identity, and the active tier set — so any source edit, a rebuilt `noeta`, or a different `--tier`/profile transparently produces a fresh compile. `run`, `dump`, and `build` share entries (a `noeta build` warms the entry a later `noeta run` reads, and vice-versa). `serve`, `test`, and `bench` are not cached.

Cached artifacts live under `~/.cache/noeta/` (XDG: `$XDG_CACHE_HOME/noeta/`; macOS `~/Library/Caches/noeta`), a per-user private directory. If the cache can't be read or written for any reason, the run silently falls back to compiling from source — it is an optimization, never a dependency.

**Disable it**

| How | Scope |
|---|---|
| `noeta run --no-cache …` | This run only. |
| `NOETA_NO_CACHE=1` | Every command, for as long as it's set. |

**Environment**

| Variable | Effect |
|---|---|
| `NOETA_NO_CACHE` | If set (to anything), disables the cache entirely. |
| `NOETA_CACHE_DIR` | Override the cache directory (default `~/.cache/noeta/`). |
| `NOETA_CACHE_MAX_BYTES` | Cap on total cache size before oldest entries are evicted; default 256 MiB, `0` disables the cap. |

### `noeta cache`

```
noeta cache <path|info|clear>
```

Inspect or clear the startup cache.

| Subcommand | Effect |
|---|---|
| `noeta cache path` | Print the cache directory (whether or not it exists yet). |
| `noeta cache info` | Show the location, entry count, total size on disk, and the size cap. |
| `noeta cache clear` | Remove all cached compilations. |

```console
$ noeta cache info
/home/you/.cache/noeta
128 entries, 11.4 MiB on disk (cap 256.0 MiB)
$ noeta cache clear
removed 128 cached compilations from /home/you/.cache/noeta
```

The cache never grows without bound: once it exceeds `NOETA_CACHE_MAX_BYTES`, the oldest entries are evicted on the next compile (silently — inspect with `noeta cache info`).

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
