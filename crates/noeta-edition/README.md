# noeta-edition

Language **editions** — a per-package pin of the language/ABI semantics a package's source is written against, declared `edition = "2026"` in its `[package]` table.

- **Takes in:** nothing — this is the lowest crate in the pipeline, depending only on `noeta-span` (for `SourceId`).
- **Emits:** the [`Edition`] enum (currently just `E2026`, the default) and its manifest parsing/validation.

Editions exist so a first-party-but-out-of-tree package can evolve on its own cadence yet stay buildable by a newer toolchain: the toolchain keeps understanding every past edition and applies that edition's rules to that package, so a language change that would otherwise be breaking ships behind a new edition instead of splitting the ecosystem — the same contract as Rust editions, opt-in and per-package. Because it depends on nothing, every layer that must name an edition (lexer, parser, checker, package manager) can depend on it without pulling in the package-manager stack; `noeta-pm` re-exports it as `noeta_pm::edition` for source compatibility. There is exactly one edition today, so no two editions yet compile differently — what's real now is that the value is validated (an unknown edition is a manifest error), pinned in `noeta.lock`, folded into the startup-cache key, and threaded into the front-end entry points (`noeta_lexer::lex_in`/`noeta_parser::parse_in`), ready for the day a future edition changes syntax or lints. Granularity is per-package, so a dependency graph may mix editions; applying each package's own edition across a merged program is later work this crate's vocabulary supports.

Part of the `noeta` compilation pipeline (see the repository `ARCHITECTURE.md` and `AGENTS.md`).
