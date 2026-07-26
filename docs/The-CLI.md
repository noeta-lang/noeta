# The `noeta` CLI

The `noeta` binary is the whole toolchain. Its main subcommands:

| Command | Purpose |
|---|---|
| [`noeta init`](#noeta-init) | Scaffold a new project — manifest, entry file, editor profiles, agent docs. |
| [`noeta run`](#noeta-run) | Type-check and execute a program. |
| [`noeta build`](#noeta-build) | Compile to a standalone artifact (`--exe`, `--native` for machine code, `--wasm`/`--serve` for [WebAssembly](WebAssembly-and-the-Edge)). |
| [`noeta check`](#noeta-check) | Parse and type-check without running or building (exit 0/1/2). |
| [`noeta expand`](#noeta-expand) | Print the source that compile-time `@`-directive expansions generated. |
| [`noeta serve`](#noeta-serve) | Run a program's HTTP handler as a server (`fn fetch(req: Request): Response`). |
| [`noeta repl`](#noeta-repl) | Interactive REPL. |
| [`noeta dump`](#noeta-dump) | Disassemble a program to its VM bytecode (a debugging aid). |
| [`noeta test`](Testing) | Discover and run `@test` blocks. |
| [`noeta bench`](Benchmarking) | Discover and measure `@bench` blocks. |
| [`noeta doc`](Documentation-and-Tiers) | Extract `@doc { … }` prose to stdout. |
| [`noeta lsp`](Editor-and-AI-Tooling) | The language server, over stdio (started by your editor, not by hand). |
| [`noeta dap`](Debugging) | The debug adapter, over stdio (started by your editor's debug UI, not by hand). |
| [`noeta mcp`](Editor-and-AI-Tooling) | The agent-native MCP server, over stdio (for AI tooling; see [Editor & AI Tooling](Editor-and-AI-Tooling)). |
| [`noeta profile`](Profiling) | Profile a program — a hot-function table or a flamegraph. |
| [`noeta fmt`](#noeta-fmt) | Format `.noe` source into the canonical style (files/dirs, `--check`, `--stdin`). |
| [`noeta add`](#noeta-add) | Add a dependency to `noeta.toml` and resolve it into `noeta.lock`. |
| [`noeta update`](#noeta-update) | Re-resolve the dependency graph, re-pinning moved refs and refreshed versions. |
| [`noeta claim`](#noeta-claim) | Claim a registry scope self-service — prove your GitHub identity (or a domain) and get the publish token. |
| [`noeta publish`](#noeta-publish) | Publish a tagged release of your package to the registry, signed ([provenance](Package-Provenance)). |
| [`noeta scope`](#noeta-scope) | Manage a scope you own — e.g. require verified provenance on every release. |
| [`noeta audit`](#noeta-audit) | Report the dependency tree's trust footprint — native/command grants, pinned provenance, and advisory hits by tier. |
| [`noeta advisory`](#noeta-advisory) | Issue a publisher advisory for a scope you own, file a public report, or list/promote the report queue. |
| [`noeta watch-scope`](#noeta-watch-scope) | Monitor a scope's advisory transparency log for silent suppression or rewrite. |
| [`noeta key`](#noeta-key) | Manage the Ed25519 signing key (the key-based provenance path). |
| [`noeta upgrade`](#noeta-upgrade) | Self-update the toolchain binary to the latest release. |
| [`noeta ide`](#noeta-ide) | Install the matching editor extension — the VS Code/VSCodium `.vsix` at this binary's version. |

Run `noeta --help` or `noeta <command> --help` for the authoritative flag list.

> [!NOTE]
> **Observability.** There is no telemetry subcommand or flag — production tracing rides `noeta run`
> and the server, configured by the standard `OTEL_EXPORTER_OTLP_ENDPOINT` env var and off until you
> set it. See [Observability](Observability). (The dev-time flamegraph tool is [`noeta profile`](Profiling).)

> [!NOTE]
> The language-conformance/differential harness that developers use is a *separate* dev binary (`noeta-conformance`), deliberately kept out of the shipped CLI so the `test` verb stays free for your program's own tests. Editor tooling is on [Editor & AI Tooling](Editor-and-AI-Tooling); the debugger on [Debugging](Debugging).

## `noeta init`

Scaffolds a new project, ready to run before you edit a line:

```text
noeta init [PATH]            # default: the current directory (created if missing)
      --name company/package # default: local/<directory-name>
      --no-git               # skip `git init`
```

What it writes — never overwriting a file that already exists, so it is safe in a non-empty directory:

- **`noeta.toml`** — package identity plus two build targets: `development` wires the four std dev tiers (`@test`, `@bench`, `@doc`, `@debug`) live, and `production` is an explicit name for the tier-free baseline (see [build targets](Documentation-and-Tiers#build-targets--noetatoml)).
- **`src/main.noe`** — a fmt-canonical entry file exercising every tier: a documented function with a `@debug` trace, a two-case `@test` block, and a `@bench`.
- **`.vscode/`** — the run/debug profiles the [Noeta extension](Editor-and-AI-Tooling) picks up (F5 debugging over `noeta dap`), plus the extension recommendation.
- **`.gitignore`** — build/profiler artifacts ignored; `noeta.lock` deliberately not (commit it).
- **`AGENTS.md`** — how an AI agent should drive this project: the CLI feedback loop and the [`noeta mcp`](Editor-and-AI-Tooling) tool surface.
- **`SYNTAX.md`** — the full language reference, assembled from the same embedded guide `noeta mcp`'s `docs_search` serves, so it always matches the installed compiler. Delete and re-run `noeta init` after upgrading to refresh it.

A fresh directory also gets `git init` (skipped inside an existing repository, or with `--no-git`). A directory that already holds a `noeta.toml` is refused.

```console
$ noeta init webapp
  created noeta.toml
  created src/main.noe
  created .gitignore
  created .vscode/launch.json
  created .vscode/extensions.json
  created AGENTS.md
  created SYNTAX.md
  created git repository
initialized Noeta package `local/webapp` in webapp
$ noeta test webapp/src/main.noe
running 2 tests on 2 threads
  ok    greets
  ok    greets_noeta

2 passed, 0 failed, 2 total
```

## `noeta fmt`

Formats `.noe` source into the canonical style — the same layout no matter how the code was written (like `gofmt`/`rustfmt`). It is a **canonical reformatter** guarded by a safety check: the formatted output is re-parsed and compared to the original, so formatting can never change what a program means; if anything looks off, the file is left untouched.

```text
noeta fmt [PATHS...]   # format files, or every .noe under a directory, in place (atomic)
noeta fmt --check ...  # write nothing; list any file that is not already formatted, exit 1 (CI)
noeta fmt --diff  ...  # write nothing; print a unified diff of the pending reformat, exit 1 if any (CI)
noeta fmt --stdin      # read source on stdin, write the formatted result to stdout (format-on-save)
noeta fmt --parens <remove|add> ...      # override the [fmt] parens policy for redundant header parens
noeta fmt --semicolons <remove|add|preserve> ...   # override the [fmt] semicolons policy
```

Style is read from a `[fmt]` table in the nearest `noeta.toml`, or built-in defaults:

```toml
[fmt]
wrap             = false      # false (default) keeps your line breaks; true = width-driven wrapping
line_width       = 100        # column budget, used only when wrap = true
match_arm_arrows = "compact"  # "compact" (default) or "align" (column-align match `=>`)
sort_imports     = false      # false (default); true alphabetizes each comment-free run of `use`
parens           = "remove"   # "remove" (default) strips redundant parens around if/while headers; "add" inserts them
semicolons       = "remove"   # "remove" (default) strips redundant statement terminators; "add" or "preserve"
```

With `wrap = false` (the default) the formatter preserves the line breaks you wrote and only normalizes indentation, spacing, and blank lines — so a tidy file is left essentially as-is. Trailing `;` and comments are always preserved. With `wrap = true` the layout is re-derived from `line_width`: over-width **collections**, **argument/parameter lists**, **pipelines** (`|>`), **binary chains** (`a + b + c …`), **method chains** (`a.b().c() …`), and **union types** (`A | B | C`) each break one element per line (wrapped lists get a trailing comma), while anything that fits stays on one line. Editors format with the same engine: the VS Code extension turns on **format-on-save** and **format-on-type** (reformatting a block when you type its closing `}`) for `.noe` files by default.

To keep a region exactly as written — a hand-aligned table, a generated block — wrap it in `// fmt: off` / `// fmt: on` markers (on their own lines); everything between them passes through byte-for-byte, and formatting resumes after `// fmt: on` (an unmatched `// fmt: off` disables formatting to the end of its scope).

```noeta
// fmt: off
matrix = [1, 0, 0,
          0, 1, 0,
          0, 0, 1]
// fmt: on
```

---

## `noeta run`

```text
noeta run [OPTIONS] <FILE> [-- <ARGS>...]
```

Loads, type-checks, and executes a `.noe` file on the **real host** — real `env`/`args`, real-disk IO, a per-isolate async runtime — using the bytecode VM. Any sibling `.noe` modules the entry file `use`s are resolved and merged automatically (see [Modules](Modules)).

**Options**

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Activate a dev-tier for this run, e.g. `--tier debug` compiles in `@debug { … }` blocks. Repeatable. Without it, every tier block is stripped. |
| `--target <NAME>` | Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`. |
| `--no-cache` | Bypass the [startup cache](#the-startup-cache) for this run — don't read a cached compile and don't write one. Same effect as `NOETA_NO_CACHE`. |
| `--jit-stats` | After the run, print the Tier-1 JIT compile-coverage summary, a bail-reason histogram, and a declined-loop report to stderr. |

The active-tier set is the target’s live tiers ∪ any `--tier` flags, resolved *before* loading (a bad target fails fast). With an empty active set — the default — every `@test`/`@bench`/`@doc`/`@debug` block strips away and the program runs as written. See [Documentation & Dev Tiers](Documentation-and-Tiers).

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

## `noeta build`

```text
noeta build [OPTIONS] <FILE>
```

Compiles a program to a standalone artifact instead of running it. It shares the same front end (and [startup cache](#the-startup-cache)) as `noeta run`.

| Flag | Effect |
|---|---|
| `--out <PATH>` | Where to write the artifact. |
| `--exe` | Emit a self-contained executable that bundles the bytecode with the runtime, launchable directly. |
| `--native` | Emit an ahead-of-time-compiled **machine-code** binary (via the AOT backend), with dead-code elimination stripping unused stdlib rings. |
| `--wasm` | Emit a single **WebAssembly** module (`wasm32-wasip1`) that runs under any WASI runtime: `wasmtime run app.wasm`. See [WebAssembly & the Edge](WebAssembly-and-the-Edge). |
| `--serve` | Emit a **`wasi:http` serve component** (`wasm32-wasip2`): your `server.serve` handler on `wasmtime serve`, Spin, and Spin-class edge clouds. See [WebAssembly & the Edge](WebAssembly-and-the-Edge). |

All executable forms see the same `args.all()` vector as `noeta run` (argv[0] = program path), so no code changes between running from source and shipping a binary.

### Shipped artifacts are lean by construction

A `noeta build --exe`/`--native` artifact bundles the **runtime**, not the toolchain. It carries the VM, the standard library, and any of your dependencies' **runtime** capabilities (a native tier's handler, a native module) — and **nothing else**: no formatter, no LSP, no debug adapter, no formatter parsers. That is a deliberate security boundary, not just a size win: every parser or protocol server linked into a production binary is reachable attack surface, so a shipped app must not carry the dev toolchain it never runs.

This is structural. `--exe` staples your program onto the lean **`noeta-runner`** (or, when your app depends on packages with native runtime code, onto a *composed* runner that adds exactly those extensions — still no dev tooling); `--native` links a fresh binary from the AOT runtime. A stapled artifact's argument vector belongs to your program — it never exposes a CLI subcommand — so the toolchain would only ever have been dead weight there.

**Running a source tree in production** (PHP/Python/Ruby-style — deploy the `.noe` sources and point a runtime at the entry file) is a first-class deployment mode with the same guarantee: run it with `noeta-runner app.noe`, the same lean runtime, which compiles on the fly and links no dev tooling. (`noeta run` is the dev-workstation entry point and does carry the full toolchain; for production source deploys, ship `noeta-runner`.)

Which of your dependencies' code is present is governed by [`noeta.toml` targets](Documentation-and-Tiers) — build the default (safe, minimal) target, or `--target <name>` to layer in more. Package authors keep dev-only capabilities (like a tier-body formatter) out of your shipped binary automatically; see *shipping dev capabilities* in [Native Extensions](Native-Extensions).

## `noeta check`

```text
noeta check [PATH]
```

Parses and type-checks without running or building — the CI/pre-commit gate (the `cargo check` / `tsc --noEmit` primitive). `PATH` defaults to the current directory, walked recursively for `.noe` files (resolving and deduping shared modules); a single file checks just that file with its sibling modules linked in. `--format json` emits a single machine-readable report on stdout for CI/editors/the MCP server; the default renders diagnostics for a terminal. Exits non-zero if any error-severity diagnostic is found (warnings print but do not fail).

## `noeta expand`

```text
noeta expand [PATH]
```

Prints the Noeta source that **compile-time `@`-directive expansions** produced — the members an extension's `expand` hook generated and the linker spliced into the declaration it decorates (see *directives that generate code* in [Native Extensions](Native-Extensions)). An expansion is code you never wrote; without this, the only way to see any of it is to make it fail.

`PATH` resolves exactly as [`noeta check`](#noeta-check)'s does, and the link is the same one — so what prints is what the compiler saw, never a second rendering of it. Each expansion is printed under a Noeta comment naming its cause (the declaration it grew and the directive that grew it, with its arguments):

```text
$ noeta expand petstore.noe
// PetStore ⟨@openapi "petstore.yaml"⟩
struct PetStore {
fn list_pets(): List<Pet> { … }
fn get_pet(id: int): Pet { … }
}

expanded 1 declaration
```

The generated source goes to stdout and the summary to stderr, so `noeta expand > expanded.noe` yields a file to check in and diff — a change to the spec then shows up in review as a delta in the code it generates, instead of silently changing what the program means. A program with no expanding directive prints `no directive expansions` and exits 0.

Exit codes match `check`: **0** on success, **1** if any expansion failed (a hook that returned `Err`, or generated code that does not parse — reported as **E0062**, blamed on the directive) or the sources otherwise failed to load, **2** if a file could not be read at all.

## `noeta migrate`

```text
noeta migrate [--db <dsn>] [--dir <path>] [--seeds-dir <path>] [--status] [--dry-run] [--seed] [--reset [--yes]]
noeta migrate new <name> [--seed] [--dir <path>]
noeta migrate seed [--db <dsn>] [--dir <path>] [--seeds-dir <path>]
```

Applies a project's **plain-SQL migrations**. `migrate` is **not a core verb** — it is an
extension-contributed command the **para/db package** provides (the `cargo clippy` model): it is
available in a project whose manifest depends on `para/db` and trusts its commands
(`[trust] commands = ["para/db"]`), dispatched through the app's composed toolchain like any other
extension subcommand. Migrations are `.sql` files in a
`migrations/` directory, applied in filename-sort order; each runs inside its own transaction, so a
failure rolls that migration back and stops, naming the file. An applied migration is tracked in a
`_noeta_migrations` table by a sha256 checksum of its contents — editing an already-applied file, or
deleting one, is a hard error (history is immutable). Migrations are **forward-only** (no down files):
`--reset` (plus `--seed`) covers the development loop.

| invocation | effect |
| --- | --- |
| `noeta migrate` | apply every pending migration, printing each applied file |
| `noeta migrate --status` | list which migrations are applied and which are pending |
| `noeta migrate --dry-run` | list what would be applied, without touching the database |
| `noeta migrate new <name>` | scaffold `migrations/<UTC-timestamp>_<name>.sql` |
| `noeta migrate new --seed <name>` | scaffold `seeds/<UTC-timestamp>_<name>.sql` |
| `noeta migrate --seed` | apply pending migrations, then run the seed files |
| `noeta migrate seed` | run the seed files ONLY (errors if any migration is pending) |
| `noeta migrate --reset --yes` | DESTRUCTIVE: drop the schema and re-apply from zero |
| `noeta migrate --reset --seed --yes` | the full dev loop: reset → migrate → seed |

**Seeds** are re-runnable development data — plain `.sql` files in a `seeds/` directory, ordered like
migrations and applied *after* them. Unlike migrations they are **never tracked**: they run in
filename order, each in its own transaction, **every time** they are invoked, so idempotency is the
seed author's job (`INSERT OR IGNORE` on SQLite / `... ON CONFLICT DO NOTHING` on Postgres makes a
re-run a no-op). `noeta migrate seed` runs seeds only and refuses when any migration is still pending
(seeding a stale schema is a footgun).

The **connection string** is resolved, highest priority first: the `--db <dsn>` flag, the
`DATABASE_URL` environment variable, then a `[db]` table in `noeta.toml`:

```toml
[db]
url = "sqlite:app.db"        # any dsn scheme db.connect accepts (sqlite:… / postgres://…)
migrations = "migrations"    # optional; the migrations directory (default "migrations")
seeds = "seeds"              # optional; the seeds directory (default "seeds")
```

`--dir` / `--seeds-dir` override the migrations / seeds directory per-invocation. `--reset` drops and
recreates the schema — on SQLite it drops every user table/view/trigger; on PostgreSQL it runs `DROP
SCHEMA public CASCADE; CREATE SCHEMA public` — and refuses to run without `--yes` or an interactive
`yes` typed at the prompt.

Exit codes: `0` success; `2` for a usage/config problem (no dsn configured, a missing migrations or
seeds directory, an unconfirmed `--reset`); `1` for a run that failed (connect failure, a SQL error,
checksum drift, a deleted migration, or seeding while migrations are pending). An app can migrate and
seed itself at boot off the same engine with `conn.migrate("migrations")` then `conn.seed("seeds")`
(see the [para-db repo](https://github.com/noeta-lang/para-db)).

## `noeta serve` and `--watch`

`noeta serve app.noe --port 8080` serves the file's top-level `fn fetch(req: Request): Response`
handler (see the `http.server` section of [Standard Library Modules](Standard-Library-Modules));
the app defines the handler and must **not** call `server.serve(...)` itself — the command runs
the file's top-level setup, then drives the handler on the given port. `--host` sets the bind
address (default `0.0.0.0`, all interfaces; pass `--host 127.0.0.1` for local-only). **Ctrl-C**
drains gracefully: the server stops accepting, finishes the requests already in flight, closes
the listener, and exits — a second Ctrl-C forces an immediate stop.

`--parallel N` serves across **N worker isolates** for true multi-core throughput: the listener
is bound once and each worker inherits a cloned handle to it, so the kernel load-balances
connections across cores (no `SO_REUSEPORT`, no extra dependency). All workers share the process
and drain together on Ctrl-C. `--parallel --watch` hot-reloads across the whole fleet — a swap
**broadcasts** to every worker's live session, so all cores serve the new code without a restart.
(Reactive/LiveView state is per-worker: signals and WebSocket subscribers live in the worker that
handled the connection, so a LiveView app still runs best single-worker — the sticky-routing
question is a separate follow-on.)

`--watch` works on **any** command (`noeta run --watch`, `noeta test --watch`, …): a file watcher
restarts the command on change — with the startup cache, a restart is a few milliseconds.

For the tier runners the watch is **impact-filtered**: `noeta test --watch` (and `bench`) diffs
each save against the previous run, walks the reverse call graph from the changed definitions,
and reruns only the impacted tier fns (via the runners' repeatable `--name` filter) — edit a leaf
function and exactly its caller-tests rerun; an inert edit (formatting between declarations, a
comment) runs nothing. The filter is **project-wide**: the watcher holds an incremental (salsa)
workspace over the entry's directory, so editing an *imported module* narrows to the entry tests
that transitively reach the change (in the linked program's qualified names — `App.Lib.add`),
and editing a module function nothing imports reruns nothing at all. Edits the engine cannot
attribute — a signature/layout change, a changed top-level statement (fixtures live there), a
new or deleted module, a manifest change, red code — degrade to a full rerun *with the reason
printed*. The closure is static, so code reached only through dynamic dispatch is matched
best-effort (method calls on untyped receivers over-approximate by name); run without the filter
occasionally if you lean on reflection-driven dispatch.

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

The same unchanged `fetch` program also deploys to the edge as a `wasi:http` component — see
[WebAssembly & the Edge](WebAssembly-and-the-Edge).

---

## The startup cache

`noeta run`, `dump`, and `build` re-lex, parse, type-check, and compile the source on every invocation. For a large program that front-end work dominates startup (≈120 ms on a 6000-line file — around 95 % of wall time). So the toolchain **caches the compiled bytecode**: the first run of a file compiles and stores it; subsequent runs of unchanged sources load the stored bytecode and skip the whole front-end (a ~17× startup win on that same file). It is **on by default** and requires no build step — a plain `noeta run app.noe` populates and reuses it.

The cache is **transparent and safe**: a cached run is byte-identical to an uncached one (verified in the test suite). An entry is keyed by everything that can change the output — the entry file *and* every sibling module's content, the toolchain version, the running binary's build identity, and the active tier set — so any source edit, a rebuilt `noeta`, or a different `--tier`/`--target` transparently produces a fresh compile. `run`, `dump`, and `build` share entries (a `noeta build` warms the entry a later `noeta run` reads, and vice-versa). `serve`, `test`, and `bench` are not cached.

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

```text
noeta cache [ls|path|info|clean [--all]|clear]
```

Inspect or clean the whole per-user cache. Beyond the `*.noeb` startup-cache entries, the cache root holds two more categories: **composed toolchains** (`compose/` — an app with native dependencies gets its own built toolchain, easily 1–2 GiB each) and **fetched package sources** (`pkg/`). Everything in it is re-derivable, so deleting any of it is always safe — the next run recompiles, recomposes, or refetches what it needs.

| Subcommand | Effect |
|---|---|
| `noeta cache` / `noeta cache ls` | Per-category summary (entry counts + sizes); compose entries — the multi-GiB ones — are listed individually with size and last-used time. |
| `noeta cache clean` | Remove the composed toolchains other toolchain builds left behind (stale versions this binary can never reuse), reporting the bytes reclaimed; this binary's own compositions are kept. |
| `noeta cache clean --all` | Wipe the whole cache: all composed toolchains, fetched package sources, and cached compilations. |
| `noeta cache path` | Print the cache directory (whether or not it exists yet). |
| `noeta cache info` | Show the startup-cache location, entry count, total size on disk, and the size cap. |
| `noeta cache clear` | Remove all cached compilations (the `*.noeb` entries only). |

```console
$ noeta cache
/home/you/.cache/noeta
  bytecode    128 entries    11.4 MiB   cached compilations (*.noeb)
  compose       2 entries     3.4 GiB   composed toolchains
  pkg          41 entries    58.3 MiB   fetched package sources
  total                       3.5 GiB

compose entries (most recently used first):
  49069baa993a0e4d…    2.0 GiB   last used just now
  6bffebc6bf79e72a…    1.4 GiB   last used 12 days ago
(`noeta cache clean` removes the entries stale toolchain builds left behind)
$ noeta cache clean
removed 1 stale composed toolchain (1.4 GiB reclaimed); kept 1 current
```

Composed toolchains are keyed on (among other things) the running binary's build identity, so every toolchain rebuild or [`noeta upgrade`](#noeta-upgrade) strands the previous build's compositions — that is what `clean` reclaims. Only the three cache categories are ever touched; anything else living under the cache root (bench baselines, watch state) is left alone.

The startup cache never grows without bound: once it exceeds `NOETA_CACHE_MAX_BYTES`, the oldest entries are evicted on the next compile (silently — inspect with `noeta cache info`).

---

## `noeta repl`

```text
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
| `:check on\|off` | | Toggle per-entry type-checking mid-session (see `--no-check` above). |
| `:reset` | | Clear all bindings. |
| `:help` | `:h`, `:?` | Show help. |
| `:quit` | `:q` | Exit (also Ctrl-D). |

---

## `noeta dump`

```text
noeta dump [OPTIONS] <FILE>
```

Disassembles a program to the register-bytecode the VM executes and prints it to stdout — **a developer/debugging aid**, not part of the normal build/run flow. It runs the *same* front end and code generator as [`noeta run`](#noeta-run) (load → type-check → lower → compile), so what you see is exactly what would execute; a type error prints diagnostics and exits non-zero, just like `run`.

Use it to answer "how does this construct actually compile?" — which opcodes a loop or method call lowers to, whether an in-place/reuse fast path fired, or how names and constants are laid out. It is the first tool to reach for when working on codegen or interpreter performance.

**Options** — the same dev-tier activation as `run`:

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Disassemble with a dev-tier active (e.g. `--tier debug` compiles in `@debug { … }` blocks). Repeatable. |
| `--target <NAME>` | Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`. |

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
- **[Documentation & Dev Tiers](Documentation-and-Tiers)** — `@doc` extraction, the tier model, and `noeta.toml` build targets.

A program (or a dependency) can also declare its **own** tier with `@tier` — `noeta <tier> <FILE>` dispatches to the declaring package's runner; see [Documentation & Dev Tiers](Documentation-and-Tiers).

All three accept `--target <NAME>`, which acts as a **gate**: if the named `noeta.toml` target does not make that tier live, the command prints a notice and no-ops with exit `0`. With no `--target`, they always proceed.

---

## Packages: `add`, `update`, `claim`, `publish`, `scope`, `audit`, `key`

Dependencies are declared in `noeta.toml` (`[dependencies]`, with elevated grants in `[trust]`) and resolve automatically on `run`/`build`/`check` — there is no separate install step; the resolved pins live in `noeta.lock` (commit it). These verbs are the *publisher/consumer trust* surface. The trust model behind them — attestations, the two signing roots, pinning, downgrade protection — is documented on [Package Provenance](Package-Provenance); the end-to-end submission walkthrough on [Package Registries](Package-Registries#publishing-to-the-hosted-registry).

### The manifest: `[package]` and dependency forms

The essentials are below; [The `noeta.toml` Manifest](Manifest) is the complete reference for every
table and key.

```toml
[package]
name = "acme/app"        # the global identity `company/package` the registry indexes
version = "0.1.0"        # SemVer
edition = "2026"         # optional — the language edition this package is written against
license = "MIT OR Apache-2.0"      # optional — declared SPDX expression, recorded with the release
keywords = ["image", "simd"]       # optional — up to 5 discovery tags the registry indexes by
description = "Fast image effects" # optional — one-line blurb shown in package search

[dependencies]
# A local source tree — no network, no resolver:
util  = { path = "../util" }
# A git dependency, pinned to a released tag (the reproducible default):
http  = { git = "https://github.com/acme/http", tag = "v1.2.0" }
# A git dependency tracking a branch's tip (re-resolved by `noeta update`):
gfx   = { git = "https://github.com/acme/gfx", branch = "main" }
# A git dependency tracking the default branch's HEAD — no tag or branch needed,
# handy for an in-development or bundled package not yet cut into tagged releases:
draft = { git = "https://github.com/acme/draft" }
# A registry dependency by SemVer requirement (`package` is the registry identity):
json  = { version = "^1.2", package = "acme/json" }
# A scope dependency — several packages sharing one scope, bound under one key:
para  = [
  { version = "^0.1", package = "para/aether" },
  { version = "^0.1", package = "para/db" },
]
```

The **dependency key** (`util`, `http`, …) is the import root you address the package by — `use util.…` — decoupled from its global `company/package` identity (like Rust's `foo = { package = "real-name" }`). A **git** source resolves its ref (`tag`/`branch`/HEAD) to a commit SHA at the remote and records that SHA in `noeta.lock`, so **every** form is reproducible regardless of ref kind — a plain build fetches by the pinned SHA (offline once cached), and only `noeta update` re-resolves a moving `branch`/HEAD ref to its new tip. `tag` and `branch` are mutually exclusive. A **scope** dependency (an array value) binds several packages that share one `company` scope under a single key, so an app can depend on `para/aether` *and* `para/db` without colliding TOML keys; each member is any non-scope source, and all members must share one scope (see [the Manifest](Manifest) for the full rules). A **published** package (`noeta publish`) may depend only via the registry — a `path`/`git` dependency is rejected at publish time, since a consumer couldn't resolve it.

Developing a dependency *against* this app? A root-manifest [`[patch]` table](Manifest#patch--dev-time-path-overrides) re-points a package identity at a local tree for the whole graph — no `[dependencies]` edits, no lock churn, loud on every resolve.

**Editions** pin the language/ABI semantics a package is written against, so a package can evolve on its own cadence and a newer toolchain still compiles it under *its* edition. `edition` is validated against the editions this toolchain understands (an unknown one is a manifest error); omitting it uses the current edition. Each package's edition is recorded in `noeta.lock`, and the toolchain keys its compiled-bytecode cache on it — so switching a package's edition never serves a stale artifact.

### `noeta add`

```text
noeta add [KEY] (--path <DIR> | --git <URL> --tag <TAG> | --version <REQ> [--package <company/pkg>])
```

Adds a dependency to the nearest `noeta.toml` (a format-preserving edit — comments and ordering survive), then resolves the graph so `noeta.lock` reflects it. Exactly one source form: `--path`, `--git` + `--tag`, or `--version` (with `--package` naming the registry identity). The import-root `KEY` may be omitted where it can be derived — from `--package`'s package half, or a `--path` dependency's own `[package]` name; a key that differs from the package's declared root is a deliberate rename and warned about (`use <key>.…`, not `use <root>.…`). A key that would capture a built-in import root (`std`/`noeta`/`core`) is refused. After resolving, the **committer signal** flags a newly-pulled release whose history introduces committers new to that repo.

### `noeta update`

```text
noeta update
```

Discards the current pins and re-resolves the whole graph, rewriting `noeta.lock`: every requirement is re-satisfied against the index, and every git ref — a moving `branch`/HEAD tip, or a tag whose pin drifted — is re-`ls-remote`d and re-pinned to its current commit SHA. This is the *only* verb that moves existing pins; a plain `run`/`build`/`check` always reproduces the lock. Trust-root changes surface here too: a legitimate maintainer migration (key rotation, a move to keyless) is accepted by `noeta update` re-pinning the scope, never silently mid-build (see [Package Provenance](Package-Provenance#trust-on-first-use-and-downgrade-protection)). The committer signal runs on every changed pin.

### `noeta claim`

```text
noeta claim <SCOPE> [--token <TOKEN>] [--audience <AUDIENCE>] [--domain <DOMAIN>]
```

Claims a registry **scope** — the `company` half of `company/package` — self-service and **squat-proof**: you can only claim the scope whose name matches an identity you prove. Two proof paths:

- **GitHub (default).** In GitHub Actions (grant `id-token: write`), the ambient OIDC token proves the workflow runs under the org/user of the scope's name — zero-config. On a laptop, it falls back to the GitHub **device flow**: a URL + code is printed, you authorize in a browser. The hosted registry's OAuth client id is built in, so this is zero-config too; `NOETA_GITHUB_CLIENT_ID` overrides it when claiming on a third-party registry with its own OAuth app.
- **`--domain <domain>`.** Prove control of a domain whose first label is the scope (`acme.dev` for `acme`): the registry fetches `https://<domain>/.well-known/noeta-registry.txt` and expects `noeta-scope=<scope>`.

On success a **publish token** is bound to the scope — a given `--token`, or a freshly minted one printed **once** (save it; `noeta publish` reads it from `NOETA_REGISTRY_TOKEN`). Re-claiming as the *same* proven identity rotates the token; any other identity is refused with a conflict — ownership never transfers implicitly. Reserved namespaces (`std`, `noeta`, `core`) are never claimable, and a first-party scope only by its designated org. Claiming targets the hosted registry the scope routes to (`[registries]`, else `NOETA_REGISTRY_URL`, else the built-in default `registry.noeta.dev`) — a git-forge registry has no claim endpoint, and `NOETA_REGISTRY_DIR` (the file-backed local index) has none either. The OIDC audience defaults to the host of that registry (`registry.noeta.dev` for the default); `--audience` or `NOETA_REGISTRY_AUDIENCE` overrides it.

### `noeta publish`

```text
noeta publish --git <URL> [--tag <TAG>] [--key | --interactive [--oob]]
```

Publishes the package in the current directory's `noeta.toml` to the registry: resolves `--tag` (default `v<version>`) to its commit SHA, pins that into the index entry, and **signs an attestation** binding *name + version → commit* so consumers can verify the release independently of trusting the registry.

How it signs — an explicit flag wins, then the environment decides:

| Situation | Result |
|---|---|
| `--key` | Force **key-based** Ed25519 signing with the key file — `[signed]`. |
| `--interactive` | Keyless via a **browser sign-in** (GitHub/Google/Microsoft; your email is the identity). `--oob` prints the URL and prompts for a code instead of opening a browser. |
| Ambient CI identity (GitHub Actions, GitLab, Buildkite) | **Keyless** (Sigstore), zero-config — `[keyless: <identity>]`. |
| A key file exists (`NOETA_SIGNING_KEY` or `./noeta-signing.key`) | **Key-based** Ed25519 — `[signed]`. |
| None of the above | `[UNSIGNED]` (resolves, but consumers can't verify it). |

A published version is **immutable** — re-publishing the same version with different coordinates is rejected. A package with `path`/`git` dependencies is rejected at publish (consumers couldn't resolve them); depend via the registry. A manifest with a non-empty [`[patch]` table](Manifest#patch--dev-time-path-overrides) is likewise rejected — a patch is a local dev-time override and must not travel with a release. Publishing to the hosted registry needs `NOETA_REGISTRY_TOKEN` (bound by [`noeta claim`](#noeta-claim)). The target index is the one **the package's scope routes to**, exactly like resolution: a `[registries]` mapping for the scope wins (so a private scope never leaks to the public registry), then `NOETA_REGISTRY_URL`, then `NOETA_REGISTRY_DIR`'s file-backed local index (offline development and tests), else the built-in hosted registry at `registry.noeta.dev`.

### `noeta scope`

```text
noeta scope require-provenance <SCOPE> [--root <key|keyless>] [--off]
```

Manages the publishing policy of a scope you own, authenticated with the scope's publish token (`NOETA_REGISTRY_TOKEN`) against the registry the scope routes to (`[registries]`, else `NOETA_REGISTRY_URL`, else the built-in default `registry.noeta.dev`). `require-provenance` makes the registry reject any release under the scope that doesn't carry verified provenance — so a leaked publish token *alone* can no longer push a release. `--root` narrows which trust root satisfies it (`key` for an Ed25519 signature, `keyless` for a Sigstore bundle; omitted, either does); `--off` lifts the requirement. See [Package Provenance](Package-Provenance).

### `noeta audit`

```text
noeta audit [PATH]
```

Answers *"what am I actually running?"* for the resolved dependency tree: every package and its source, which ones run **native code** or add **CLI commands** (the `[trust]` grants that make that authority active), and each scope's **pinned provenance trust root** — a signing key or a keyless identity. Resolution *enforces* verification, so a build that succeeds already means every signed release verified; the audit is the human-readable report of what that trust rests on.

It also cross-references every dependency against the registry's **security advisory feed**, showing each hit's **intake tier** (`operator` / `publisher` / `imported`) and, for a publisher advisory, its verified signing identity. An imported advisory that carried a **CVSS v3.x vector** upstream shows its severity band with the base score re-derived client-side from that vector — `high (CVSS 7.8)`. Whether a tier fails or merely warns is set per-project by `[trust.advisories]` (default: all warn) — see [Package Provenance](Package-Provenance#security-advisories-and-intake-tiers).

### `noeta advisory`

```text
noeta advisory publish <ID> <PACKAGE> <RANGES> <SEVERITY> <SUMMARY> [--details …] [--url …] [--patched …] [--withdraw] [--interactive [--oob]]
noeta advisory report  <PACKAGE> <SUMMARY> [--ranges …] [--details …] [--url …] [--reporter …]
noeta advisory reports [--scope <SCOPE>] [--status <pending|promoted|dismissed>] [--all]
noeta advisory promote <REPORT-ID> --id <ID> --severity <SEVERITY> [--ranges …] [--summary …] [--details …] [--url …] [--patched …] [--operator] [--interactive [--oob]]
```

`advisory publish` issues (or updates) a **publisher**-tier advisory for a package in a scope you own — keyless-signed with your OIDC identity, sent with the scope's publish token (`NOETA_REGISTRY_TOKEN`), so consumers verify it offline. `advisory report` files a **public report** against any package (unauthenticated, rate-limited): not an advisory, but queued for an operator or the scope owner to triage.

`advisory reports` lists the reports queued for triage — what's **promotable**. Without `--scope` it shows the operator triage queue (needs `NOETA_REGISTRY_ADMIN_TOKEN`); with `--scope`, the scope owner's own queue (their packages' reports; needs the scope's `NOETA_REGISTRY_TOKEN`). It shows the `pending` reports by default (`--all` for every status).

`advisory promote` turns a queued report into a signed advisory. The advisory is **prefilled from the report** (package, ranges, summary, details, url) and finalised with the triaged `--id` and `--severity`. As an **operator** (`--operator`, `NOETA_REGISTRY_ADMIN_TOKEN`) it becomes an `operator`-tier advisory; otherwise the report package's **scope owner** promotes it into a keyless-signed `publisher`-tier advisory — the same keyless Sigstore bundle a fresh `advisory publish` produces, prefilled from the report. See [Package Provenance](Package-Provenance#issuing-and-reporting-from-the-client).

### `noeta watch-scope`

```text
noeta watch-scope <SCOPE> [--state <PATH>]
```

Monitors a scope's advisory **transparency log** over time for silent suppression or history rewrite: it pins the feed head, the log checkpoint, and the advisory ids seen for the scope, then on each run verifies the log only grew (append-only) and that no previously-seen advisory disappeared. A rewrite, key change, feed rollback, or disappearance exits non-zero — ideal as a CI cron. See [Package Provenance](Package-Provenance#noeta-watch-scope-scope--suppression-monitoring).

### `noeta key`

```text
noeta key new [--out <PATH>]
```

Generates an Ed25519 keypair for the key-based signing path: writes the **private** key (default `noeta-signing.key`, mode 0600 — keep it out of git) and prints the **public** key to register with your registry scope. Only needed if you can't sign keyless (no CI identity and no browser); see [Package Provenance](Package-Provenance) for the trade-offs — keyless has nothing to steal and is publicly monitorable, a key file is neither.

## `noeta upgrade`

```text
noeta upgrade [--version <vX.Y.Z>] [--check]
```

Self-updates the **toolchain binary** to the latest [release](https://github.com/noeta-lang/noeta/releases) — the counterpart of [`noeta update`](#noeta-update), which re-resolves a *project's dependencies*. It resolves the latest release, downloads the binary for this machine, verifies its SHA-256 checksum against the release's `SHA256SUMS`, and atomically replaces the running executable (staged beside it, then renamed over — safe while `noeta` runs). Already current is a no-op; a successful swap prints the old → new version.

- `--version vX.Y.Z` installs that exact release instead of the latest — downgrades are allowed and labeled as such. **Prereleases are never installed**: the latest-release resolution excludes them by definition, and an explicit prerelease tag (any `-` suffix, e.g. `v0.3.0-rc.1`) is refused.
- `--check` reports whether an upgrade is available and changes nothing: exit 0 when current, exit 1 when a newer release exists — so scripts can gate on the exit code.

A `noeta` installed by `cargo install` is refused (upgrade that through cargo — the binary belongs to cargo's bookkeeping), and platforms without release binaries are pointed at [building from source](https://github.com/noeta-lang/noeta#building-from-source). Set `GITHUB_TOKEN` (or `GH_TOKEN`) to lift GitHub's unauthenticated API rate limit in CI; it is never required.

## `noeta ide`

```text
noeta ide --vscode [--bin <NAME|PATH>]
```

Installs the **Noeta VS Code extension at this binary's own version**: downloads `noeta-<version>.vsix` from the toolchain's GitHub release `v<version>`, verifies it against the release's `SHA256SUMS` (the same artifact contract [`noeta upgrade`](#noeta-upgrade) consumes), and installs it with the editor's own `--install-extension --force` — so re-running updates the installed extension in place. The version pinning is the point: the extension's grammar and language-server integration then always match the running toolchain — after a [`noeta upgrade`](#noeta-upgrade), run `noeta ide --vscode` again to move the extension in step.

The editor is auto-detected — the first of `code`, `codium`, `code-insiders` found on PATH — and `--bin <name-or-path>` overrides the pick (any binary speaking `--install-extension` works). The `.vsix` is staged in the noeta cache and removed after a successful install; if the editor's install invocation fails, the file is **kept** and its path printed so you can install it by hand.

This verb exists because the Visual Studio Marketplace listing is still pending — the release asset is the install path in the meantime — and it stays useful beyond that: it is the install path for **VSCodium** (which the Microsoft marketplace does not serve) and for offline or version-pinned setups. A cargo-installed or source-built `noeta` has no matching release asset and is refused; install from the source tree instead ([`editors/vscode-noeta`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta)). Bare `noeta ide` prints a short pointer at `--vscode` (more editors may follow).
