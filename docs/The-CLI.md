# The `noeta` CLI

The `noeta` binary is the whole toolchain. Its subcommands come from three places: the toolchain's own verbs, the standard library, and — in a project that asks for them — your dependencies.

## Toolchain verbs

The compiler, the runtime, and the package surface — every one of these is built into the binary and always available.

| Command | Purpose |
|---|---|
| [`noeta init`](#noeta-init) | Scaffold a new project — manifest, entry file, editor profiles, agent docs. |
| [`noeta run`](#noeta-run) | Type-check and execute a program. |
| [`noeta build`](#noeta-build) | Compile to a standalone artifact (`--exe`, `--native` for machine code, `--wasm`/`--serve` for [WebAssembly](WebAssembly-and-the-Edge)). |
| [`noeta check`](#noeta-check) | Parse and type-check without running or building (exit 0/1/2). |
| [`noeta docs`](#noeta-docs) | Search and read this guide offline — it is embedded in the binary. |
| [`noeta explain`](#noeta-explain) | Explain a diagnostic code — what `E0xxx` means and how to fix it. |
| [`noeta expand`](#noeta-expand) | Print the source that compile-time `@`-directive expansions generated. |
| [`noeta repl`](#noeta-repl) | Interactive REPL. |
| [`noeta dump`](#noeta-dump) | Disassemble a program to its VM bytecode (a debugging aid). |
| [`noeta fmt`](#noeta-fmt) | Format `.noe` source into the canonical style (files/dirs, `--check`, `--stdin`). |
| [`noeta profile`](Profiling) | Profile a program — a hot-function table or a flamegraph. |
| [`noeta cache`](#noeta-cache) | Inspect or clean the per-user cache — compilations, composed toolchains, fetched sources. |
| [`noeta grammar`](Extending-Tiers) | Generate the editor grammar overlay for a project's own [text tiers](Extending-Tiers). |
| [`noeta lsp`](Editor-and-AI-Tooling) | The language server, over stdio (started by your editor, not by hand). |
| [`noeta dap`](Debugging) | The debug adapter, over stdio (started by your editor's debug UI, not by hand). |
| [`noeta mcp`](Editor-and-AI-Tooling) | The agent-native MCP server, over stdio (for AI tooling; see [Editor & AI Tooling](Editor-and-AI-Tooling)). |
| [`noeta add`](#noeta-add) | Add a dependency to `noeta.toml` and resolve it into `noeta.lock`. |
| [`noeta update`](#noeta-update) | Re-resolve the dependency graph, re-pinning moved refs and refreshed versions. |
| [`noeta claim`](#noeta-claim) | Claim a registry scope self-service — prove your GitHub identity (or a domain) and get the publish token. |
| [`noeta publish`](#noeta-publish) | Publish a tagged release of your package to the registry, signed ([provenance](Package-Provenance)). |
| [`noeta scope`](#noeta-scope) | Manage a scope you own — e.g. require verified provenance on every release. |
| [`noeta audit`](#noeta-audit) | Report the dependency tree's trust footprint — native/command grants, pinned provenance, and advisory hits by tier. |
| [`noeta advisory`](#noeta-advisory) | Issue a publisher advisory for a scope you own, file a public report, list/promote the report queue, or `watch` a scope's transparency log for silent suppression. |
| [`noeta key`](#noeta-key) | Manage the Ed25519 signing key (the key-based provenance path). |
| [`noeta upgrade`](#noeta-upgrade) | Self-update the toolchain binary to the latest release. |
| [`noeta ide`](#noeta-ide) | Install the matching editor extension — the VS Code/VSCodium `.vsix` at this binary's version. |

Run `noeta --help` or `noeta <command> --help` for the authoritative flag list.

## Global flags

A few flags apply to every command, including the ones your dependencies contribute.

| Flag | Purpose |
|---|---|
| `--color <when>` | Whether diagnostics are printed in colour: `auto` (the default), `always`, or `never`. |
| [`--watch`](#noeta-serve-and---watch) | Restart the command whenever project sources change. |

Under `--color auto`, diagnostics are coloured when the toolchain is writing to a terminal and plain otherwise, so a pipe, a redirect and a CI log capture get exactly the text they always did.
Two environment variables move that line without a flag: `NO_COLOR` (set to anything non-empty) turns colour off, and `CLICOLOR_FORCE` turns it on even when the destination is not a terminal — which is what you want for a CI system whose log viewer renders ANSI.
A `TERM` of `dumb` also disables it.
Passing `--color` explicitly overrides all three, so `--color always` still colours output you are piping into a pager like `less -R`.

An abort **traceback** follows the same flag as the diagnostic it prints under: the frame locations are dimmed and the function names are left bright, so the names are what your eye lands on.

The flag describes the *human* rendering only.
`noeta check --format json` emits the same diagnostics as machine-readable JSON, and that never carries escape sequences whatever you ask for — nor do the diagnostics the [language server](Editor-and-AI-Tooling), the [debug adapter](Debugging) and the MCP server send to their clients.

## Commands the standard library provides

These need no opt-in — `std` ships with the toolchain — but every one of them is an [extension command](#commands-a-package-contributes) `std` contributes, not a verb the binary hardcodes. They are here by default because std is the *default* provider, not a privileged one.

| Command | Purpose |
|---|---|
| [`noeta test`](Testing) | Discover and run `@test` blocks. |
| [`noeta bench`](Benchmarking) | Discover and measure `@bench` blocks. |
| [`noeta doc`](Documentation-and-Tiers) | Extract `@doc { … }` prose to stdout, or generate the package's documentation artifact. |
| [`noeta serve`](#noeta-serve-and---watch) | Run a program's HTTP handler as a server (`fn fetch(req: Request): Response`). |

**So any of them can be replaced.** A [`[trust.commands]`](Manifest#trustcommands--contributed-subcommands) binding under one of these names takes the name over, and the new provider owns the whole verb — its own flags, its own `--help`, its own exit codes:

```toml
[trust.commands]
test = "thirdparty/ExcellentTesting"   # `noeta test` now runs theirs
```

You get the batteries out of the box, and swapping one out is a line of manifest rather than a fork. The core toolchain verbs above are not replaceable this way — `run`/`build`/`check` are the compiler.

A package that declares its own `@tier` earns `noeta <tier> <FILE>` the same way — see [Extending Tiers](Extending-Tiers). That is a different seam from this one, and worth keeping straight: a [`[directives]`](Manifest#directives--where-each-name-comes-from) binding decides what `@test` *means* at compile time (and so what `noeta check` verifies), while the command binding decides what *runs* it. A framework that runs your existing `@test` blocks its own way needs only the command binding.

## Commands a package contributes

A dependency can add subcommands to the toolchain — the `cargo clippy` model. They are **not** part of the CLI, and are documented by the package that ships them: such a command exists only inside a project whose manifest depends on the providing package *and* binds the command in [`[trust.commands]`](Manifest#trustcommands--contributed-subcommands), dispatched through that app's composed toolchain. In any other directory the verb does not exist, and `noeta --help` there never mentions it.

```toml
[trust.commands]
migrate = "para/db"           # `noeta migrate` — apply this project's schema migrations
undo    = "para/db:rollback"  # the key is the name you type, so this is `noeta undo`
```

The binding *is* the grant: one entry both authorizes the package to contribute the command and fixes the name it appears under, so two packages exporting the same name coexist and nothing is registered you did not ask for. [`noeta audit`](#noeta-audit) reports every command grant in the tree.

`noeta --help` inside such a project lists these commands, and `noeta <cmd> --help` renders the package's own arguments — but only once the project's toolchain has been composed, since a package's commands live in that build and not in the `noeta` on your `PATH`. Any command run in the project composes it (the first one pays a build and says so); after that, help describes what will actually run. A `--help` on a cold cache prints the stock list rather than disappearing into a multi-minute build.

The canonical example is [para/db](para-db)'s `noeta migrate` — forward-only migrations from a `migrations/` directory, re-runnable seeds from `seeds/` — whose full reference lives on that package's own page. Writing one is on [Native Extensions](Native-Extensions#extension-commands).

> [!NOTE]
> **Observability.** There is no telemetry subcommand or flag — production tracing rides `noeta run` and the server, configured by the standard `OTEL_EXPORTER_OTLP_ENDPOINT` env var and off until you set it. See [Observability](Observability). (The dev-time flamegraph tool is [`noeta profile`](Profiling).)

## `noeta init`

Scaffolds a new project, ready to run before you edit a line:

```text
noeta init [PATH]            # default: the current directory (created if missing)
      --name company/package # default: local/<directory-name>
      --no-git               # skip `git init`
```

What it writes — never overwriting a file that already exists, so it is safe in a non-empty directory:

- **`noeta.toml`** — package identity plus two build targets: `development` wires the four std dev tiers (`@test`, `@bench`, `@doc`, `@debug`) live, and `production` is an explicit name for the tier-free baseline (see [build targets](Dev-Tiers#naming-tiers-and-build-targets--noetatoml)).
- **`src/main.noe`** — a fmt-canonical entry file exercising every tier: a documented function with a `@debug` trace, a two-case `@test` block, and a `@bench`.
- **`.vscode/`** — the run/debug profiles the [Noeta extension](Editor-and-AI-Tooling) picks up (F5 debugging over `noeta dap`), plus the extension recommendation.
- **`.gitignore`** — build/profiler artifacts ignored; `noeta.lock` deliberately not (commit it).
- **`AGENTS.md`** — how an AI agent should drive this project: the CLI feedback loop and the [`noeta mcp`](Editor-and-AI-Tooling) tool surface.
- **`SYNTAX.md`** — the full language reference, assembled from the same embedded guide `noeta mcp`'s `docs_search` serves, so it always matches the installed compiler. Delete and re-run `noeta init` after upgrading to refresh it.

A fresh directory also gets `git init` (skipped inside an existing repository, or with `--no-git`).

Re-running it inside a package it already scaffolded is **additive**, not an error: every missing file is created, every existing one — the manifest included — is left byte-identical, and a run with nothing left to create says so and exits 0. That is how the generated `SYNTAX.md` above is refreshed: delete it, re-run `noeta init`. `--name` is ignored (with a warning) once a `noeta.toml` exists — rename the package by editing it.

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
| `--jit-stats` | After the run, print a summary to stderr of what the Tier-1 JIT compiled and why anything bailed or was declined (see [The Virtual Machine](The-Virtual-Machine#tier-1--the-jit)). |

The active-tier set is the target’s live tiers ∪ any `--tier` flags, resolved *before* loading (a bad target fails fast). With an empty active set — the default — every `@test`/`@bench`/`@doc`/`@debug` block strips away and the program runs as written. See [Dev Tiers](Dev-Tiers).

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

Which of your dependencies' code is present is governed by [`noeta.toml` targets](Dev-Tiers) — build the default (safe, minimal) target, or `--target <name>` to layer in more. Package authors keep dev-only capabilities (like a tier-body formatter) out of your shipped binary automatically; see *shipping dev capabilities* in [Native Extensions](Native-Extensions).

## `noeta check`

```text
noeta check [PATH]
```

Parses and type-checks without running or building — the CI/pre-commit gate (the `cargo check` / `tsc --noEmit` primitive). `PATH` defaults to the current directory, walked recursively for `.noe` files (resolving and deduping shared modules); a single file checks just that file with its sibling modules linked in. `--format json` emits a single machine-readable report on stdout for CI/editors/the MCP server; the default renders diagnostics for a terminal. Exits non-zero if any error-severity diagnostic is found (warnings print but do not fail).

**It covers dev-tier blocks too, with no `--target`.** Each file is checked once as it ships (every `@test`/`@bench`/`@debug` block stripped) and then once per code tier its own blocks name — the exact shape `noeta test`/`noeta bench` compiles — so a green `noeta check` is never followed by a `noeta test` that fails to compile. The summary names what it looked inside, and the JSON report carries the same list as `tiers_checked`:

```console
$ noeta check .
checked 3 files (tiers: test, debug): 0 error(s), 0 warning(s)
```

`--tier <NAME>`/`--target <NAME>` still select a shape explicitly, checked as one program the way that build would compile it; the per-tier sweep then covers whatever the selection left out. See [Dev Tiers](Dev-Tiers#checking-is-not-building) for why one tier at a time.

## `noeta docs`

```text
noeta docs <QUERY>...              # rank this guide's sections against a query
noeta docs --page <SLUG>           # print one page
noeta docs --page <SLUG>#<SECTION> # print just one section of it
noeta docs --list                  # every page, slug and title
noeta docs … --format json         # machine-readable hits, page, or index
```

This whole guide is compiled into the binary, so it is searchable with no network and no repository beside it — and it documents *the toolchain you are running*, not whatever version the website is serving.

```console
$ noeta docs packed struct --limit 2
1. Fixed-Width Ints & Packed Types › Packed value types — `@packed`
   The `@packed` directive marks a **struct** as a *packed value type*: a `List` of it is stored…
   noeta docs --page Fixed-Width-Integers#packed-value-types--packed

2. Fixed-Width Ints & Packed Types › `bytes` — serialize a packed list
   A `List` of a packed type round-trips through an opaque `bytes` buffer with `.to_bytes()`…
   noeta docs --page Fixed-Width-Integers#bytes--serialize-a-packed-list

2 results.
```

Every hit prints the exact command that reads it. **Prefer the `#section` form**: guide pages run to hundreds of lines, and a section is typically one or two percent of one — the difference between reading a paragraph and reading a chapter.

A page resolves by slug or title, exactly or by substring, so `--page types` finds `Type-System`. Asking for a section that does not exist lists the ones that do.

Search is BM25F over heading-delimited sections, weighting a term by where it appears (page title, heading, prose) and discounting it by how many sections use it — so a query's rare word decides the answer rather than its common one. Identifiers are matched whole *and* by their parts, so `read_line_async`, `try_parse` and `E0059` each find the sections that name them.

Exit `0` on a hit, `1` when nothing matches or the page does not exist, `2` when no query, `--page`, or `--list` was given.

The corpus, the ranker, and the `#section` addressing are shared with the editor's docs browser (`noeta lsp`) and the [`docs_search`/`docs_get`](Editor-and-AI-Tooling#the-agent-surface-noeta-mcp) tools `noeta mcp` serves, so no two surfaces can disagree about what the guide says.

## `noeta explain`

```text
noeta explain <CODE>         # what a diagnostic code means, and how to fix it
noeta explain --all          # the whole catalog, grouped
noeta explain … --format json   # the machine-readable catalog
```

Every diagnostic the toolchain renders carries a stable `E0xxx` code; this is how you look one up without leaving the terminal. The code may be spelled any way you would type it — `E0059`, `e0059`, or a bare `59`.

```console
$ noeta explain E0059
E0059 — shadowed binding (error)

A binder reuses a name that already means something in scope.

One name means one thing per scope stack. Assignment already reassigns rather than
re-declaring, and an `is` test narrows the same binding, so shadowing is never needed to
express anything — rename the binder.

  more: https://docs.noeta.dev/Syntax-Basics
```

A code the catalog does not know exits `1` and suggests its neighbours (a transposed digit is the usual miss); naming neither a code nor `--all` exits `2`.

The explanations live with the codes in the compiler, so the three places they surface cannot disagree: this command, the [`explain_diagnostic`](Editor-and-AI-Tooling#the-agent-surface-noeta-mcp) tool `noeta mcp` serves to agents, and the generated [Diagnostics](Diagnostics) reference — which is rendered from `noeta explain --all --format json`.

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

## `noeta serve` and `--watch`

`noeta serve app.noe --port 8080` serves the file's top-level `fn fetch(req: Request): Response` handler (see [std.http](std-http)); the app defines the handler and must **not** call `server.serve(...)` itself — the command runs the file's top-level setup, then drives the handler on the given port. `fetch` need only *be* a handler, not be spelled as a `fn`: any top-level binding of that shape serves, so an app that already has a router hands it over directly — `fetch = app.route` — instead of writing a wrapper that forwards to it. Hot reload works either way; the swap follows the definitions the handler reaches, not the name it was bound under. `--host` sets the bind address (default `0.0.0.0`, all interfaces; pass `--host 127.0.0.1` for local-only). **Ctrl-C** drains gracefully: the server stops accepting, finishes the requests already in flight, closes the listener, and exits — a second Ctrl-C forces an immediate stop.

`noeta serve` accepts **plain HTTP, by design** — put it behind a reverse proxy (nginx, Caddy, a cloud load balancer) and terminate TLS there. This is about *inbound* connections only: your program's own outbound calls already speak TLS, because `std.http`'s client is built on rustls, so `http.get("https://…")` works with no proxy involved. What the server does not do is present a certificate. Adding that would mean a TLS server stack plus certificate loading, rotation and renewal in the toolchain — weight that lands on exactly the programs which today shed the TLS tree entirely by never importing the client, and operational work a proxy already does better and which you likely already run for routing or static assets. Bind to loopback (`--host 127.0.0.1`) when a proxy on the same host is the only thing that should reach it.

`--parallel N` serves across **N worker isolates** for true multi-core throughput: the listener is bound once and each worker inherits a cloned handle to it, so the kernel load-balances connections across cores (no `SO_REUSEPORT`, no extra dependency). All workers share the process and drain together on Ctrl-C. `--parallel --watch` hot-reloads across the whole fleet — a swap **broadcasts** to every worker's live session, so all cores serve the new code without a restart. (Reactive/LiveView state is per-worker: signals and WebSocket subscribers live in the worker that handled the connection. That is a constraint on where an app's **source of truth** sits, not a bar on serving a LiveView across cores — an app backed by a database serves fine on all of them, since each worker opens its own connection and drains its own change notifications, while an app whose truth is an in-memory signal wants a single worker until session state is shared.)

`--watch` works on **any** command (`noeta run --watch`, `noeta test --watch`, …): a file watcher restarts the command on change — with the startup cache, a restart is a few milliseconds.

For the tier runners the watch is **impact-filtered**: `noeta test --watch` (and `bench`) diffs each save against the previous run, walks the reverse call graph from the changed definitions, and reruns only the impacted tier fns (via the runners' repeatable `--name` filter) — edit a leaf function and exactly its caller-tests rerun; an inert edit (formatting between declarations, a comment) runs nothing. The filter is **project-wide**: editing an *imported module* narrows to the entry tests that transitively reach the change, and editing a module function nothing imports reruns nothing at all. Edits the engine cannot attribute — a signature/layout change, a changed top-level statement (fixtures live there), a new or deleted module, a manifest change, red code — degrade to a full rerun *with the reason printed*. The closure is static, so code reached only through dynamic dispatch is matched best-effort (method calls on untyped receivers over-approximate by name); run without the filter occasionally if you lean on reflection-driven dispatch.

`noeta serve --watch` upgrades from restarts to **in-process hot reload**. On each save of the entry file the watcher re-links the project (the same load the boot did), type-checks the whole program (**transactionally** — red code never swaps, wherever in the project it is; the old version keeps serving and the diagnostics go to the terminal *and* to connected LiveView clients as an error overlay), diffs the entry against the running version, and swaps the changed definitions into the live server. It re-links rather than re-reading the one file because a module's path derives from its file: inside a package the entry's `fn fetch` is bound as `pkg.main.fetch`, and a fragment carrying the unqualified name would install beside the running handler instead of over it. The state rule is the language behavior to know:

- **Reactive state survives edits** — an unchanged `signal(...)`/`cell`/`synced_signal` binding keeps its value across the swap; effects are disposed and re-created by the new version.
- **Plain state re-initializes** — ordinary top-level bindings are re-run from the new source. State that must survive belongs in a signal.

Connected LiveView clients (the bundled `server.liveview_js()` shim) are told over their own websocket: a landed swap pushes `{"type":"reload"}` and closes the socket — the page reloads and its fresh session snapshots the *preserved* signal state, so the browser view carries the same counter through the edit; a rejected edit pushes `{"type":"error",…}`, which the shim renders as a full-screen diagnostics overlay, cleared by the next good frame. Swaps apply immediately even when the server is idle (the watcher wakes the blocked executor).

Changes the live process cannot absorb — a type-layout or signature change, an edit to another project file — fall back to a **full restart**, automatically, with the reason printed (`[hot] restart needed: the layout of type \`P\` changed`). After a restart, an open browser page reconnects and re-syncs state but keeps its old markup until refreshed (the reload push needs a live server to send it).

**Memory during an edit marathon.** A long editing session does not cost memory proportional to the number of saves: a swap is broadcast to every worker through one queue, and its payload — the changed code plus the whole-program analysis it was compiled against, which is program-sized, not edit-sized — is released as soon as the last worker has installed it, leaving about a hundred bytes of ordering bookkeeping per save behind. Nothing is released while any worker could still need it, so a worker busy with a long request delays reclamation rather than missing the edit. What a swap does keep alive for a while is *state*, not code: in-flight requests finish on the code they started on, and all of it is reclaimed when the process exits. The machinery behind this — what is retained and why — lives on the [architecture pages](Architecture-and-Pipeline).

The same unchanged `fetch` program also deploys to the edge as a `wasi:http` component — see [WebAssembly & the Edge](WebAssembly-and-the-Edge).

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

Starts an interactive session. The prompt is `» `. An entry that is still being typed — an unclosed `(`/`[`/`{`, or a statement that simply runs out of input — is not submitted; the delimiter count is over lexer tokens, so braces inside strings and `${…}` never miscount.

### At a terminal

When both stdin and stderr are a terminal, the prompt opens a full line editor: arrow keys and the usual editing bindings, Ctrl-R history search, persistent history, and Ctrl-C to abandon the entry you are typing without losing the session. Pressing Enter inside an unfinished block opens a new line **within the same entry**, so a `class` or `fn` body is edited as one unit and recalled from history as one; a multi-line paste arrives as a single entry the same way. Piped input — `noeta repl < script.noe`, or a script driving the prompt — reads lines exactly as before, and a continuation line then shows `… `.

Your entry is **syntax-coloured as you type**, and **TAB completes**. Neither is a separate implementation of Noeta: the colouring classifies with the compiler's own lexer (the same function that highlights code in `noeta doc`), and completion is the engine behind [`noeta lsp`](Editor-and-AI-Tooling), asked about the whole accumulated session rather than the line — so a type declared three entries ago completes like one written in a file, `x.` offers the receiver's fields and methods, and `@` offers the directives. TAB after `:` completes the meta-commands below, including live binding names for `:drop` and `:type`.

The prompt's colour follows the same [`--color`](#global-flags) rule as diagnostics, so `--color never` (or `NO_COLOR`, or `TERM=dumb`) turns off the highlighting and the error underneath it together. History is written to `$XDG_STATE_HOME/noeta/repl-history` (`~/.local/state/noeta/repl-history` by default), or to `NOETA_REPL_HISTORY` if you set it.

### Sessions

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

**Example.** The opening of a recursive `fib`'s prototype — the base-case compare and its branch:

```console
$ noeta dump fib.noe
...
=== proto 1 ===
params: 1, registers: 4
code:
    0  LoadConst   r2 <- k0
    1  Binary      r1 <- r0 < r2
    2  RequireCondBool r1 (if)
    3  JumpIfFalse r1 -> 5
    4  Return      r0
    ...
```

The opcode set and prototype/side-table layout are described in [The Virtual Machine](The-Virtual-Machine).

---

## The tier subcommands

`noeta test`, `noeta bench`, and `noeta doc` each operate on the dev-tier content co-located in a source file. They are documented on their own pages:

- **[Testing](Testing)** — `@test` blocks, `assert`, metadata attributes, isolation, and concurrency.
- **[Benchmarking](Benchmarking)** — `@bench` blocks and the timing method.
- **[Dev Tiers](Dev-Tiers)** — the tier model and `noeta.toml` build targets; **[Documentation](Documentation-and-Tiers)** — `@doc` extraction.

A program (or a dependency) can also declare its **own** tier with `@tier` — `noeta <tier> <FILE>` dispatches to the declaring package's runner; see [Extending Tiers](Extending-Tiers).

All three accept `--target <NAME>`, which acts as a **gate**: if the named `noeta.toml` target does not make that tier live, the command prints a notice and no-ops with exit `0`. With no `--target`, they always proceed.

`noeta test` additionally bounds **every test** by a per-test deadline — `--timeout <SECONDS>` (default `60`, `0` disables), overridden for one test with `#[std.test.Timeout(N)]`. A test that overruns is reported as its own `TIME` outcome rather than as a pass or a failure, the rest of the suite still runs and reports, and the run exits `1`; see [Testing → Timeouts](Testing#timeouts) for what the runner can and cannot do to a test that will not stop.

---

## Packages: `add`, `update`, `claim`, `publish`, `scope`, `audit`, `advisory`, `key`

Dependencies are declared in `noeta.toml` (`[dependencies]`, with elevated grants in `[trust]`) and resolve automatically on `run`/`build`/`check` — there is no separate install step; the resolved pins live in `noeta.lock` (commit it). These verbs are the *publisher/consumer trust* surface. The trust model behind them — attestations, the two signing roots, pinning, downgrade protection — is documented on [Package Provenance](Package-Provenance); the end-to-end submission walkthrough on [Package Registries](Package-Registries#publishing-to-the-hosted-registry).

### The manifest: `[package]` and dependency forms

The orientation you need for the verbs below: `[package]` gives the package its global `company/package` identity and SemVer version, and each `[dependencies]` key is the **import root** you address the package by (`use util.…`), decoupled from its registry identity. A dependency names exactly one source — a local `path`, a `git` URL (pinned `tag`, moving `branch`, or default-branch HEAD), or a registry `version` requirement — and **every** form resolves to an exact pin recorded in `noeta.lock`, which only [`noeta update`](#noeta-update) moves.

```toml
[package]
name = "acme/app"
version = "0.1.0"

[dependencies]
util = { path = "../util" }                                      # a local source tree
http = { git = "https://github.com/acme/http", tag = "v1.2.0" }  # git, pinned to a tag
json = { version = "^1.2", package = "acme/json" }               # registry, by SemVer requirement
```

[The `noeta.toml` Manifest](Manifest) is the complete reference — every table and key, the scope (array) dependency form, editions, and the exact rules the parser enforces. Developing a dependency *against* this app? A root-manifest [`[patch]` table](Manifest#patch--dev-time-path-overrides) re-points a package identity at a local tree for the whole graph — no `[dependencies]` edits, no lock churn, loud on every resolve.

### `noeta add`

```text
noeta add <company/pkg>                                                        # registry, current version
noeta add [KEY] (--path <DIR> | --git <URL> --tag <TAG> | --version <REQ>) [--package <company/pkg>]
```

Adds a dependency to the nearest `noeta.toml` (a format-preserving edit — comments and ordering survive), then resolves the graph so `noeta.lock` reflects it. The import-root `KEY` may be omitted where it can be derived — from `--package`'s package half, or a `--path` dependency's own `[package]` name; a key that differs from the package's declared root is a deliberate rename and warned about (`use <key>.…`, not `use <root>.…`). A key that would capture a built-in import root (`std`/`noeta`/`core`) is refused. After resolving, the **committer signal** flags a newly-pulled release whose history introduces committers new to that repo.

**A registry dependency needs no source.** Given a package identity and none of `--path`/`--git`/`--version`, `add` asks the registry that serves that scope for the package's current version and writes a caret requirement for it — `noeta add para/cli` against a 0.2.0 package writes `{ version = "^0.2", package = "para/cli" }`. The identity may be the positional argument (a `/` cannot occur in an import-root key, so `noeta add para/cli` is unambiguous — and the key is then derived, here `cli`) or `--package`, which is what you want alongside an explicit key: `noeta add para --package para/cli`. Auto-selection is a *new* selection, so it never lands on a **prerelease** or a **yanked** release; depend on either deliberately with `--version`. The requirement written is `^<major>.<minor>` (`^0.0.<patch>` in the `0.0.x` range, where SemVer lets every patch break), which is what the manifests in this documentation already say and what the lookup's own version resolves back to. A lookup that cannot be answered — the registry unreachable, no such package, nothing but prereleases — reports why and leaves the manifest untouched.

To state the version yourself, give exactly one source instead: `--path`, `--git` + `--tag`, or `--version`. Those forms are unchanged; `--version "^0.2"` still means exactly `^0.2`.

`--package` applies to every source form, meaning on each what [the manifest reference](Manifest#dependencies--what-the-package-builds-against) says it means: for `--version` it is the registry identity resolution needs — and the release it resolves to is checked against it, so a served tree that is not the package it was published as never installs; for `--path`/`--git` it is a **checked claim** about the tree the source points at, written into the entry and verified against that package's own `[package] name`. A `--path` claim is checked before anything is written, so a wrong identity leaves the manifest untouched; a `--git` claim is checked once the repo is fetched, during the resolve.

Adding under a key that is **already in the manifest widens it into a [scope array](Manifest#dependencies--what-the-package-builds-against)** rather than refusing: the existing value and the new one become the first two members of `key = [ … ]`, and a key that is already an array gains a member. Only that one entry is rewritten, and the existing member's own text is reused verbatim, so member formatting and trailing comments survive. This is how `noeta add para --package para/aether` followed by `noeta add para --package para/db` binds both packages under one `para` root — the form a family published to be addressed as `scope.package.module` needs. Re-adding the *identical* source is still refused, since an array with two equal members would resolve one package twice under one root.

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
noeta publish --docs-only
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

`--docs-only` regenerates the release's documentation artifact through the same pipeline a publish runs (for a native package: the composed-toolchain build and its API-reference extraction) and re-uploads it for a version **already in the index** — no `--git`, no new version, no provenance; the upload needs only the scope's publish token (`NOETA_REGISTRY_TOKEN`). It refuses when the manifest's version is not published: docs belong to a release, so a docs-only upload for an unpublished version is a mistake. Use it to fix a shelf release whose stored docs are wrong or empty without bumping the version.

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

Exit 0 means *checked and clean*. Besides a `fail`-tier advisory hit, the audit also exits non-zero when the advisory data **did not verify** — a signature that fails against the pinned feed key, a signed head that does not attest to the advisories served, or a transparency-log leaf that does not match the advisory it is supposed to be. Nothing was checked in that case, and "could not verify" is not "verified clean"; the reason is printed on stderr. An *unreachable* registry is different and stays a note with exit 0: this whole section is best-effort, and being offline is evidence of nothing. `[trust.advisories]` does not soften a verification failure — it selects which intake tier's *hits* fail the build, and a feed that never verified has no tier.

### `noeta advisory`

```text
noeta advisory publish <ID> <PACKAGE> <RANGES> <SEVERITY> <SUMMARY> [--details …] [--url …] [--patched …] [--withdraw] [--interactive [--oob]]
noeta advisory report  <PACKAGE> <SUMMARY> [--ranges …] [--details …] [--url …] [--reporter …]
noeta advisory reports [--scope <SCOPE>] [--status <pending|promoted|dismissed>] [--all]
noeta advisory promote <REPORT-ID> --id <ID> --severity <SEVERITY> [--ranges …] [--summary …] [--details …] [--url …] [--patched …] [--operator] [--interactive [--oob]]
noeta advisory watch   [SCOPE] [--state <DIR>]
```

`advisory publish` issues (or updates) a **publisher**-tier advisory for a package in a scope you own — keyless-signed with your OIDC identity, sent with the scope's publish token (`NOETA_REGISTRY_TOKEN`), so consumers verify it offline. `advisory report` files a **public report** against any package (unauthenticated, rate-limited): not an advisory, but queued for an operator or the scope owner to triage.

`advisory reports` lists the reports queued for triage — what's **promotable**. Without `--scope` it shows the operator triage queue (needs `NOETA_REGISTRY_ADMIN_TOKEN`); with `--scope`, the scope owner's own queue (their packages' reports; needs the scope's `NOETA_REGISTRY_TOKEN`). It shows the `pending` reports by default (`--all` for every status).

`advisory promote` turns a queued report into a signed advisory. The advisory is **prefilled from the report** (package, ranges, summary, details, url) and finalised with the triaged `--id` and `--severity`. As an **operator** (`--operator`, `NOETA_REGISTRY_ADMIN_TOKEN`) it becomes an `operator`-tier advisory; otherwise the report package's **scope owner** promotes it into a keyless-signed `publisher`-tier advisory — the same keyless Sigstore bundle a fresh `advisory publish` produces, prefilled from the report. See [Package Provenance](Package-Provenance#issuing-and-reporting-from-the-client).

### `noeta advisory watch`

```text
noeta advisory watch [SCOPE] [--state <DIR>]
```

`noeta audit` asks *does the advisory feed verify right now*; `advisory watch` asks *has anything been rewritten since*, which is the question a registry silently withholding an advisory from you can only be caught by. It pins the feed head, the transparency-log checkpoint, and the advisory ids seen for a scope, then on each run verifies the log only grew (append-only) and that no previously-seen advisory disappeared. A rewrite, key change, feed rollback, or disappearance exits non-zero.

**With no `SCOPE` it watches every scope your `noeta.lock` pins** — which is what a CI cron wants, since the list then keeps itself current as dependencies come and go. Name a scope to watch just that one. State lives in `--state <DIR>` (default: `watch/` under the noeta cache), one `<scope>.toml` per scope, so it survives between runs; commit it or cache it in CI, because a baseline that resets on every run detects nothing.

```yaml
# a daily GitHub Actions job — the whole gate
- run: noeta advisory watch --state .noeta-watch
```

The scope set is deliberately not filtered by source: an advisory names a *package*, so one against `acme/http` applies whether you resolved it from the registry or from git. See [Package Provenance](Package-Provenance#noeta-advisory-watch--suppression-monitoring).

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

The GitHub release asset is the extension's distribution channel: this verb is the install path for **VS Code**, for **VSCodium** (which the Microsoft marketplace does not serve), and for offline or version-pinned setups. A cargo-installed or source-built `noeta` has no matching release asset and is refused; install from the source tree instead ([`editors/vscode-noeta`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta)). Bare `noeta ide` prints a short pointer at `--vscode`.
