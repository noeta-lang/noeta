# P-SIMD — SIMD kernels over the flat packed `List<Vec3>` buffers

**Status: ready to pick up.** First track of the post-sweep perf work ([`remaining.md`](remaining.md)).
Self-contained, unblocked, **zero differential risk** (a perf-only swap behind byte-identical scalar
semantics). Branch: `types-inferred-static` (or a fresh `perf-simd`), standard commit trailers.

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

**S2 — SIMD the `zip_buffers`-family kernels.** Add `wide`; rewrite `add_buffers`/`sub_buffers`/
`scale_buffer` (and `zip_buffers`) to process `f32x8` (or `f32x4`) lanes over the flat buffer with a
scalar remainder tail; `dot_buffers`/`length_buffer` reduce per element (3 f32s → dot/length) — either
SIMD the per-element math or, better, process N elements' components across lanes. Keep the results
**bit-identical** to the scalar version (f32 SIMD add/mul is IEEE-per-lane, so results match; if any op
reorders a reduction, verify the bytes still match `vec3_bulk.lang`'s expectations). Re-run the bench;
record before/after in this doc.

**S3 — confirm the oracle + widen coverage.** `vec3_bulk.lang` (and `vec3.lang`/`vec3_more.lang`/
`quat.lang`) must stay green — they pin the scalar results the SIMD path now produces. If `quat` has an
analogous bulk kernel, apply the same swap; otherwise note it out of scope. The differential is
unaffected by construction (both backends call the one kernel).

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
