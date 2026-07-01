//! Micro-benchmarks for the bulk `vec` kernels over a packed `List<Vec3<f32>>` byte buffer
//! (`lang_stdlib::vec3::*_buffers`) — the P-SIMD target. These time the kernels **directly** on flat
//! `f32` byte buffers, isolating the arithmetic from the language-level list-build/marshal cost that
//! dominates the end-to-end `vm_vec_add_all` bench in `lang-vm`. This is the lens where the SIMD swap
//! (S2) shows its gain: a scalar baseline (S1) vs the `wide`-crate SIMD kernels (S2), recorded in
//! `plans/perf/p-simd.md`.
//!
//! Each buffer is `n` elements × 3 `f32` × 4 bytes = `12n` bytes of little-endian `f32`. The inputs
//! are built once in setup; the timed closure runs only the kernel.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use lang_stdlib::vec3;

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
}

criterion_group!(benches, vec3_kernels);
criterion_main!(benches);
