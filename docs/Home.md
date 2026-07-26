# The Noeta Docs

> **AI-native, human-first.**
> A general-purpose language that makes machine-written code checkable and human-readable — strong static types, an agent-native toolchain, single-binary output.

Noeta is a general-purpose language for a world where agents write much of the code. An ML-grade type system — algebraic data types, `Result`-typed errors, exhaustive matching, real generics — makes machine-written code mechanically checkable, while every surface decision answers to human readability first. It compiles to a fast bytecode VM and ships from one toolchain that speaks agent natively (`noeta mcp`, an LSP, structured diagnostics).

## Start here

1. **[Getting Started](Getting-Started)** — install the toolchain and run your first program.
2. **[Language Tour](Language-Tour)** — the whole language, example-driven, in one sitting.

Prefer to poke at it first? The [playground](https://play.noeta.dev) runs Noeta right in your browser.

## A thirty-second taste

```noeta
namespace Demo;

struct Item { price: float  qty: int }

enum OrderError {
    Empty
    NegativePrice(index: int)
}

fn total(items: List<Item>): float {
    return items.map(fn(it) => it.price * it.qty).sum();
}

fn validate(items: List<Item>): Result<void, OrderError> {
    if items.len() == 0 {
        return Err(OrderError.Empty);
    }
    for (i, item) in items.enumerate() {
        if item.price < 0 {
            return Err(OrderError.NegativePrice(index: i));
        }
    }
    return Ok();
}

items = [Item { price: 9.99, qty: 2 }, Item { price: 4.50, qty: 1 }];
echo match validate(items) {
    Ok()   => "total: ${total(items)}",
    Err(e) => match e {
        OrderError.Empty            => "empty order",
        OrderError.NegativePrice(i) => "item ${i} is negative",
    },
};
```

```console
$ noeta run demo.noe
total: 24.48
```

## Browse the docs

### Onboarding
Learn the language from zero.
- [Getting Started](Getting-Started)
- [Language Tour](Language-Tour)
- [Using Packages](Using-Packages)

### Bundled tools
Everything the `noeta` binary does beyond running code.
- [The `noeta` CLI](The-CLI)
- [The `noeta.toml` Manifest](Manifest)
- [Package Registries](Package-Registries)
- [Package Provenance](Package-Provenance)
- [Testing](Testing)
- [Benchmarking](Benchmarking)
- [Documentation & Dev Tiers](Documentation-and-Tiers)
- [Editor & AI Tooling (highlighting / LSP)](Editor-and-AI-Tooling)
- [Debugging (`noeta dap`)](Debugging)
- [Profiling (`noeta profile`)](Profiling)
- [Observability](Observability)
- [WebAssembly & the Edge (`--wasm`, `--serve`, the playground)](WebAssembly-and-the-Edge)
- [Edge Deployment (Fermyon Spin, Fastly Compute)](Edge-Deployment)

### Language & standard-library reference
The exhaustive reference for syntax, semantics, and the stdlib.
- [Syntax Basics](Syntax-Basics)
- [Control Flow & Pattern Matching](Control-Flow-and-Pattern-Matching)
- [Functions & Closures](Functions-and-Closures)
- [Structs, Classes & Enums](Structs-Classes-and-Enums)
- [Generics & Traits](Generics-and-Traits)
- [The Type System](Type-System)
- [Error Handling](Error-Handling)
- [Validation](Validation)
- [Modules & Visibility](Modules)
- [Fixed-Width Integers, Bitwise & Packed Types](Fixed-Width-Integers)
- [Attributes & Reflection](Attributes-and-Reflection)
- [Concurrency](Concurrency)
- [Reactivity](Reactivity)
- [Standard Library](Standard-Library)
- [Standard-Library Modules](Standard-Library-Modules)

### Concepts & design
How the implementation actually works — for the curious and the systems-minded. Noeta runs on a
register-based bytecode VM over NaN-boxed values, a shape-based object model with inline caches,
compiled precise reference counting with in-place reuse, and a cycle collector.
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
> **Alpha.** Prebuilt binaries cover Linux and macOS (x86_64/aarch64) — see
> [Getting Started](Getting-Started#1--install-the-toolchain); other platforms build from source.
> The language core and its tooling are complete and usable today: the full syntax,
> the type system, traits/generics/derives, multi-file modules, a layered standard library, real
> host IO, structured concurrency, the package manager, native AOT and WebAssembly builds
> (`noeta build --native`/`--wasm`/`--serve`), and the `run`/`repl`/`test`/`bench`/`doc`
> toolchain, along with `noeta lsp`/`noeta dap` editor tooling and the `noeta mcp` agent surface.
> Still on the roadmap: desktop packaging. Until alpha, anything may change
> without notice — syntax, stdlib, and file formats included. Where a feature is a plan rather than
> a reality, these docs say so plainly.
