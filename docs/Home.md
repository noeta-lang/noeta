# The Noeta Wiki

> **AI-native, human-first.**
> A language for shipping reactive applications as single binaries — web, desktop, or service — with a type system that makes illegal states unrepresentable.

Noeta is a new, general-purpose programming language built from scratch in Rust. It is designed for a world where agents write much of the code — types make machine-written code mechanically checkable, and the toolchain speaks agent natively (`noeta mcp`, the LSP, structured diagnostics) — while every surface decision answers to human readability first. It pairs an approachable, modern surface with an ML-grade type system (algebraic data types, `Result`-typed errors, exhaustive matching, real generics) and a runtime engineered for correctness and speed: a register-based bytecode VM over NaN-boxed values, a shape-based object model with inline caches, compiled precise reference counting with in-place reuse, and a cycle collector.

This wiki is the complete documentation for the language, its tooling, its design, and how to contribute.

> [!NOTE]
> **Project status: pre-alpha.** The **language core** and its **tooling** are complete and stable to use: the full syntax, the type system, traits/generics/derives, multi-file modules, a layered standard library, real host IO, structured concurrency (isolates + channels + async), and the `run`/`repl`/`test`/`bench`/`doc` toolchain all ship today. Server-side reactivity (`signal`/`computed`/`effect`), a bundled HTTP server, native ahead-of-time builds (`noeta build --native`), the package manager (`noeta.toml` + `noeta.lock`, path/git/registry dependencies, native extension packages), the `noeta lsp`/`noeta dap` editor tooling, and the `noeta mcp` agent surface ship today too; the larger vision still on the roadmap — WebSockets/LiveView and desktop packaging — is not yet shipped. Until alpha, anything may change without notice — syntax, stdlib, and file formats included. Where a feature is a plan rather than a reality, this wiki says so plainly.

---

## Start here

If you are new, read these in order:

1. **[Getting Started](Getting-Started)** — install the toolchain, run your first program, meet the REPL.
2. **[Language Tour](Language-Tour)** — a friendly, example-driven walkthrough of the whole language in one sitting.

---

## The five sections

### 1 · Onboarding
Learn the language from zero.
- [Getting Started](Getting-Started)
- [Language Tour](Language-Tour)

### 2 · Bundled tools
Everything the `noeta` binary does beyond running code.
- [The `noeta` CLI](The-CLI)
- [Testing](Testing)
- [Benchmarking](Benchmarking)
- [Documentation & Dev Tiers](Documentation-and-Tiers)
- [Editor & AI Tooling (highlighting / LSP)](Editor-and-AI-Tooling)
- [Debugging (`noeta dap`)](Debugging)
- [Profiling (`noeta profile`)](Profiling)

### 3 · Language & standard-library reference
The exhaustive reference for syntax, semantics, and the stdlib.
- [Syntax Basics](Syntax-Basics)
- [Control Flow & Pattern Matching](Control-Flow-and-Pattern-Matching)
- [Functions & Closures](Functions-and-Closures)
- [Structs, Classes & Enums](Structs-Classes-and-Enums)
- [Generics & Traits](Generics-and-Traits)
- [The Type System](Type-System)
- [Error Handling](Error-Handling)
- [Modules & Visibility](Modules)
- [Fixed-Width Integers, Bitwise & Packed Types](Fixed-Width-Integers)
- [Attributes & Reflection](Attributes-and-Reflection)
- [Concurrency](Concurrency)
- [Reactivity](Reactivity)
- [Standard Library](Standard-Library)
- [Standard-Library Modules](Standard-Library-Modules)

### 4 · Concepts & design (under the hood)
How the implementation actually works, for the curious and the systems-minded.
- [Architecture & Pipeline](Architecture-and-Pipeline)
- [The Virtual Machine](The-Virtual-Machine)
- [Memory Management](Memory-Management)
- [The Type Checker](Type-Checker-Internals)
- [Concurrency Internals](Concurrency-Internals)
- [Performance Techniques](Performance-Techniques)
- [Native Extensions](Native-Extensions)

### 5 · Contributing
Build the compiler, run the tests, and add a feature.
- [Contributing & Developer Guide](Contributing)

---

## The thirty-second taste

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
