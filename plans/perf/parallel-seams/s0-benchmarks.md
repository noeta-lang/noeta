# S0 — P-PAR-BENCH: baseline benchmarks

Three measurements, one per downstream slice. All run on the **real** path (`jit` feature where
relevant, RealHost executor); none touch the sandbox. Durable harnesses go in criterion where the
workload is steady-state; the fan-out and pause measurements are end-to-end timings (a bench
binary or integration-test-style harness), because criterion's model fits per-op loops, not
spawn-once lifecycles.

## S0a — fan-out copy cost (gates S2)

**Workload:** build one large `Send` value graph (a `List` of structs — e.g. 100k records with a
string + ints, deep enough that `marshal` does real work), then `isolate score(corpus)` fanned to
N ∈ {1, 2, 4, 8} workers inside one `concurrent` scope, each worker doing a cheap reduction so
copy time dominates compute.

**Measure:** wall time of the scope vs N, plus child max-RSS as the residency proxy — today's
path materializes ~N copies of the corpus; the S2 target is ~1 promotion. (A direct
`marshal`/`rebuild` Rust micro-harness was considered and skipped: the n0→n1 wall delta plus the
per-worker RSS slope already isolate the copy cost cleanly.)

**Where:** `.noe` programs + `run.py` under `tests/bench/parallel-seams/` (committed — the
"after" columns re-run them).

## S0b — stall latency under the sleep-spin (gates S3)

**Workload:** a parent↔worker ping-pong over a shared cross-thread channel (send one small
message, worker echoes, K rounds), plus a "worker finishes while parent stalls" case (parent
joins an isolate that sleeps ~1 ms then returns).

**Measure:** per-round latency. The 100 µs sleep quantum in `isolate_in_flight_wait`
(`crates/noeta-vm/src/scheduler.rs:78`) puts a floor of ~½ quantum per stall on every round; the
S3 target is wakeup-bound (µs-scale). Also observe parent CPU time vs wall time (the spin burns
a core's worth of wakeups while idle).

## S0c — JIT promotion pause (gates S4's go/no-go)

**Workload:** a program with M distinct functions each crossing `JIT_HOT_THRESHOLD` (50), so M
tier-1 compiles happen mid-run via `jit_enter`/`jit_osr_backedge`
(`crates/noeta-vm/src/lib.rs:1474/1566` → `noeta_jit::Jit::compile`, `noeta-jit/src/lib.rs:436`).

**Measure:** per-compile wall time (instrument around the `compile` call — a `debug`-gated or
feature-gated timer is fine for the measurement pass; it does not ship), distribution across
function sizes, and total pause as a fraction of program runtime. **Decision input:** if the
typical pause is trivial (≪ 1 ms and a negligible runtime fraction on realistic programs), S4 is
a no-go — take that to the user with the numbers, don't restructure for its own sake.

## Numbers (before)

2026-07-07, AMD Ryzen AI 9 365 (20 threads), release build, harnesses in
`tests/bench/parallel-seams/` (`run.py`, median of 7; `jit_pause` example).

### S0a — fan-out (100k-record corpus, string + 2 ints per record)

| Fixture | wall median | cpu/run | max RSS |
|---|--:|--:|--:|
| `fanout_n0` (no isolate) | 61.3 ms | 60.8 ms | 43 MB |
| `fanout_n1` | 112.1 ms | 110.9 ms | 94 MB |
| `fanout_n2` | 131.6 ms | 175.7 ms | 143 MB |
| `fanout_n4` | 168.1 ms | 289.3 ms | 189 MB |
| `fanout_n8` | 232.6 ms | 519.7 ms | 224 MB |

Both premises confirmed: **residency scales ~linearly in N** (≈25–50 MB/worker — each worker
rebuilds its own corpus copy; late-worker frees mask some of it in max-RSS), and **wall time
grows with N even though compute is parallel** (+17 ms/worker past n1) because each worker's
`marshal` runs serially on the parent thread before its spawn. S2 target: ~1× corpus residency,
near-flat wall in N (one promotion, N borrows).

### S0b — ping-pong (2000 rounds, capacity-1 channels)

| Fixture | wall median | cpu/run |
|---|--:|--:|
| `pingpong_coop` (spawn, one thread) | 13.6 ms | 13.4 ms |
| `pingpong` (real isolate) | 319.3 ms | 24.3 ms |

**~160 µs/round vs ~7 µs cooperative — 23× —** and CPU is only 24 ms of the 319 ms, so the gap
is pure sleeping: the parent eats the `isolate_in_flight_wait` 100 µs quantum (plus wake/reschedule
slop) every round. S3 target: wakeup-bound rounds, wall collapsing toward the coop floor + a few
µs/round of condvar signalling.

### S0c — JIT promotion pause (30 functions × 60 calls, hot-counter tiering)

`JitStats` now carries `compile_ns_total`/`compile_ns_max` (accounted inside `Jit::compile`,
cache hits excluded). `JIT_PAUSE_STMTS=<n>` makes the 30 bodies uniform at n statements.

| Body size | wall | compile total (% wall) | worst pause | avg/compile |
|---|--:|--:|--:|--:|
| 5 stmts | 110.7 ms | 109.8 ms (99.2%) | 15.5 ms | 3.5 ms |
| 40 stmts | 659.2 ms | 653.6 ms (99.2%) | 33.3 ms | 21.1 ms |
| 160 stmts | 4515.5 ms | 4490.4 ms (99.4%) | 193.5 ms | 144.9 ms |
| mixed 5/40/160 | 1444.8 ms | 1434.9 ms (99.3%) | 126.8 ms | 46.3 ms |

**Unambiguous go-signal for S4** — synchronous compilation dominates wall time in every scenario,
and single pauses reach ~200 ms. Two observations for the S4 pass: (1) cost per compile is much
higher than typical Cranelift folklore (ms-scale for a 7-line int function) — the JIT runs
`opt_level=speed` (egraph opts) and emits **two** bodies per eligible prototype (classic + fast
convention), so an `opt_level` A/B belongs in the S4 decision; (2) the pause scales superlinearly
with body size, so real programs with large hot functions feel this worst exactly at their
latency-sensitive moment (OSR inside a running loop).

## Verification

Arc-standard invariants (README) — plus: the harnesses themselves are deterministic enough to
re-run for the "after" columns (fixed seeds, fixed N/K/M).
