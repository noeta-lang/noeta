# Dev capabilities & target-scoped dependencies — lean, safe prod artifacts

**Goal.** Dev-only capabilities a package ships — a tier's **formatter**, a linter, a codegen
helper — must never be **linked** into a production artifact. Not for size (that's the minor
win) but for **security**: a formatter drags in a *parser* (`malva` for CSS, an SWC-sized thing
for JS), and every parser in a prod binary is reachable attack surface. The fix is build-time
exclusion driven by the **target**, so `noeta build --target prod` yields a runtime that contains
only what the app runs, while the dev toolchain keeps everything.

Two mechanisms, addressing the two shapes a dev capability arrives in:

1. **Target-scoped dependencies** — `[targets.<name>.dependencies]` — for a **standalone** dev
   tool (a package that is *only* dev tooling). Present in `dev`, absent in `prod`.
2. **Dev-capability gating** — for a **mixed** package that ships a runtime tier handler *and* its
   formatter in one crate. Target-scoped deps can't split one crate; the tier and formatter come
   as a unit. So the *prod build compiles that crate with its dev-kind capabilities off* (and their
   optional heavy deps uncompiled), automatically, with no per-dependency config by the app author.

## Why this shape

- **The ABI already classifies capabilities by kind.** `Extension::body_formatters()` is
  dev-time; `modules()`/`types()`/a tier handler is runtime. So "is this dev-only?" is answered by
  the *kind* of capability, not an author's label — we don't need a "dev extension" marker, we need
  the prod build to omit dev-kind capability code. (A whole-extension "dev-only" flag is just the
  coarse, standalone-crate case, i.e. mechanism 1.)
- **Runtime gating is the wrong lever.** A formatter registered through the `Extension` vtable is
  reachable and *not* DCE-able; a runtime flag leaves the parser present-and-exploitable in the
  address space. Only *not linking it* removes the risk — a build-time decision.
- **Secure by default.** `prod` excludes dev-kind capabilities unless a dep's tooling is explicitly
  opted back in (which one essentially never does). The developer's mental model stays "I pick a
  target"; package authors handle the feature mechanics once.
- **Reuse `[targets.*]`.** We already have dev/prod targets selecting *tier sets* (`extends` for
  inheritance). Extending them to also govern dependencies and dev capabilities keeps one concept.

## The live gap this closes (D0 — confirmed)

`noeta build --exe` (P-AOT L2) staples the bytecode bundle onto **a copy of the running
executable**; `--native` (L3) does the same plus AOT machine code (they differ only in
interpreted-vs-native execution + the `cc` requirement, not in what is bundled). That running
executable is the full `noeta-cli`, whose `run_cli` installs fmt, **every formatter including
`noeta-css`/malva**, the LSP, and the DAP. So **every** prod artifact — even a plain
`noeta build --exe app.noe` from the stock binary — ships the entire toolchain and its parsers. The
gap is broader than native deps.

**Decision (approach for the prod-artifact fix).** Rather than introduce a new lean runtime binary,
**feature-gate the toolchain's dev capabilities** and build the prod artifact with them off:
- `noeta-cli`'s own dev extensions (`noeta-html`, `noeta-css`→`malva`) move behind a `fmt` feature
  (`run_cli` installs them only when enabled; the crates become optional deps);
- a prod build compiles the artifact (the composed shim, or the stock runtime) **without** `fmt` and
  without any target's dev-only native crates, so the formatter code and its parsers are never
  emitted.
This removes the dev code + parsers from the prod binary (the security core) with far less blast
radius than a separate runtime crate. A dedicated lean `noeta-runner` (mirroring the existing
`noeta-wasm-runner`) stays a possible future refinement, not this arc's path.

## Current state (grounding)

- `Manifest { package, dependencies: BTreeMap<String, Dependency>, targets: BTreeMap<String, Target>, trust }`
  (`crates/noeta-pm/src/manifest.rs`). `Target { extends: Option<String>, tiers: BTreeMap<String,String> }`
  — **no deps under a target yet**. `Dependency` = Path | Git | Registry.
- `resolve_graph` → `ResolvedGraph { native_crates: Vec<NativeCrate>, … }` (`noeta-pm/src/graph.rs`);
  one `noeta.lock`.
