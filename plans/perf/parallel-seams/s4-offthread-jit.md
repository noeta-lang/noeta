# S4 — P-PAR-JITBG: off-thread JIT compilation (measure-first, go/no-go)

## Today

Tier-0→tier-1 promotion compiles Cranelift code **synchronously on the mutator thread**: a call
crossing `JIT_HOT_THRESHOLD` (50) blocks in `jit_enter` (`crates/noeta-vm/src/lib.rs:1474`), a
hot back-edge blocks in `jit_osr_backedge` (`lib.rs:1566`), both inside
`noeta_jit::Jit::compile` (`crates/noeta-jit/src/lib.rs:436`). The program pauses for every
compile, exactly when it just proved itself hot.

## Measure first (S0c) — the P-CALL lesson

S2 of P-CALL disproved its own premise by measuring before restructuring; same discipline here.
S0c produces per-compile pause times and total-pause-as-runtime-fraction. **Go/no-go is a user
decision with those numbers in hand** — plausible outcomes:

- Pauses ≪ 1 ms and a negligible fraction → **no-go**, close the slice with the numbers recorded.
- Pauses material on real programs (many/large functions, OSR in latency-sensitive loops) →
  **go**, design below.

## Design (if go)

- One background compile thread per VM (lazy-spawned on first promotion; real path only — the
  sandbox/differential tier-0 baseline never JITs, unchanged). Channel of requests
  `(Arc<Module>, proto)` — `Module` is already `Arc` and immutable; a `CompiledFn` result
  mailbox (`mpsc` or mutex slot) polled at the existing promotion sites.
- Mutator semantics: on hot, *request* compilation and **keep running tier-0**; install the
  native entry when the mailbox delivers it (next `jit_enter`/back-edge check — the sites
  already run per call/loop, so installation needs no interrupt). `jit_declined` and counter
  bookkeeping unchanged; a proto with an in-flight request is not re-requested.
- `force_jit` (the `--jit-differential` oracle path) stays **synchronous** — the oracle needs
  deterministic "compiled before run" behaviour, and it is not a perf path.
- Prereqs to verify at implementation time: `cranelift_jit` module + finalized code pointers are
  safely transferable to the executing thread (finalize on the compile thread, ship the fn
  pointer; the JITModule must outlive execution — likely keep the `Jit` engine owning memory on
  the VM side and do codegen-only off-thread, or ship the whole finalized module). This is the
  one real design risk; resolve it before writing the threading.

## Gate

- S0c re-run: promotion pauses off the mutator (compile time unchanged, mutator no longer
  blocked); end-to-end wall time on a compile-heavy program improves or is flat (never worse —
  tier-0 continuing during compile can only add instructions retired, not pauses).
- `--jit-differential` oracle byte-identical (forced path untouched); leak-under-JIT oracle
  green.

## Shipped (2026-07-07) — GO, as designed plus two findings

**`opt_level` A/B first (the measurement the user asked for):** `NOETA_JIT_OPT=none` halves
compile time (mixed: 1695.6 → 835.6 ms total, worst pause 165 → 70 ms) for only **~5% slower
generated code** (200k-call runtime 13.75 → 14.43 s). Worth knowing — the env knob ships as a
dev tool — but not the fix: even `none` averages 27 ms/compile, so the cost is *not* the egraph
optimizer. Follow-up observation (out of this slice): ms-scale compiles for 7-line functions
suggest a compile-throughput investigation (two bodies/proto, regalloc on long chains?).

**Off-thread service (`crates/noeta-vm/src/jit_service.rs`):** a background thread owns the
Cranelift engine for its whole life (`!Send` raw-pointer bakes make moving it impossible and
unnecessary); the mutator queues prototype indices, keeps interpreting, and drains a mailbox
into **mirror tables** (`jit_entries`/`jit_fast`) at its existing promotion checkpoints — the
single tier-1 lookup source in both modes, so the engine's tables are never shared. Every
request gets exactly one response (failed compile ⇒ per-proto decline), so the pending counter
always drains. OSR requests born at a back-edge enter mid-loop the moment the entry lands
(`jit_osr_pending`). Teardown shuts the service down **last** (destructors may call compiled
code) and **abandons** the outstanding queue — nothing will ever run those entries; the stats
entry points (`run_module_jit_hot_with_stats`) instead set `jit_drain_at_exit` so the OSR
promotion tests keep deterministic counts. `force_jit` (the oracle) keeps the synchronous
on-VM engine: the jit-differential is untouched by construction.

## Numbers

| Measurement | before (sync) | after (off-thread) |
|---|--:|--:|
| `jit_promo.noe` end-to-end (`noeta run`, 12 hot 100-stmt fns, tests/bench/parallel-seams) | 1096.2 ms | **46.9 ms** (**23×**) |
| fib(30) end-to-end (compile lands mid-flight, native entered) | 26.6 ms | 31.2 ms (−4.6 ms: mutator interprets while Cranelift works — the intended trade on quick-hot programs) |
| worst mutator pause (S0c) | up to ~194 ms | **0** (compile never on the mutator) |

Gates: workspace 73 suites, vm+jit 140, CLI 59, conformance 501, differential agree,
jit-differential "tier 1 agrees with tier 0 and leaks nothing" — all green.
