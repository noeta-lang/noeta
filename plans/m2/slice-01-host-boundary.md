# Slice M2.1 — Host capability boundary (sandbox/host split)

Status: **done**

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** nothing structural (best sequenced after M2.0 so the perf baseline predates the indirection). **Determinism posture:** pure refactor — conformance keeps running the deterministic sandbox; the differential must stay **88+/0 skipped/100%/zero divergence**, which is the slice's proof of correctness.

## Goal
Introduce one **`Host` capability** seam that abstracts every host-coupled effect (`fs`, `env`, `args`, clock, rng), and move the existing ad-hoc per-backend fields onto it — making the sandbox/host split *structural* rather than test-arranged, with **zero behavior change** proven by the oracle.

## Why this is the keystone
Today each backend carries loose, hard-coded determinism fields: `fs: Vfs`, `rng: u64`, `clock: u64` (initialized to `Vfs::new()` / `DEFAULT_SEED` / `0` in both `Interpreter` and `Vm`). Real disk, real `env`/`args`, and async (M2.2–M2.4) all need a second, non-deterministic implementation of those same effects **without** the conformance differential ever seeing it. The clean way is a trait with two impls; everything downstream in this cluster plugs into it. This is the same "land the plumbing first, behavior-preserving, prove it with the differential" discipline as M1.1 (salsa db).

## Architecture (decided)
- A **`Host` trait** in `lang-stdlib` (no new crate yet) gathers the effect surface the stdlib modules already call: filesystem ops (the current `Vfs` methods), env/args lookups (M2.2), `clock` read-and-advance, and `rng` state stepping. The pure *semantics* stay in `lang-stdlib` (`fs`/`random`/`math`/`json` steppers are unchanged); the trait only abstracts *where the bytes/state come from*.
- **`SandboxHost`** (in `lang-stdlib`) is the deterministic impl: in-memory `Vfs`, an injected env map + args list (empty until M2.2), the logical monotonic `clock`, the seeded `rng`. It resolves everything synchronously.
- Both backends become **generic (or trait-object) over `Host`**, replacing the three loose fields with one `host: H`. The conformance harness constructs `SandboxHost` for both backends (so the differential is unchanged); the CLI will later construct `RealHost` (M2.3).

## Scope
- In:
  - `Host` trait + `SandboxHost` in `lang-stdlib`.
  - Refactor `Interpreter` (`lang-eval`) and `Vm` (`lang-vm`) to hold `host` instead of `fs`/`rng`/`clock`; route `call_fs`/`call_time`/`call_random` through it.
  - Update `lang-conformance` (`differential.rs`) and `lang-cli` to construct `SandboxHost` (CLI moves to `RealHost` in M2.3).
- Out: any new user-visible behavior; real disk / real env (M2.2+); async (M2.3); streaming (M2.4). If a user program's output changes, this slice is wrong.

## Checklist (vertical slice)
- [ ] Grammar / AST: none.
- [ ] Checker rule: none.
- [ ] Bytecode: none — dispatch still flows through `Op::CallMethod → call_native_module`.
- [ ] VM op: `call_fs`/`call_time`/`call_random` read/write state through `self.host` instead of `self.fs`/`self.clock`/`self.rng`; tree-walker mirrors exactly.
- [ ] Conformance cases: **existing corpus only** — it must remain differential-identical through the new seam (the oracle proves the refactor is behavior-preserving, exactly as M1.1 did).
- [ ] Snapshots: none new; existing snapshots unchanged.

## Definition of done
- `lang test --differential` shows **zero change** in output or coverage vs. the pre-refactor run (≥ 88 matched / 0 skipped / 100% / zero divergence).
- Both backends hold a single `host`; no remaining direct `Vfs`/`rng`/`clock` fields.
- `cargo test --workspace`, fmt, clippy clean. No new `unsafe`.

## Notes / traps
- Keep the trait minimal — only the effects the stdlib already performs. Resist adding async to the signatures now; M2.3 introduces the async `RealHost` behind the same surface (the sandbox stays sync, the VM blocks on the leaf future at the boundary).
- Do not move stdlib *semantics* into the host — the SplitMix64 stepper, the JSON tree, math functions stay pure in `lang-stdlib`; the host only owns *state and bytes*.
- Generic-vs-trait-object is a judgment call: prefer whichever keeps both backends' `RunResult` identical with the least churn; record which was chosen and why in the Outcome.

## Outcome (done)

Landed `crates/lang-stdlib/src/host.rs`: a `pub trait Host` gathering every host-coupled effect both backends perform — filesystem (`fs_write`/`fs_read`/`fs_exists`/`fs_remove`/`fs_list`), seeded PRNG (`rng_seed`/`rng_int`/`rng_float`), and the logical clock (`clock_monotonic`/`clock_sleep`) — plus `SandboxHost`, the deterministic impl owning the in-memory `Vfs`, the SplitMix64 state (`DEFAULT_SEED`), and the clock counter. Re-exported as `lang_stdlib::{Host, SandboxHost}`. The pure stepper/semantics (`random`, `fs`) are untouched — the host owns *state and bytes only*.

**Trait object, not generics (decision).** Both backends replaced their three loose fields (`rng: u64`, `fs: Vfs`, `clock: u64`) with a single `host: Box<dyn lang_stdlib::Host>`, default-constructed as `Box::new(SandboxHost::new())` exactly where the old fields were initialized. `dyn Host` (object-safe — all `&self`/`&mut self`, no generics/`Self`-return) was chosen over `Vm<H: Host>` to keep the large VM file churn-free and let a real host slot in later by swapping the constructor, not re-touching internals. IO is never a hot path, so dynamic dispatch is immaterial — confirmed by the M2.0 benches (property/allocation unchanged at ~1.48/~1.39 ms; dispatch untouched).

**Pure refactor, oracle-proven.** The host-coupled sites were localized to `call_fs`/`call_random`/`call_time` in each backend; each became a one-line delegation to `self.host.*`. No public API changed (the conformance harness still gets a fresh `SandboxHost` per run via the default). `cargo test --workspace` **310 passed / 0 failed**, `clippy --all-targets -D warnings` clean, and `lang test --differential` **unchanged at 88 matched / 0 skipped / 100% / backends agree** — including the `std/random` (pinned RNG sequence), `std/time` (pinned clock), and `std/fs` (round-trip) cases now routed through `SandboxHost`. No `unsafe` added or touched (the seam is plain safe code; the VM's NaN-box `unsafe` is on an unrelated path), so miri carries no new obligation.

This is the keystone: M2.2 (env/args) and M2.3 (the async `RealHost`) now plug into `Host` without re-touching either backend.
