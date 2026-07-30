# Noeta

[![CI](https://github.com/noeta-lang/noeta/actions/workflows/ci.yml/badge.svg)](https://github.com/noeta-lang/noeta/actions/workflows/ci.yml)

> **AI-native, human-first.** A general-purpose language that makes machine-written code checkable and human-readable — strong static types, an agent-native toolchain, single-binary output.

Noeta is a new, general-purpose programming language built from scratch in Rust — currently **alpha**. It reads cleanly and familiarly, but underneath it pairs an ML-grade type system with a runtime engineered for correctness and speed.

```noeta
enum OrderError { Empty; NegativePrice(index: int) }

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.count() == 0 { return Err(OrderError.Empty) }
    for (i, item) in items.enumerate() {
        if item.price < 0 { return Err(OrderError.NegativePrice(index: i)) }
    }
    return Ok()
}

echo match validate(cart) {
    Ok()   => "ready to ship",
    Err(e) => match e {
        OrderError.Empty            => "cart is empty",
        OrderError.NegativePrice(i) => "item ${i} has a negative price",
    },
}
```

## Why it's interesting

- **AI-native, human-first.** Built for a world where agents write much of the code — the type system makes machine-written code mechanically checkable, and the toolchain speaks agent natively (`noeta mcp`, LSP, structured diagnostics) — but every surface decision answers to human readability first. Code an agent wrote should be code you *want* to review and own.
- **Correct by construction.** Algebraic data types, `Result`-typed errors, exhaustive matching, real generics, and inferred-static typing with `dyn` as the one explicit escape — illegal states don't compile.
- **Fast without a tracing GC.** A register bytecode VM over NaN-boxed values and shape-based objects with inline caches; memory is *compiled* — precise reference counting with in-place reuse and a cycle-collector backstop, no stop-the-world pauses.
- **Value/reference by intent.** `struct` is a value (copy-on-write, structural equality); `class` is a reference (identity, in-place mutation). The same axis decides what's safe to send across an isolate.
- **Real concurrency.** `async`/`await` with structured `concurrent { }` scopes, lazy iterators and generators, and shared-nothing **isolates** with typed channels for true multi-core parallelism.
- **Batteries and tooling.** A layered standard library and a toolchain that runs, checks, builds, formats, tests, benchmarks, profiles, and documents your code — `run`/`build`/`check`/`repl`, `test`/`bench`/`doc`, `fmt`/`profile`, plus `lsp`/`dap`/`mcp` editor & agent servers and a package manager.

> [!NOTE]
> **Status: alpha.** The **language core and tooling are complete and usable** — full syntax, the type system, traits/generics/derives, modules, the standard library, real host IO, concurrency, server-side reactivity (`signal`/`computed`/`effect`), a bundled HTTP server, native ahead-of-time builds (`noeta build --native`), WebAssembly builds and the browser playground (`noeta build --wasm`/`--serve`), a package manager, and the `noeta lsp` / `noeta dap` / `noeta mcp` editor & agent tooling all ship today. LiveView (server-driven UI over WebSockets) ships as the `para/html` package — the first-party `para` package family (html, api, cli, aether, aether_db, db, p2p) lives in its own repositories under the noeta-lang org and is published on the hosted registry at [registry.noeta.dev](https://registry.noeta.dev); still on the roadmap: desktop packaging. Through alpha, anything may change without notice — syntax, stdlib, and file formats included. The [docs](https://docs.noeta.dev) mark the plan-vs-reality boundary everywhere.

## Try it

The quickest taste is the **[playground](https://play.noeta.dev)** — the real toolchain compiled to WebAssembly, running in your browser with nothing to install.

### Install

One line, on Linux or macOS (x86_64/aarch64) — downloads the latest [release](https://github.com/noeta-lang/noeta/releases), verifies its checksum, and installs to `~/.local/bin`:

```sh
curl -fsSL https://noeta.dev/install | sh
```

`--version vX.Y.Z` pins a release; `--to <dir>` (or `NOETA_INSTALL_DIR`) changes the destination. Upgrade later with `noeta upgrade`. At alpha these two platforms are the only ones with prebuilt binaries — anything else (musl-only Linux, Windows, *BSD) builds from source, below.

**macOS note:** the binaries are not Apple-notarized. The installer's `curl | sh` path is unaffected, but if you download a release archive with a browser, Gatekeeper will quarantine it and refuse to run `noeta`; clear it with `xattr -d com.apple.quarantine <path-to>/noeta`. Then:

```sh
echo 'echo "hello"' > hello.noe
noeta run hello.noe     # -> hello
noeta repl              # interactive REPL
```

### Building from source

You need a recent stable Rust toolchain (1.95+).

```sh
cargo build                                 # build the workspace + the `noeta` binary
cargo run -p noeta-cli -- run hello.noe
```

To put a source-built `noeta` on your `PATH`: `cargo install --path crates/noeta-cli`.

## Documentation

- **Website** — **<https://noeta.dev>**
- **Docs** — **<https://docs.noeta.dev>** — the complete language & standard-library reference:
  - [Getting Started](https://docs.noeta.dev/getting-started) and the [Language Tour](https://docs.noeta.dev/language-tour) — learn the language from zero.
  - [Bundled tools](https://docs.noeta.dev/the-cli) — the CLI, test runner, benchmarks, and doc extraction.
  - [Concepts & design](https://docs.noeta.dev/architecture-and-pipeline) — the VM, memory management, concurrency, and every technique under the hood.
- **Playground** — **<https://play.noeta.dev>** — run Noeta in your browser; the real toolchain compiled to WebAssembly, not a transpiler.
- **Registry** — **<https://registry.noeta.dev>** — the hosted package registry.

For contributors: the [developer guide](https://docs.noeta.dev/contributing) walks through the workflow; `ARCHITECTURE.md` (implementation overview), `AGENTS.md` (conventions + the new-feature template), `CONTRIBUTING.md`, and `plans/` (the roadmap and task tracker) live in this repo.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
