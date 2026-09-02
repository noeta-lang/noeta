# The Noeta Docs

Noeta is a general-purpose language with an ML-grade type system: algebraic data types, `Result`-typed errors, exhaustive matching, and real generics. It compiles to a bytecode VM, and the whole toolchain ships as a single binary that speaks LSP, DAP and MCP.

## Start here

1. **[Getting Started](Getting-Started)** — install the toolchain and run your first program.
2. **[Language Tour](Language-Tour)** — the whole language, example-driven, in one sitting.

The [playground](https://play.noeta.dev) runs Noeta in your browser with nothing to install.

## A thirty-second taste

```noeta
struct Item { price: float  qty: int }

enum OrderError {
    Empty
    NegativePrice(index: int)
}

fn total(items: List<Item>): float {
    return items.map(fn(it) => it.price * it.qty).sum()
}

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.len() == 0 {
        return Err(OrderError.Empty)
    }
    for (i, item) in items.enumerate() {
        if item.price < 0 {
            return Err(OrderError.NegativePrice(index: i))
        }
    }
    return Ok()
}

items = [Item { price: 9.99, qty: 2 }, Item { price: 4.50, qty: 1 }]
echo match validate(items) {
    Ok()   => "total: ${total(items)}",
    Err(e) => match e {
        OrderError.Empty            => "empty order",
        OrderError.NegativePrice(i) => "item ${i} is negative",
    },
}
```

```console
$ noeta run demo.noe
total: 24.48
```

That program uses enums with payloads, `Result`-typed validation, an exhaustive nested `match`, a closure over a typed list, and an interpolated string. The [Language Tour](Language-Tour) walks through each of them.

## Browse the docs

### Onboarding
Learn the language from zero.
- [Getting Started](Getting-Started)
- [Language Tour](Language-Tour)
- [Conventions](Conventions) — how the ecosystem names things, and which of those names the compiler enforces
- [Using Packages](Using-Packages)

### Bundled tools
Everything the `noeta` binary does beyond running code.
- [The `noeta` CLI](The-CLI)
- [The `noeta.toml` Manifest](Manifest)
- [Package Registries](Package-Registries)
- [Package Provenance](Package-Provenance)
- [Testing](Testing)
- [Benchmarking](Benchmarking)
- [Dev Tiers](Dev-Tiers)
- [Documentation](Documentation-and-Tiers)
- [Editor & AI Tooling (highlighting / LSP)](Editor-and-AI-Tooling)
- [Debugging (`noeta dap`)](Debugging)
- [Profiling (`noeta profile`)](Profiling)
- [Observability](Observability)
- [WebAssembly & the Edge (`--wasm`, `--serve`, the playground)](WebAssembly-and-the-Edge)
- [Edge Deployment (Fermyon Spin, Fastly Compute)](Edge-Deployment)

### Language reference
One page per topic, holding the exhaustive rules. Read them in order after the Tour, or open one to look a rule up.
- [Syntax Basics](Syntax-Basics)
- [Control Flow & Pattern Matching](Control-Flow-and-Pattern-Matching)
- [Functions & Closures](Functions-and-Closures)
- [Structs, Classes & Enums](Structs-Classes-and-Enums)
- [Generics & Traits](Generics-and-Traits) · [Derives](Derives)
- [The Type System](Type-System)
- [Error Handling](Error-Handling) · [Validation](Validation)
- [Modules & Visibility](Modules)
- [Concurrency](Concurrency)
- [Reactivity](Reactivity)
- [Built-ins (Ring 1)](Standard-Library) — and the generated [standard library reference](Std) for `use std.{…}`
- [Diagnostics (`E0xxx`)](Diagnostics) — every code the toolchain reports, generated from the compiler

### Specialized
Pages for one kind of problem.
- [Fixed-Width Integers, Bitwise & Packed Types](Fixed-Width-Integers) — binary formats, protocol code, bulk numeric data
- [Attributes & Reflection](Attributes-and-Reflection) — `#[…]` metadata and the runtime reflection surface, for framework and codegen work

### Concepts & design
How the implementation works. Noeta runs on a register-based bytecode VM over NaN-boxed values, a shape-based object model with inline caches, compiled precise reference counting with in-place reuse, and a cycle collector.
- [Architecture & Pipeline](Architecture-and-Pipeline)
- [The Virtual Machine](The-Virtual-Machine)
- [Memory Management](Memory-Management)
- [The Type Checker](Type-Checker-Internals)
- [Concurrency Internals](Concurrency-Internals)
- [Performance Techniques](Performance-Techniques)
- [Native Extensions](Native-Extensions)

### Contributing
Build the compiler, run the tests, and add a feature.
- [Contributing & Developer Guide](Contributing)

## Project status

> [!NOTE]
> **Alpha.** The language core and its tooling are complete and usable today: the full syntax, the type system, traits, generics and derives, multi-file modules, a layered standard library, host IO, structured concurrency, the package manager, native and WebAssembly builds (`noeta build --native`/`--wasm`/`--serve`), and the `run`/`repl`/`test`/`bench`/`doc` toolchain, alongside `noeta lsp`, `noeta dap` and `noeta mcp`.
>
> Prebuilt binaries cover Linux and macOS on x86_64 and aarch64; see [Getting Started](Getting-Started#1--install-the-toolchain) for other platforms, which build from source. Until beta, anything may change without notice, syntax and stdlib and file formats included. These docs describe what ships today.
