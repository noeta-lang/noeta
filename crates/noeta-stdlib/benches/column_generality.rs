//! Does the `@packed(Layout.Column)` reduction win generalize past `f32`? (P-SIMD, exploratory.)
//!
//! The C3 win comes from reading each contiguous column as a *typed* slice so LLVM autovectorizes the
//! reduction across elements — the AoS/row layout strides one field every `k` elements, which stays
//! scalar. Nothing in that mechanism is `f32`-specific: it should hold for any primitive whose
//! arithmetic vectorizes. This bench checks that directly, comparing a **sum-of-squares** reduction
//! (`a² + b² + c²` per element — the same shape as `length_all`, minus the `sqrt`) over a 3-field
//! struct stored row-major (interleaved `[a,b,c, a,b,c, …]`) vs column-major (`[a×n][b×n][c×n]`), for
//! `f32`, `f64`, and `i64`.
//!
//! The column kernels read each column as `&[T]` via `bytemuck::cast_slice` — exactly the shipped
//! `col_dot`/`col_length` path (which uses the alignment-checked `try_cast_slice`), just monomorphized
//! per type here. If column beats row by a similar margin for `i64`/`f64` as for `f32`, the win is a
//! property of the *layout*, not the element type.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const SIZES: &[usize] = &[10_000, 100_000];

// --- f32 ---
fn dotself_row_f32(buf: &[u8], _n: usize) -> Vec<f32> {
    buf.chunks_exact(12)
        .map(|e| {
            let a = f32::from_le_bytes(e[0..4].try_into().unwrap());
            let b = f32::from_le_bytes(e[4..8].try_into().unwrap());
            let c = f32::from_le_bytes(e[8..12].try_into().unwrap());
            a * a + b * b + c * c
        })
        .collect()
}
fn dotself_col_f32(buf: &[u8], n: usize) -> Vec<f32> {
    let all: &[f32] = bytemuck::cast_slice(buf);
    let (a, rest) = all.split_at(n);
    let (b, c) = rest.split_at(n);
    a.iter()
        .zip(b)
        .zip(c)
        .map(|((&a, &b), &c)| a * a + b * b + c * c)
        .collect()
}

// --- f64 ---
fn dotself_row_f64(buf: &[u8], _n: usize) -> Vec<f64> {
    buf.chunks_exact(24)
        .map(|e| {
            let a = f64::from_le_bytes(e[0..8].try_into().unwrap());
            let b = f64::from_le_bytes(e[8..16].try_into().unwrap());
            let c = f64::from_le_bytes(e[16..24].try_into().unwrap());
            a * a + b * b + c * c
        })
        .collect()
}
fn dotself_col_f64(buf: &[u8], n: usize) -> Vec<f64> {
    let all: &[f64] = bytemuck::cast_slice(buf);
    let (a, rest) = all.split_at(n);
    let (b, c) = rest.split_at(n);
    a.iter()
        .zip(b)
        .zip(c)
        .map(|((&a, &b), &c)| a * a + b * b + c * c)
        .collect()
}

// --- i64 ---
fn dotself_row_i64(buf: &[u8], _n: usize) -> Vec<i64> {
    buf.chunks_exact(24)
        .map(|e| {
            let a = i64::from_le_bytes(e[0..8].try_into().unwrap());
            let b = i64::from_le_bytes(e[8..16].try_into().unwrap());
            let c = i64::from_le_bytes(e[16..24].try_into().unwrap());
            a * a + b * b + c * c
        })
        .collect()
}
fn dotself_col_i64(buf: &[u8], n: usize) -> Vec<i64> {
    let all: &[i64] = bytemuck::cast_slice(buf);
    let (a, rest) = all.split_at(n);
    let (b, c) = rest.split_at(n);
    a.iter()
        .zip(b)
        .zip(c)
        .map(|((&a, &b), &c)| a * a + b * b + c * c)
        .collect()
}

/// A row-major `[a,b,c, a,b,c, …]` byte buffer of `n` 3-field elements, each field `width` bytes,
/// filled with a cheap deterministic pattern.
fn make_buffer(n: usize, width: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 3 * width);
    for i in 0..(n * 3) {
        let v = ((i % 97) as u64).wrapping_mul(2654435761) & 0xffff;
        out.extend_from_slice(&v.to_le_bytes()[..width]);
    }
    out
}

fn column_generality(c: &mut Criterion) {
    let mut g = c.benchmark_group("column_generality");
    for &n in SIZES {
        // f32 (4-byte fields) and f64/i64 (8-byte fields) — same values, row and column orderings.
        let row32 = make_buffer(n, 4);
        let col32 = make_buffer(n, 4); // same pattern; layout differs only in how the kernel reads it
        let row64 = make_buffer(n, 8);
        let col64 = make_buffer(n, 8);

        g.bench_with_input(BenchmarkId::new("f32-row", n), &n, |bch, _| {
            bch.iter(|| black_box(dotself_row_f32(black_box(&row32), n)));
        });
        g.bench_with_input(BenchmarkId::new("f32-col", n), &n, |bch, _| {
            bch.iter(|| black_box(dotself_col_f32(black_box(&col32), n)));
        });
        g.bench_with_input(BenchmarkId::new("f64-row", n), &n, |bch, _| {
            bch.iter(|| black_box(dotself_row_f64(black_box(&row64), n)));
        });
        g.bench_with_input(BenchmarkId::new("f64-col", n), &n, |bch, _| {
            bch.iter(|| black_box(dotself_col_f64(black_box(&col64), n)));
        });
        g.bench_with_input(BenchmarkId::new("i64-row", n), &n, |bch, _| {
            bch.iter(|| black_box(dotself_row_i64(black_box(&row64), n)));
        });
        g.bench_with_input(BenchmarkId::new("i64-col", n), &n, |bch, _| {
            bch.iter(|| black_box(dotself_col_i64(black_box(&col64), n)));
        });
    }
    g.finish();
}

criterion_group!(benches, column_generality);
criterion_main!(benches);
