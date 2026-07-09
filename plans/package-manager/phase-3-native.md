# Phase 3 — Third-party native packages (static cargo composition)

*Parent: [`README.md`](README.md). Builds on Phase 1's assembled multi-unit `REGISTRY` and Phase 2's
package system. Status: **SCOPING (2026-07-09)** — slices drafted, open decisions marked; per the
planning norm, implementation starts when the phase shape is confirmed.*

## What Phase 3 is

Phases 0–2 delivered "declare it to get it" for **pure-Noeta** packages and **first-party in-tree**
native capability units. Phase 3 extends it to **out-of-tree native code**: a third-party package
that ships a Rust crate `impl Extension` against `noeta-native`, statically composed into the
consumer's build by `cargo` (the confirmed model: no cdylib, no C ABI, no dynamic loading, no
freeze — `noeta-native` is a normal versioned crate and Cargo's semver governs compatibility).

## The central design fact: the toolchain is the composition unit

A native extension does not just *run* — its module/function **signatures feed the checker**, its
completions/hover feed the **LSP**, its extern types surface in the **debugger**, and its
`ExtCommand`s extend the **CLI**. Under static composition there is exactly one artifact that can
carry all of that: **a composed `noeta` toolchain binary** whose registry includes the extension
units the app's manifest declares. Composing a bare "runner" would fork the toolchain — `noeta
check` would reject programs the runner accepts.

So the flow is the `resolve_aot_runtime` precedent scaled up: the stock `noeta`, when an app's
dependency graph carries native crates, **generates a shim crate, builds it with cargo, and
delegates** (exec) to the composed binary. The shim is ~20 lines: depend on `noeta-cli` (as a lib)
+ each native dep crate; `fn main()` passes the extra `&'static [&dyn Extension]` units into the
CLI entry. Composition is **cached** (content-addressed by lockfile + toolchain identity, the
`noeta-cache`/store pattern) so the cargo build runs once per dependency-set change, and every
subsequent `noeta <anything>` in that app is one exec-delegation (~ms).

A **pure-Noeta app never touches any of this** — no Rust toolchain, no shim, the stock binary
serves as today. The Rust-toolchain requirement is confirmed scope for native-dep consumers.

## Slices

### N3.0 — Registry mechanism moves to `noeta-native`; assembly-time install; CLI lib entry

The lookup layer in `noeta-stdlib::registry` (find_module/find_function_sig/ring_of/
is_extension_root/qualified resolution/dispatch routers) is already fully generic — it iterates
`extensions()` and touches nothing std-specific; it lives in stdlib only because it grew around
the dogfood. Phase 3's assembler (the composed shim) must not reach through the dogfood crate to
register its peers, so the mechanism moves to the ABI crate (user-confirmed 2026-07-09):

- `noeta-native::registry` gains the **runtime registry**: `OnceLock`-held assembled unit list,
  `install(units)` (once, before any lookup) + `install_default(provider)` (the facade's lazy
  seam) + `extensions()` + the whole generic lookup layer moved verbatim. Uniqueness at install:
  duplicate **unit name** or duplicate **qualified module** (`root + "." + module`) = hard startup
  error — roots are deliberately shared (the six std units all root `"std"`; an earlier draft
  wrongly said duplicate root).
- `noeta-stdlib::registry` becomes a **facade**: same public paths, each function delegates to
  `noeta_native::registry` after lazily ensuring the std units are installed — zero call-site
  churn across backends/checker/tests, no forgot-to-seed failure mode. Std residue stays behind
  deliberately: the six unit definitions, `static_dispatch_ctx`/`_method` fast routes (they name
  `cell`/`reactive` concretely — the monomorphized per-crate fast path), and the `vec`/`fs`
  special-case notes N3.4 deletes.
- `noeta-cli` gains a `lib` target exposing `pub fn run_cli(extra: &'static [&'static (dyn
  Extension + Sync)]) -> ExitCode` (installs std + extras, then dispatches); the `[[bin]]` main
  becomes `run_cli(&[])`. Zero behavior change; differential/conformance untouched.
