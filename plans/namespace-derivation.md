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

## Corrections after slices A and B landed (2026-07-29)

Slice A implemented the rule and disproved parts of this ledger. **These override the text above.**

- **"The migration is a no-op on the ecosystem" was wrong for the corpus.** Of the 79 declarations,
  only the 12 in package subdirectories agree; the other ~67 disagree in name or case
  (`App.Models` in `models.noe`, `app.storage` in `models.noe`, `di.container` in `main.noe`,
  `Tmpl` in `tmpl.noe`). **Slice C is a rename-and-strip job, not a strip job.** They are invisible
  today only because those directories are not packages.
- **The collapse rule as stated contradicted its own examples.** Implemented: *drop the stem when it
  repeats the last segment accumulated so far* — which reproduces every example here. The prose
  version ("a stem equal to the package's root segment") would have given `para` and `mycli`.
- **A leading `src/` is not a segment** — dropped, so `src/human.noe` → `dirscan.human`.
- **Derivation needs a package.** With no `noeta.toml` above the entry there is no prefix, nothing
  derives, and declared namespaces stand. This is what keeps the corpus working before slice C, and
  it keeps a bare `noeta run` from swallowing whatever tree it stands in.
- **A plain key derives one segment shallower than a scope array.** `para/api` under
  `para = { package = "para/api" }` gives `para.middleware`, **not** `para.api.middleware`. For
  `para/*` to keep its published addresses, a consumer manifest **must** use the scope-array form.
- **`use mycli.cli.run` works only once the declaration is gone.** A package that still declares
  `namespace para.cli` and is keyed `mycli` is now a loud E0072. The sibling `para/*` repos must be
  stripped before non-conventional keying works there — **coordinator's job, not an agent's.**
- **`reroot_path`'s assumption is not made true for `use` paths.** A dependency's *internal*
  `use para.cli.run` still leads with the scope half and re-rooting will not touch it; intra-package
  `use`s must lead with the package's own root segment (`use cli.run`). Unchanged by derivation.
- **Behaviour change worth knowing:** an app entry inside a package now derives a module path, so its
  declarations carry qualified identities — a tier target reads `app.main.add` where a
  `namespace`-less entry previously gave the bare `add`. Two tests asserted the old bare spelling
  and were migrated.
- **Slice B's decision stands and shipped**: full linking, 40 lines in one function. The check-time
  diagnostic fell out for free (E0019 inside a block). Its brief pointed at `UnitMap::tier_scopes`,
  which was a **dead end** — that holds rewrite maps, not import paths — and "when the tier is
  active" is not implementable in the loader, since activation is downstream and the linked program
  is one memoized salsa value shared by `check`/`run`/`test`.

### Process notes earned the hard way

- **`cargo build --workspace` does not compile test targets.** Stale call sites survive a green
  build; `clippy --workspace --all-targets` is what catches them. Run clippy before believing a merge.
- **Do not run the fast gates and the workspace suite concurrently against one target dir** — cargo
  holds an exclusive lock per target dir, so they queue rather than parallelise, and each queued run
  re-runs the whole suite. Run gates in cost order, suite once at the end.
- **Do not poll with `until pgrep …; do sleep; done`.** Run commands synchronously and read the
  output. Stale poll loops outlived their suites here and generated a stream of phantom completions.
- **`main` moved four times during slice A's review.** Slice C touches 79 files and will conflict
  with almost anything — keep its review window short and merge the moment it is green.

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

## Outcome (2026-07-30)

All five slices merged. `namespace` is retired: refused by the **loader**, not the parser.

**Why the loader and not the grammar.** Rejecting it in the parser broke **96 tests across ~90
files**, and most were checker/IR fixtures that legitimately need a declaration to express qualified
identity — they never touch a package on disk, so the parser is their only seam. `Stmt::Namespace`
also has to stay constructible because `apply_derived_paths` writes each derived path through it.
Every user-facing command routes through the loader, so no real file escapes the refusal; the cost
is that `noeta fmt` still parses one (a file carrying it fails `check` regardless).

### The `para/*` migration recipe — four parts, not one

Getting this wrong twice is what found all four. For each package:

1. **Delete the declarations.**
2. **Lead an intra-package `use` with the package's own root segment** — but *only* for `.noe`
   modules. `use para.db.Connection` and bare `use para.db` name the **native** extension's module,
   whose root is fixed by the Rust crate and has nothing to do with where a file sits. Rewriting
   those points at a module that does not exist.
3. **Bind the parent as a scope array in every example** (`para = [{ path = "../.." }]`). A plain key
   becomes the *whole* prefix, so `para/aether`'s `openapi.noe` derived `para.openapi` instead of
   `para.aether.openapi`. This is also what makes one file appear to derive two different paths in a
   single `check .` — it is analyzed both as its own package and as an example's dependency.
4. **Move a file whose declaration was deeper than its location.** `para/ai`'s four providers sat at
   the package root declaring `namespace para.ai.providers.anthropic`. Deleting the declaration alone
   silently renames four *public* modules; moving them to `providers/` is what preserves the address.

Migrated and checking clean: para/cli, para/html, para/aether, para/api, para/db, para/ai.

### Open

- **A whole-module `use` of a package's own *collapsed root* module resolves to an empty module
  name** when that package is consumed as a dependency — filed in `backlog.md` with a repro.
  Worked around in para/aether with named imports.
- `reroot_program` walks top-level statements only, so a `use` inside a **dependency** module's tier
  block is never re-rooted. Nothing breaks today (slice B's linking is entry-only) — noted by slice F.
- `editors/vscode-noeta/test-workspace` and `examples/aether-rest/noeta.toml` carry unrelated
  migration debt from other slices (a `[targets.*.tiers]` schema change).

## Protocol

- Worktree off `main` under `.claude/worktrees/<branch>`; **absolute paths everywhere** (the shell's
  cwd silently resets to the root checkout).
- Own `CARGO_TARGET_DIR` inside the worktree. **Never `/tmp`** (14 GB tmpfs). Set
  `CARGO_BUILD_JOBS` — divide cores by the number of concurrent agents.
- **Commit every green slice.** Agents have been lost mid-run here and uncommitted work died.
- Never `git stash` (shared across worktrees), never `git add -A` (sweeps other sessions' drift —
  stage by path), never push, never merge.
