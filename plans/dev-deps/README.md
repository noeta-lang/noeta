# Dev capabilities & target-scoped dependencies — lean, safe prod artifacts

**Goal.** Dev-only capabilities a package ships — a tier's **formatter**, a linter, a codegen
helper — must never be **linked** into a production artifact. Not for size (that's the minor
win) but for **security**: a formatter drags in a *parser* (`malva` for CSS, an SWC-sized thing
for JS), and every parser in a prod binary is reachable attack surface. The fix is build-time
exclusion driven by the **target**, so `noeta build --target prod` yields a runtime that contains
only what the app runs, while the dev toolchain keeps everything.

Three mechanisms, addressing the shapes a dev capability arrives in and where it is shipped:

1. **Lean runtime binary** (`noeta-runner`) — the prod-artifact executor. Depends on runtime crates
   only; the toolchain (fmt/LSP/DAP/MCP/compiler) is *structurally absent* — no dependency edge, no
   `#[cfg]`. `build --exe`/`--native` staple onto **this**, not onto a copy of the full CLI. Native
   analogue of `noeta-wasm-runner`.
2. **Target-scoped dependencies** — `[targets.<name>.dependencies]` — for a **standalone** dev tool
   (a package that is *only* dev tooling). Present in `dev`, absent in `prod`; excluded by the same
   no-dependency-edge principle at the manifest layer.
3. **Dev-capability feature-gating** — for a **mixed** package that ships a runtime tier handler
   *and* its formatter in one crate. Target-scoped deps can't split one crate; the tier and
   formatter come as a unit. So the *prod build compiles that crate with its dev-kind capabilities
   off* (and their optional heavy deps uncompiled) via a Cargo feature — the one place `#[cfg]` is
   irreducible, confined to the package author's own crate.

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

**Decision (approach for the prod-artifact fix) — REVISED.** The prod-artifact fix is a **dedicated
lean runtime binary** (`noeta-runner`, the native analogue of the existing `noeta-wasm-runner`),
*not* feature-gating the CLI. The reason is mechanism: excluding the toolchain by `#[cfg]`-gating it
within `noeta-cli` requires threading feature checks across every dev seam (fmt, LSP, DAP, MCP,
prof) and making each an optional dep — a growing, leak-prone maze where a single missed `#[cfg]`
ships dev code to prod and "is the LSP absent?" is answered only by tracing cfgs. A lean runtime
excludes them **structurally**: a binary crate whose `[dependencies]` simply never names
`noeta-lsp`/`noeta-dap`/`noeta-fmt`/`noeta-html`/`noeta-css`/`noeta-mcp`/`noeta-compiler`. With no
dependency edge, that code is unreachable and never compiled — **zero `#[cfg]` anywhere**, auditable
by reading one Cargo.toml. `noeta-wasm-runner` already proves the shape: ~90 lines around
`noeta_bundle::read` + the VM, *"no compiler, no source,"* target-agnostic.

Feature-gating is **not dropped — it is rescoped** to the one case a lean runtime cannot solve: a
**mixed third-party crate** that ships a runtime tier handler *and* its formatter in one crate. The
app legitimately depends on that crate for its runtime tier, so "don't depend on it" fails; the
formatter must be carved out *within* that crate via a Cargo feature (`fmt = ["dep:malva"]`,
`#[cfg(feature = "fmt")] fn body_formatters`). That is a handful of lines in the *package author's
own* crate — never cfgs threaded across our CLI.

**Drift firewall.** The lean runtime and the toolchain's own execution path share **one**
bundle-execution library (`noeta-vm`/`noeta-backend` over `noeta-bundle`) — exactly as
`noeta-wasm-runner` and the native CLI do today. The runner is a thin *shell* (argv + Host wiring)
over shared guts, so prod-run and dev-run cannot diverge in behavior. Only the dependency shell
differs, which is the entire point.

**Execution model (three rings over the shared core).** The exclusion boundary is *shipped artifact
vs toolchain*, not *run vs debug*:
- **Shipped artifact** — executes a *pre-built bundle*. No compiler, no dev caps. This is the lean
  runtime (`build --exe`/`--native`; and `noeta run app.noeb` should dispatch through the identical
  path).
- **`noeta run app.noe` (from source)** — needs the **compiler** front-half, so it stays in the
  toolchain binary; but it installs only *runtime* capabilities (a tier's handler), never *dev* ones
  (its formatter). It is a dev command on a machine that already has the toolchain, so it is *not*
  its own lean binary (see non-goals).
- **`noeta profile` / `noeta dap`** — run the *same* VM with an Option-gated hook installed (the DAP
  "debugs the PROD VM"); zero-cost when absent. Profiling/debugging therefore observe identical
  execution semantics and are toolchain-only surfaces.

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
  malva). Decided (REVISED): a **lean runtime binary** for the prod artifact (structural exclusion,
  no CLI cfg-threading); feature-gating rescoped to mixed crates only. Remaining D0 decision carried
  into D4: the default-target story (which target `run`/`test` use = dev, `build` = prod) and whether
  `--target` is the sole selector. Dev-capability set starts at `body_formatters`.
