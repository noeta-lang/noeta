# Phase 7 — the cumulative before/after benchmark

The mandate's closing deliverable: run the complete suite against the pre-migration (Phase-0)
baseline and report deltas per metric. Captured 2026-06-26, debug toolchain, criterion bench profile
(`--warm-up-time 1 --measurement-time 2`), single coherent run; peak residency via the `live_peak`
meter; leak oracle via `lang test --check-leaks`.

## Read this first: the measurement machine changed — compare *ratios*, not raw µs

Phase 0 was captured 2026-06-25; this run is a day later on a **materially slower environment**. Three
benches the MM migration *never touches* prove it — they are pure dispatch / cached reads / list
build-and-drop with no change to their code paths between Phase 0 and now:

| MM-neutral bench | Phase 0 | Phase 7 | factor |
|---|---:|---:|---:|
| `vm/dispatch_fib` (recursive fib 24) | 13.33 ms | 32.77 ms | **2.46×** |
| `vm/property_access` (cached field reads) | ~615 µs | 1.49 ms | **2.42×** |
| `vm/allocation_list` (build+drop a 3-list/iter) | ~567 µs | 1.63 ms | **2.88×** |

So the whole machine is **~2.4–2.9× slower** than the Phase-0 capture. Raw-µs deltas against the
Phase-0 table would therefore read every bench as a 2.5× "regression" that is entirely the
environment. The honest comparison — and the one the Phase-0 doc anticipated ("the *comparison* is
what matters, not absolute targets") — is the **machine-independent** view: within-run ratios
(reuse off/on, collector trace/trial), **complexity-class slopes**, and **peak residency object
counts**. Those are reported below; absolute µs are given for completeness but normalized against the
~2.46× yardstick (median of the three neutral benches) when a verdict needs them.

---

## 1. Throughput

Linear `n ∈ {1000,2000,4000,8000}` benches; the **slope** (complexity class) is the portable signal.

| Bench (Phase 7 median) | n=1000 | n=2000 | n=4000 | n=8000 | per-iter | shape |
|---|---:|---:|---:|---:|---:|---|
| `vm_mm/alloc_churn` | 318 µs | 615 µs | 1.27 ms | 2.54 ms | ~0.317 µs | linear |
| `vm_mm/destructor_heavy` | 627 µs | 1.23 ms | 2.53 ms | 4.90 ms | ~0.61 µs | linear |
| `vm_member_dispatch` | 407 µs | 838 µs | 1.71 ms | 3.30 ms | ~0.41 µs | linear |
| `vm_accumulate` (list) | 292 µs | 580 µs | 1.16 ms | 2.27 ms | ~0.28 µs | linear |

**vs Phase 0, normalized:**

| Bench | Phase 0 (n=8000) | Phase 7 (n=8000) | raw ratio | ÷2.46 env |
|---|---:|---:|---:|---:|
| `vm_mm/alloc_churn` | 865 µs | 2.54 ms | 2.93× | **1.19×** |
| `vm_mm/destructor_heavy` | 1.525 ms | 4.90 ms | 3.21× | **1.31×** |

**Verdict.** Both hot paths stay **strictly linear** — no complexity regression. After removing the
~2.46× environment factor, allocation churn sits ~1.19× (essentially flat — the residue is the trace
collector's per-alloc registry `HashSet` op, the artifact Phase 6 already flagged as closable by an
intrusive free-list) and destructor-heavy ~1.31×. The destructor residue is the *expected* cost of
Phase 4 generalizing `__destruct` from globals-only to **last-use in every scope** — more destructors
now fire, promptly and correctly, for a modest linear constant. Member dispatch and the list
accumulator are linear with no regression (the P-IC inline-cache and P-COW work hold up).

## 2. Peak heap residency — the headline footprint metric (machine-independent)

Object counts from the `live_peak` high-water meter; immediates never counted. **These numbers do not
depend on the machine** — the win shows here, undistorted:

| Program | Phase 0 | Phase 7 | shape |
|---|---:|---:|---|
| `alloc_churn` (n=4000) | 3 | **2** | **n-independent** — each short-lived record dies before the next |
| `accumulate_records` (n=4000) | 4004 | **4003** | scales with n — the list + n live `Pair`s, a genuinely-live structure |
| `sequential_intermediates` (n=50 / n=400) | — | **2 / 2** | **n-independent** — transient intermediates reclaimed at last use |

**Verdict.** The "built right" result. Prompt last-use reclamation makes the footprint of
transient-intermediate patterns **n-independent** — `sequential_intermediates` holds a flat **2**
objects whether it produces 50 or 400 of them, and `alloc_churn` even shaved 3→2. Genuinely-live
structures retain *exactly* their live set (`accumulate_records` ≈ n, which no reclamation strategy can
shrink). Peak memory is now a function of what the program *keeps alive*, not of how much it churns
through — the Phase-3 target, met.

## 3. Reuse — the full matrix vs the copying baseline (machine-independent ratio)

`eval_record_update` runs the same record-update accumulator (`acc = T { ...acc, … }`) with reuse
**off** (always copy) and **on** (mutate-in-place when unique), same machine, same run — so the ratio
is clean:

| n | off (copy) | on (reuse) | speedup |
|---:|---:|---:|---:|
| 1000 | 2.164 ms | 0.502 ms | 4.31× |
| 2000 | 4.224 ms | 0.957 ms | 4.41× |
| 4000 | 8.465 ms | 1.884 ms | 4.49× |
| 8000 | 16.90 ms | 3.804 ms | **4.44×** |

VM read-update vs blind-update parity (both reuse-on): `vm_record_update` (blind) 1.407 ms vs
`vm_record_update_read` (reads the accumulator) 1.358 ms at n=8000 — **equal within noise**.

**Verdict.** Generalized reuse is a **~4.4× constant-factor** win on the record-update accumulator,
both branches linear. This **matches and exceeds the targeted P-REUSE prototype** (~3.1× on eval) —
now via the *general* IR reuse pass over all constructors, not a syntactic special case. The VM
read-update ≈ blind-update parity confirms Phase-3 drop insertion unlocked reuse for the idiomatic
read-modify accumulator the prototype could not reach on the VM (the receiver-temporary that blocked
it is now dropped at last use).

## 4. Destructor promptness & correctness

`vm_mm/destructor_heavy` (linear, §1) exercises destructor firing on every iteration; both backends
run the *same* RC-annotated IR, so last-use destruction order is identical by construction. The
conformance `gc/` cases (`cycle_capture_destructor`, `cycle_external_ref`, the destructor-order suite)
pin the observable order and pass on both backends; the differential is **0-skipped / agree**.

**Verdict.** Destruction is prompt (at last use, §2 proves the footprint effect) and identical across
backends — correctness by shared-IR construction, not by two hand-matched implementations.

## 5. Cycle reclamation — collector overhead (trace vs trial)

`vm_collector`, both collectors, both workloads (`cyclic_*` = build/abandon n closure↔cell cycles;
`churn_*` = n acyclic short-lived records — pure collector tax on ordinary code):

| n | cyclic_trace | cyclic_trial | churn_trace | churn_trial |
|---:|---:|---:|---:|---:|
| 1000 | 1.198 ms | 1.175 ms | 306 µs | 263 µs |
| 2000 | 2.395 ms | 2.324 ms | 610 µs | 519 µs |
| 4000 | 4.884 ms | 4.777 ms | 1.223 ms | 1.052 ms |
| 8000 | 10.38 ms | 9.39 ms | 2.447 ms | 2.109 ms |

**Verdict.** The trade-off Phase 6 characterized persists: trial-deletion wins **acyclic churn by
~14%** (2.11 vs 2.45 ms at n=8000 — it pays nothing per allocation; the trace pays one registry op).
On **cyclic** garbage the two are now close (within ~10%, trial marginally ahead this run vs the trace
marginally ahead in Phase 6 — inside the environment's noise band). The default stays
**`CollectorMode::Trace`** on the Phase-6 rationale (simplicity, it never mutates the hot `free` path,
robustness to floating garbage), with `TrialDeletion` behind a flag for churn-dominated workloads.
Both reach **residency 0** on the whole corpus. The recommended follow-up — an intrusive free-list to
erase the trace's per-alloc registry tax — is unchanged (it would also close the §1 alloc-churn
residue).

## 6. Compile-time cost (the front-end tax)

The lowering + RC passes add front-end work. Measured over the corpus: lowering every comparable
program to Core IR and running `insert_drops` + `thread_reuse` (the `ir_lowering_is_total_over_the_corpus`
sweep — 186 programs lowered, 0 skipped, plus parse attempts on 79 more) completes in **~0.40 s** of
test time (~2 ms/program including parse). The RC passes are **single linear walks over the IR**; the
tax is small, fixed per program, and **compile-once** (the runtime benches above compile their module
once, outside the timed loop, so the throughput numbers already exclude it).

**Verdict.** The front-end cost of moving RC onto the IR is negligible and linear — well within the
budget the runtime wins (n-independent peak residency, 4.4× reuse) justify.

---

## Overall

Normalizing away the ~2.46× environment shift, the migration is a clear net win and **regresses
nothing in complexity class**:

- **Peak residency** of transient-intermediate patterns is now **n-independent** (the headline).
- **Reuse** is a **~4.4× constant factor**, general across constructors, matching/beating the targeted
  prototype.
- **Hot paths** stay linear; allocation churn is ~flat after normalization, destructor-heavy carries a
  modest linear constant that *buys* correct last-use destruction in every scope.
- **Cycles** are collected to **residency 0** on both backends; the collector tax is small and the
  trace/trial trade-off is understood.
- **Compile-time** cost is negligible and linear.
- **Leak oracle: residency 0 at clean exit on every one of 186 programs, both backends** — the durable
  guarantee the whole track was built to make un-shippable-to-break.

## How to reproduce

```sh
cargo bench -p lang-vm   --bench vm   -- --warm-up-time 1 --measurement-time 2
cargo bench -p lang-eval --bench eval -- --warm-up-time 1 --measurement-time 2
cargo test  -p lang-vm --lib mm_peak_residency -- --nocapture
cargo run   -q -p lang-cli -- test --check-leaks
```
