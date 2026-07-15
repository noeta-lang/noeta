# Editions — compiler arc (per-package edition application)

**Status:** in progress — **S0 done** (`1d6cca5a`). Prerequisite **done** (editions resolution-side,
branch `editions-resolution`): each package's edition is validated (`Edition::parse`), pinned per package
in `noeta.lock`, and **carried to the loader** on `DepPackage.edition`. This plan is the compiler half —
making that per-package edition actually *change how each package compiles*, which today it does not.

**S0 landed:** `Edition` now lives in the zero-dep leaf crate **`noeta-edition`** (decision #1, option
(a)); `noeta-pm` re-exports it. The front-end entry points `noeta_lexer::lex_in` and
`noeta_parser::parse_in` take an `edition: Edition` parameter (defaulted to `E2026` by the `lex()`/
`parse()` wrappers, so ~90 wrapper call sites are untouched); the parser flows it to a `${…}` hole's
re-lex. All real compilation paths (loader/db/fmt/conformance) thread `Edition::DEFAULT`, with `// S1`
markers in the loader where each package's own edition will thread next. The differential oracle is
byte-identical. **Refinement to the original plan:** the checker becomes edition-aware in **S1** (via
per-declaration AST tagging — the mechanism S1 already specifies), not via a transient S0 param on
`check_all` that S1 would immediately remove. S0's checker footprint is therefore zero; arc scope is
unchanged.

## Where we are

Editions exist as a **seam, not a divergence** (see `crates/noeta-pm/src/edition.rs`). There is one
edition, `E2026`; no two editions compile differently yet. What already works:

- `[package] edition = "2026"` is validated against a closed set — an unknown edition is a hard,
  actionable manifest error.
- Resolution records each package's edition (`LockedPackage.edition`) and pins it in the lock.
- Resolution carries each package's edition to the loader (`DepPackage.edition`, this arc's prerequisite).
- The startup cache key folds in the **root** edition, so a future root-edition change invalidates
  cached bytecode.

## The gap

**Every package is compiled under the *root app's* edition.** The loader merges all packages' modules
into one `Program` and the lexer/parser/checker run over it with no edition awareness:

- `noeta_lexer::lex_in(source, text_tiers)` — no edition parameter.
- `noeta_parser::parse_in(source, tokens, text_tiers)` — no edition parameter.
- `noeta_check` — runs on the merged program; no per-declaration edition.
- `compile.rs` folds only `manifest::root_edition(file)` into the cache key.

So `DepPackage.edition` is carried but **inert**. The day two editions differ in syntax or semantics, a
dependency written for a newer edition would be **silently miscompiled** under the root's edition. The
data model anticipates per-package editions; the compilation path throws them away.

Under this is an architectural fact: all packages **merge into one `Program` compiled as one unit**.
There is nowhere to switch editions mid-program today.

## Decision points (settle first, with the user)

1. **Where does `Edition` live?** ✅ **Settled (S0): option (a)** — the new zero-dep leaf crate
   `noeta-edition`, which the lexer/parser now depend on; `noeta-pm` re-exports it for source
   compatibility. (`DepPackage.edition` is still a `String`; S1 resolves it back to `Edition` at the
   loader boundary.)
