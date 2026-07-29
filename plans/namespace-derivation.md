# Namespace derivation — the module path comes from the filesystem

**Decision (2026-07-29).** A module's path stops being declared and starts being **derived from where
the file sits**. `namespace` becomes redundant and is removed in stages. This ledger is the contract
every slice works against; read it before starting one.

## Why

`namespace` is declared in every module and is, in practice, always derivable. Across every
first-party package there is not one exception:

```
para-api/api.noe        → para.api            para-db/query.noe    → para.db.query
para-api/middleware.noe → para.api.middleware para-db/repo.noe     → para.db.repo
para-aether/aether.noe  → para.aether         para-db/sql.noe      → para.db.sql
```

The declaration is not merely ceremony, though — it is **actively broken in three ways** that
derivation fixes by construction. All three verified on `2992a070e`:

1. **The import-root key is silently inert for most packages.** `reroot_path` rewrites a dependency
   module's namespace only when its *first segment* equals the package's root segment
   (`ScopeRoot::Package` → the package half, `cli` for `para/cli`). Its doc comment states the
   assumption: *"The `namespace` only ever leads with the package's own root."* `para/cli` declares
   `namespace para.cli`, leading with the **scope** half, so the rewrite never fires. Keying the real
   `para/cli` as `mycli`: `use mycli.cli.run` → *"no module `mycli.cli` in this project"*, while
   `use para.cli.run` resolves. The manifest documents the key as *"the import root — the name you
   write after `use` — decoupled from the package's real identity"*. For any package whose namespace
   does not lead with its package half, that decoupling does not exist, and the package's **internal**
   name is its API — so a rename inside the package breaks consumers. `use para.cli.{…}` works today
   only because the convention keys it `para` and the literal namespace also starts with `para`.

2. **Subdirectories do not work in an app.** `src/deep/nested.noe` is invisible to `src/main.noe`
   ("no module `Deep.Nested` in this project"). A *dependency package* **is** walked recursively —
   verified: a path dependency's `inner/deep.noe` declaring `acme.lib.deep` resolves fine. So the app
   scan being flat is an inconsistency, not a consequence.

3. **Two files claiming one namespace fail silently.** The second file's exports vanish and the error
   blames the *importing* file (`module Shared.Pieces has no export two`). `Modules.md` says two
   siblings "must not" do this; nothing enforces it.

A namespace also need not relate to its path at all — `src/helper.noe` declaring
`namespace Totally.Unrelated.To.The.Path` compiles and runs.

## The derivation rule

**Module path = key prefix + relative path from the package root.**

- **Relative path**, directories and file stem, `/` → `.`, **case preserved verbatim**
  (`Helpers/URI.noe` → `Helpers.URI`).
- **The package-named root file collapses into the prefix**: a stem equal to the package's root
  segment contributes nothing (`para-cli/cli.noe` → prefix alone). This is already the convention
  (`api.noe` → `para.api`, `middleware.noe` → `para.api.middleware`).
- **Key prefix**:
  - plain `[dependencies]` entry → the **key** (`mycli = { … }` → `mycli.…`);
  - **scope-array** member → `{key}.{package root segment}` (`para = [{ package = "para/db" }, …]`
    → `para.db.…`).
- **The app's own modules** take the app package's root segment as prefix
  (`local/dirscan`, `src/human.noe` → `dirscan.human`).

This reproduces **every existing namespace exactly** — `para/cli` keyed `para` is a plain entry, so
prefix `para`, and `cli.noe` collapses to give `para.cli`; `para/db` under the scope array gives
`para.db.query`. The migration is a no-op on the ecosystem, and it makes the key *real*: keying
`para/cli` as `mycli` yields `mycli.cli`, which is broken today.

### Case

**Case-sensitive, preserved verbatim.** Not lowercased, and `use` matching is **exact**.

