# AGENTS.md

## Project Overview

This project is a **new programming language, built from scratch in Rust** — a persistent, reactive runtime with a real type system, deployable to any surface (CLI, web, desktop) as a single binary.

The canonical design lives in `docs/resources/` (positioning, architecture, syntax, implementation plan, cross-reference). The implementation overview is in `ARCHITECTURE.md`. The work tracker is `plans/` (start at `plans/roadmap.md`).

> [!NOTE]
> **Current milestone: M1 (real language core)** — replacing the tree-walker with a register-based bytecode VM over NaN-boxed values, a shape-based object model, refcount+cycle GC, and a salsa-based type checker. M0 (the tree-walking interpreter) is complete and **retained as the differential oracle** (`TreeWalkBackend`), against which the new `VmBackend` is asserted identical. **Thrust A is complete (M1.0–M1.6): the `VmBackend` runs 100% of the M0 corpus differential-identical to the tree-walker (incl. the §14 demo), with deterministic `destruct` in both backends and a trial-deletion cycle collector.** Thrust B is complete: M1.1 (salsa query graph, `lang-db`), M1.7 (the gradual type checker, `lang-types`/`lang-check`, run as a shared front-end), and **M1.8 (traits) are done** — every operator is trait-dispatched (`+ - * / ~` via `impl` blocks, `==`/`!=` via `Equatable`, `< <= > >=` via `Comparable` + the `Ordering` enum, fallible `TryAdd`); the `Index` (`a[i]` over lists/maps/strings), `Length` (`len`), `Display` (`to_string`), and `Iterable` (`for`) protocols dispatch in both backends; `@derive(Comparable)` (structural ordering) and `@derive(ToJson)` (JSON codegen) are the two new-behavior derives, alongside the `@derive(...)` vs `#[...]` sigil split (E0014–E0018); the `#[...]` attribute manifest is a queryable build artifact; and erased generics (`class Box<T>`) parse, check, and run. **M1.9 (modules) is done** — multi-file `lang-loader` resolves `use` to real sibling-module declarations honoring `pub` visibility (E0019/E0020 import errors, E0013 unknown-type), merged into one program both backends run identically, with the module graph expressed as salsa queries (`lang-db` `Workspace`/`linked`, editing one module recomputes only dependents). **M1.10 (layered stdlib) is in progress** — the `lang-stdlib` crate and the Ring 1 *string* surface (`upper`/`split`/`replace`/...) have landed, shared by both backends so the differential holds by construction; Ring 1 list/map/set and the Ring 2 modules remain. See `plans/roadmap.md` for the slice sequence. Crate prefix `lang-` and binary name `lang` are placeholders pending the real language name.

## The compilation pipeline

```
                                                  ┌─► lang-eval ─────────────► RunResult   (M0 tree-walker, the oracle)
source ─► lang-lexer ─► tokens ─► lang-parser ─► AST (lang-ast) ─┤
                                                  └─► lang-compiler ─► Chunk ─► lang-vm ─► RunResult   (M1 VM)
                                   (lang-diagnostics renders every stage's typed Diagnostics)

  The M1 lex→parse→compile path is also exposed as a salsa query graph (lang-db):
  SourceProgram (input) ─► tokens(db) ─► ast(db) ─► checked(db) ─► bytecode(db).
  The checker (lang-check) is a shared front-end: programs with type errors are
  rejected before either backend runs, so both stay observably identical.
```

Both backends implement `lang-backend::Backend`. The conformance harness runs a program through both and asserts identical `RunResult`s — the **differential oracle** (`lang test --differential`). The tree-walker is frozen as the reference; the VM must reproduce it. While the VM compiles only a growing subset, programs it can't lower yet are *skipped* (a climbing coverage %), never failed.

Each stage is its own crate with explicit input/output types and no hidden shared mutable state, so a change is local to one crate and verifiable by that crate's tests.

## Crate map (M0) — where each change goes

