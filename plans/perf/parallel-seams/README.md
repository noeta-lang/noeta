# Parallel-seams arc (P-PAR) — making the runtime's parallelism seams pay

**Status: in progress (branch `perf-parallel-seams`, off local main `6d069ca`).** Motivated by the
2026-07-07 parallelism assessment: the value/VM/GC path is deliberately single-threaded per isolate
(thread-local heap, non-atomic RC — the P-VMT wins depend on it), and the language-level parallelism
seam (shared-nothing OS-thread isolates + channels) already exists. This arc does **not** add
intra-heap parallelism; it removes the three taxes that make the *existing* seams expensive:

1. **P-SHARE** — a big shared input fanned to N isolates is `Wire`-copied N times; the I.3
   `SharedRegion` borrow-share machinery is built and miri-proven but never wired to the real
   spawn path (`plans/perf/remaining.md` inventory row, picked up here).
2. **P-POLISH wakeups** — a stalled parent scheduler waits on cross-thread work with a 100 µs
   sleep-spin (`scheduler.rs isolate_in_flight_wait`), burning CPU and adding latency to every
   producer/consumer isolate pipeline.
3. **P-JIT-BG** — tier-1 Cranelift compilation runs synchronously on the mutator thread at
   promotion/OSR; a background compile thread is the classic engine fix — *if* the pause is
   measurable (P-CALL's lesson: measure before restructuring).

Explicitly **out of scope** (rejected in the assessment, do not drift into them): shared heap /
atomic RC on the value path, parallel dispatch, concurrent GC, parallel compile front-end.

## Posture (inherited from the perf sweep — unchanged)

- **Every slice ships with a benchmark that validates the gain.** A perf claim without a
  before/after number does not land (S0 builds the harnesses first).
- **Everything here is real-path only and invisible to `RunResult`.** The deterministic sandbox
  keeps copying per isolate and never spins threads or a JIT, so the differential's
  `0 skipped / backends agree` gate holds **by construction** — same out-of-oracle posture as
  isolates I.4. The leak oracle and miri (on `noeta-value`) are the safety gates that *do* see
  this work.
- S4 ends at a **go/no-go decision with the user** after its S0 measurement — not a silent drop.

## Slices

| # | Tag | Slice | Impact | Effort | Independent? |
|---|---|---|---|---|---|
| S0 | P-PAR-BENCH | [Baseline benchmarks](s0-benchmarks.md): fan-out copy cost, stall latency, JIT pause | enables the rest | S | yes |
| S1 | P-PAR-ARC | [`Rc<Shape>`/`Rc<PackedSchema>` → `Arc`](s1-arc-shape.md) — the Send prereq for borrow-share | prereq (gate: no hot-path regression) | M | yes |
| S2 | P-PAR-SHARE | [Wire `SharedRegion` into the real spawn path](s2-shared-region-spawn.md): promote once, borrow N times | high (fan-out workloads) | M–L | needs S1 |
| S3 | P-PAR-WAKE | [Park/wakeup instead of sleep-spin](s3-scheduler-wakeup.md) | medium (pipeline latency, idle CPU) | S | yes |
| S4 | P-PAR-JITBG | [Off-thread JIT compilation](s4-offthread-jit.md) — measure-first, go/no-go | unknown until measured | M (if go) | yes |

## Sequencing (value × independence, ascending risk — the sweep's spine)

1. **S0 first** — the mandate is bench-first, and S1's gate (no regression on the M2.0 VM
   baselines) plus S4's go/no-go both need numbers before any restructuring.
2. **S1 → S2** — the dependent spine. S1 is the one real structural change (`Send` shapes); it
   carries its own regression gate because `Arc` clones put atomic ops where `Rc` clones were.
   If the M2.0 baselines regress measurably, fall back to shapes-by-`Module`-index (design in
   the slice doc) rather than eating a hot-path tax for the seam's benefit.
3. **S3** — small, self-contained, lands any time; sequenced after the S1/S2 spine starts only
   because S0's stall-latency bench must exist first.
4. **S4 last** — measurement may kill it (that decision goes to the user with the S0c numbers).

## Invariants every slice asserts before "done"

- `cargo run -q -p noeta-cli -- test` conformance green; differential matched / **0 skipped** /
  backends agree (sandbox untouched ⇒ by construction, but assert it).
- `cargo test --workspace` green; miri clean on `noeta-value` (S1/S2 especially); leak oracle 0.
- `cargo clippy --all-targets` and `cargo fmt --all --check` clean.
- The slice's benchmark, before/after, numbers recorded in the slice doc.
- Commit per green slice (standing directive); never push without authorization.
