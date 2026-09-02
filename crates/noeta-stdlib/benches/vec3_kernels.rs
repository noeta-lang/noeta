//! Micro-benchmarks for the bulk `vec` kernels over a packed `List<Vec3<f32>>` byte buffer
//! (`noeta_stdlib::vec3::*_buffers`) — the P-SIMD target. These time the kernels **directly** on flat
//! `f32` byte buffers, isolating the arithmetic from the language-level list-build/marshal cost that
//! dominates the end-to-end `vm_vec_add_all` bench in `noeta-vm`. This is the lens where the SIMD swap
//! shows its gain: a scalar baseline vs the `wide`-crate SIMD kernels.
//!
//! Each buffer is `n` elements × 3 `f32` × 4 bytes = `12n` bytes of little-endian `f32`. The inputs
//! are built once in setup; the timed closure runs only the kernel.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use noeta_stdlib::vec3;

/// Element counts — small/medium/large so the per-element cost (and the SIMD win, which needs enough
/// lanes to amortize the remainder tail) is visible across scales.
const SIZES: &[usize] = &[1_000, 10_000, 100_000];

/// Build a flat little-endian `f32` byte buffer of `3 * n` components, filled with a cheap
/// deterministic pattern (so the values differ per lane but the build cost is trivial).
fn make_buffer(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 12);
    for i in 0..(n * 3) {
        // A varied but bounded pattern; exact values don't matter, only that lanes differ.
        let f = (i % 97) as f32 * 0.5 + 1.0;
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn vec3_kernels(c: &mut Criterion) {
    let mut group = c.benchmark_group("vec3_kernels");
    for &n in SIZES {
        let a = make_buffer(n);
        let b = make_buffer(n);

        group.bench_with_input(BenchmarkId::new("add_buffers", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::add_buffers(black_box(&a), black_box(&b))));
        });
        group.bench_with_input(BenchmarkId::new("sub_buffers", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::sub_buffers(black_box(&a), black_box(&b))));
        });
        group.bench_with_input(BenchmarkId::new("scale_buffer", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::scale_buffer(black_box(&a), black_box(2.0))));
        });
        group.bench_with_input(BenchmarkId::new("dot_buffers", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::dot_buffers(black_box(&a), black_box(&b))));
        });
        group.bench_with_input(BenchmarkId::new("length_buffer", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::length_buffer(black_box(&a))));
        });
    }
    group.finish();

    // SoA head-to-head: the two reductions (`dot`/`length`) over the opt-in contiguous columns vs the
    // shipped AoS kernels. The SoA layout lets LLVM autovectorize each column across elements (the AoS
    // stride-12 layout cannot), so the SoA *scalar* kernels win — explicit `wide` SIMD on the same
    // columns was benched and was not faster, so it was dropped. `build` is the one-time AoS→SoA
    // transpose the opt-in batch pays at construction (amortized over repeated reductions).
    let mut soa = c.benchmark_group("soa_reductions");
    for &n in SIZES {
        let a_aos = make_buffer(n);
        let b_aos = make_buffer(n);
        let a = vec3::soa_from_packed(&a_aos);
        let b = vec3::soa_from_packed(&b_aos);

        soa.bench_with_input(BenchmarkId::new("dot-aos", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::dot_buffers(black_box(&a_aos), black_box(&b_aos))));
        });
        soa.bench_with_input(BenchmarkId::new("dot-soa", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::soa_dot(black_box(&a), black_box(&b))));
        });
        soa.bench_with_input(BenchmarkId::new("length-aos", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::length_buffer(black_box(&a_aos))));
        });
        soa.bench_with_input(BenchmarkId::new("length-soa", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::soa_length(black_box(&a))));
        });
        soa.bench_with_input(BenchmarkId::new("build", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::soa_from_packed(black_box(&a_aos))));
        });
    }
    soa.finish();

    // row-vs-column dispatch: the *realistic* end-to-end kernel path each `vec.*_all` takes.
    // A `Layout.Row` list feeds the AoS `*_buffers` kernels (read the interleaved bytes directly). A
    // `Layout.Column` list feeds the column path: the reductions (`dot`/`length`) read the three
    // contiguous columns directly via `col_dot`/`col_length` (no decode — a per-call `SoaVec3` decode
    // benched *slower* than AoS), and the element-wise `add` is layout-agnostic (`add_buffers` on the
    // column bytes is a correct column result). This decides whether `column` is worth dispatching to.
    let mut disp = c.benchmark_group("column_dispatch");
    for &n in SIZES {
        let a_aos = make_buffer(n);
        let b_aos = make_buffer(n);
        // Column-order buffers with the same values (`[x×n][y×n][z×n]`).
        let a_col = vec3::soa_to_columns(&vec3::soa_from_packed(&a_aos));
        let b_col = vec3::soa_to_columns(&vec3::soa_from_packed(&b_aos));

        disp.bench_with_input(BenchmarkId::new("dot-row", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::dot_buffers(black_box(&a_aos), black_box(&b_aos))));
        });
        disp.bench_with_input(BenchmarkId::new("dot-col", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::col_dot(black_box(&a_col), black_box(&b_col))));
        });
        disp.bench_with_input(BenchmarkId::new("length-row", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::length_buffer(black_box(&a_aos))));
        });
        disp.bench_with_input(BenchmarkId::new("length-col", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::col_length(black_box(&a_col))));
        });
        // `add` is layout-agnostic (element-wise over the flat `f32` array), so the column path is
        // just `add_buffers` on the column bytes — same kernel, same speed as row, correct result.
        disp.bench_with_input(BenchmarkId::new("add-row", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::add_buffers(black_box(&a_aos), black_box(&b_aos))));
        });
        disp.bench_with_input(BenchmarkId::new("add-col", n), &n, |bch, _| {
            bch.iter(|| black_box(vec3::add_buffers(black_box(&a_col), black_box(&b_col))));
        });
    }
    disp.finish();
}

criterion_group!(benches, vec3_kernels);
criterion_main!(benches);
