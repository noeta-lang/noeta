# lang *(working title — name TBD)*

> **A language for shipping reactive applications as single binaries — web, desktop, or service — with a type system that makes illegal states unrepresentable.**

A new, general-purpose programming language built from scratch in Rust: a persistent, reactive runtime with an ML-grade type system (algebraic data types, `Result`-typed errors, exhaustive matching, real generics), compiled to a single static binary for any surface — a web server, a desktop app, or a CLI tool — from one codebase. The surface reads cleanly and will look broadly familiar to anyone coming from PHP, JavaScript, or similar; that familiarity is a convenience, not the point.

> [!NOTE]
> **Status: pre-alpha, not public.** The codebase is at milestone **M0 (walking skeleton)** — a tree-walking interpreter for a growing subset of the language. The crate name prefix `lang-` and the binary name `lang` are placeholders pending the real language name.

## What it is (and is not)

- **Is:** general-purpose and application-oriented; a persistent runtime (not request-per-process); reactive at the language level (server-side `signal`/`computed`/`effect`); strongly, statically typed with a gradual on-ramp; single-binary, any-surface.
- **Is not:** a PHP runtime (it does not run PHP/Composer/Laravel), a "better PHP," a framework, or a systems language (embedded/bare-metal/hard-real-time are out of scope).

## Documentation

- `docs/resources/` — the canonical design: [positioning](docs/resources/00-positioning.md), [architecture](docs/resources/01-architecture.md), [syntax](docs/resources/02-syntax.md), [implementation plan](docs/resources/03-implementation-plan.md), [cross-reference](docs/resources/04-cross-reference.md).
- `ARCHITECTURE.md` — technical overview of the implementation.
- `AGENTS.md` — entry point for coding agents: conventions, crate map, the pipeline, the new-feature template.
- `CONTRIBUTING.md` — entry point for developers.
- `plans/` — the in-repo task tracker (roadmap + per-slice work units).

## Building

Requires a recent stable Rust toolchain.

```sh
cargo build                              # build the workspace
cargo test                               # unit + snapshot + conformance + property tests
cargo run -p lang-cli -- run <file>.lang # run a program
cargo run -p lang-cli -- test            # run the language conformance suite
cargo run -p lang-cli -- repl            # interactive REPL
```
