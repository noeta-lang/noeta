# Slice M2.0 — Benchmark harness + hot-path baselines (criterion)

Status: **done**

> **Cluster:** M2 cluster 1 (host IO & async foundation). **Depends on:** nothing — independent; sequenced first deliberately. **Determinism posture:** none — benchmarks measure the VM, they do not run in the differential and add no host coupling.

## Goal
Stand up the `criterion` benchmark harness over the VM's hot paths and commit a baseline **before** the host/async indirection of M2.1–M2.4 lands, so every later slice can prove it introduced no dispatch/property/allocation regression.

## Why now
`roadmap.md` standing requirements: *"`criterion` perf-regression gates are M1+ … when the M1 VM lands, every VM-touching slice adds/maintains a bench over the hot paths (dispatch loop, property access through inline caches, allocation)."* That bench was reserved but never built during M1. This slice pays that debt and — because the next four slices thread a `Host` indirection through both backends — captures the clean pre-refactor baseline that makes "no regression" checkable rather than asserted.

## Scope
- In:
  - **`benches/`** (or per-crate `[[bench]]` targets on `lang-vm`) with `criterion` as a `[dev-dependencies]` entry.
  - Three benches matching the roadmap's named hot paths: **dispatch loop** (tight arithmetic/call loop, many small ops), **property access through inline caches** (monomorphic object field/method lookup, exercising the IC hit path), **allocation** (list/map/string construction under load).
  - A committed baseline and a short note on how to run/compare (`cargo bench`), wired into the CI gate position from implementation-plan §6.6 (Performance regression) / §6.7 (gate order: bench last/scheduled, regression threshold fails CI).
- Out: micro-optimizing anything the baseline reveals (separate work); the Tier-1 specializing interpreter (later M2); benchmarking IO/async paths (their own slices add IO benches over the sandbox host).

## Checklist (vertical slice)
- [ ] Grammar / AST: none.
- [ ] Checker rule: none.
- [ ] Bytecode: none (benches consume already-compiled `Module`s).
- [ ] VM op: none — benches *measure* the existing dispatch loop / IC / allocator, they do not change them.
- [ ] Conformance cases: none (benches are not conformance; they must not perturb the corpus).
- [ ] Snapshots: none. Instead: a committed criterion baseline + a documented threshold.

## Definition of done
- `cargo bench` runs the three benches green and emits a baseline.
- The regression threshold is documented and positioned as the last/scheduled CI gate (§6.7), consistent with `fmt → clippy → unit/snapshot → conformance → proptest → miri → bench`.
- `AGENTS.md` verification list and the roadmap standing-requirements note reflect that benches now exist (done when this slice lands, not now).
- fmt/clippy clean.

## Notes / traps
- Benches must be deterministic in *shape* (fixed inputs, fixed iteration counts) even though timing varies; do not seed them from wall-clock.
- Keep bench programs in the VM-compilable subset so they exercise the real `Op::*` dispatch, not a fallback path.
- The baseline is the artifact this slice exists to produce — review the numbers, do not blind-accept criterion output.

## Outcome (done)

Landed `crates/lang-vm/benches/vm.rs` (a `[[bench]]` target, `harness = false`) plus `criterion = { version = "0.5", default-features = false }` pinned once in the workspace `Cargo.toml` and pulled into `lang-vm` dev-deps. Default features are off to keep the dep tree lean (no plotters/rayon/html) — terminal reports only, matching the project's small-by-default ethos.

**Three benches over the roadmap's named hot paths**, each compiling its program to a `Module` once in setup and timing only `VmBackend::run_module` (so the measurement is execution — dispatch loop / inline cache / allocator — not the lexer/parser/compiler):

- `vm/dispatch_fib` — recursive `fib(24)` (~150k calls): dense call/compare/arith/return dispatch.
- `vm/property_access` — read `p.x`/`p.y` on a monomorphic object across 5 000 loop iterations: the cached `LOAD` path through inline caches.
- `vm/allocation_list` — build and drop a 3-element list per iteration across 5 000 iterations: heap/refcount churn.

Loop counts come from list literals generated in Rust (`int_list(n)`) so the iteration count is high without a giant source line. The VM dispatches calls iteratively over a `frames` vector (no Rust-stack recursion), so depth is heap-bounded — but the property/allocation benches use bounded `for` loops anyway to keep frame depth flat.

**Indicative baseline** (release `bench` profile, one dev machine — for shape/regression detection, not an absolute spec): `dispatch_fib` ≈ tens of ms, `property_access` ≈ 1.5 ms / 5 000 iters, `allocation_list` ≈ 1.3 ms / 5 000 iters. Run with `cargo bench -p lang-vm`; CI positions it as the last/scheduled gate per implementation-plan §6.6/§6.7 with a regression threshold.

**No corpus perturbation:** `cargo test --workspace` green, `cargo clippy --all-targets -- -D warnings` clean (benches linted), and `lang test --differential` unchanged at **88 matched / 0 skipped / 100% / backends agree**. No `unsafe` added.