The usual objection (PSR-4's cross-platform wound) does not apply: PHP's autoloader *constructs a
path from the name and asks the filesystem to open it*, which is where case-insensitivity bites.
The loader here **scans a directory and matches derived strings** (`read_dir_modules`), so the
filesystem's case rules never enter the comparison and a mis-cased `use` fails identically on every
platform, at check time. Case-sensitivity is also the rule everywhere else in the language
(`Uuid` ≠ `uuid`), so this keeps one rule end to end rather than making module segments fuzzy while
the imported item stays exact.

Two files differing only in case are distinct modules on Linux and cannot coexist on
macOS/Windows — that is git's problem, it already breaks such repos at checkout, and it fails
loudly. Lowercasing would be *worse* here: it would silently merge them into one name on Linux.

**Convention, not rule:** document lowercase single-word stems (what every existing module already
uses). Do not enforce it in the compiler. A stem that is not a legal identifier segment
(`my-utils.noe`) **is** an error, with a rename hint — no silent `-`→`_` mapping, which would
recreate two-spellings-for-one-thing.

## Slices

Each is a branch + worktree off `main`, own `CARGO_TARGET_DIR`, own commits. **Agents never merge
and never push** — the coordinator does both.

| # | Slice | Depends on | Notes |
|---|---|---|---|
| A | **Loader: derive the module path.** Derivation rule above; `namespace` still *accepted* but must **agree** with the derived path (disagreement = a new error naming both). Recursive app scan, matching the package walk. Collision diagnostic naming both files. | — | The foundation. Full suite. |
| B | **Tier-block `use` of a package module links.** The backlog row below. | — | Independent code path; may conflict textually with A in `link_core`. |
| C | **Corpus + examples migration.** Strip the 79 conformance + 2 example declarations; rename files where a stem disagrees with its declared namespace. | A | Mechanical, large. |
| D | **Docs + scaffold.** `Modules.md`, `SYNTAX.md`, `noeta init`'s template, the `App.Models` examples. Document the convention and the derivation rule. | A | |
| E | **Remove the `namespace` syntax.** Parser, AST, the 23 Rust files referencing `Stmt::Namespace`, fmt, IDE, MCP. | C, D | Last, once nothing declares one. |

`para/*` packages live in **sibling repos** and are the coordinator's to update, not an agent's.

## Slice B: the decision the handoff asked for

**Do the full linking fix.** Not the check-time diagnostic alone.

The workaround (import at top level) is clean but wrong in a specific way: it puts an import that
only `@test` needs into the production unit, which is exactly what block-scoped `use` exists to
avoid. And `noeta check` reporting 0 errors on a program that cannot run is the defect — a
diagnostic that says "this will fail at run time" is an admission that the feature does not work.

**Fallback, if and only if linking proves to need restructuring beyond this slice:** ship the
check-time error instead, and report precisely what blocked linking. Do not ship both.

Prior art to build on, already on `main` (2026-07-28): `qualify::UnitMap::tier_scopes` is a
per-tier-block overlay keyed by the block's span, swapped in by `qualify_stmt_scoped`. That closed
the *qualification* half. The linking half is the same construction one layer down — wherever the
loader collects a unit's imports for the graph, it must also see `tier_scopes`' sources when the
tier is active. Gates already in place for the sibling fix:
`tests/conformance/tiers/block_scoped_use_strips.noe`, two `noeta test` CLI tests, three
`noeta-loader` unit tests.

Repro (three files, no dependencies): package `probe/lib`; `side.noe` = `pub struct Thing { n: int }`;
`entry.noe` whose whole body is `@test { use probe.lib.side.{Thing}  fn t(): void { x = Thing { n: 3 } } }`.
`noeta check entry.noe` → 0 errors. `noeta test entry.noe` → *cannot find type probe.lib.side.Thing
in this scope*. Both import forms fail; the same `use` at top level works; the same shapes over
**std** work inside the block (std resolves through the registry, a package module must be in the
unit graph).

## Gates

Every slice, before reporting done:

```
cargo build --workspace                      # 0 warnings
cargo run -p noeta-conformance
cargo run -p noeta-conformance -- --differential   # full agreement, 0 skipped
cargo test -p noeta-cli --test examples --test doc_samples
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

A loader change decides linking and merging, so slices A and B run the **full** `cargo test
--workspace`. Toolchain is pinned at 1.97.0.

## Protocol

- Worktree off `main` under `.claude/worktrees/<branch>`; **absolute paths everywhere** (the shell's
  cwd silently resets to the root checkout).
- Own `CARGO_TARGET_DIR` inside the worktree. **Never `/tmp`** (14 GB tmpfs). Set
  `CARGO_BUILD_JOBS` — divide cores by the number of concurrent agents.
- **Commit every green slice.** Agents have been lost mid-run here and uncommitted work died.
- Never `git stash` (shared across worktrees), never `git add -A` (sweeps other sessions' drift —
  stage by path), never push, never merge.
