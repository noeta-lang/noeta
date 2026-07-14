# Dev capabilities & target-scoped dependencies — lean, safe prod artifacts

**Goal.** Dev-only capabilities a package ships — a tier's **formatter**, a linter, a codegen
helper — must never be **linked** into a production artifact. Not for size (that's the minor
win) but for **security**: a formatter drags in a *parser* (`malva` for CSS, an SWC-sized thing
for JS), and every parser in a prod binary is reachable attack surface. The fix is build-time
exclusion driven by the **target**, so `noeta build --target prod` yields a runtime that contains
only what the app runs, while the dev toolchain keeps everything.

Three mechanisms, addressing the shapes a dev capability arrives in and where it is shipped:

1. **Lean prod runtime binary** (`noeta-runner`) — the prod-artifact executor. Runs `.noe` **source**
   or a `.noeb` **bundle** or a **stapled** bundle (L1+L2), so it serves *both* PHP-style
   source deployment and `build --exe`/`--native` (which staple onto **this**, not a copy of the full
   CLI). Depends on app-execution crates only; the **dev tooling (L3)** — fmt/LSP/DAP/MCP/profiler +
   `malva` — is *structurally absent* (no dependency edge, no `#[cfg]`). Native analogue of
   `noeta-wasm-runner`, extended with the compiler so it can run source.
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

**Execution model — three capability layers; prod excludes only L3.** The security boundary is
**dev tooling vs app execution**, *not* run-vs-build and *not* source-vs-bundle. Running a source
tree in production (PHP/Python/Ruby/Node style — deploy source, point the runtime at an entry file,
compile on the fly) is a first-class mode, so the *compiler* is a prod-legitimate layer.

- **L1 — execute a bundle**: VM + real `Host` + runtime extensions. In *every* prod artifact.
- **L2 — compile source → bundle**: parser + checker + compiler + **tier activation/desugar** +
  manifest/target tier-set & provider resolution (`noeta-pm` *minus* `registry-http`/`keyless`).
  Needed to run *source*; baked away in a pre-built bundle. The compiler is our own language's
  front-end fed only the app's own trusted source — the expected surface of any run-from-source
  runtime, not the "dev tooling dragged in" risk.
- **L3 — dev tooling**: fmt + LSP + DAP + MCP + profiler, **and their parsers** (`malva`, the HTML
  reindenter, …). *Never* in any prod artifact — the whole point.

**A tier is split across these layers, not filed under one.** For a **program tier** (`@html`: its
`@tier(html, …) fn render(statics, holes)` handler is idiomatic Noeta): the **handler compiles to
bytecode baked into the bundle** (linker closure) + core `std` it desugars into — so its L1 cost is
*zero extra linkage*, it rides inside the bundle; the **desugar** (`@html { … } → render(…)`) is L2
(build-only); the **formatter** (`noeta-html`) is L3 (dev-only). For an **extension tier** (native
Rust handler, expr-tiers `ExtTier`): the handler is L1 *native* — linked into every prod artifact —
while its formatter stays L3. This is exactly the **mixed-package** case the D3b feature-gate targets:
keep the native handler, drop the native formatter + its parser.

**The prod artifacts, by deployment style (both exclude L3):**
- **Pre-compiled (`build --exe`/`--native`)** — L1 (+ any extension tier's native L1 handler). The
  `@html` handler is already bytecode in the bundle; nothing `@html`-specific is linked.
- **PHP-style (ship source)** — L1+L2. `noeta-runner app.noe` compiles + runs, no dev tooling.
- **Dev workstation (`noeta`)** — L1+L2+L3.

`noeta-runner` (this arc, Option A) is the **L1+L2 prod runtime**: runs `.noe` source *or* a `.noeb`
bundle *or* a stapled bundle, links no L3. `build --exe` staples onto it (bundle path skips L2).
**`noeta profile`/`dap`** are L3 surfaces that install an Option-gated hook into the *same* VM (the
DAP "debugs the PROD VM"), zero-cost when absent — so they observe identical execution semantics.

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
- **D3 — the lean prod runtime binary (`noeta-runner`), source-capable (Option A). ✅ DONE.** New
  lib+bin crate; runs a `.noe` **source** file, a `.noeb` **bundle**, or a **stapled** bundle,
  linking **L1+L2, no L3**. Shipped in three green sub-slices:
  - **D3a (`d753e940`)** — extract the shared execution tail (`run_module_real_host`/
    `run_compiled_module` + `--jit-stats` renderer); `p2p_app_namespace`→`app_id` param so the runner
    never links `noeta-pm`. Pure refactor; CLI delegates.
  - **D3b (`8e47fa95`)** — the runner **binary**: bundle execution (stapled + two-file); stapled
    reader moved to the shared lib. 5 integration tests.
  - **D3c (`e320c6f2`)** — extract the whole **compile pipeline** (`compile_whole_file`/
    `open_startup_cache`/`compile_real`/`resolve_providers` + `Compiled`/`CompileFailure`) into
    `noeta-runner::compile`; the runner runs `.noe` source (dispatch by bundle-magic). **Closed a
    real L3 leak:** `noeta-pm`'s manifest parser read `[fmt]` into `noeta_fmt::FmtConfig`, so
    resolving any manifest dragged in the formatter — now gated behind a non-default `fmt-config`
    feature (CLI enables it), the **first dogfood of the dev-capability gate on a first-party crate**.
  - **Security proof (isolated `-p noeta-runner` build):** `noeta-pm` resolves to `[]` features,
    `cargo tree -e features -i noeta-fmt` is empty — no L3 linked, auditable by Cargo.toml.
  - **⚠ Build-isolation invariant (carried into D4):** feature unification means a `--workspace`
    build turns `fmt-config` **on** for the shared `noeta-pm`, so a workspace-unified build of the
    runner *does* link `noeta-fmt`. **The prod artifact MUST be built with `-p noeta-runner` (its own
    crate graph), never pulled from a unified workspace build.** D4's composer already builds an
    isolated Cargo project, which satisfies this; D4 must assert it.
- **D4 — repoint `--exe`/`--native` onto the lean runtime + per-target compose.** `build --exe`/
  `--native` staple the bundle onto **`noeta-runner`** (or a composed shim whose base is the runner,
  for apps with native runtime deps) instead of cloning the running full CLI — the security fix. The
  composer (`compose.rs`) builds **for a target**: include only that target's native crates (D2), and
  for a *mixed* crate flip its dev feature **off** in prod / **on** for the toolchain (D5's contract).
  Content-hash key includes target + feature set so dev/prod artifacts cache separately. Assert
  `malva`/`noeta-fmt` symbols are **absent** from the prod artifact. **Build in isolation** (the
  runner's own crate graph, not the workspace) so feature unification can't turn `fmt-config` back on
  — the D3c invariant.
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
- The absolute-minimal L1-only `--exe` (no compiler). `noeta-runner` is L1+L2 so one binary serves
  both source and bundle deploys; carrying the (idle) compiler in a stapled `--exe` is a *size* cost,
  and the plan deprioritizes size vs the L3 attack surface — which is fully excluded either way. A
  bundle-only L1 variant stays a possible later refinement.
