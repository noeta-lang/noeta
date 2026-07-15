# Editions — compiler arc (per-package edition application)

**Status:** machinery **complete**; the whole toolchain is edition-aware. **S0** (`1d6cca5a`), **S1**
(`d51aa472`), **S2** (`2e766517`) done; the S3 **mechanism is proven then reverted** (a throwaway `E2027`
+ divergence passed the acceptance test — we are not shipping a `2027` edition yet); the **tooling arc
T1–T5** (below) makes every tool — batch (`run`/`test`/`bench`/`prof`/`dap`/REPL), `fmt`, and the salsa
IDE stack (LSP/IDE/MCP) — honor each package's edition. **Decision:** *not* shipping a real edition
divergence before the language's first release (no reason to), so **S3-proper and S4 are deferred** until
a real language change is chosen — that is the only remaining work, and it is language design, not
plumbing. Prerequisite **done** (editions resolution-side): each package's edition is validated
(`Edition::parse`), pinned per package in `noeta.lock`, and **carried to the loader** on
`DepPackage.edition`. This plan is the compiler half — making that per-package edition actually *change
how each package compiles*. The machinery is complete and validated end-to-end; what remains (S3/S4) is a
real edition divergence + its diagnostics/migration, which is a language-design decision, not plumbing.

**S3 acceptance proof (run then reverted, not committed):** a single graph — app@2026 depending on
lib@2027, where 2027 forbids a `pub fn` named `legacy` — flagged the rule on the *lib*'s declaration and
not the *app*'s, from one check over the one merged program; the **same lib source** at 2026 was clean.
Same bytes, flipped edition, different result ⇒ per-module application (loader `EditionMap` →
`Checker::edition_at`) genuinely works. (Note: the first divergence *candidate* in the plan — "pub fn must
declare a return type" — turned out to be a rule 2026 *already* enforces, so it isn't a divergence; the
proof used a reserved-name rule instead.) The experiment was reverted so no `E2027` ships.

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
| 2 | **All editions in the cache key** ✅ `2e766517` | 1 | Each dependency's edition is now folded into the startup-cache key (via a pure, unit-tested `key_deps` helper) alongside its identity + sources — a dep-edition bump with byte-identical sources no longer serves stale bytecode. Root edition was already keyed. Test: same sources + different dep edition ⇒ different key. |
| 3 | **First edition-gated behavior — the acceptance test** ✅ *mechanism proven, reverted* | 1 | The per-module application mechanism is **validated**: a throwaway `E2027` + a reserved-name divergence made the app@2026 / lib@2027 acceptance test pass (rule fires on the lib, not the app; same lib source clean at 2026), then was **reverted** so no `2027` edition ships. What remains is choosing a *real* language divergence to gate behind the first shipped edition — a language-design decision, not plumbing. (Candidate note: "pub fn must declare a return type" is **not** viable — 2026 already enforces it.) |
| 4 | **Edition-aware diagnostics + `noeta fix` + docs** | 3 | Diagnostics name the edition whose rule fired; a mechanical migrator for the shipped-edition change (opt-in); the docs page (`docs/Package-Registries.md` sibling, or a section in the language docs) explaining the per-package contract. *(Deferred until a real edition ships — see the tooling arc, which subsumed "all tooling honors editions".)* |

## Tooling arc — every tool honors the package edition (T1–T5, done)

Decided (after the S3 proof) **not** to ship a real divergence pre-1.0, but to make the *whole toolchain*
edition-aware now so a future divergence is honored everywhere, not just on the cached `noeta run` path.
All byte-identical (one edition), verified by the corpus differential + the salsa-vs-direct `session_parity`
oracle.

| # | Slice | Commit | Gist |
|---|---|---|---|
| T1 | **`CheckOptions` entry** | `80d7a500` | One `check_all_with(program, CheckOptions{record_expr_types, registry, editions})` + `check_all_session_with`; the `check_all*` family are presets. Kills a combinatorial `_with_types_and_editions…` API as tools wire editions. |
| T2 | **Batch tools thread `Linked::editions`** | `895c41dc` | `load`/`link` (deps-free path) take a `root_edition` + build the `EditionMap`; `noeta run`/`serve`/`check`/`test`/`bench`, `prof`, `dap`, REPL, MCP-debug check under it (incl. the run/hot/test/bench helper fns). prof/dap/mcp gained `noeta-pm` (already in the CLI closure). |
| T3 | **`noeta fmt`** | `36e0ecb1` | Formatter parses/re-parses (safety gate) + the printer's token lookup all under the file's edition; bare `format_source` wrappers default it. CLI passes `root_edition`; LSP resolves it per document (`edition_of_uri`). |
| T4 | **Salsa `SourceProgram` refactor** | `979ac355` | `SourceProgram` gains an `edition`; `source_program`/`workspace`/`workspace_with_deps` + `DepSources` carry it; every query (`tokens`/`ast`/`checked`/`linked_checked`, single-file + workspace) runs under it; `workspace_editions` is the salsa analogue of `Linked::editions`. Feeders (LSP/IDE/MCP) pass real editions. So the whole IDE stack (diagnostics/hover/completion) is edition-aware. |
| T5 | **Residual analysis paths** | `9a95b71e` | MCP test-run (`workspace_editions`, now public), `--watch` hot-reload + `--watch --impact` (`impact_of_edit` gained an edition). Remaining `check_all` sites are test/synthetic snippets (correctly default). |

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
