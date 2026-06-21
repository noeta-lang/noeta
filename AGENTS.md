# AGENTS.md

## Project Overview

This project is a **new programming language, built from scratch in Rust** — a persistent, reactive runtime with a real type system, deployable to any surface (CLI, web, desktop) as a single binary.

The canonical design lives in `docs/resources/` (positioning, architecture, syntax, implementation plan, cross-reference). The implementation overview is in `ARCHITECTURE.md`. The work tracker is `plans/` (start at `plans/roadmap.md`).

> [!NOTE]
> **Current milestone: M0 (walking skeleton)** — a tree-walking interpreter for a growing subset of the language. Crate prefix `lang-` and binary name `lang` are placeholders pending the real language name.

## The compilation pipeline (M0)

```
source ─► lang-lexer ─► tokens ─► lang-parser ─► AST (lang-ast) ─► lang-eval ─► RunResult
                                   (lang-diagnostics renders every stage's typed Diagnostics)
```

Each stage is its own crate with explicit input/output types and no hidden shared mutable state, so a change is local to one crate and verifiable by that crate's tests.

## Crate map (M0) — where each change goes

| Crate | What it does (in → out) |
|---|---|
| `lang-span` | Spans/source map (shared vocabulary). |
| `lang-diagnostics` | The one error catalog (`DiagnosticCode`, stable `E0xxx`) + the single `ariadne` renderer. Add a new diagnostic here. |
| `lang-ast` | AST node types (pure data, every node carries a `Span`) + `SyntaxKind`. Add a new node here. |
| `lang-lexer` | Source → tokens (`logos`). |
| `lang-parser` | Tokens → AST (`chumsky`, error recovery). |
| `lang-eval` | AST → `RunResult` via `trait Backend`. Add evaluation behavior here. |
| `lang-builtins` | The M0 prelude. |
| `lang-conformance` | The test harness (`// expect:` runner, JSON, `--stage`/`--file`). |
| `lang-cli` | The `lang` binary (`run`/`repl`/`test`). |

Deferred to later milestones (do **not** stub now): `checker`, `bytecode`, `vm`, `gc`, `runtime`, `server`, `lsp`, `stdlib`, and `salsa` integration.

## The new-feature template (the standard shape of a change)

A language feature is added as a **vertical slice** in this order — see `plans/m0/` for per-feature task files:

1. **Grammar / AST** — token(s) in `lang-lexer`, node(s) in `lang-ast`, production in `lang-parser` (keep surface sugar as its own AST node).
2. **Checker rule** — n/a in M0 (no type checker yet; arrives in M1).
3. **Bytecode** — n/a in M0 (tree-walker only; the VM arrives in M1).
4. **Eval op** — evaluation in `lang-eval`.
5. **Conformance cases** — `tests/conformance/**.lang` with `// expect:` headers, including negative/error cases.
6. **Snapshot update** — `insta` snapshots (tokens / AST / rendered diagnostics), reviewed, never blind-accepted.

**The iron rule: every feature or fix lands with a conformance corpus entry.** Prefer vertical-slice tasks ("implement `~` end-to-end") over diffuse refactors — a slice's done-condition is "its conformance cases pass."

## Naming

- Files: `snake_case.rs`
- Types: `PascalCase`
- Functions/variables: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
  
## Spelling

Use **American English** throughout: code comments, doc comments, and documentation. For example: `sanitization` not `sanitisation`, `behavior` not `behaviour`, `specialized` not `specialised`.

## Enums & Constants Over Magic Strings

Prefer enums and constants over raw string literals. Variant names, format identifiers, provider names, severity levels, and similar fixed sets should be modeled as enums with `Display`/`FromStr` impls (or `strum` derives) rather than compared as ad-hoc strings. 

## Formatting & Linting

- **`cargo fmt --all`** — format the entire workspace with `rustfmt`. All code must be formatted before committing.
- **`cargo clippy -- -D warnings`** — run Clippy with warnings-as-errors. Fix all diagnostics; do not `#[allow]` them without justification.
- No custom `rustfmt.toml` — we use the default `rustfmt` style.

## Design Patterns

- Keep a performance oriented architecture in mind, follow SOLID and keep code DRY.
- Where applicable (ie. not in a data oriented context), take inspiration from DDD to keep code maintainable.
- Avoid god-classes, prefer DI and the strategy pattern.

## Documentation

The following documentation files should always be kept up to date.

- `README.md` serves as a starting point for newcomers, introducing the project, directing users to the wiki and developers to `CONTRIBUTING.md`. If project setup or basic architecture changes, align these files.
- `AGENTS.md` serves as the entry point for coding agents, providing a comprehensive overview of conventions and a very high-level architectural overview so they know where to find more details.
- `CONTRIBUTING.md` serves as the entry point for developers, less heavy on the details than `AGENTS.md` and instead referencing external documents rather than repeating it. 
- `ARCHITECTURE.md` should reflect a thorough technical overview of the system architecture, giving agents and humans necessary technical context.
- `docs/` should comprehensively document the language and all of its features. The content and directory should follow Github Wiki conventions. The target audience for these are developers wanting to find a fresh take on modern DX.
- Each crate should have its own `README.md` that there instead serves as the primary documentation of that crate.

> [!NOTE]
> Markdown should never have hard line wrap.

## Agent Workflow

Follow these practices when working on this codebase as an AI coding agent.

### Before You Start

- Read this file and the module layout to orient yourself.
- Use the codebase — search, read files, check types — before making assumptions about how something works.
- When a task spans multiple modules, plan the full set of changes before editing.

### While Working

- Build after every meaningful change (`cargo build`). Fix errors before moving on.
- Keep the compiler warning-free. Do not introduce new warnings.
- Evaluate whether one should refactor files when they grow large.


### Testing

This project is primarily developed by coding agents, so its imperative that we maintain a high quality and high coverage test suite.

**Coverage.** Measure with `cargo-llvm-cov`, never `cargo-tarpaulin` (tarpaulin can't see across the process boundary, so it reports the subprocess-driven CLI tests in `crates/lang-cli/tests/` as 0% coverage of the `lang` binary).

```sh
cargo llvm-cov --workspace --summary-only   # per-file line/region/function summary
cargo llvm-cov --workspace --html           # browsable report under target/llvm-cov/html
```

Setup if missing: `rustup component add llvm-tools-preview && cargo install cargo-llvm-cov`. Treat a coverage drop on a touched file as a regression to fix, not to ignore.

### Version Control and Continuous Work

Commit as you go and always implement features in full, no stubs or todos unless deferring entire subsystems. When a task is clear, work independently and verify changes using the comprehensive test suite.

This project is currently pre-alpha and not public, so you don't need to worry about pull requests, but do work in branches and worktrees as to not introduce conflicts with other agents working in parallel.

> [!NOTE]
> We follow conventional commits for all commit titles and PRs.

### Before You're Done

- Verify zero compiler warnings (`cargo build` should produce no `warning:` lines).
- Run `cargo fmt --all` and `cargo clippy -- -D warnings`. Fix any issues.
- Run the full test suite and confirm all tests pass.
- If you added new functionality, add tests for it.
- If you made architectural changes or added new features, make sure documentation is up to date.

