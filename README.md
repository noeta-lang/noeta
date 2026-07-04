# Noeta

[![CI](https://github.com/nsrosenqvist/noeta/actions/workflows/ci.yml/badge.svg)](https://github.com/nsrosenqvist/noeta/actions/workflows/ci.yml)

> **A language for shipping reactive applications as single binaries — web, desktop, or service — with a type system that makes illegal states unrepresentable.**

Noeta is a new, general-purpose programming language built from scratch in Rust. It reads cleanly and familiarly, but underneath it pairs an ML-grade type system with a runtime engineered for correctness and speed.

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

- **Correct by construction.** Algebraic data types, `Result`-typed errors, exhaustive matching, real generics, and inferred-static typing with `dyn` as the one explicit escape — illegal states don't compile.
- **Fast without a tracing GC.** A register bytecode VM over NaN-boxed values and shape-based objects with inline caches; memory is *compiled* — precise reference counting with in-place reuse and a cycle-collector backstop, no stop-the-world pauses.
- **Value/reference by intent.** `struct` is a value (copy-on-write, structural equality); `class` is a reference (identity, in-place mutation). The same axis decides what's safe to send across an isolate.
- **Real concurrency.** `async`/`await` with structured `concurrent { }` scopes, lazy iterators and generators, and shared-nothing **isolates** with typed channels for true multi-core parallelism.
- **Batteries and tooling.** A layered standard library and a toolchain that runs, tests, benchmarks, and documents your code — `run`, `repl`, `test`, `bench`, `doc`.

> [!NOTE]
> **Status: pre-alpha, not public.** The **language core and tooling are complete and usable** — full syntax, the type system, traits/generics/derives, modules, the standard library, real host IO, concurrency, and the CLI all ship today. The larger north-star vision — server-side reactivity (`signal`/`computed`/`effect`), a bundled HTTP/WS server, desktop packaging, an embedded LSP, and an agentic MCP surface — is the roadmap, not yet built. The [wiki](docs/Home.md) marks the boundary between the two everywhere.

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

MIT — see [LICENSE](LICENSE).
