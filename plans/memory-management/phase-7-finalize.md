# Phase 7 — Finalize & full benchmark

Cleanup, the oracle decision, the cumulative "vs what we have now" benchmark the mandate names, and the
durable documentation.

## Status

**7.1 cleanup + oracle decision — DONE.**
- The targeted P-REUSE detection (`record_self_update`, `linear_record_accumulators`, `drop_receivers`)
  was already deleted when the general IR reuse pass (`lang_ir_passes::reuse::thread_reuse`) landed in
  Phase 2 — verified absent; the in-place ops it emits (`Op::MakeRecordInPlace`, `Op::Drop`) and the
  P-REUSE conformance tests remain and pass via the general path.
- Stale caveats removed: the eval `Scope` "leaks until process exit in M0 / M1" doc (now: Phase-6
  reaper → residency 0) and the `corpus.rs` `KNOWN_LEAKS` "built but dormant (never wired)" present-tense
  narration (now: empty allowlist, Phase 6 closed the debt). `lang-gc`'s module doc was already
  accurate (its "dormant `CycleCollector`" refers to the retained prototype *struct*, not the wired
  `collect_trace`/`collect_trial_deletion` functions). The perf-sweep docs were reconciled with what
  shipped: `p-gc-tracing.md` got a **superseded** banner (gc-arena dropped; cycles via the Phase-6
  backup trace + trial-deletion; VM-COW via P-COW), `p-reuse-analysis.md`'s "milestone-scale" notes now
  point at the MM track that delivered general drop insertion + reuse, and `research-notes.md`'s P-GC
  reframe got a **SHIPPED** note.
- **AST-walker reference oracle — RETIRED (decision recorded).** It fired destructors only at global
  teardown, so once last-use destruction became observable it could not reproduce the reference
  semantics. We **retire it as an oracle** rather than promote a semantics-updated third oracle:
  maintaining a second destruction model is pure cost and would re-introduce the "two hand-written
  interpretations coinciding" fragility the shared IR removed. The independence that matters (memory
  *mechanism* — Rust `Rc` vs manual RC) is still cross-checked between the IR-interpreter and the VM,
  and the role is covered by the differential (0 skipped) + leak oracle (both backends) +
  static-≤-dynamic property test + miri. Concretely: the dead `reference_run` AST-walk fallback was
  removed (the lowering is total — gate `ir_lowering_is_total_over_the_corpus` — so the reference lowers
  unconditionally), and the now-dead AST-walk `TreeWalkBackend` wrappers (`run_with_sites`,
  `run_with_host`, `run_with_host_sites`) were deleted. The `Interpreter` machinery survives as the
  shared destructor/leaf executor the IR interpreter reuses and as the perf/property AST-walk baseline
  (`Backend::run`) — neither an oracle.

**7.2 full benchmark — see `phase-7-benchmarks.md`.** **7.3 docs — `docs/resources/05-memory-management.md`.**

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
- **REPL on the IR + AST-fallback removal — DONE early (committed during Phase 5).** `Session::eval`
  now lowers each batch, runs the precise-RC drop + reuse passes, and executes on the **Core-IR
  interpreter** in the persistent global scope (a `pub(crate) run_ir_batch` running `exec_ir_stmts` with
  a per-batch `Frame`). A trailing bare expression is rewritten to a reserved sentinel binding
  (`\0repl-value`) so its value is captured once and echoed. **No AST-walker fallback** — `lang_ir::lower`
  is *total* over the parsed language (it never produces `Unsupported`; the `ir_lowering_is_total_over_the_corpus`
  gate enforces this) and is purely syntactic, so every parsed program lowers; a fallback would be a
  second, divergent destruction semantics. The same dead fallback was **also removed from `lang run`**
  (`run_linked`), so both user-facing execution paths now have exactly one model. This narrows the AST
  walker's remaining roles to (a) the destructor/leaf executor the IR interp reuses and (b) the
  conformance reference's own fallback (`reference.rs`) + the bench/property baselines — 7.1's
  retire-or-keep decision covers the rest. Behavior change (accepted): the REPL now fires within-function
  destructors at last use, matching `lang run`. REPL multi-line input fixed too: `repl_step` now treats a
  buffer with unclosed `(`/`{`/`[` (counted over lexer tokens, so string/template braces don't miscount)
  as incomplete, so multi-line `class`/`fn`/literal entries accumulate instead of erroring. Known
  limitation: reflection (`attributes_of`/`roles_of`) is rebuilt per batch (resolves within an entry, not
  across) — cross-entry accumulation is a small follow-up if wanted.

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
