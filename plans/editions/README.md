# Editions — compiler arc (per-package edition application)

**Status:** in progress — **S0 done** (`1d6cca5a`), **S1 done** (`d51aa472`). Prerequisite **done**
(editions resolution-side, branch `editions-resolution`): each package's edition is validated
(`Edition::parse`), pinned per package in `noeta.lock`, and **carried to the loader** on
`DepPackage.edition`. This plan is the compiler half — making that per-package edition actually *change
how each package compiles*. As of S1 the edition is carried all the way into the checker (no longer
inert); the remaining slices (S2–S4) add cache-keying, the first real divergence (`E2027`), and
diagnostics/migration.

**S0 landed:** `Edition` now lives in the zero-dep leaf crate **`noeta-edition`** (decision #1, option
(a)); `noeta-pm` re-exports it. The front-end entry points `noeta_lexer::lex_in` and
`noeta_parser::parse_in` take an `edition: Edition` parameter (defaulted to `E2026` by the `lex()`/
`parse()` wrappers, so ~90 wrapper call sites are untouched); the parser flows it to a `${…}` hole's
re-lex. All real compilation paths (loader/db/fmt/conformance) thread `Edition::DEFAULT`, with `// S1`
markers in the loader where each package's own edition will thread next. The differential oracle is
byte-identical. **Refinement to the original plan:** the checker becomes edition-aware in **S1** (via
a `SourceId`-keyed side-table — decision #2 option (b)), not via a transient S0 param on `check_all`
that S1 would immediately remove. S0's checker footprint is therefore zero; arc scope is unchanged.

**S1 landed:** the carrier is an `EditionMap` (`SourceId -> Edition`) in `noeta-edition`, keyed from a
span via `at(span)`. `noeta-loader`'s `Linked` gains an `editions` map: `link_with_deps`/`load_with_deps`
take the root edition, the entry+siblings parse under it and each dependency's modules under that
package's own edition (`DepPackage.edition` resolved back to `Edition` at the loader boundary);
`parse_clean` threads the owning edition. `noeta-check`'s `Checker` gains an `editions` field +
`check_all_with_editions` + `Checker::edition_at(span)` — the per-declaration edition switch, wired and
unit-tested but consulted by no rule yet (first consumer = S3, so it's `allow(dead_code)` until then).
The runner/CLI pass the root edition into the loader and `Linked::editions` into the checker.
Differentiation is unfalsifiable with one edition (see the S3 note), so S1 is verified structurally (a
two-package graph's `editions` map keys every source) + byte-identically.

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

- ✅ (S0) `noeta_lexer::lex_in` / `noeta_parser::parse_in` now take an `edition` parameter.
- ✅ (S1) the loader parses each package under its own edition and records a `SourceId`-keyed
  `EditionMap` on `Linked`; `noeta_check::Checker` carries it and can recover a declaration's edition
  from its span (`edition_at`). The compile/run path threads it end-to-end.
- ⬜ (S2) `compile.rs` still folds only `manifest::root_edition(file)` into the cache key — a *dependency's*
  edition change may not invalidate its cached bytecode.
- ⬜ (S3) no rule branches on the edition yet — `edition_at` is wired but unread. Until a second edition
  with a real divergence exists, per-module application is unfalsifiable.

So as of S1 `DepPackage.edition` is **carried into the checker**, no longer inert — but nothing *acts*
on it. The day two editions differ, the checker/lowering read `edition_at(span)`; S3 adds the first such
rule (and the second edition that makes it testable).

Under this is an architectural fact: all packages **merge into one `Program` compiled as one unit** —
which is why the edition is a `SourceId`-keyed side-table consulted per declaration, not a switch
between compilation units.

## Decision points (settle first, with the user)

1. **Where does `Edition` live?** ✅ **Settled (S0): option (a)** — the new zero-dep leaf crate
   `noeta-edition`, which the lexer/parser now depend on; `noeta-pm` re-exports it for source
   compatibility. (`DepPackage.edition` is still a `String`; S1 resolves it back to `Edition` at the
   loader boundary.)
2. **Granularity: per-module or per-declaration?** ✅ **Settled (S1): per-source** via option (b) — a
   `SourceId`-keyed `EditionMap` side-table on `Linked`/`Checker`, not a field spread across AST nodes.
   Each module is one source, so this is per-module granularity with zero AST-node churn, recovered from
   any span. (Per-*declaration* divergence within one module was never needed.)
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
| 1 | **Per-module edition at the merge boundary** ✅ `d51aa472` | 0 | `link_with_deps` parses each `DepPackage`'s modules under that package's `Edition` (String→`Edition` resolved at the loader boundary), entry+siblings under the root edition. `Linked` carries a `SourceId`-keyed `EditionMap`; `Checker` gains `editions` + `edition_at(span)` + `check_all_with_editions`; runner/CLI wire it. No divergence (one edition) ⇒ oracle byte-identical; verified structurally (a 2-package graph keys every source). The differentiation proof is genuinely S3's (needs `E2027`) — see the S3 note. |
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
