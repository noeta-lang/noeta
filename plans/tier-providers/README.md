# Tier providers — build targets, one directive mechanism, third-party tiers

*Status: IN PROGRESS (started 2026-07-11, branch `tier-providers`). ✅ T0 (`--target`/`[targets.*]`,
`6457c36f`), ✅ T1 (tier knobs = attribute system, `#[Bench]`, `c6fba9dc`), ✅ T5 (`@doc` adjacency
attachment — hover/symbol headers/runtime docstrings, `a9986539`). REMAINING: T2 `@tier`
declarations (surface syntax awaiting user sign-off), T3 open name set, T4 runner dispatch.*

## Motivation

The dev-tier system (object-model slice 6) shipped complete but closed: four hardcoded tiers
(`test`/`bench`/`doc`/`debug`), a hand-rolled directive-argument schema (`tier_params` in
`noeta-check::tiers`), and a manifest grammar that *validates* third-party tier providers but
drops them after validation — naming a provider changes nothing at runtime. The `tiers.rs`
module doc always promised the second half: "once `@tier` declarations + the package manifest
land, the active set becomes a build profile's resolved provider-map and this constant gives
way to that set." This arc is that second half, plus three convergence decisions ratified
2026-07-11:

1. **Targets, not profiles.** The build-variant noun becomes **target** — a named build recipe
   in `noeta.toml` that decides which tiers are included (`dev` includes all, `prod` strips
   all), with room to absorb platform/artifact selection later (`--wasm`, `--exe` become
   target properties, resolving the industry "target = platform" collision by *subsumption*:
   the platform lives inside the target). This frees the word "profile" for `noeta profile`
   (the execution profiler), killing the CLI's standing word collision.
2. **One directive mechanism.** Tier directive args (`@bench(iterations: 1000)`), decorator
   directives (`@derive(Eq)`), and data attributes (`#[Name("…")]`) converge on the attribute
   system's typed binder. A directive's schema is an attribute declaration — compiler-intrinsic
   for `@packed`/`@derive`/built-in tier knobs, package-declared (`@attribute`) for third-party
   tiers. The semantic rule stays permanent: **a tier directive gates content per target; a
   decorator directive modifies a declaration unconditionally** (stripping `@packed`/`@derive`
   per target would change program semantics — never allowed).
3. **`@doc` attaches.** A `@doc { … }` block immediately preceding a declaration desugars to an
   intrinsic doc attribute *on that declaration* (parse-time, structural); an unattached block
   is the module doc. One representation feeds reflection (`attributes_of`), LSP hover, MCP,
   and a future doc generator.

Third-party runner protocol (decision): **in-process reflected handles** — a tier provider's
runner is an ordinary package function invoked with reflected handles to the activated root
fns (the E0031 `invoke` machinery), not a contributed CLI subcommand fed JSON. Compilation and
white-box activation stay in one place; a runner is just a function. Contributed commands
remain available for exotic out-of-process cases.

## Slices

### T0 — terminology: `[targets]` / `--target`
- `noeta.toml`: `[profiles.*]` → `[targets.*]` (no compatibility alias — v0.1.0 is unpushed).
  `extends` unchanged. `noeta-pm::manifest` renames (`Profile` → `Target`,
  `resolve_active_tiers(entry, target)`).
- CLI: `--profile <NAME>` → `--target <NAME>` on `run`/`build`/`test`/`bench`/`doc` (+ any
  other carrier). Gate semantics unchanged (`test`/`bench`/`doc` no-op when the target does
  not make their tier live). `--tier` still unions.
- `noeta profile` subcommand docstring drops the disambiguation note (no longer needed).
- Docs (`Documentation-and-Tiers.md`, `Testing.md`, `Benchmarking.md`, `The-CLI.md`, others by
  grep) and CLI tests updated.
- Acceptance: workspace green; `grep -ri "profiles\." docs crates` finds no build-variant use.

### T1 — one binder: tier knobs ride the attribute machinery
- Kill `tier_params`/`ArgType` in `noeta-check::tiers`. A tier's knobs are declared as
  attribute schemas (intrinsic declarations for the built-in four; `bench` declares
  `iterations: int`).
- `@<tier>(args) { … }` block args distribute onto contained fns as the corresponding
  attribute (block args = distribution sugar), so a per-fn attribute can override a block arg.
- E0037 folds into the attribute-typing diagnostics path (code may stay for message
  compatibility; the binder is shared).
- Precedence chain (documented + tested): CLI flag → per-fn attribute → block directive arg →
  target option → runner default.
- Acceptance: `@bench(iterations: N)` behaves identically; a per-fn override works; oracle
  corpus unchanged.

### T2 — `@tier` declarations
- A package declares a tier: name, knob schema (T1 attributes), runner entry point (a fn in
  the package). Surface shape to be finalized in-slice; the declaration is exported like any
  other package item.
- The built-in four become std-declared (dogfood), keeping their current runners.
- Acceptance: a fixture package declares a custom tier; the declaration parses, checks, and
  is visible through the loader.

### T3 — open the name set
- `BUILTIN_TIERS` gives way to the target's resolved provider-map: the manifest's
  tier → provider entries resolve against *declared* tiers (std's four + any dependency's
  `@tier` declarations). E0036 validates against that set.
- The checker needs the resolved tier set where it validates `TierBlock` names in place
  (loader already discovers `noeta.toml`).
- Acceptance: a custom tier name in source + manifest is accepted end-to-end; a typo is
  still E0036; a tier named in a target but declared nowhere is a manifest error.

### T4 — runner dispatch
- `noeta <tier> <file>` (generalized runner command) resolves the active target's provider for
  that tier and invokes its runner fn in-process with reflected handles to the activated
  roots (name, span, bound knob values, attrs — the `TierFn` surface, reflected).
- Built-in `test`/`bench`/`doc` keep their commands and become the intrinsic providers'
  runners behind the same dispatch.
- Trust: running a provider's runner is running a declared dependency's code — ordinary
  library trust, no new grant (white-box access belongs to the *tier content*, which is the
  consumer's own code; the runner only invokes).
- Acceptance: the composed-toolchain e2e proves a third-party package providing a working
  custom tier (declare → manifest opt-in → `noeta <tier>` runs its runner over the roots).

### T5 — `@doc` attachment (rides alongside, independent of T2–T4)
- Parse-time: `@doc { … }` immediately preceding a declaration attaches (desugars to the
  intrinsic doc attribute on that declaration); file-leading or unattached block = module doc.
- `collect_docs` keeps working from a bare parse (extractable from broken code).
- Consumers: reflection (`attributes_of`), LSP hover (`documentation` fields stop being
  `None` for documented symbols), `noeta doc` output gains the symbol association.
- Acceptance: hover on a documented fn shows its docs; `noeta doc` output unchanged for
  unattached blocks; parse-only property preserved.

## Non-goals (this arc)
- Platform/artifact keys inside `[targets.*]` (`platform = "wasm"`, `exe = true`) — the
  grammar leaves room; wiring them is the WASM/AOT arcs' business.
- Bench runner UX (`--name`/`--json`, calibration, baselines) — separate follow-up.
- Doc site generation (`noeta doc --package`) — enabled by T5, not built here.
