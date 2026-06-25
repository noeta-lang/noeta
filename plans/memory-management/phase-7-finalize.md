# Phase 7 — Finalize & full benchmark

Cleanup, the oracle decision, the cumulative "vs what we have now" benchmark the mandate names, and the
durable documentation.

## 7.1 Retire superseded machinery & decide the oracle

- Delete the targeted P-REUSE detection now subsumed by the IR reuse pass (`record_self_update`,
  `linear_record_accumulators`, `drop_receivers`, the COW/record special-case *detection* — keep the
  in-place *ops* the lowering emits). The P-REUSE conformance tests stay and pass via the general path.
- Remove the now-false caveats: `lang-gc` "dormant / not wired", eval `Scope` "leaks until process
  exit", "M0 immutable so no cycles", and the `plans/perf/p-*.md` "drop insertion is milestone-scale,
  not done" notes → point at this track. Reconcile `plans/perf/research-notes.md` (the P-GC reframe) with
  what shipped.
- **AST-walker reference oracle:** decide its fate (raised in Phase 4 — it predates last-use
  destruction). Either retire it (the IR-interpreter + leak oracle + property test + VM differential now
  cover its role) or keep a *semantics-updated* version as a permanent third oracle. Record the decision
  and rationale (README §2 independence tradeoff).
- **REPL on the IR — DONE early (committed during Phase 5).** `Session::eval` now lowers each batch,
  runs the precise-RC drop + reuse passes, and executes on the **Core-IR interpreter** in the persistent
  global scope (a `pub(crate) run_ir_batch` running `exec_ir_stmts` with a per-batch `Frame`), falling
  back to the AST walker only when lowering fails. A trailing bare expression is rewritten to a reserved
  sentinel binding (`\0repl-value`) so its value is captured once and echoed. This removes the REPL as an
  AST-walker user — so the walker's remaining roles narrow to (a) destructor/leaf executor the IR interp
  reuses and (b) the totality fallback, both of which 7.1's retire-or-keep decision now covers cleanly.
  Behavior change (accepted): the REPL now fires within-function destructors at last use, matching
  `lang run`. Known limitation: reflection (`attributes_of`/`roles_of`) is rebuilt per batch (resolves
  within an entry, not across entries) — cross-entry accumulation is a small follow-up if wanted.

## 7.2 The full benchmark vs the Phase-0 baseline

Run the complete suite against the pre-migration numbers, per metric:

- **Throughput:** dispatch, property, member dispatch, allocation churn (did the IR + reuse-aware
  allocation + prompt reclamation help or hurt the hot loop? — the ANF lowering's register pressure vs
  the allocator's reuse).
- **Peak residency:** the leak counter's peak — prompt last-use reclamation should cut peak memory
  materially vs the old reclaim-at-teardown model. A headline "built right" result.
- **Reuse:** the full matrix across all constructors vs the P-REUSE targeted numbers (match/beat, now
  general) and vs `off`.
- **Destructor promptness & correctness:** destructor-heavy workloads reclaim at last use, both backends
  identical.
- **Cycle reclamation:** the chosen collector vs the (now-impossible) leak.
- **Compile-time cost:** the lowering + RC passes add front-end work — measure and record it, so the
  runtime wins are weighed against the compile-time tax.

Write `phase-7-benchmarks.md` with the before/after table and a one-paragraph verdict per metric.

## 7.3 Documentation & memory

- `docs/` MM model writeup: the **Core IR + precise-RC-as-passes + backup-trace** architecture, the
  safety invariant, the destructor spec, the backend-independence tradeoff, and the JIT-readiness seam —
  the durable reference.
- Update memory: a `memory-management` project note recording the shipped model (shared IR, RC passes,
  collector chosen by data), the JIT-readiness decisions, the oracle decision, and pointers here;
  refresh `MEMORY.md` and the `perf-sweep` note (P-REUSE → superseded by this track).

## Verification gate

- Full conformance + **differential 0 skipped / agree**; leak oracle **0 on the entire corpus, both
  backends** (the durable guarantee); static-≤-dynamic property test green.
- `cargo test --workspace` + a miri sweep over all touched unsafe/refcount/collector paths.
- clippy + fmt clean.
- The before/after benchmark table is complete with written verdicts — the deliverable the mandate
  asked for ("after implementation, benchmark against what we have now").
