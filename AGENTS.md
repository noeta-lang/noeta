# AGENTS.md

## Project Overview

This project is a **new programming language, built from scratch in Rust** — a persistent, reactive runtime with a real type system, deployable to any surface (CLI, web, desktop) as a single binary.

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

