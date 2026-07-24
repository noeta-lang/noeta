# Noeta

[![CI](https://github.com/noeta-lang/noeta/actions/workflows/ci.yml/badge.svg)](https://github.com/noeta-lang/noeta/actions/workflows/ci.yml)

> **AI-native, human-first.**
> A general-purpose language that makes machine-written code checkable and human-readable — strong static types, an agent-native toolchain, single-binary output.

Noeta is a new, general-purpose programming language built from scratch in Rust — currently **pre-alpha**. It reads cleanly and familiarly, but underneath it pairs an ML-grade type system with a runtime engineered for correctness and speed.

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
> **Status: pre-alpha, not public.** The **language core and tooling are complete and usable** — full syntax, the type system, traits/generics/derives, modules, the standard library, real host IO, concurrency, server-side reactivity (`signal`/`computed`/`effect`), a bundled HTTP server, native ahead-of-time builds (`noeta build --native`), WebAssembly builds and the browser playground (`noeta build --wasm`/`--serve`), a package manager, and the `noeta lsp` / `noeta dap` / `noeta mcp` editor & agent tooling all ship today. LiveView (server-driven UI over WebSockets) ships as the `para.html` package; still on the roadmap: desktop packaging. Until alpha, anything may change without notice — syntax, stdlib, and file formats included. The [wiki](docs/Home.md) marks the plan-vs-reality boundary everywhere.

## Try it

Requires a recent stable Rust toolchain (1.95+).

```sh
cargo build                                 # build the workspace + the `noeta` binary
echo 'echo "hello"' > hello.noe
cargo run -p noeta-cli -- run hello.noe     # -> hello
cargo run -p noeta-cli -- repl               # interactive REPL
```

To put `noeta` on your `PATH`: `cargo install --path crates/noeta-cli`.

## Documentation

The complete documentation is the **[wiki](docs/Home.md)** (`docs/`, GitHub-Wiki format):

- **[Getting Started](docs/Getting-Started.md)** and the **[Language Tour](docs/Language-Tour.md)** — learn the language from zero.
- **[Bundled tools](docs/The-CLI.md)** — the CLI, test runner, benchmarks, and doc extraction.
- **[Language & standard-library reference](docs/Home.md)** — the exhaustive reference for syntax and the stdlib.
- **[Concepts & design](docs/Architecture-and-Pipeline.md)** — the VM, memory management, concurrency, and every technique under the hood.

For contributors: **[the developer guide](docs/Contributing.md)** and, in the repo, `ARCHITECTURE.md` (implementation overview), `AGENTS.md` (conventions + the new-feature template), `CONTRIBUTING.md`, and `plans/` (the roadmap and task tracker).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
