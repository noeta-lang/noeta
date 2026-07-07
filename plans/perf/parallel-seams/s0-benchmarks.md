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

**Measure:** wall time of the scope vs N, and (from `noeta-alloc-probe` / heap counters) peak
live objects — today's path materializes ~N copies of the corpus; the S2 target is ~1 promotion.

**Where:** a `.noe` program under `scratch-bench/` for the end-to-end number + a Rust harness
that calls `marshal`/`rebuild` directly on the same graph for the isolated copy cost.

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

_To be recorded here as S0 lands._

## Verification

Arc-standard invariants (README) — plus: the harnesses themselves are deterministic enough to
re-run for the "after" columns (fixed seeds, fixed N/K/M).
