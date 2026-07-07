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

## Numbers

_S0c baseline + (if go) after table to be recorded here._
