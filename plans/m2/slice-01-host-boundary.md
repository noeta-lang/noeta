# Slice M2.1 — Host capability boundary (sandbox/host split)

Status: todo

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
