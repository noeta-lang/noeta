# Phase 0 — benchmark baseline (the "before")

The pre-migration snapshot every later phase compares against (architecture §0.3, README §5/§7). Two
kinds of measurement: **throughput** (criterion, `crates/lang-vm/benches/vm.rs`) and **peak heap
residency** (the `live_peak` meter, `lang-vm` test `mm_peak_residency_baseline`). Numbers are
machine- and build-specific — the *comparison* is what matters, so Phase 7 re-runs this exact suite on
the same machine and reports deltas, not absolute targets.

Captured 2026-06-25, debug toolchain, criterion release/bench profile (`--warm-up-time 1
--measurement-time 2–3`), single run. Treat them as order-of-magnitude anchors, not precise constants.

---

## 1. MM-stress throughput (new this phase)

The reclamation-cost benches, parameterized over loop size `n` so the *slope* (complexity class) is
visible, not just a constant.

| Bench | n=1000 | n=2000 | n=4000 | n=8000 | shape |
|-------|-------:|-------:|-------:|-------:|-------|
| `vm_mm/alloc_churn` (build+drop a short-lived record/iter) | 109 µs | 220 µs | 437 µs | 865 µs | **linear**, ≈0.108 µs/iter |
| `vm_mm/destructor_heavy` (reassign a `destruct`-bearing global/iter) | 202 µs | 389 µs | 775 µs | 1525 µs | **linear**, ≈0.19 µs/iter |

| Bench | time | note |
|-------|-----:|------|
| `vm_mm/deep_free` (tear down a 100-deep nested list) | 3.26 µs | one recursive teardown |

Readings:
- **Allocation churn is linear** at ~108 ns/iteration — alloc + refcount-to-zero of one record. This
  is the path prompt last-use reclamation (Phase 3) and reuse (Phase 5) most directly move.
- **Destructor firing roughly doubles** the per-iteration cost vs plain churn (~190 ns vs ~108 ns):
  the reassignment runs the displaced instance's `destruct`. Phase 4 generalizes destructor firing to
  all scopes; this is the cost basis it must not regress for destructor-free code.
- **Deep teardown is cheap per call** but **stack-bounded** — see §3.

## 2. Reference hot paths (existing benches, for context)

The shared baseline the perf sweep already tracks; reproduced here so MM changes are weighed against
non-MM throughput.

| Bench | time |
|-------|-----:|
| `vm/dispatch_fib` (recursive fib 24) | 13.33 ms |
| `vm/property_access` (cached field reads, 5000 iters) | ~615 µs |
| `vm/allocation_list` (build+drop a 3-list/iter, 5000 iters) | ~567 µs |

The `vm_accumulate`, `vm_member_dispatch`, `vm_record_update`, and `vm_record_update_read` matrices
(P-COW / P-IC / P-REUSE) remain in the bench file as the existing baseline; the MM track must not
regress them, and Phase 3's reuse-aware allocation should match or beat the `vm_record_update_read`
numbers via the general path.

---

## 3. Peak heap residency (the headline footprint metric)

The `live_peak` high-water meter, per program (objects; immediates are never counted):

| Program | peak live objects | shape |
|---------|------------------:|-------|
| `alloc_churn` (n=4000) | **3** | **n-independent** — each short-lived record dies before the next; last-use reclamation already keeps the footprint flat |
| `accumulate_records` (n=4000) | **4004** | **scales with n** — the list + 4000 live `Pair`s; a genuinely-live structure prompt reclamation *cannot* shrink |

This is the metric Phase 3 targets: for accumulator patterns that build transient intermediate copies,
prompt last-use reclamation should cut the *peak* materially vs the reclaim-at-teardown model. The
`alloc_churn` peak is already flat (3), so the win shows up in patterns that today retain intermediates
to scope/teardown — the record-update accumulators the reuse matrix measures.

**Note on immediates:** an `int` list of any length has peak ~2–4 objects (the list is one heap
object; its elements are NaN-boxed immediates). Peak-residency benches therefore use **record**
elements to expose the scaling footprint.

---

## 4. Finding: recursive teardown is stack-bounded

`lang-value`'s `free` releases a container's children by **recursion** (`free → release_child →
free → …`), so tearing down a deeply *nested* structure recurses one frame per level. Measured limits
(debug frames are large and vary with codegen):

- **Debug build, main thread (8 MiB):** overflows around **60–200 levels** (noisy across rebuilds).
- **Debug build, libtest thread (2 MiB):** overflows much shallower (tens of levels) — why the
  peak-residency *test* does not measure deep nesting (it is benched on the optimized profile instead).
- **Release/optimized build:** handles **2000+ levels** comfortably (tiny frames), still ultimately
  bounded.

This is a real reclamation limitation, recorded here as a **candidate for an iterative teardown**
(an explicit work-list instead of native recursion) — a natural fit for Phase 3/4, where drop
placement moves onto the Core IR and the release walk can be restructured. It is not a correctness
bug (well-nested real programs are far below the limit) but it caps the depth of structure the runtime
can reclaim and is worth closing while the teardown path is being rewritten.

---

## 5. Leak-oracle baseline

The leak oracle (`run_leak_check`, `lang test --check-leaks`) over the corpus at Phase 0:

```
leak oracle: 146 programs on the tree-walker, 146 on the VM (7 parse/load-failed, not run)
4 LEAK(s) — live residency at clean exit:
  [eval] closures/capture_immutable_error.lang — 3 object(s) still live
  [eval] closures/counter_nested_fn.lang       — 3 object(s) still live
  [eval] closures/recursive_nested_fn.lang     — 3 object(s) still live
  [vm]   closures/recursive_nested_fn.lang      — 2 object(s) still live
```

Every other program reaches **residency 0 at clean exit on both backends** — the migration starts from
a nearly-clean baseline. The four residuals are all **nested-function capture cycles** (a closure ↔ its
child call-scope): the tree-walker's `destroy_globals` drain breaks only the *global*-scope cycle, and
the VM's trial-deletion collector is built but **dormant**. They are the explicit `KNOWN_LEAKS`
allowlist in the corpus gate (`leak_oracle_residency_is_zero_except_known_cycles`), and **Phase 6**
drives the list to empty (structural `Weak` parent for eval, wiring the collector for the VM). The gate
fails on any *new* leak and on any *fixed* leak (forcing the allowlist to shrink), so the debt can only
go down.

---

## How to reproduce

```sh
# Throughput (MM group):
cargo bench --bench vm -- 'vm_mm' --warm-up-time 1 --measurement-time 3
# Core hot paths:
cargo bench --bench vm -- 'vm/' --warm-up-time 1 --measurement-time 2
# Peak residency:
cargo test -p lang-vm --lib mm_peak_residency -- --nocapture
# Leak-oracle baseline:
cargo run -q -p lang-cli -- test --check-leaks
```
