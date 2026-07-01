# P-SIMD — SIMD kernels over the flat packed `List<Vec3>` buffers

**Status: DONE — delivered via an opt-in SoA type (not via explicit SIMD).** The throughput win is real
— **2.7×–4× on `dot`/`length`** — but it comes from a **struct-of-arrays layout that unlocks LLVM
autovectorization**, not from SIMD intrinsics. The `wide`-crate approach was tried twice (on the AoS
buffer and again on SoA columns) and was **not** faster than the autovectorized scalar loop either time,
so it was dropped. Shipped surface (S5): the opt-in `vec.soa*` family — `soa`/`soa_dot`/`soa_length`/
`soa_add`/`soa_sub`/`soa_scale`/`soa_list`/`soa_count` — over a new native `SoaVec3` batch value in both
backends (`tests/conformance/std/vec3_soa.lang`). Branch: `perf-simd`.

**The arc in one paragraph.** S1 recorded the scalar AoS baseline. S2 swapped the AoS kernels to
`wide` `f32x8` and **measured a 1.8×–9× regression** (both default and AVX2 builds), reverted. The
finding: the AoS `chunks_exact` loop is already autovectorized, and the 1-aligned `Vec<u8>` **AoS**
buffer forces a scalar gather/scatter to fill a lane register that no SIMD width pays for. The user
then directed implementing the deferred **SoA** layout. Rather than re-layout the general packed list
(which would revert P-COW's O(n²)→O(n) append — appends become mid-buffer column inserts), we added an
**opt-in [`SoaVec3`] columnar type** the user explicitly builds for bulk math. On its contiguous
columns the reduction kernels autovectorize *across elements* (the AoS stride-12 layout could not),
giving the win. Explicit `wide` SIMD on those same columns was **still not faster** than the scalar
loop — so the shipped kernels are plain scalar; the lever is the layout, not the intrinsics.

## Goal

The bulk 3D-math kernels already operate on the flat packed `List<Vec3>` buffer element-by-element in
**scalar** Rust. Swap their inner loops to **portable SIMD** so a batch op over a large `List<Vec3>`
processes multiple lanes at once — same results, faster. This is the throughput payoff P-PACK phases
0–3 (flat packed layout) were built for.

## Exactly what exists today (verified)

- **Surface:** `vec.add_all / sub_all / scale_all / dot_all / length_all(List<V3>, …)` — bulk ops over
  a packed `List<Vec3>`. `add_all/sub_all/scale_all` return `List<Vec3>` (stay packed); `dot_all/
  length_all` reduce to `List<f32>`. Conformance test: `tests/conformance/std/vec3_bulk.lang`.
- **Impl (the SIMD targets):** `crates/lang-stdlib/src/vec3.rs` —
  `add_buffers`/`sub_buffers`/`scale_buffer`/`dot_buffers`/`length_buffer`, most via the scalar
  `zip_buffers(a, b, op)` helper. They take the flat `&[u8]` buffer of the packed list (raw little-endian
  `f32`s) and return a new buffer / `Vec<f32>`. These stay **per-backend** (a packed-layout
  specialization, not a value-seam concern — see `registry.rs` ~547), so both backends call the *same*
  Rust kernel → the differential pins the result by construction.
- A non-packed (boxed) `List<B3>` takes a scalar fallback that must match the packed kernel — already the
  case (`vec3_bulk.lang` checks both), and it stays scalar; only the flat-buffer kernels get SIMD.

## The one decision to make first: which SIMD library

**Portable `std::simd` is nightly-only; the toolchain is stable (rustc 1.96.0).** So:
- **Recommended: the `wide` crate** (stable, pure-Rust portable SIMD — `f32x4`/`f32x8`, autoselects the
  best target feature at runtime). No `unsafe` in our code (the crate encapsulates it), which keeps the
  workspace `unsafe`-forbidden rule intact (only `lang-value` is quarantined). Add `wide` to
  `crates/lang-stdlib/Cargo.toml`.
- Alternatives (note, don't default to): `std::arch` target intrinsics (stable but `unsafe` +
  per-arch → needs a quarantine exception, more code); rely on autovectorization (no dep, but unreliable
  and unmeasurable). The P-PACK Phase 4 plan said "std::simd" — that predates noticing the toolchain is
  stable; `wide` is the stable equivalent.

## Slices

**S1 — bench harness + baseline.** Add a criterion bench (extend `crates/lang-vm/benches/vm.rs` or a
new `bulk_vec` bench) that builds a large `List<Vec3>` (parameterized over n — e.g. 1k/10k/100k) and
times `vec.add_all` and `vec.dot_all`. Record the scalar baseline numbers here before touching the
kernels. (Driving through the language exercises the real packed-buffer path; a direct
`vec3::add_buffers` micro-bench is a fine second lens.)

**S2 — SIMD the `zip_buffers`-family kernels. → DONE (negative result, reverted).** Added `wide` and
rewrote `add`/`sub`/`scale` to `f32x8` lanes + scalar tail, and `dot`/`length` lane-per-element (N
elements' components across lanes, preserving per-element reduction order → bit-identical, unit-tested).
Benched it: **1.8×–9× regression** (table above), so reverted the swap and dropped the `wide` dep. The
kernels stay scalar (already autovectorized). The bit-identical property held — the problem was purely
throughput, from the AoS/1-aligned marshaling. See [Results](#results) for the full analysis and the
layout-change unblock condition.

**S3 — confirm the oracle. → DONE.** With the kernels reverted to scalar, `vec3_bulk.lang`,
`vec3.lang`, `vec3_more.lang`, `quat.lang` and the full conformance + differential suite stay green by
construction (no behaviour change landed). `quat` has no analogous flat-buffer bulk kernel, so it was
out of scope regardless.

## Results

Bench: `crates/lang-stdlib/benches/vec3_kernels.rs` — the kernels timed **directly** on flat `f32`
byte buffers (the clearest SIMD signal; the language-level `vm_vec_add_all` bench is dominated by
list-build/marshal cost, so a kernel win is diluted there). `n` = element count (each element = 3
`f32` = 12 bytes). Criterion median, `cargo bench -p lang-stdlib --bench vec3_kernels`.

**S1 — scalar baseline** (rustc 1.96.0, `-O`, LLVM autovectorization only):

| kernel | n=1k | n=10k | n=100k |
|---|---|---|---|
| `add_buffers`    | 2.68 µs | 30.1 µs | 311 µs |
| `sub_buffers`    | 2.90 µs | 29.6 µs | 328 µs |
| `scale_buffer`   | 1.72 µs | 16.9 µs | 174 µs |
| `dot_buffers`    | 0.729 µs | 7.49 µs | 74.6 µs |
| `length_buffer`  | 1.12 µs | 12.0 µs | 113 µs |

**S2 — `wide` f32x8 swap (REVERTED — measured regression).** Rewrote the kernels to process 8 `f32`
lanes/step with `wide::f32x8` + a scalar remainder tail, bit-identical to scalar (verified by a
19/57-element unit test spanning the SIMD body and the tail — no horizontal reduction, no fused
`mul_add`, so every lane matched the scalar bytes). Result at n=100k, across two build configs
(default = baseline `x86-64`/SSE2; AVX2 = `RUSTFLAGS="-C target-cpu=native"` on a Ryzen AI 9 365):

| kernel (n=100k) | scalar default | scalar AVX2 | wide default | wide AVX2 | wide vs scalar |
|---|---|---|---|---|---|
| `add_buffers`    | 311 µs | 300 µs | 595 µs | 581 µs | **1.9× slower** |
| `sub_buffers`    | 328 µs | 306 µs | 609 µs | 587 µs | **1.9× slower** |
| `scale_buffer`   | 174 µs | 168 µs | 423 µs | 435 µs | **2.4× slower** |
| `dot_buffers`    | 74.6 µs | 52.3 µs | 662 µs | 659 µs | **8.9× slower** |
| `length_buffer`  | 113 µs | 116 µs | 337 µs | 338 µs | **3.0× slower** |

**Why it regressed (the load-bearing finding):**
- The scalar byte-direct loops (`chunks_exact(4)` + `from_le_bytes`/`to_le_bytes`, which fold to plain
  LE loads/stores) are **already autovectorized by LLVM** under `-O`. The baseline *is* SIMD.
- `wide::f32x8` can't zero-copy-load the buffer: it's a `Vec<u8>`, **1-aligned**, laid out **AoS**
  (`x0,y0,z0,x1,…`). Getting 8 `f32` into a lane register means gathering them through `f32::from_le_bytes`
  into a stack `[f32; 8]`, then scattering `.to_array()` back to bytes. That marshaling is scalar and
  LLVM can't fuse it into the vector op, so it *adds* cost on top of arithmetic that was never the
  bottleneck (these kernels are memory-bound).
- **AVX2 doesn't help** (`wide` default ≈ `wide` AVX2): the loss is the gather/scatter, not the SIMD
  width. Widening lanes can't pay for a marshaling step that dominates.
- `dot`/`length` are worst (≈9×) because they need a **strided AoS→SoA gather** (component `x` of 8
  consecutive elements lives at byte stride 12) — 24 scalar strided loads per 8-element step, pure
  overhead against a scalar `dot` LLVM already vectorizes well (and AVX2 speeds the *scalar* `dot` up
  further, 74.6→52.3 µs, widening the gap).

**Unblock condition:** a real win here requires a **layout change**, not a kernel swap — a **SoA**
layout (`xxxx…yyyy…zzzz…`) so a whole reduction runs over a contiguous same-type column. See the SoA
result next.

**S4 — opt-in SoA columnar type (the win).** Added [`SoaVec3`] (`crates/lang-stdlib/src/vec3.rs`): three
contiguous `f32` columns, built once from a `List<Vec3>` (an O(n) transpose) and reduced many times. On
its columns each reduction is a contiguous same-type `f32` loop LLVM **autovectorizes across elements**.
`soa_reductions` bench, n=100k (criterion median, default build):

| reduction | AoS (shipped scalar kernel) | **SoA (opt-in columns)** | speedup |
|---|---|---|---|
| `dot`    | 75.6 µs | **28.0 µs** | **2.7×** |
| `length` | 117 µs  | **29.5 µs** | **4.0×** |

(Consistent 2.7×–3.8× / 3.6×–4.0× across n=1k/10k/100k.) One-time `build` (AoS→SoA transpose) at n=100k
is ~126 µs — one transpose amortizes over repeated reductions. Two guardrails learned here:
- **It's the layout, not intrinsics.** Explicit `wide` `f32x8` over the *same* SoA columns benched
  ~10% **slower** than the autovectorized scalar loop (`soa_dot` 28.0 µs vs a `wide` 31.3 µs), so the
  shipped kernels are scalar and the `wide` dep was dropped. LLVM autovectorizes the clean column loop
  better than hand-rolled lane marshaling — the same lesson as S2, now with the layout that lets it.
- **Bounds checks dominated the first cut.** An indexed SoA loop (`a.xs[i]…`, 6 bounds checks/step)
  was *2.7× slower* than AoS; rewriting to zipped iterators (bounds-check-free, like AoS's
  `chunks_exact`) is what exposed the 2.7×–4× win. The AoS kernels were already bounds-check-free — the
  fair comparison is iterator-vs-iterator.

Kernels are **bit-identical to the AoS kernels** (unit-tested: `soa_dot`/`soa_length` equal
`dot_buffers`/`length_buffer` on the transposed buffer; the reductions keep per-element
`(x·bx + y·by) + z·bz` order). The general packed `List<Vec3>` is untouched (keeps O(1) append); SoA is
a separate value type. Language surface: `vec.soa(list)` → an opaque batch, `vec.soa_dot`/`soa_length`
(→ `List<f32>`), `vec.soa_add`/`soa_sub`/`soa_scale` (→ batch), `vec.soa_list`/`soa_count`.

## Oracle posture / risk

- **Behaviour invisible:** the kernel's output bytes are unchanged; `RunResult` identical; differential
  `0 skipped / backends agree` holds by construction.
- **The one real correctness watch:** f32 reduction order. Lane-wise add/mul/scale are per-element and
  bit-identical to scalar; a horizontal reduction (dot's sum, length's sqrt-of-sum) can reorder float
  adds and change the last ULP. Keep the reduction order matching the scalar loop (or accept + re-bless
  only if the conformance expectations shift — but prefer exact match so no `.lang` file changes).

## Verification (the sweep's gate)

- `cargo run -q -p lang-cli -- run tests/conformance/std/vec3_bulk.lang` and the vec3/quat suite green;
  full conformance + `--differential` (0 skipped / agree); leak oracle 0; `cargo test --workspace`;
  clippy + fmt clean.
- The **bench numbers** (S1 baseline vs S2 SIMD), recorded above. A perf claim without them doesn't ship.

## Out of scope (later)

- A user-visible `Simd<T, N>` type + const generics → P-BITS Tier P (`plans/bitwise/`).
- `f32`→wider families, monomorphization → P-MONO. Zero-copy isolate sharing → P-SHARE.
