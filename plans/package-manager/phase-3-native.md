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

### N3.7 — `ExtCommand` external-binary form

The cargo model: an unknown `noeta <cmd>` falls back to `noeta-<cmd>` on `PATH` (exec, argv
passed through, exit code forwarded). Registered (compiled-in) commands win over PATH; a PATH miss
keeps today's clap error. Small, independent of composition.

### N3.8 — Docs

`docs/Native-Extensions.md`: "Writing a native package" walkthrough (manifest `native` key, the
Rust crate shape, what composition does, the Rust-toolchain requirement); Deferred section shrinks
to finalizers-won't-build note (if confirmed) + distribution/packaging.

## Open decisions (user)

1. **Finalizers** — N3.5 recommends won't-build (rationale above). Confirm or overrule.
2. **Composed toolchain scope** — recommendation: the composed binary IS the app's toolchain
   (all verbs delegate). The alternative (compose for `run` only) forks check/LSP behavior.
3. **Interim source requirement** — composition builds against the noeta workspace (path deps)
   when available, git+tag otherwise; packaged/hermetic toolchain distribution stays a later
   decision (same posture as `resolve_aot_runtime`). Confirm.
4. **Manifest key** — `[package] native = "<dir>"` explicit key (vs a `native/` dir convention).

## Gates

Every slice: full workspace suite, conformance + differential + leak oracles, clippy/fmt. N3.2:
composed-binary cache hit on unchanged lockfile (second invocation adds only exec cost). N3.4:
pinned interleaved A/B on the vec bulk benches — parity or better with the special case. N3.3:
statically-caught signature error across the composition boundary (the "toolchain is the
composition unit" proof).
