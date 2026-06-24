# Phase 0 — baseline & benchmark infrastructure

Status: **in progress**.

Goal: make every later slice *measurable*. Capture the current numbers and add the bench cases
the optimizations need — crucially an **eval-side** harness (P-COW lands on the tree-walker, which
the VM benches can't see) and a **parameterized accumulator** (so P-COW's asymptotic win is
visible, not just a constant factor).

## What exists today

`crates/lang-vm/benches/vm.rs` (M2.0 baseline) benches the **VM** on three fixed-size programs:
`dispatch_fib` (recursive fib(24)), `property_access` (cached field LOADs — aspirational; ICs
don't exist yet), `allocation_list` (build/drop a small list per iteration). No eval-side bench.
No parameterized/scaling bench. `crates/lang-eval` has no `benches/`.

## Work

### 0.1 — eval-side bench harness (`crates/lang-eval/benches/eval.rs`)
- New criterion bench mirroring the VM harness's structure, but driving the **tree-walker**
  (`lang_eval`'s program entry point — the same one the CLI/conformance use for the eval backend).
- Compile/parse once in setup; time only evaluation.
- `criterion` as a dev-dependency + `[[bench]] harness = false` in `crates/lang-eval/Cargo.toml`.

### 0.2 — parameterized accumulator bench (both backends)
- A program that builds a list by repeated self-append: `mut acc = []; for i in 0..n { acc ~= [i]; } echo acc.count();`
- Run it over `n ∈ {1000, 2000, 4000, 8000}` via criterion's `bench_with_input`, on **both**
  backends. Pre-COW the eval-side timings should ~quadruple as n doubles (O(n²)); post-COW they
  should ~double (O(n)). The VM side stays O(n²) until P-GC.
- This is the bench that *validates* P-COW. It lives where both slices can run it.

### 0.3 — member-dispatch bench (for P-IC)
- A program with a hot polymorphic-ish call/property site in a loop (method call + field read on
  the same receiver), parameterized similarly, on the VM. Gives P-IC a before/after target beyond
  the existing `property_access`/`dispatch_fib`.

### 0.4 — capture the baseline
- Run `cargo bench` for both harnesses; paste the headline numbers into this doc's
  **Baseline** section below so later slices have a fixed reference. (criterion also writes its
  own `target/criterion` comparison data, but the doc is the durable record.)

## Baseline (captured)

Captured on branch `types-inferred-static` with `--warm-up-time 0.3 --measurement-time 1.5
--sample-size 10` (quick settings — the *scaling ratio* is the signal, not absolute precision).
Means below.

**Fixed-size:**

| bench | mean |
|---|---|
| `eval/dispatch_fib` (fib 24) | 32.96 ms |
| `vm/dispatch_fib` | 14.53 ms |
| `vm/property_access` | 599 µs |
| `vm/allocation_list` | 598 µs |

**Parameterized accumulator `acc ~= [i]` — the P-COW target (O(n²) today):**

| n | `eval_accumulate` | ratio | `vm_accumulate` | ratio |
|---|---|---|---|---|
| 1000 | 1.33 ms | — | 334 µs | — |
| 2000 | 4.70 ms | ×3.5 | 934 µs | ×2.8 |
| 4000 | 17.6 ms | ×3.7 | 3.44 ms | ×3.7 |
| 8000 | 68.6 ms | ×3.9 | 11.6 ms | ×3.4 |

The ≈×4-per-doubling on both backends is the quadratic signature. P-COW (eval) should flatten the
eval column to ≈×2; P-GC's VM-COW half flattens the VM column.

**Parameterized member dispatch — the P-IC target (already O(n); P-IC cuts the constant):**

| n | `vm_member_dispatch` | ratio |
|---|---|---|
| 1000 | 198 µs | — |
| 2000 | 397 µs | ×2.0 |
| 4000 | 793 µs | ×2.0 |
| 8000 | 1.62 ms | ×2.0 |

Clean linear scaling; P-IC targets the per-iteration dispatch/property-lookup constant, not the
complexity class.

## Verification
- `cargo bench --no-run` compiles both harnesses (bench programs stay in the VM subset).
- Workspace/clippy/fmt clean. No `RunResult` change (benches are not conformance), so differential
  is untouched — but run it anyway to confirm no accidental coupling.

## Notes
- Bench programs must stay **compilable to the VM subset** (the VM harness `.expect()`s it), so
  keep them in the shared language fragment both backends accept.
- `Date.now()`/wall-clock is unavailable in bench *programs* (language has a logical clock); fine —
  criterion does the timing in Rust.
