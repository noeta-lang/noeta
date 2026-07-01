# P-SIMD — SIMD kernels over the flat packed `List<Vec3>` buffers

**Status: BLOCKED on a layout change (measured negative result — S2 did not land).** S1 (bench + scalar
baseline) shipped. S2 attempted the `wide`-crate swap and **measured it a 1.8×–9× regression** in both
default and AVX2 builds, so it was reverted (a measured regression can't ship under the bench-gated
mandate). Root cause and the unblock condition are in [Results](#results) below. Branch: `perf-simd`.

**One-line finding:** the scalar byte-direct kernels are *already* LLVM-autovectorized; manual portable
SIMD can't beat them because the 1-aligned `Vec<u8>` **AoS** buffer forces a scalar gather/scatter to
get lanes in/out (no zero-copy `&[f32]` reinterpret without `unsafe` or an SoA layout — both out of
scope here). Real SIMD needs the deferred layout change (SoA or aligned buffers), not a kernel swap.

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

**Unblock condition:** a real SIMD win here requires a **layout change**, not a kernel swap — either an
**SoA** packed layout (`xxxx…yyyy…zzzz…`, so a component of N elements is one contiguous vector load) or
**aligned** buffers enabling a zero-copy `&[f32]` reinterpret (needs `unsafe`/`bytemuck` in a value crate).
Both are explicitly deferred (see the P-PACK Phase 4 note in `crates/lang-stdlib/src/vec3.rs` and the
"Out of scope" list below). The `vec3_kernels` bench stays as the gate that will validate any such attempt.

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
