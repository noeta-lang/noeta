# VM-throughput arc (P-VMT) — closing the scalar/loop/call gap

**Status: planning (proposal for sign-off).** Motivated by the 2026-07-03 lang-vs-PHP 8.4
benchmark + profiling pass (findings saved in memory `php-benchmark-perf-findings`; throwaway
scripts in `scratch-bench/`). The perf *sweep* ([`../README.md`](../README.md)) tuned collections,
inline caches, reuse, and the SoA column layout. This arc goes after the thing those left on the
table: **raw interpreter throughput** — the per-instruction, per-call, and per-loop cost that makes
scalar/loop/call-heavy code 28–350× slower than PHP even though we *win* on the workloads the design
targets (SoA column vector math, 1.7× vs PHP-JIT / 4.4× vs plain).

## What the benchmark showed

| Workload | lang | PHP (CPU) | PHP+JIT | Gap |
|---|--:|--:|--:|:--|
| `@packed(layout: column)` `vec.length_all` | **24 ms** | 105 | 40 | **lang wins 1.7–4.4×** |
| fib(32) | 669 ms | 48 | 24 | PHP 28× |
| tight loop, 10M | 1574 ms | 36 | 15 | PHP 103× |
| wordcount 200k (string-interp + map) | 2600 ms | 7 | – | PHP ~350× |
| assoc build+lookup 100k (in fn) | 90 ms | 8 | – | PHP 12× |
| top-level map build 40k | **33 s** | ~5 | – | **O(n²) cliff** |

Ablation (2M iters): empty loop **80 ns/iter** (PHP ≈ 3.6), `+i%500` +36, `+fn call` +87,
`+string interp` +262. `size_of::<Op>()` = **128 bytes** (2 cache lines/instruction). Every function
call heap-allocates its register file (`vec![Value::unit(); n]`).

## Posture (inherited from the perf sweep — unchanged)

- **Every slice ships with a benchmark that validates the gain.** A perf claim without a before/after
  number does not land. Durable benches are criterion (`crates/lang-vm/benches/vm.rs`); the
  `scratch-bench/` PHP pairs are the end-to-end sanity check (not CI).
- **Perf slices S1–S5 are invisible to `RunResult`** (behaviour already correct) → the differential's
  `0 skipped / backends agree` gate holds **by construction**, and each may land **VM-first** (a
  temporary perf asymmetry, never a behaviour asymmetry). The eval tree-walker already reuses global
  accumulators, so S1 in particular only brings the VM up to parity.
- **S0 is the exception.** It adds language surface (new conversion methods) so it is *not*
  invisible: it needs new conformance cases **and** differential coverage (both backends implement
  the same conversion, agree by construction because the semantics are shared in `lang-stdlib`).

## Slices

| # | Tag | Slice | Impact | Effort | Independent? |
|---|---|---|---|---|---|
| S0 | **P-VMT-CONV** | ✅ **DONE** — [Numeric conversion tower](s0-numeric-conversions.md): `to_float`/`to_f32`/`to_f64` + float→int | correctness gap; unblocks building `f32`/`float` data programmatically | S | yes |
| S1 | **P-VMT-GACC** | ✅ **DONE** — [Global-accumulator reuse](s1-global-accumulator-reuse.md): killed the top-level collection O(n²) cliff (33.5 s → 40 ms at n=40k) | **huge** (unbounded; ~850× at n=40k) | **S** | yes |
| S2 | **P-VMT-FRAME** | ✅ **DONE** — [Register stack](s2-register-stack.md): one contiguous per-run register file, frames are `base` offsets, no per-call alloc. fib(28) 214.9 ms → 103.6 ms (**2.1×**) | high (call-heavy: fib, recursion) | M | yes |
| S3 | **P-VMT-DISP** | ✅ **DONE** — [Dispatch register window](s3-dispatch-window.md): hoist frame/chunk/pc into loop-locals re-derived only on call/return, direct-index the code stream. Dispatch floor 63 → 43 ns/iter (**1.46×** on a tight loop) | high (every loop; the 80 ns/iter floor) | M | builds on S2 |
| S4 | **P-VMT-OPSZ** | [Shrink `Op` via name interning](s4-op-interning.md) — 128 B → ~32 B | broad, modest (icache) | M–L (cross-crate) | co-schedule w/ S3 |
| S5 | **P-VMT-STR** | ✅ **DONE** — [Single-pass interpolation](s5-interp-buildstring.md): one `Op::BuildString`, not an N-concat fold. `"word${i}"` 1M 303.6 → 149.7 ms (**2.0×**); multi-hole **2.7×** | medium (string-heavy: wordcount) | S–M | yes |

## Sequencing (value × independence, ascending risk)

1. **S0** first — self-contained warmup, closes a real correctness gap, and lets later benches build
   `f32`/`float` inputs without literal-only hacks.
2. **S1** next — the highest impact-to-effort item in the whole arc: a near-one-site compiler change
   that turns the most natural scripting idiom (build a map/list at top level) from O(n²) to O(n).
3. **S2 → S3** — the register file is the shared foundation. S2 removes per-call allocation; S3 then
   rewrites the dispatch loop around the new contiguous representation (a window into the register
   stack), so doing S2 first avoids reworking S3.
4. **S5** — localized, lands anytime after S0; grouped here because wordcount (string-interp-bound)
   is the worst outlier and S5 is what moves it.
5. **S4** last — the biggest blast radius (bytecode + compiler + VM + disassembler). Shrinking `Op`
   compounds with S3's window (smaller instructions stream faster), so it lands on top of the
   restructured loop rather than being redone by it.

## Invariants every slice asserts before "done"

- `cargo run -p lang-conformance -- --differential` → **0 skipped, backends agree**.
- Full conformance corpus green; `cargo test` workspace green; miri clean on `lang-value`.
- The slice's criterion bench shows the claimed before/after; the number goes in the slice doc.
- `git` commit per green slice (standing directive — commit as you go, never push without
  authorization).

## Open questions (resolve at sign-off, non-blocking)

- **S0 float→int rounding:** truncate-toward-zero with saturation (Rust `as` post-1.45) vs error on
  NaN/overflow. Proposal: match Rust `as` (saturating), document it, no new diagnostic.
- **Minor, out of arc:** `RealHost::clock_monotonic` is a logical counter, not wall time, so
  `time.monotonic()` can't measure real elapsed time from inside the language. Tracked as a one-line
  follow-up (RealHost-only; must not perturb the deterministic SandboxHost the differential depends
  on). Not part of P-VMT unless we want in-language microbenchmarks.