2. **Granularity: per-module or per-declaration?** A package's modules all share its edition, so
   **per-module** is the natural unit and matches the merge boundary (each `DepPackage`'s modules parsed
   under that package's edition). Per-*declaration* divergence within one module isn't needed and isn't
   worth the AST-tagging cost. Recommend **per-module**.
3. **What can an edition change — syntax, semantics, or ABI?** Recommend constraining editions to
   **surface syntax + lints + name-resolution/defaults**, never the value ABI or bytecode contract, so a
   mixed-edition graph always links (the Rust-editions guarantee). This keeps editions a *front-end*
   concern (lex/parse/check), not a codegen or runtime one — which is what makes per-module application
   tractable over a merged program.
4. **Migration story.** When a real edition ships, is there a `noeta fix`-style migrator (mechanical
   rewrite old→new)? Recommend yes, but as a **later** slice — the acceptance test (S3) doesn't need it.

## Slices

Pick the lowest-numbered `todo`. Each keeps the differential oracle green: a graph where **every** package
is the same edition must compile **byte-identical** to today (the one-edition world is unchanged); only a
**mixed-edition** graph exercises new behavior.

| # | Slice | Depends on | Gist |
|---|---|---|---|
| 0 | **Relocate `Edition` + thread it (no behavior change)** ✅ `1d6cca5a` | — | Moved `Edition` to leaf crate `noeta-edition` (decision #1a); `noeta-pm` re-exports. Added `edition: Edition` to `lex_in`/`parse_in`, defaulted by the `lex()`/`parse()` wrappers; parser flows it to the `${…}` hole re-lex. Checker deferred to S1 (per-decl tagging, not a transient entry param). Oracle byte-identical. Pure plumbing. |
| 1 | **Per-module edition at the merge boundary** | 0 | `link_with_deps` parses each `DepPackage`'s modules under `DepPackage.edition` (a real `Edition` — resolve the `String`→`Edition` at the loader boundary; the entry + siblings under the root edition. Thread the real edition through the loader's `// S1`-marked sites (entry parse + a new `parse_clean` edition param). Tag each parsed module/declaration with its edition so the checker/lowering can read it — this is where the checker becomes edition-aware. Still no divergence (one edition), so oracle holds — the wiring that *would* diverge is live and tested with a stubbed second edition behind `#[cfg(test)]`. |
| 2 | **All editions in the cache key** | 1 | Fold every resolved package's edition into `CacheKey` (not just the root), so a dependency's edition bump invalidates its cached bytecode. Today only root + dep *sources* are keyed; a dep `noeta.toml` edition change may not be in the hashed source set. Add a key-changes-on-dep-edition test. |
| 3 | **First edition-gated behavior — the acceptance test** | 1 | Introduce `E2027` with exactly **one** concrete, minimal divergence (candidate: a lint promoted to an error, or a defaulting/name-resolution tweak — *not* new syntax, to keep the lexer stable). Prove end-to-end that a `2026` package and a `2027` package in the **same graph** each compile under their own rules. This is the real validation of per-module application; until it exists, S0–S1 are unfalsifiable. |
| 4 | **Edition-aware diagnostics + `noeta fix` + docs** | 3 | Diagnostics name the edition whose rule fired; a mechanical migrator for the S3 change (opt-in); the docs page (`docs/Package-Registries.md` sibling, or a section in the language docs) explaining the per-package contract. |

## Acceptance test (the S3 gate)

A single resolved graph with two packages of different editions compiles, and the edition-gated rule
fires **per package**:

```
app (edition 2026) ── depends on ──▶ lib (edition 2027)
```

…where the S3 divergence (e.g. "unused import is a warning in 2026, an error in 2027") is applied to
`lib` but not `app`, in one build, from one merged program. If that holds — and an all-2026 graph is
still byte-identical to pre-arc output — per-module editions work.

## Risks / notes

- **The merged-program model is the hard constraint.** Per-module edition application means the
  front-end (lex/parse/check) must consult a per-module edition while still merging into one `Program`.
  Keeping editions to front-end-only changes (decision #3) is what makes this possible without a
  per-package compilation boundary or a second `Program`.
- **Oracle discipline:** the differential oracle asserts same-program-same-output *within* an edition.
  Cross-edition divergence is expected and must be tested with explicit mixed-edition fixtures, not the
  general corpus (which is all default-edition).
- **Don't over-build the seam.** S0–S2 add machinery that is inert until S3 gives it a reason to exist;
  land S3's minimal divergence close behind so the plumbing is exercised, not speculative.
- **`git-forge` registries + editions:** `GitForgeIndex::registry_deps` currently *skips* a tag whose
  `noeta.toml` fails to parse — which includes a future-edition manifest. So a git-forge release needing
  a newer toolchain silently vanishes instead of erroring. A small follow-up should distinguish
  "unparseable" from "future edition" and surface the latter (private-registries follow-on, noted here
  because it's edition-specific).

## See also

- `crates/noeta-pm/src/edition.rs` — the `Edition` type + validation (the seam this arc builds on).
- Prerequisite commit: `feat(pm,loader): carry each package's edition to the loader` (branch
  `editions-resolution`).
- Rust editions (the model): opt-in, per-package, never silently changing an existing package's meaning.