- Gate beyond the standard suite: dispatch-path A/B (the `OnceLock` load replaces a `static` —
  expected noise-level, but H5's rule is measure, not assume).

### N3.1 — The manifest declares a native crate

- A dependency package's `noeta.toml` `[package]` gains `native = "<relative dir of the Rust
  crate>"` (explicit key, not a directory convention — greppable, self-documenting).
- `noeta-pm`: `PackageMeta.native`, carried through `graph.rs`; the lockfile records it (the
  composed build is keyed off the lock, so native crates must be pinned content like sources).
- Validation: the dir must contain a `Cargo.toml`; a *registry/git* native dep composes from the
  fetched store checkout (same content-addressed tree the sources load from).

### N3.2 — Composed-toolchain build + delegation

- `noeta-cli`: when the resolved graph has ≥1 native crate, resolve the composed binary: cache hit
  → exec it with the original argv (env guard `NOETA_COMPOSED=1` stops recursion); miss → generate
  the shim crate under the store (`compose/<hash>/`), `cargo build --release`, cache, exec.
- Shim generation: `Cargo.toml` (noeta-cli lib by **path when in-workspace, git+tag otherwise** —
  the same interim-vs-packaged split as `resolve_aot_runtime`; packaging the toolchain for
  source-less composition is the same later distribution decision) + `main.rs` listing each native
  dep's extension static.
- `noeta build --native` composes the **AOT archive** the same way: the shim pattern extends
  `noeta-aot-runtime` with the extension crates (their ring = the whole extension unit).
- Failure honesty: a native dep without a Rust toolchain on PATH is a clear diagnostic naming the
  dep and the requirement, not a cargo stack trace.

### N3.3 — The proving package (out-of-tree dogfood)

A fixture package (CLI-test-local, path + git-tag forms) with a real Rust crate: one module
(`imgfx.blur(...)` or similar), one extern type with a ctx method, one `ExtCommand` — exercising
plain dispatch, the ctx seam, and command registration through composition. E2E CLI tests: run,
check (a signature error in a cross-package call to the native fn is caught **statically**),
composed-binary cache hit on second run, `noeta <ext-command>` dispatch.

### N3.4 — Raw-buffer ABI: `with_packed` + the `vec.*_all` migration

The known capability gap (P-SIMD tier-3, `plans/perf/p-simd-column-layout.md`,
`plans/deferred.md:128`): the neutral seam can't hand a native fn a packed list's contiguous
bytes — the reason `vec`'s bulk `*_all` kernels are the **last per-backend special case**
(`noeta-vm/src/methods.rs:410`).

- Seam: borrow-shaped like `with_extern` — `with_packed(slot, &mut |schema, bytes| …)` +
  `with_packed_mut`, exposing the shared `PackedSchema` (incl. `layout`) + the raw byte buffer;
  plus an allocating `make_packed(schema, bytes)` for producing results. Both backends implement;
  the tree-walker's is the oracle twin.
- Dogfood: migrate `vec.add_all/sub_all/scale_all/dot_all/length_all` (+ column twins) onto it —
  **zero unmigrated native functions remain**; delete the per-backend intercepts.
- Third-party proof: the N3.3 fixture registers a column kernel for its own `@packed` type
  (`(module/type, operation)` keying = ordinary registered functions/methods; the *capability* was
  the only missing piece).
- **Perf gate:** `vm_vec_add_all` (and the column bench) pinned interleaved A/B — the migrated
  kernels must hold the special-case numbers (the seam adds one borrow-shaped call per *bulk op*,
  not per element, so the floor is negligible; verify, don't assume).

**Design refinements found at implementation (2026-07-09):** the two backends hold *different*
concrete schema types (the VM an interned `&'static noeta_object::PackedSchema`; the eval oracle
its own private `Rc<PackedSchema>` over `TypeDef`), so "exposing the shared `PackedSchema`" is
realized the way this ABI always crosses type gaps — a **neutral vocabulary**: callbacks receive a
read-only `PackedView { fields, byte_size, column, count }` built by each backend from its own
schema. Consequences and additions:

- `make_packed(schema, bytes)` becomes **`make_packed_like(like, bytes)`** — a result's element
  schema is named by an existing packed *slot*, never constructed by the extension (schemas are
  backend-interned; and `SigType` cannot name a user's `@packed` type anyway, so a result typing
  as anything but an input's type was never expressible).
- `with_packed_mut` preserves **value semantics**: the callback gets a uniquely-owned COW buffer
  (in place only under proven sole ownership — free on the eval side via `Rc::make_mut`); the
  result is a fresh slot, a non-seed input slot is spent (the `take`/`make_list` convention).
- Two structural-object primitives make the *boxed fallback* expressible in the one shared
  dispatch: `object_scalars(slot)` (the ctx twin of the shallow `NativeValue::Object` projection)
  and `make_object_like(like, fields)` (the ctx twin of `NativeOut::Object` +
  `RetTy::SameAsArg` materialization, which `intern` deliberately rejects for lack of shape).
- Bug fixed by the migration: the old `add_all`/`sub_all` fast path accepted two packed operands
  of **different layouts** (row × column) and added their buffers flat — silently wrong values.
  The shared dispatch requires layout agreement and otherwise takes the (correct) element-wise
  fallback; a conformance test pins the mixed-layout result.
- `fs.list`'s checker special case dies too: the signature becomes `params: &[Optional(&Str)]`
  (the http-arc H4 trailing-optional machinery, which post-dates the special case), so
  `is_module_function` reduces to pure registry delegation.

**✅ N3.4 DONE** (`4eb736b8` seam+migration, `e8932f08` third-party proof, `f2f396f4` perf gate).
Also fixed en route: the VM's ctx element reads (`list_get`/`call_with_element`) errored on packed
lists since H2 (pinned by `map_bounded_packed.noe`). Gate additions the first A/B forced:
`NativeOut::Scalars(ScalarVec)` (bulk reduction results as ONE typed vector — the boxed
`NativeOut::List` form was +80%) and the fused `object_scalars_at`/`make_object_like_element`
(reused scalar buffer, zero per-element slots). **Final numbers** (pinned interleaved, median of 7,
`tests/bench/pm-native/`): add row −3%, dot column −2..−5%, scale −29% (`with_packed_mut` COW),
boxed fallback +5..+11% (three dyn calls/element vs the intercept's direct value access — accepted:
it is the compat path; `@packed` is the bulk-math representation).

### N3.5 — Host-coupled finalizers: **recommend CLOSING as won't-build** *(decision pending)*

The gap-fill list named finalizers alongside raw buffers. Analysis says the two differ:

- A Rust-side resource in an extern box **already finalizes deterministically**: RC-zero drops the
  box, Rust `Drop` runs (files close, sockets shut). No capability is missing for that.
- The *host-coupled* variant (an extension callback **with `Host` access** at free time — flush
  through the seam, touch extension state) has no sound access point: values die in
  `heap`/release paths that carry no `Host`/ctx, including teardown cascades where the arena and
  state are mid-drop. Threading a host there is exactly the coupling the value/runtime split
  forbids — and explicit `close()` (the standing norm) plus `Drop` cover the real cases.
- Deferral is **not** a future break: `ExtType` is constructed via `..ExtType::DEFAULTS`, so a
  later `finalizer: Option<…>` field is additive.

### N3.6 — Version discipline on the consumed crates

"Publish" under the git-only model = **version + tag in this repo**, not crates.io: workspace
version leaves `0.0.0` → `noeta-native` (and the crates a composed shim consumes: `noeta-cli` lib,
`noeta-stdlib`, …) get `0.1.0`; additive-evolution audit (`#[non_exhaustive]` on the ABI enums,
`DEFAULTS` on structs — most already hold from the higher-order arc); a semver policy note in
`docs/Native-Extensions.md`: pre-1.0, minor = breaking allowed, composed shims pin by git tag.

**Audit outcome (2026-07-09): `#[non_exhaustive]` is deliberately NOT applied.** (a) On the
registration structs it would forbid literal construction outside the crate entirely — killing the
`..DEFAULTS` pattern that *is* the additive-evolution mechanism; on the enums it would force
wildcard arms in our own sibling crates (`non_exhaustive` binds per *crate*, not per workspace),
discarding exactly the exhaustiveness that caught the `NativeOut::Scalars` materializer gaps the
day it was added. (b) There is no binary skew to defend against: a composed shim pins the
toolchain by version tag and cargo unifies the extension's `noeta-native` onto the same source —
compatibility is source-level semver, which the pre-1.0 policy (minor may break) already governs.
Instead the `DEFAULTS` convention is completed: `ExtFn::DEFAULTS` + `ExtCommand::DEFAULTS` join
`ExtModule`/`ExtType`'s. Revisit `non_exhaustive` at 1.0 if dynamic loading ever decouples the
extension's ABI version from the toolchain's.

### N3.7 — `ExtCommand` external-binary form

The cargo model: an unknown `noeta <cmd>` falls back to `noeta-<cmd>` on `PATH` (exec, argv
passed through, exit code forwarded). Registered (compiled-in) commands win over PATH; a PATH miss
keeps today's clap error. Small, independent of composition.

### N3.8 — Docs

`docs/Native-Extensions.md`: "Writing a native package" walkthrough (manifest `native` key, the
Rust crate shape, what composition does, the Rust-toolchain requirement); Deferred section shrinks
to finalizers-won't-build note (if confirmed) + distribution/packaging.

## Decisions (user, 2026-07-09)

1. **Finalizers: SKIP for now** — N3.5 closed as won't-build-now (deterministic Rust `Drop`
   covers resources; host access at free time is unsound; explicit `close()` stays;
   `..DEFAULTS` makes a later `finalizer` field additive). Re-open only on a concrete demand.
2. **Composed toolchain scope: confirmed** — the composed binary IS the app's toolchain.
3. **Toolchain source retrieval: cargo fetches it** — the shim's `Cargo.toml` declares the
   toolchain crates as **git deps pinned to the running binary's version tag** (`noeta-cli =
   { git = …, tag = "vX.Y.Z" }`); cargo's own git cache handles fetch/caching/offline (no temp
   dirs, no manual fetch — the toolchain is just one more git+tag dependency, the Phase-2
   model). In-workspace: path deps. `NOETA_TOOLCHAIN_SRC` overrides for hermetic setups.
   Packaged/hermetic distribution stays a later decision.
4. **Manifest key: confirmed** — `[package] native = "<dir>"`, one **entry crate** per package.
   Not a one-extension limit: the entry crate exports a **slice of units** (std's own shape —
   six units in one crate); a multi-crate package aggregates in its entry crate's own
   Cargo.toml.

## Gates

Every slice: full workspace suite, conformance + differential + leak oracles, clippy/fmt. N3.2:
composed-binary cache hit on unchanged lockfile (second invocation adds only exec cost). N3.4:
pinned interleaved A/B on the vec bulk benches — parity or better with the special case. N3.3:
statically-caught signature error across the composition boundary (the "toolchain is the
composition unit" proof).