| Crate | What it does (in → out) |
|---|---|
| `lang-span` | Spans/source map (shared vocabulary). |
| `lang-diagnostics` | The one error catalog (`DiagnosticCode`, stable `E0xxx`) + the single `ariadne` renderer. Add a new diagnostic here. |
| `lang-ast` | AST node types (pure data, every node carries a `Span`) + `SyntaxKind`. Add a new node here. |
| `lang-lexer` | Source → tokens (`logos`). |
| `lang-parser` | Tokens → AST (`chumsky`, error recovery). |
| `lang-backend` | The `Backend` trait + `RunResult` — the seam both runtimes implement. |
| `lang-eval` | AST → `RunResult` (M0 tree-walker, retained as the **differential oracle**). |
| `lang-object` | Shapes (hidden classes): `Shape`/`ShapeKind`, the flat-slot layout descriptor for records/classes/enums. Pure data; sits *below* `lang-value` (which holds `Rc<Shape>`). |
| `lang-value` | The M1 NaN-boxed `Value` + operator semantics; heap strings, closures, lists/maps, and shaped objects/enums. **The one crate with `unsafe`** (miri-gated). |
| `lang-gc` | Refcount/`__destruct` GC policy over `lang-value`. |
| `lang-bytecode` | The register IR: `Op`, `Chunk` (a function prototype), `Module` (the proto table + shape/method tables), disassembler (pure data). |
| `lang-compiler` | AST → `Module` (returns `Unsupported` outside the VM's current subset). |
| `lang-vm` | `Module` → `RunResult` (M1 frame-based register VM, `VmBackend`). Add VM behavior here. |
| `lang-builtins` | The prelude. |
| `lang-conformance` | The test harness (`// expect:` runner, JSON, `--stage`/`--file`, `--differential`). |
| `lang-cli` | The `lang` binary (`run`/`repl`/`test`). |

| `lang-db` | The salsa (0.27) query graph: `SourceProgram` input → memoized `tokens`/`ast`/`checked`/`bytecode` queries, plus the M1.9.3 module graph — a `Workspace` input (entry + sibling sources) → `linked`/`linked_checked`/`linked_bytecode` (resolve+merge then check/compile the whole program), so editing one module recomputes only dependents. Carries the crate's one small `unsafe` (always-replace `Update` for foreign-result newtypes). |
| `lang-types` | The `Type` lattice (pure data): primitives, `List`/`Map`/`Option`/`Result`, named/`Fn`, the gradual top `Unknown`, and the `?T` → `Option<T>` desugar. Also the built-in trait registry (`BuiltinTrait`/`BUILTIN_TRAITS`) the checker validates `impl`/`@derive` against. |
| `lang-check` | The gradual type checker (`check(&Program) -> Vec<Diagnostic>`), the `checked` query's body. A shared front-end run upstream of both backends: exhaustiveness (E0011), `?`-typing (E0012), arithmetic mismatch (E0007), unknown-type resolution (E0013, lit up once modules give type names referents), trait/derive validation (E0014 unknown trait, E0015 invalid impl), and data-attribute validation (E0017 invalid attribute). |

| `lang-loader` | Multi-file module loading + linking (M1.9): parses the entry `.lang` file and its sibling modules (each declaring a `namespace`), resolves the entry's `use` declarations to real declarations honoring `pub` visibility, and merges them into one `Program` both backends run unchanged. A `use` no module provides falls back to the M0 opaque stub. Import errors: E0019 (private/missing export), E0020 (name collision with another import or a local declaration). |

| `lang-stdlib` | The layered standard library (M1.10). Where a Ring 1 operation is expressible over data represented *identically* in both runtimes, its semantics live here once and both backends call in — so the differential holds by construction. The string surface (`string_method` over `Arg`/`Output`/`Dispatch`: `upper`/`lower`/`trim`/`contains`/`starts_with`/`ends_with`/`split`/`replace`/`repeat`) is the first such; the two backends are reduced to thin value↔primitive glue at their existing built-in-method dispatch site (no compiler/bytecode change). Misuse → `E0007`. |

In progress (M1, see `plans/m1/`): the layered stdlib (`lang-stdlib`, M1.10) — the Ring 1 string surface has landed; Ring 1 list/map/set and the Ring 2 modules remain. The salsa query graph (`lang-db`, M1.1), the gradual type checker (`lang-types`/`lang-check`, M1.7), the trait system (M1.8), and multi-file modules (`lang-loader`, M1.9 — `pub` visibility, E0019/E0013/E0020, and the module graph as salsa queries: `lang-db`'s `Workspace`/`linked`/`linked_checked`/`linked_bytecode`, editing one module recomputes only dependents) have landed. Deferred to later milestones (do **not** stub now): `runtime`, `server`, `lsp`.

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