- **D1 — target-scoped dependencies (manifest).** Parse `[targets.<name>.dependencies]` into
  `Target`; `extends` inherits deps (like tiers). Validate shape; a target's tier provider may now
  name a target-scoped dep. Errors point at the missing/duplicated key. Unit tests over `from_toml`.
- **D2 — resolution & lockfile.** Resolve the **union** of all targets' deps into one `noeta.lock`
  (everything pinned); a per-target *view* selects its subset. Shared deps unify to one version
  (dev-only deps can't conflict with prod). `resolve_graph` gains a target parameter (or returns
  per-target native-crate sets). No churn for manifests without target deps.
- **D3 — the lean runtime binary (`noeta-runner`).** A new binary crate, native analogue of
  `noeta-wasm-runner`: `[dependencies]` = runtime crates only (`noeta-vm`, `noeta-backend`,
  `noeta-bundle`, `noeta-runtime`, `noeta-stdlib`, the real `Host`) — **no** `noeta-fmt`/`-lsp`/
  `-dap`/`-mcp`/`-html`/`-css`/`-compiler`. `main` reads a stapled or path bundle
  (`noeta_bundle::read`) and runs it on the VM, sharing the **exact** execution tail the CLI's `run`
  verb uses (extract that tail into a shared lib fn if not already one — the drift firewall). Prove
  it runs a `--exe`-shaped bundle byte-identically to the CLI (reuse the differential-oracle shape).
  This crate *cannot* contain a formatter/parser — auditable by its Cargo.toml.
- **D4 — repoint `--exe`/`--native` onto the lean runtime + per-target compose.** `build --exe`/
  `--native` staple the bundle onto **`noeta-runner`** (or a composed shim whose base is the runner,
  for apps with native runtime deps) instead of cloning the running full CLI — the security fix. The
  composer (`compose.rs`) builds **for a target**: include only that target's native crates (D2), and
  for a *mixed* crate flip its dev feature **off** in prod / **on** for the toolchain (D5's contract).
  Content-hash key includes target + feature set so dev/prod artifacts cache separately. Assert
  `malva`/`noeta-fmt` symbols are **absent** from the prod artifact.
- **D3b — dev-capability feature-gating convention (mixed crates).** The package-author contract for
  the *one* case D3 can't cover structurally: gate a mixed crate's dev-kind impls + optional heavy
  deps behind a Cargo feature — `malva = { optional = true }`, `fmt = ["dep:malva"]`,
  `#[cfg(feature = "fmt")] fn body_formatters(...)`. The composer flips it per target (D4).
  Document + (stretch) a lint flagging an un-gated dev capability in a crate meant to ship to prod.
- **D5 — dogfood + docs.** A first-party **mixed** example package: a tier with a native handler +
  a feature-gated formatter, proving prod strips the formatter (`malva` absent from the prod binary
  — assert via symbol/size or a link check) while dev formats it. Migrate `noeta-css`'s `malva` to
  an optional-dep/feature so even the toolchain demonstrates the gate. Docs: the dev/prod target
  model, `[targets.*.dependencies]`, and the "put dev tooling behind the `fmt` feature or in a
  dev-dep crate" guidance for package authors.

## Open questions

- **Prod-artifact composition. ✅ RESOLVED.** `--exe`/`--native` staple onto a copy of the running
  full CLI today (D0). D4 repoints them onto the lean `noeta-runner` (D3). The shared bundle-exec
  library is the drift firewall.
- **Shared-execution-lib extraction (D3).** Is the CLI's `run`-verb bundle-execution tail already a
  reusable lib fn, or does D3 extract it? `noeta-wasm-runner` proves the tail is small and
  target-agnostic; confirm the native path can call the identical fn.
- **Version unification across targets** — same constraint Cargo solves; confirm our resolver
  handles a dev-only dep that is absent from prod cleanly.
- **Feature name / granularity (mixed crates only).** One conventional `fmt`/`noeta-dev` feature per
  package, vs a global `--cfg noeta_prod` the ABI keys off. Feature is more Cargo-native (drops
  optional deps); cfg is less author-ceremony. Likely: ABI methods `#[cfg]` on a std feature the
  composer flips. (Only relevant to mixed crates — the lean runtime excludes standalone dev crates
  structurally, no feature involved.)
- **Repetition** — expected non-issue: `extends` inherits target deps, and the real shape is one
  `dev` (+ maybe `ci extends dev`) and one `prod`. Confirm no per-target duplication is forced.

## Non-goals

- A JS/`<script>` formatter (SWC — heavy; the delegation hook already reaches `"javascript"`).
- Runtime gating of dev *invocation* (prod never calls dev verbs; the concern is *linkage*).
- Reworking `noeta run`'s toolchain (it *is* the dev toolchain; carrying dev tooling there is fine).
