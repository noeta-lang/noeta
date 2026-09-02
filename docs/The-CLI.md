# The `noeta` CLI

The `noeta` binary is the whole toolchain. Its subcommands come from three places: the toolchain's own verbs, the standard library, and the dependencies of a project that asks for them.

## Toolchain verbs

Every verb below is built into the binary and available in any directory.

| Command | Purpose |
|---|---|
| [`noeta init`](#noeta-init) | Scaffold a new project (manifest, entry file, editor profiles, agent docs). |
| [`noeta run`](#noeta-run) | Type-check and execute a program. |
| [`noeta build`](#noeta-build) | Compile to a standalone artifact: a `.noeb` bundle, an executable, machine code, or [WebAssembly](WebAssembly-and-the-Edge). |
| [`noeta check`](#noeta-check) | Parse and type-check without running or building (exit 0/1/2). |
| [`noeta docs`](#noeta-docs) | Search and read this guide offline, from the copy embedded in the binary. |
| [`noeta explain`](#noeta-explain) | Explain what an `E0xxx` diagnostic code means and how to fix it. |
| [`noeta expand`](#noeta-expand) | Print the source that compile-time `@`-directive expansions generated. |
| [`noeta repl`](#noeta-repl) | Interactive REPL. |
| [`noeta dump`](#noeta-dump) | Disassemble a program to its VM bytecode (a debugging aid). |
| [`noeta fmt`](#noeta-fmt) | Format `.noe` source into the canonical style (files/dirs, `--check`, `--stdin`). |
| [`noeta profile`](Profiling) | Profile a program to a hot-function table or a flamegraph. |
| [`noeta cache`](#noeta-cache) | Inspect or clean the per-user cache of compilations, composed toolchains and fetched sources. |
| [`noeta grammar`](Extending-Tiers) | Generate the editor grammar overlay for a project's own [text tiers](Extending-Tiers). |
| [`noeta lsp`](Editor-and-AI-Tooling) | The language server, over stdio (started by your editor). |
| [`noeta dap`](Debugging) | The debug adapter, over stdio (started by your editor's debug UI). |
| [`noeta mcp`](Editor-and-AI-Tooling) | The agent-native MCP server, over stdio (for AI tooling; see [Editor & AI Tooling](Editor-and-AI-Tooling)). |
| [`noeta add`](#noeta-add) | Add a dependency to `noeta.toml` and resolve it into `noeta.lock`. |
| [`noeta update`](#noeta-update) | Re-resolve the dependency graph, re-pinning moved refs and refreshed versions. |
| [`noeta claim`](#noeta-claim) | Claim a registry scope by proving your GitHub identity or a domain, which binds the publish token. |
| [`noeta publish`](#noeta-publish) | Publish a tagged release of your package to the registry, signed ([provenance](Package-Provenance)). |
| [`noeta scope`](#noeta-scope) | Manage a scope you own, such as requiring verified provenance on every release. |
| [`noeta audit`](#noeta-audit) | Report the dependency tree's trust footprint: native and command grants, pinned provenance, advisory hits by tier. |
| [`noeta advisory`](#noeta-advisory) | Issue a publisher advisory for a scope you own, file a public report, list or promote the report queue, or `watch` a scope's transparency log for silent suppression. |
| [`noeta key`](#noeta-key) | Manage the Ed25519 signing key (the key-based provenance path). |
| [`noeta upgrade`](#noeta-upgrade) | Self-update the toolchain binary to the latest release. |
| [`noeta ide`](#noeta-ide) | Install the VS Code or VSCodium `.vsix` matching this binary's version. |

Run `noeta --help` or `noeta <command> --help` for the authoritative flag list.

## Global flags

Two flags apply to every command, including the ones your dependencies contribute.

| Flag | Effect | Default |
|---|---|---|
| `--color <when>` | Print diagnostics in color: `auto`, `always`, or `never`. | `auto` |
| [`--watch`](#noeta-serve-and---watch) | Restart the command whenever project sources change (`*.noe`, `noeta.toml`). | off |

Under `--color auto`, diagnostics are colored when the toolchain writes to a terminal and plain otherwise, so a pipe, a redirect and a CI log capture receive plain text. Three environment settings move that line without a flag, in this order of precedence:

| Setting | Effect |
|---|---|
| `CLICOLOR_FORCE` set to anything non-empty other than `0` | Color on, even off a terminal, which is what a CI log viewer that renders ANSI wants. |
| `NO_COLOR` set to anything non-empty | Color off. An empty `NO_COLOR=` does nothing. |
| `TERM=dumb` | Color off. |

An explicit `--color` outranks all three, so `--color always` still colors output you are piping into a pager like `less -R`.

An abort **traceback** follows the same flag as the diagnostic it prints under. Frame locations are dimmed and function names stay bright.

The flag describes the human rendering. Machine-readable output stays plain whatever you ask for: `noeta check --format json` emits the same diagnostics as JSON without escape sequences, and so do the diagnostics the [language server](Editor-and-AI-Tooling), the [debug adapter](Debugging) and the MCP server send to their clients.

## Commands the standard library provides

Each command below is an [extension command](#commands-a-package-contributes) that `std` contributes. `std` ships with the toolchain, so they are registered by default.

| Command | Purpose |
|---|---|
| [`noeta test`](Testing) | Discover and run `@test` blocks. |
| [`noeta bench`](Benchmarking) | Discover and measure `@bench` blocks. |
| [`noeta doc`](Documentation-and-Tiers) | Extract `@doc { … }` prose to stdout, or generate the package's documentation artifact. |
| [`noeta serve`](#noeta-serve-and---watch) | Run a program's HTTP handler as a server (`fn fetch(req: Request): Response`). |

Any of them can be replaced. A [`[trust.commands]`](Manifest#trustcommands--contributed-subcommands) binding under one of these names takes the name over, and the new provider owns the whole verb: its flags, its `--help` and its exit codes.

```toml
[trust.commands]
test = "thirdparty/ExcellentTesting"   # `noeta test` now runs theirs
```

The toolchain verbs above are reserved. A binding whose key names one of them is refused with exit `2`.

A package that declares its own `@tier` earns `noeta <tier> <FILE>` through a different seam, described in [Extending Tiers](Extending-Tiers). The two are worth keeping straight. A [`[directives]`](Manifest#directives--where-each-name-comes-from) binding decides what `@test` means at compile time, and therefore what `noeta check` verifies; a command binding decides what runs it. A framework that runs your existing `@test` blocks its own way needs only the command binding.

## Commands a package contributes

A dependency can add subcommands to the toolchain, on the `cargo clippy` model. The package that ships one documents it. Such a command exists inside a project whose manifest depends on the providing package and binds the command in [`[trust.commands]`](Manifest#trustcommands--contributed-subcommands), dispatched through that app's composed toolchain. In any other directory `noeta --help` does not list it and the verb does not resolve.

```toml
[trust.commands]
migrate = "para/db"           # `noeta migrate` applies this project's schema migrations
db      = "para/db:migrate"   # the key is the name you type, so this is `noeta db`
```

The binding is the grant. One entry both authorizes the package to contribute the command and fixes the name it appears under, so two packages exporting the same name coexist. [`noeta audit`](#noeta-audit) reports every command grant in the tree.

`noeta --help` inside such a project lists these commands, and `noeta <cmd> --help` renders the package's own arguments, once the project's toolchain has been composed. A package's commands live in that build rather than in the `noeta` on your `PATH`. Any command run in the project composes it, and the first one pays a build and says so; after that, help describes what will actually run. A `--help` on a cold cache prints the stock list instead of starting a multi-minute build.

[para/db](para-db)'s `noeta migrate` is the worked example: forward-only migrations from a `migrations/` directory and re-runnable seeds from `seeds/`, with the full reference on that package's own page. Writing one is on [Native Extensions](Native-Extensions#extension-commands).

> [!NOTE]
> **Observability.** Production tracing runs under `noeta run` and the server, configured by the standard `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable and off until you set it. See [Observability](Observability). The development-time flamegraph tool is [`noeta profile`](Profiling).

## `noeta init`

Scaffolds a new project, ready to run before you edit a line:

```text
noeta init [PATH]            # default: the current directory (created if missing)
      --name company/package # default: local/<directory-name>
      --no-git               # skip `git init`
```

A file that already exists is never overwritten, so `noeta init` is safe in a non-empty directory. What it writes:

| File | Contents |
|---|---|
| `noeta.toml` | Package identity plus two build targets. `development` makes the four std dev tiers (`@test`, `@bench`, `@doc`, `@debug`) live, and `production` names the tier-free baseline. See [build targets](Dev-Tiers#naming-tiers-and-build-targets--noetatoml). |
| `src/main.noe` | A fmt-canonical entry file exercising every tier: a documented function with a `@debug` trace, a two-case `@test` block, and a `@bench`. |
| `.vscode/` | The run and debug profiles the [Noeta extension](Editor-and-AI-Tooling) picks up (F5 debugging over `noeta dap`), plus the extension recommendation. |
| `.gitignore` | Build and profiler artifacts. `noeta.lock` is left tracked, since it belongs in the repository. |
| `AGENTS.md` | How an AI agent drives this project: the layout, the CLI feedback loop, the naming conventions nothing lints, and the [`noeta mcp`](Editor-and-AI-Tooling) tool surface for a harness that speaks MCP. |
| `SYNTAX.md` | A short language reference: the [tour](Language-Tour), the [tier model](Dev-Tiers), the [conventions](Conventions), and an index of every remaining guide page. |

`SYNTAX.md` indexes the guide rather than reproducing it, and [`noeta docs`](#noeta-docs) searches the full text on demand. It is assembled from the same embedded pages as that command, so it matches the installed compiler; delete it and re-run `noeta init` after an upgrade to refresh it.

A fresh directory also gets `git init`, skipped inside an existing repository or with `--no-git`.

Re-running `noeta init` inside a package it already scaffolded is additive. Every missing file is created and every existing one, the manifest included, is left byte-identical; a run with nothing left to create says so and exits 0. Once a `noeta.toml` exists, `--name` is ignored with a warning, and renaming the package means editing the manifest.

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

Formats `.noe` source into the canonical style, the same layout however the code was written, as `gofmt` and `rustfmt` do. Before anything is written the formatted output is re-parsed and compared to the original, so a reformat cannot change what a program means. A file whose comparison fails is left untouched, as is a file that does not parse.

```text
noeta fmt [PATHS...]   # format files, or every .noe under a directory, in place (atomic)
noeta fmt --stdin      # read source on stdin, write the formatted result to stdout
```

| Flag | Effect |
|---|---|
| `--check` | Write nothing. List each file that is not already formatted and exit 1 if any exist (CI). |
| `--diff` | Write nothing. Print a unified diff of the pending reformat and exit 1 if any diff is shown (CI). |
| `--stdin` | Read source on stdin, write the formatted result to stdout (format-on-save). |
| `--parens <remove\|add>` | Override the `[fmt] parens` policy for redundant control-flow header parens. |
| `--semicolons <remove\|add\|preserve>` | Override the `[fmt] semicolons` policy. |

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

With `wrap = false`, the default, the formatter keeps the line breaks you wrote and normalizes indentation, spacing and blank lines. Trailing `;` and comments are preserved under either setting.

With `wrap = true` the layout is re-derived from `line_width`. An over-width collection, argument or parameter list, pipeline (`|>`), binary chain (`a + b + c …`), method chain (`a.b().c() …`) or union type (`A | B | C`) breaks one element per line, and a wrapped list gains a trailing comma. Anything that fits stays on one line.

Editors format through the same engine. The VS Code extension turns on format-on-save and format-on-type for `.noe` files by default, the latter reformatting a block when you type its closing `}`.

To keep a region exactly as written, such as a hand-aligned table or a generated block, wrap it in `// fmt: off` and `// fmt: on` markers on their own lines. Everything between them passes through byte-for-byte, and formatting resumes after `// fmt: on`. An unmatched `// fmt: off` disables formatting to the end of its scope.

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

Loads, type-checks, and executes a `.noe` file on the bytecode VM against the real host: real `env` and `args`, real-disk IO, and a per-isolate async runtime. Any sibling `.noe` modules the entry file `use`s are resolved and merged automatically (see [Modules](Modules)).

**Options**

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Activate a dev-tier for this run, so `--tier debug` compiles in `@debug { … }` blocks. Repeatable. |
| `--target <NAME>` | Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`. |
| `--no-cache` | Bypass the [startup cache](#the-startup-cache) for this run, reading no cached compile and writing none. Same effect as `NOETA_NO_CACHE`. |
| `--jit-stats` | After the run, print a summary to stderr of what the Tier-1 JIT compiled and why anything bailed or was declined (see [The Virtual Machine](The-Virtual-Machine#tier-1--the-jit)). |

The active-tier set is the target's live tiers unioned with any `--tier` flags, resolved before loading, so a bad target fails fast. The default set is empty, which strips every `@test`, `@bench`, `@doc` and `@debug` block and runs the program as written. See [Dev Tiers](Dev-Tiers).

**Passing arguments to the program**

Everything after a `--` separator is passed straight through to the program, which reads it with `args.all()`:

```console
$ noeta run app.noe -- --verbose input.txt
```

The `--` protects hyphen-prefixed values from being parsed as `noeta`'s own options. `args.all()` reports the program path as the first element (argv[0]) followed by these arguments. That is the same vector the program sees when shipped as a `noeta build --exe` binary and invoked directly (`./app --verbose input.txt`), so the same code serves both.

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

Compiles a program to a standalone artifact instead of running it, through the same front end and [startup cache](#the-startup-cache) as `noeta run`.

With no emit flag the artifact is a `.noeb` bundle: versioned bytecode that `noeta run app.noeb` executes directly, so a program ships without its `.noe` source.

| Flag | Effect |
|---|---|
| `--out <PATH>` | Where to write the artifact. Defaults to the input path with a `.noeb` extension, or with its extension stripped under `--exe` (`app.noe` → `app`). |
| `--exe` | Emit a self-contained executable that bundles the bytecode with the runtime, launchable directly. |
| `--native` | Emit an ahead-of-time-compiled **machine-code** binary (via the AOT backend), with dead-code elimination stripping unused stdlib rings. Requires a C toolchain (`cc`). |
| `--wasm` | Emit a single **WebAssembly** module (`wasm32-wasip1`) that runs under any WASI runtime: `wasmtime run app.wasm`. See [WebAssembly & the Edge](WebAssembly-and-the-Edge). |
| `--serve` | Emit a **`wasi:http` serve component** (`wasm32-wasip2`): your `server.serve` handler on `wasmtime serve`, Spin, and Spin-class edge clouds. See [WebAssembly & the Edge](WebAssembly-and-the-Edge). |
| `--tier <NAME>` | Activate a dev-tier for this build. Repeatable. |
| `--target <NAME>` | Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`. |

All executable forms see the same `args.all()` vector as `noeta run`, with the program path at argv[0], so the same code serves a source run and a shipped binary.

### Shipped artifacts are lean by construction

A `noeta build --exe` or `--native` artifact carries the VM, the standard library, and the runtime capabilities your dependencies ship, such as a native module or a native tier's handler. The formatter, the language server, the debug adapter and their parsers stay out of it, so a production binary exposes no parser or protocol server it never runs.

`--exe` staples your program onto the lean **`noeta-runner`**, or onto a composed runner carrying exactly the native extensions your app depends on. `--native` links a fresh binary from the AOT runtime. A stapled artifact's argument vector belongs to your program, which exposes no CLI subcommand of its own.

**Deploying a source tree** works the same way. Ship the `.noe` sources, and run them with `noeta-runner app.noe`, which compiles on the fly against that same lean runtime. `noeta run` carries the full toolchain and is the development entry point.

Which of your dependencies' code is present is governed by [`noeta.toml` targets](Dev-Tiers). The default target is the minimal one; `--target <name>` layers in more. Package authors keep development-only capabilities, such as a tier-body formatter, out of your shipped binary; see *shipping dev capabilities* in [Native Extensions](Native-Extensions).

## `noeta check`

```text
noeta check [PATH]
```

Parses and type-checks without running or building, as the CI and pre-commit gate. It is the `cargo check` and `tsc --noEmit` primitive.

| Argument or flag | Effect | Default |
|---|---|---|
| `PATH` | A directory is walked recursively for `.noe` files, resolving and deduping shared modules. A file is checked with its sibling modules linked in. | `.` |
| `--format <human\|json>` | `json` emits one machine-readable report on stdout for CI, editors and the MCP server. | `human` |
| `--tier <NAME>` | Check with a dev-tier active. Repeatable. | |
| `--target <NAME>` | Check the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`. | |

| Code | Meaning |
|---|---|
| `0` | Checked and clean. Warnings print and leave this code. |
| `1` | At least one error-severity diagnostic, or a tier-activation error. |
| `2` | The check could not run: an unreadable file, a dependency graph that would not resolve, or no `.noe` files under `PATH`. |

**It covers dev-tier blocks too, with no `--target`.** Each file is checked once as it ships, with every `@test`, `@bench` and `@debug` block stripped, and then once per code tier its own blocks name. That second pass compiles the exact shape `noeta test` and `noeta bench` do, so a green `noeta check` is followed by a `noeta test` that compiles. The summary names what it looked inside, and the JSON report carries the same list as `tiers_checked`:

```console
$ noeta check .
checked 3 files (tiers: debug, test): 0 error(s), 0 warning(s)
```

`--tier <NAME>` and `--target <NAME>` select a shape explicitly, checked as one program the way that build would compile it. The per-tier sweep then covers whatever the selection left out. See [Dev Tiers](Dev-Tiers#checking-is-not-building) for why one tier at a time.

## `noeta docs`

```text
noeta docs <QUERY>...              # rank this guide's sections against a query
noeta docs --page <SLUG>           # print one page
noeta docs --page <SLUG>#<SECTION> # print just one section of it
noeta docs --list                  # every page, slug and title
noeta docs … --limit <N>           # how many search results to print (default 10)
noeta docs … --format json         # machine-readable hits, page, or index
```

The whole guide is compiled into the binary, so it searches with no network and no repository beside it, and it documents the toolchain you are running.

```console
$ noeta docs packed struct --limit 2
1. Fixed-Width Ints & Packed Types › Packed value types — `@packed`
   The `@packed` directive marks a **struct** as a *packed value type*: a `List` of it is stored as a flat, unboxed, contiguous numeric buffer rather than an array of heap-object pointers. This is a pure…
   noeta docs --page Fixed-Width-Integers#packed-value-types--packed

2. Fixed-Width Ints & Packed Types › `bytes` — serialize a packed list
   @packed struct V3 { x: f32  y: f32  z: f32 }
   noeta docs --page Fixed-Width-Integers#bytes--serialize-a-packed-list

2 results.
```

Every hit prints the command that reads it. Prefer the `#section` form, since a guide page runs to hundreds of lines and a section is a small part of one.

A page resolves by slug or title, exactly or by substring, so `--page types` finds `Type-System`. Asking for a section that does not exist lists the ones that do.

Search is BM25F over heading-delimited sections. A term is weighted by where it appears, whether page title, heading or prose, and discounted by how many sections use it, so a query's rare word decides the answer. Identifiers are matched whole and by their parts, so `read_line_async`, `try_parse` and `E0059` each find the sections that name them.

| Code | Meaning |
|---|---|
| `0` | A hit, a page, or the index. |
| `1` | Nothing matched, or the page does not exist. |
| `2` | No query, `--page`, or `--list` was given. |

The corpus, the ranker, and the `#section` addressing are shared with the editor's docs browser (`noeta lsp`) and the [`docs_search`/`docs_get`](Editor-and-AI-Tooling#the-agent-surface-noeta-mcp) tools `noeta mcp` serves, so the three surfaces answer alike.

## `noeta explain`

```text
noeta explain <CODE>         # what a diagnostic code means, and how to fix it
noeta explain --all          # the whole catalog, grouped
noeta explain … --format json   # the machine-readable catalog
```

Every diagnostic the toolchain renders carries a stable `E0xxx` code, and this command looks one up without leaving the terminal. Spell the code `E0059`, `e0059`, or a bare `59`.

```console
$ noeta explain E0059
E0059 — shadowed binding (error)

A binder reuses a name that already means something in scope.

One name means one thing per scope stack. Assignment already reassigns rather than
re-declaring, and an `is` test narrows the same binding, so shadowing is never needed to
express anything — rename the binder.

  more: https://docs.noeta.dev/Syntax-Basics
```

A code the catalog does not know exits `1` and suggests its neighbors, a transposed digit being the usual miss. Naming neither a code nor `--all` exits `2`.

The explanations live with the codes in the compiler, so the three places they surface agree: this command, the [`explain_diagnostic`](Editor-and-AI-Tooling#the-agent-surface-noeta-mcp) tool `noeta mcp` serves to agents, and the generated [Diagnostics](Diagnostics) reference, which is rendered from `noeta explain --all --format json`.

## `noeta expand`

```text
noeta expand [PATH]
```

Prints the Noeta source that **compile-time `@`-directive expansions** produced: the members an extension's `expand` hook generated and the linker spliced into the declaration it decorates (see *directives that generate code* in [Native Extensions](Native-Extensions)). An expansion is code you never wrote, and this is how you read it.

`PATH` resolves as [`noeta check`](#noeta-check)'s does, over the same link, so what prints is what the compiler saw. Each expansion is printed under a Noeta comment naming its cause, meaning the declaration it grew and the directive that grew it with its arguments:

```text
$ noeta expand petstore.noe
// PetStore ⟨@openapi "petstore.yaml"⟩
struct PetStore {
fn list_pets(): List<Pet> { … }
fn get_pet(id: int): Pet { … }
}

expanded 1 declaration
```

The generated source goes to stdout and the summary to stderr, so `noeta expand > expanded.noe` yields a file to check in and diff. A change to the spec then shows up in review as a delta in the code it generates. A program with no expanding directive prints `no directive expansions` and exits 0.

Exit codes match `check`. **0** is success. **1** covers a failed expansion, whether a hook that returned `Err` or generated code that does not parse (reported as **E0062** and blamed on the directive), and sources that otherwise failed to load. **2** means a file could not be read at all.

## `noeta serve` and `--watch`

```text
noeta serve [OPTIONS] <FILE>
```

`noeta serve app.noe` runs the file's top-level setup and then drives its `fn fetch(req: Request): Response` handler as a server (see [std.http](std-http)). The app supplies the handler and the command owns the listener, so the program leaves `server.serve(...)` to it.

| Flag | Effect | Default |
|---|---|---|
| `-p, --port <PORT>` | The TCP port to bind. | `8080` |
| `--host <HOST>` | The bind address. `127.0.0.1` serves local-only. | `0.0.0.0` (all interfaces) |
| `--parallel <N>` | Number of worker isolates to serve across. | `1` |

`fetch` need only *be* a handler. Any top-level binding of that shape serves, so an app that already has a router hands it over directly with `fetch = app.route`. Hot reload follows the definitions the handler reaches rather than the name it was bound under, so either spelling reloads.

**Ctrl-C** drains gracefully. The server stops accepting, finishes the requests already in flight, closes the listener, and exits. A second Ctrl-C forces an immediate stop.

`noeta serve` accepts plain HTTP and presents no certificate, so terminate TLS upstream in a reverse proxy such as nginx, Caddy or a cloud load balancer. Bind to loopback with `--host 127.0.0.1` when a proxy on the same host is the only thing that should reach it. That applies to inbound connections only: your program's outbound calls speak TLS through `std.http`'s rustls-backed client, so `http.get("https://…")` works with no proxy involved.

The same unchanged `fetch` program also deploys to the edge as a `wasi:http` component. See [WebAssembly & the Edge](WebAssembly-and-the-Edge).

### `--parallel` — serving across worker isolates

`--parallel N` serves across N worker isolates for multi-core throughput. The listener is bound once and each worker inherits a cloned handle to it, so the kernel load-balances connections across cores. All workers share the process, drain together on Ctrl-C, and take a hot-reload swap together, so all cores serve the new code without a restart.

Reactive and LiveView state is per-worker, since signals and WebSocket subscribers live in the worker that handled the connection. An app whose source of truth is a database serves on every worker, each opening its own connection and draining its own change notifications. An app whose truth is an in-memory signal wants a single worker until its session state is shared.

### `--watch` — rerunning on save

`--watch` works on any command, including `noeta run --watch` and `noeta test --watch`. A file watcher restarts the command on change, and the startup cache puts a restart at a few milliseconds.

For the tier runners the watch is **impact-filtered**. `noeta test --watch`, and `bench` likewise, diffs each save against the previous run, walks the reverse call graph from the changed definitions, and reruns only the impacted tier fns through the runners' repeatable `--name` filter. Edit a leaf function and exactly its caller-tests rerun; an inert edit such as a comment or whitespace between declarations runs nothing.

The filter is project-wide. Editing an imported module narrows to the entry tests that transitively reach the change, and editing a module function nothing imports reruns nothing at all.

Some edits degrade to a full rerun, with the reason printed: a signature or layout change, a changed top-level statement (fixtures live there), a new or deleted module, a manifest change, and red code. The closure is static, so code reached only through dynamic dispatch is matched best-effort, with method calls on untyped receivers over-approximating by name. Run without the filter occasionally if you lean on reflection-driven dispatch.

### Hot reload — `noeta serve --watch`

`noeta serve --watch` upgrades from restarts to **in-process hot reload**. On each save of the entry file the watcher re-links the project with the same load the boot did, type-checks the whole program, diffs the entry against the running version, and swaps the changed definitions into the live server. It re-links rather than re-reading the one file because a module's path derives from its file: inside a package the entry's `fn fetch` is bound as `pkg.main.fetch`, and a fragment carrying the unqualified name would install beside the running handler.

The type-check is transactional. Red code anywhere in the project never swaps: the old version keeps serving, and the diagnostics go to the terminal and to connected LiveView clients as an error overlay.

State across a swap:

- **Reactive state survives edits.** An unchanged `signal(...)`, `cell` or `synced_signal` binding keeps its value across the swap, and effects are disposed and re-created by the new version.
- **Plain state re-initializes.** Ordinary top-level bindings are re-run from the new source. State that must survive belongs in a signal.

A change the live process cannot absorb, such as a type-layout or signature change or an edit to another project file, falls back to a full restart automatically, with the reason printed (`[hot] restart needed: the layout of type \`P\` changed`). In-flight requests finish on the code they started on either way, and a worker busy with a long request delays a swap rather than missing it.

### LiveView clients during a reload

Connected LiveView clients, running the bundled `server.liveview_js()` shim, are told over their own websocket. A landed swap pushes `{"type":"reload"}` and closes the socket; the page reloads and its fresh session snapshots the preserved signal state, so the browser view carries the same counter through the edit. A rejected edit pushes `{"type":"error",…}`, which the shim renders as a full-screen diagnostics overlay, cleared by the next good frame. Swaps apply immediately even when the server is idle, because the watcher wakes the blocked executor.

After a full restart an open page reconnects and re-syncs state, and keeps its old markup until refreshed, since the reload push needs a live server to send it.

---

## The startup cache

The toolchain **caches compiled bytecode**. The first run of a file compiles and stores it, and subsequent runs of unchanged sources load the stored bytecode and skip the whole front end. It is on by default and needs no build step, so a plain `noeta run app.noe` populates and reuses it.

That front-end work is what the cache buys back. On a 6000-line file, lexing, parsing, type-checking and compiling take about 120 ms, around 95 % of wall time, and a cached start of that same file is roughly 17× faster.

A cached run is byte-identical to an uncached one, verified in the test suite. An entry is keyed by everything that can change the output: the entry file's content, every sibling module's content, the toolchain version, the running binary's build identity, and the active tier set. A source edit, a rebuilt `noeta`, or a different `--tier` or `--target` therefore produces a fresh compile.

`run`, `dump`, and `build` share entries, so a `noeta build` warms the entry a later `noeta run` reads and the reverse. `serve`, `test`, and `bench` are uncached.

Cached artifacts live under `~/.cache/noeta/` (XDG: `$XDG_CACHE_HOME/noeta/`; macOS `~/Library/Caches/noeta`), a per-user private directory. A cache that cannot be read or written for any reason leaves the run compiling from source, since the cache is an optimization rather than a dependency.

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

Inspects or cleans the whole per-user cache. The cache root holds three categories: the `*.noeb` startup-cache entries, **composed toolchains** in `compose/` (an app with native dependencies gets its own built toolchain, easily 1–2 GiB each), and **fetched package sources** in `pkg/`. Everything in it is re-derivable, so deleting any of it is safe and the next run recompiles, recomposes, or refetches what it needs.

| Subcommand | Effect |
|---|---|
| `noeta cache` / `noeta cache ls` | Per-category summary with entry counts and sizes. Compose entries, the multi-GiB ones, are listed individually with size and last-used time. |
| `noeta cache clean` | Remove the composed toolchains other toolchain builds left behind, which this binary can never reuse, reporting the bytes reclaimed. This binary's own compositions are kept. |
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

Composed toolchains are keyed on the running binary's build identity among other things, so every toolchain rebuild and every [`noeta upgrade`](#noeta-upgrade) strands the previous build's compositions. Those are what `clean` reclaims. Only the three cache categories are touched, and anything else living under the cache root, such as bench baselines and watch state, is left alone.

The startup cache stays bounded. Once it exceeds `NOETA_CACHE_MAX_BYTES`, the oldest entries are evicted on the next compile, silently; `noeta cache info` reports the current size and cap.

---

## `noeta repl`

```text
noeta repl [--no-check] [--load <FILE>]
```

Starts an interactive session at the prompt `» `. An entry that is still being typed waits for more input, whether it holds an unclosed `(`, `[` or `{` or is a statement that runs out of input. The delimiter count is over lexer tokens, so braces inside strings and `${…}` are not counted.

### At a terminal

When both stdin and stderr are a terminal, the prompt opens a full line editor: arrow keys and the usual editing bindings, Ctrl-R history search, persistent history, and Ctrl-C to abandon the entry you are typing while keeping the session.

Pressing Enter inside an unfinished block opens a new line within the same entry, so a `class` or `fn` body is edited as one unit and recalled from history as one. A multi-line paste arrives as a single entry the same way. Piped input, such as `noeta repl < script.noe` or a script driving the prompt, reads line by line, and a continuation line shows `… `.

Your entry is syntax-colored as you type, and TAB completes. The coloring classifies with the compiler's own lexer, the same function that highlights code in `noeta doc`. Completion is the engine behind [`noeta lsp`](Editor-and-AI-Tooling), asked about the whole accumulated session rather than the current line, so a type declared three entries ago completes like one written in a file, `x.` offers the receiver's fields and methods, and `@` offers the directives. TAB after `:` completes the meta-commands below, including live binding names for `:drop` and `:type`.

The prompt's color follows the same [`--color`](#global-flags) rule as diagnostics, so `--color never`, `NO_COLOR` or `TERM=dumb` turns off the highlighting and the error underneath it together. History is written to `$XDG_STATE_HOME/noeta/repl-history` (`~/.local/state/noeta/repl-history` by default), or to `NOETA_REPL_HISTORY` if you set it.

### Sessions

`--load <FILE>` opens a **bootstrapped session**. The program runs to completion first, fully checked with imports resolved and output printed, and the prompt then opens with everything it declared and bound live. Entries are type-checked against the app's real signatures, which is the mechanism behind a framework "tinker" command: point it at your app's bootstrap script and explore the running app interactively. A bootstrap that fails to load, check, or run exits with its diagnostics instead of opening a broken prompt. Isolates in a bootstrapped session run cooperatively.

Entries **type-check before running**, against everything the session has accumulated. An entry with a type error prints its `E0xxx` diagnostics and is skipped, so your bindings keep their values and the skipped entry commits nothing. A fully-checked session compiles entries with the checker's optimizations active, meaning full-fidelity `type_of` and packed lists, exactly as `noeta run` does.

`--no-check`, or `:check off` at the prompt, opens the permissive session where type errors surface at run time. Once any entry runs unchecked the session stays on conservative codegen even after checking is re-enabled, and `:reset` earns it back.

A bare expression with no trailing `;` is retried with a `;` appended so its value prints:

```console
» 1 + 2 * 3
7
» xs = [1, 2, 3]
» xs.reverse()
[3, 2, 1]
```

Bindings persist across entries. The REPL keeps a top-level binding alive past its last use, where a compiled program destroys the value there.

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

Disassembles a program to the register-bytecode the VM executes and prints it to stdout, as a debugging aid. It runs the same front end and code generator as [`noeta run`](#noeta-run), from load through type-check and lowering to compilation, so what you see is what would execute. A type error prints diagnostics and exits non-zero as it does under `run`.

Use it to answer how a construct compiles: which opcodes a loop or method call lowers to, whether an in-place reuse fast path fired, or how names and constants are laid out. It is the first tool to reach for when working on codegen or interpreter performance.

**Options.** The same dev-tier activation as `run`:

| Flag | Effect |
|---|---|
| `--tier <NAME>` | Disassemble with a dev-tier active (e.g. `--tier debug` compiles in `@debug { … }` blocks). Repeatable. |
| `--target <NAME>` | Activate the tiers a `noeta.toml` build target makes live. Unioned with any `--tier`. |

**Output.** The module's non-empty side tables come first, covering shapes, packed schemas, and method and destructor tables. Then `=== main ===` and each numbered function prototype (`=== proto N ===`), each listing its parameter and register counts, its constant pool, and its numbered instructions. The text is stable and human-readable, the same form the VM's disassembly snapshot tests assert, so it diffs cleanly across changes.

**Example.** The opening of a recursive `fib`'s prototype, showing the base-case compare and its branch:

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

| Page | Covers |
|---|---|
| [Testing](Testing) | `@test` blocks, `assert`, metadata attributes, isolation, and concurrency. |
| [Benchmarking](Benchmarking) | `@bench` blocks and the timing method. |
| [Dev Tiers](Dev-Tiers) | The tier model and `noeta.toml` build targets. |
| [Documentation](Documentation-and-Tiers) | `@doc` extraction. |

A program or a dependency can also declare its own tier with `@tier`, and `noeta <tier> <FILE>` then dispatches to the declaring package's runner. See [Extending Tiers](Extending-Tiers).

All three accept `--target <NAME>`, which acts as a gate. A named `noeta.toml` target that does not make the tier live leaves the command printing a notice and exiting `0` with nothing run. With no `--target` they always proceed.

`noeta test` bounds every test by a per-test deadline, `--timeout <SECONDS>`, which defaults to `60` and is removed for the run by `--timeout 0`. One test overrides it with `#[std.test.Timeout(N)]`. A test that overruns is reported under its own `TIME` outcome, the rest of the suite still runs and reports, and the run exits `1`. See [Testing → Timeouts](Testing#timeouts) for what the runner can do to a test that will not stop.

---

## Packages: `add`, `update`, `claim`, `publish`, `scope`, `audit`, `advisory`, `key`

Dependencies are declared in `noeta.toml`, under `[dependencies]` with elevated grants in `[trust]`, and resolve automatically on `run`, `build` and `check`. There is no separate install step, and the resolved pins live in `noeta.lock`, which belongs in the repository.

The verbs below are the publisher and consumer trust surface. The trust model behind them, covering attestations, the two signing roots, pinning and downgrade protection, is documented on [Package Provenance](Package-Provenance), and the end-to-end submission walkthrough on [Package Registries](Package-Registries#publishing-to-the-hosted-registry).

### The manifest: `[package]` and dependency forms

`[package]` gives the package its global `company/package` identity and SemVer version. Each `[dependencies]` key is the **import root** you address the package by (`use util.…`), decoupled from its registry identity.

A dependency names exactly one source: a local `path`, a `git` URL (a pinned `tag`, a moving `branch`, or default-branch HEAD), or a registry `version` requirement. Every form resolves to an exact pin recorded in `noeta.lock`, which only [`noeta update`](#noeta-update) moves.

```toml
[package]
name = "acme/app"
version = "0.1.0"

[dependencies]
util = { path = "../util" }                                      # a local source tree
http = { git = "https://github.com/acme/http", tag = "v1.2.0" }  # git, pinned to a tag
json = { version = "^1.2", package = "acme/json" }               # registry, by SemVer requirement
```

[The `noeta.toml` Manifest](Manifest) is the complete reference, covering every table and key, the scope (array) dependency form, editions, and the exact rules the parser enforces.

To develop a dependency against this app, a root-manifest [`[patch]` table](Manifest#patch--dev-time-path-overrides) re-points a package identity at a local tree for the whole graph. It needs no `[dependencies]` edit, adds no lock churn, and prints a notice on every resolve.

### `noeta add`

```text
noeta add <company/pkg>                                                        # registry, current version
noeta add [KEY] (--path <DIR> | --git <URL> --tag <TAG> | --version <REQ>) [--package <company/pkg>]
```

Adds a dependency to the nearest `noeta.toml` through a format-preserving edit that keeps comments and ordering, then resolves the graph so `noeta.lock` reflects it. After resolving, the **committer signal** flags a newly-pulled release whose history introduces committers new to that repo.

| Argument | Meaning |
|---|---|
| `KEY` | The import root, the name you write after `use`. Derivable from `--package`'s package half, or from a `--path` dependency's own `[package]` name. A key that renames the package's declared root is warned about; one that would capture a built-in root (`std`, `noeta`, `core`) is refused. |
| `--path <DIR>` | A local source tree, relative to the manifest. |
| `--git <URL> --tag <TAG>` | A git repository pinned to a tag. |
| `--version <REQ>` | A registry SemVer requirement, written verbatim: `--version "^0.2"` means exactly `^0.2`. |
| `--package <company/pkg>` | The registry identity, meaning on each source form what [the manifest reference](Manifest#dependencies--what-the-package-builds-against) says it means. |

Give exactly one source, or none. The identity may be the positional argument or `--package`: a `/` cannot occur in an import-root key, so `noeta add para/cli` is unambiguous and derives the key, here `cli`, where `noeta add para --package para/cli` names the key itself.

**A registry dependency needs no source.** Given a package identity and none of `--path`, `--git` or `--version`, `add` asks the registry that serves that scope for the package's current version and writes a caret requirement for it, so `noeta add para/cli` against a 0.2.0 package writes `{ version = "^0.2", package = "para/cli" }`. The form is `^<major>.<minor>`, or `^0.0.<patch>` in the `0.0.x` range where SemVer lets every patch break. Auto-selection lands on a released, unyanked version, so depend on a prerelease or a yanked release deliberately, with `--version`. A lookup that cannot be answered, whether the registry is unreachable, the package does not exist, or only prereleases do, reports why and leaves the manifest untouched.

`--package` is checked from both sides. On a `--version` dependency it is the identity resolution needs, and the release it resolves to is checked against it, so a served tree that is not the package it was published as never installs. On `--path` and `--git` it is a claim about the tree the source points at, verified against that package's own `[package] name`: before anything is written for `--path`, so a wrong identity leaves the manifest untouched, and once the repo is fetched for `--git`.

#### Widening a key into a scope array

Adding under a key that is **already in the manifest widens it into a [scope array](Manifest#dependencies--what-the-package-builds-against)**, so `noeta add para --package para/aether` followed by `noeta add para --package para/db` binds both packages under one `para` root. The existing value and the new one become the first two members of `key = [ … ]`, and a key that is already an array gains a member. Only that one entry is rewritten and the existing member's text is reused verbatim, so member formatting and trailing comments survive. Re-adding the identical source is refused, since an array with two equal members would resolve one package twice under one root.

### `noeta update`

```text
noeta update
```

Discards the current pins and re-resolves the whole graph, rewriting `noeta.lock`. Every requirement is re-satisfied against the index, and every git ref, whether a moving `branch` or HEAD tip or a tag whose pin drifted, is re-`ls-remote`d and re-pinned to its current commit SHA.

This is the only verb that moves existing pins; a plain `run`, `build` or `check` reproduces the lock. Trust-root changes surface here too. A legitimate maintainer migration, such as a key rotation or a move to keyless, is accepted at the moment `noeta update` re-pins the scope and never mid-build (see [Package Provenance](Package-Provenance#trust-on-first-use-and-downgrade-protection)). The committer signal runs on every changed pin.

### `noeta claim`

```text
noeta claim <SCOPE> [--token <TOKEN>] [--audience <AUDIENCE>] [--domain <DOMAIN>]
```

Claims a registry **scope**, the `company` half of `company/package`, self-service and squat-proof: the scope you can claim is the one whose name matches an identity you prove. Two proof paths:

- **GitHub (default).** In GitHub Actions with `id-token: write` granted, the ambient OIDC token proves the workflow runs under the org or user of the scope's name, with no configuration. On a laptop it falls back to the GitHub device flow, printing a URL and a code you authorize in a browser. The hosted registry's OAuth client id is built in; `NOETA_GITHUB_CLIENT_ID` overrides it when claiming on a third-party registry with its own OAuth app.
- **`--domain <domain>`.** Prove control of a domain whose first label is the scope, so `acme.dev` for `acme`. The registry fetches `https://<domain>/.well-known/noeta-registry.txt` and expects `noeta-scope=<scope>`.

On success a **publish token** is bound to the scope, either a given `--token` or a freshly minted one printed once. Save it; `noeta publish` reads it from `NOETA_REGISTRY_TOKEN`.

Re-claiming as the same proven identity rotates the token. Any other identity is refused with a conflict, so ownership transfers explicitly or not at all. Reserved namespaces (`std`, `noeta`, `core`) are never claimable, and a first-party scope is claimable only by its designated org.

Claiming targets the hosted registry the scope routes to: `[registries]`, else `NOETA_REGISTRY_URL`, else the built-in default `registry.noeta.dev`. A git-forge registry has no claim endpoint, and neither does `NOETA_REGISTRY_DIR`, the file-backed local index. The OIDC audience defaults to the host of that registry, `registry.noeta.dev` for the default, and `--audience` or `NOETA_REGISTRY_AUDIENCE` overrides it.

### `noeta publish`

```text
noeta publish --git <URL> [--tag <TAG>] [--key | --interactive [--oob]] [--no-docs] [--no-readme]
noeta publish --docs-only
```

Publishes the package in the current directory's `noeta.toml` to the registry. It resolves `--tag`, which defaults to `v<version>`, to its commit SHA, pins that into the index entry, and **signs an attestation** binding *name + version → commit*, so consumers can verify the release independently of trusting the registry.

The release's documentation artifact and its `README.md` ride along by default, and a failure in either warns rather than blocking the publish. `--no-docs` and `--no-readme` skip them.

How it signs. An explicit flag wins, then the environment decides:

| Situation | Result |
|---|---|
| `--key` | Force **key-based** Ed25519 signing with the key file, recorded as `[signed]`. |
| `--interactive` | Keyless via a **browser sign-in** (GitHub, Google or Microsoft; your email is the identity). `--oob` prints the URL and prompts for a code instead of opening a browser. |
| Ambient CI identity (GitHub Actions, GitLab, Buildkite) | **Keyless** (Sigstore), with no configuration, recorded as `[keyless: <identity>]`. |
| A key file exists (`NOETA_SIGNING_KEY` or `./noeta-signing.key`) | **Key-based** Ed25519, recorded as `[signed]`. |
| None of the above | `[UNSIGNED]`. The release resolves, and consumers have nothing to verify it against. |

`--docs-only` regenerates the release's documentation artifact through the same pipeline a publish runs, which for a native package is the composed-toolchain build and its API-reference extraction, and re-uploads it for a version already in the index. It takes no `--git`, publishes no new version, and carries no provenance; the upload needs only the scope's publish token (`NOETA_REGISTRY_TOKEN`). Docs belong to a release, so it refuses when the manifest's version is unpublished. It is how a shelf release whose stored docs are wrong or empty is fixed without a version bump.

A published version is **immutable**, and re-publishing the same version with different coordinates is rejected.

Two manifests are rejected at publish: one with `path` or `git` dependencies, which consumers could not resolve, so depend through the registry; and one with a non-empty [`[patch]` table](Manifest#patch--dev-time-path-overrides), which is a local development-time override that must not travel with a release.

Publishing to the hosted registry needs `NOETA_REGISTRY_TOKEN`, bound by [`noeta claim`](#noeta-claim). The target index is the one the package's scope routes to, exactly as resolution picks it: a `[registries]` mapping for the scope wins, so a private scope stays off the public registry, then `NOETA_REGISTRY_URL`, then `NOETA_REGISTRY_DIR`'s file-backed local index for offline development and tests, else the built-in hosted registry at `registry.noeta.dev`.

### `noeta scope`

```text
noeta scope require-provenance <SCOPE> [--root <key|keyless>] [--off]
```

Manages the publishing policy of a scope you own, authenticated with the scope's publish token (`NOETA_REGISTRY_TOKEN`) against the registry the scope routes to: `[registries]`, else `NOETA_REGISTRY_URL`, else the built-in default `registry.noeta.dev`.

`require-provenance` makes the registry accept only releases under the scope that carry verified provenance, so pushing a release takes more than a leaked publish token. `--root` narrows which trust root satisfies it, `key` for an Ed25519 signature and `keyless` for a Sigstore bundle, with either accepted when it is omitted. `--off` lifts the requirement. See [Package Provenance](Package-Provenance).

### `noeta audit`

```text
noeta audit [PATH]
```

Reports what the resolved dependency tree actually runs: every package and its source, which ones run **native code** or add **CLI commands** through the `[trust]` grants that make that authority active, and each scope's **pinned provenance trust root**, a signing key or a keyless identity. Resolution enforces verification, so a build that succeeds already means every signed release verified, and the audit is the human-readable report of what that trust rests on.

It also cross-references every dependency against the registry's **security advisory feed**, showing each hit's intake tier (`operator`, `publisher` or `imported`) and, for a publisher advisory, its verified signing identity. An imported advisory that carried a CVSS v3.x vector upstream shows its severity band with the base score re-derived client-side from that vector, as in `high (CVSS 7.8)`. Whether a tier fails or warns is set per-project by `[trust.advisories]`, which warns on every tier by default. See [Package Provenance](Package-Provenance#security-advisories-and-intake-tiers).

Exit 0 means checked and clean. The audit exits non-zero on a `fail`-tier advisory hit, and also when the advisory data did not verify: a signature that fails against the pinned feed key, a signed head that does not attest to the advisories served, or a transparency-log leaf that does not match the advisory it claims to be. Nothing was checked in that case, and the reason is printed on stderr.

An unreachable registry stays a note with exit 0, since this whole section is best-effort and being offline is evidence of nothing. `[trust.advisories]` selects which intake tier's hits fail the build, and a feed that never verified has no tier, so the table cannot soften a verification failure.

### `noeta advisory`

```text
noeta advisory publish <ID> <PACKAGE> <RANGES> <SEVERITY> <SUMMARY> [--details …] [--url …] [--patched …] [--withdraw] [--interactive [--oob]]
noeta advisory report  <PACKAGE> <SUMMARY> [--ranges …] [--details …] [--url …] [--reporter …]
noeta advisory reports [--scope <SCOPE>] [--status <pending|promoted|dismissed>] [--all]
noeta advisory promote <REPORT-ID> --id <ID> --severity <SEVERITY> [--ranges …] [--summary …] [--details …] [--url …] [--patched …] [--operator] [--interactive [--oob]]
noeta advisory watch   [SCOPE] [--state <DIR>]
```

`advisory publish` issues or updates a **publisher**-tier advisory for a package in a scope you own. It is keyless-signed with your OIDC identity and sent with the scope's publish token (`NOETA_REGISTRY_TOKEN`), so consumers verify it offline.

`advisory report` files a **public report** against any package, unauthenticated and rate-limited. A report is queued for an operator or the scope owner to triage, and becomes an advisory only through `promote`.

`advisory reports` lists the reports queued for triage, meaning the promotable ones. Without `--scope` it shows the operator triage queue, which needs `NOETA_REGISTRY_ADMIN_TOKEN`; with `--scope` it shows the scope owner's own queue of their packages' reports, which needs that scope's `NOETA_REGISTRY_TOKEN`. It lists the `pending` reports by default, and `--all` covers every status.

`advisory promote` turns a queued report into a signed advisory. The advisory is prefilled from the report's package, ranges, summary, details and url, and finalized with the triaged `--id` and `--severity`. With `--operator` and `NOETA_REGISTRY_ADMIN_TOKEN` it becomes an `operator`-tier advisory. Otherwise the report package's scope owner promotes it into a keyless-signed `publisher`-tier advisory, carrying the same Sigstore bundle a fresh `advisory publish` produces. See [Package Provenance](Package-Provenance#issuing-and-reporting-from-the-client).

### `noeta advisory watch`

```text
noeta advisory watch [SCOPE] [--state <DIR>]
```

`noeta audit` asks whether the advisory feed verifies right now. `advisory watch` asks whether anything has been rewritten since, which is the question that catches a registry silently withholding an advisory from you.

It pins the feed head, the transparency-log checkpoint, and the advisory ids seen for a scope. On each run it verifies that the log only grew, staying append-only, and that every previously-seen advisory is still served. A rewrite, key change, feed rollback, or disappearance exits non-zero.

**With no `SCOPE` it watches every scope your `noeta.lock` pins**, which is what a CI cron wants, since the list keeps itself current as dependencies come and go. Name a scope to watch just that one.

State lives in `--state <DIR>`, defaulting to `watch/` under the noeta cache, as one `<scope>.toml` per scope, so it survives between runs. Commit it or cache it in CI, because a baseline that resets on every run detects nothing.

```yaml
# a daily GitHub Actions job: the whole gate
- run: noeta advisory watch --state .noeta-watch
```

The scope set covers every source. An advisory names a package, so one against `acme/http` applies whether you resolved it from the registry or from git. See [Package Provenance](Package-Provenance#noeta-advisory-watch--suppression-monitoring).

### `noeta key`

```text
noeta key new [--out <PATH>]
```

Generates an Ed25519 keypair for the key-based signing path. It writes the private key, by default to `noeta-signing.key` at mode 0600, which belongs outside git, and prints the public key to register with your registry scope. Reach for it where keyless signing is unavailable, meaning no CI identity and no browser. [Package Provenance](Package-Provenance) covers the trade-off: a keyless identity has nothing to steal and is publicly monitorable, and a key file is a secret you hold.

## `noeta upgrade`

```text
noeta upgrade [--version <vX.Y.Z>] [--check]
```

Self-updates the **toolchain binary** to the latest [release](https://github.com/noeta-lang/noeta/releases). Its counterpart, [`noeta update`](#noeta-update), re-resolves a project's dependencies. This verb resolves the latest release, downloads the binary for this machine, verifies its SHA-256 checksum against the release's `SHA256SUMS`, and atomically replaces the running executable by staging it alongside and renaming over, which is safe while `noeta` runs. A binary that is already current is left alone, and a successful swap prints the old and new versions.

- `--version vX.Y.Z` installs that exact release instead of the latest, and a downgrade is allowed and labeled as such. Every install resolves to a full release: the latest-release resolution excludes prereleases, and an explicit prerelease tag (any `-` suffix, such as `v0.3.0-rc.1`) is refused.
- `--check` reports whether an upgrade is available and changes nothing. It exits 0 when this binary is current and 1 when a newer release exists, so scripts can gate on the exit code.

A `noeta` installed by `cargo install` is refused, since that binary belongs to cargo's bookkeeping; upgrade it through cargo. A platform without release binaries is pointed at [building from source](https://github.com/noeta-lang/noeta#building-from-source). Setting `GITHUB_TOKEN` or `GH_TOKEN` lifts GitHub's unauthenticated API rate limit in CI, and is optional.

## `noeta ide`

```text
noeta ide --vscode [--bin <NAME|PATH>]
```

Installs the **Noeta VS Code extension at this binary's own version**. It downloads `noeta-<version>.vsix` from the toolchain's GitHub release `v<version>`, verifies it against the release's `SHA256SUMS` under the same artifact contract [`noeta upgrade`](#noeta-upgrade) consumes, and installs it with the editor's `--install-extension --force`, so re-running updates the installed extension in place.

The version pinning keeps the extension's grammar and language-server integration matched to the running toolchain. After a [`noeta upgrade`](#noeta-upgrade), run `noeta ide --vscode` again to move the extension in step.

The editor is auto-detected as the first of `code`, `codium` and `code-insiders` found on PATH, and `--bin <name-or-path>` overrides the pick with any binary that speaks `--install-extension`. The `.vsix` is staged in the noeta cache and removed after a successful install. When the editor's install invocation fails, the file is kept and its path printed so you can install it by hand.

The GitHub release asset is the extension's distribution channel, so this verb is the install path for VS Code, for VSCodium, and for offline or version-pinned setups. A cargo-installed or source-built `noeta` has no matching release asset and is refused; install from the source tree at [`editors/vscode-noeta`](https://github.com/noeta-lang/noeta/tree/main/editors/vscode-noeta) instead. Bare `noeta ide` prints a short pointer at `--vscode`.