- `compose.rs` generates the `noeta-composed` Cargo project (`shim_cargo_toml`/`shim_main_rs`):
  `[dependencies]` = `noeta-cli` + `noeta-native` + one `extN` per native crate, then `cargo build`,
  cache by content hash, delegate. **This is the hook** for both dep-subsetting and feature toggling.
- `Extension` capabilities: `modules`/`types`/`tiers`/`attributes`/`commands` (runtime) vs
  `body_formatters` (dev). `noeta-html`/`noeta-css` are already **formatter-only** crates (the ideal
  separate-crate shape); a *mixed* example package does not yet exist.

## Slices

- **D0 — verify + decide. ✅ (see above).** Confirmed `--exe`/`--native` copy the full CLI (fmt +
  malva). Decided: feature-gate dev capabilities, prod builds with them off. Remaining D0 decision
  carried into D4: the default-target story (which target `run`/`test` use = dev, `build` = prod)
  and whether `--target` is the sole selector. Dev-capability set starts at `body_formatters`.
- **D1 — target-scoped dependencies (manifest).** Parse `[targets.<name>.dependencies]` into
  `Target`; `extends` inherits deps (like tiers). Validate shape; a target's tier provider may now
  name a target-scoped dep. Errors point at the missing/duplicated key. Unit tests over `from_toml`.
- **D2 — resolution & lockfile.** Resolve the **union** of all targets' deps into one `noeta.lock`
  (everything pinned); a per-target *view* selects its subset. Shared deps unify to one version
  (dev-only deps can't conflict with prod). `resolve_graph` gains a target parameter (or returns
  per-target native-crate sets). No churn for manifests without target deps.
- **D3 — dev-capability gating convention + toggle.** Establish the package-author contract: gate
  dev-kind capability impls + their optional heavy deps behind a Cargo feature (`fmt`, or a
  standard `noeta-dev`), e.g. `malva = { optional = true }`, `fmt = ["dep:malva"]`,
  `#[cfg(feature = "fmt")] fn body_formatters(...)`. The composer sets a single build flag
  (`--cfg`/feature) — **on** for toolchain builds, **off** for prod. Document + (stretch) a lint
  that flags an un-gated dev capability in a package meant to ship to prod.
- **D4 — composer per-target build.** `compose.rs` builds the Cargo project **for a target**:
  include only that target's native crates (D2), enable/disable the dev feature per `extN` (D3).
  The prod-artifact path (`build --exe`/`--native`) composes a **runtime** shim (no dev features,
  no dev-only crates) rather than stapling onto the toolchain — the security fix. Content-hash key
  includes the target + feature set so dev and prod artifacts cache separately.
- **D5 — dogfood + docs.** A first-party **mixed** example package: a tier with a native handler +
  a feature-gated formatter, proving prod strips the formatter (`malva` absent from the prod binary
  — assert via symbol/size or a link check) while dev formats it. Migrate `noeta-css`'s `malva` to
  an optional-dep/feature so even the toolchain demonstrates the gate. Docs: the dev/prod target
  model, `[targets.*.dependencies]`, and the "put dev tooling behind the `fmt` feature or in a
  dev-dep crate" guidance for package authors.

## Open questions

- **Prod-artifact composition (blocking D4).** Does `--exe`/`--native` today carry the toolchain?
  Determines whether D4 is a refactor or a from-scratch lean-runtime compose path.
- **Version unification across targets** — same constraint Cargo solves; confirm our resolver
  handles a dev-only dep that is absent from prod cleanly.
- **Feature name / granularity** — one conventional `fmt`/`noeta-dev` feature per package, vs a
  global `--cfg noeta_prod` the ABI keys off. Feature is more Cargo-native (drops optional deps);
  cfg is less author-ceremony. Likely: ABI methods `#[cfg]` on a std feature the composer flips.
- **Repetition** — expected non-issue: `extends` inherits target deps, and the real shape is one
  `dev` (+ maybe `ci extends dev`) and one `prod`. Confirm no per-target duplication is forced.

## Non-goals

- A JS/`<script>` formatter (SWC — heavy; the delegation hook already reaches `"javascript"`).
- Runtime gating of dev *invocation* (prod never calls dev verbs; the concern is *linkage*).
- Reworking `noeta run`'s toolchain (it *is* the dev toolchain; carrying dev tooling there is fine).
