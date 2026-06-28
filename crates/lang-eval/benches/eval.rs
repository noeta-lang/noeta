//! Criterion benchmarks over the **Core-IR interpreter** (`TreeWalkBackend::run`, the reference
//! eval backend that `lang run` and the conformance oracle execute) hot paths.
//!
//! The `accumulate` group is **parameterized over input size** so an *asymptotic* change is
//! visible, not just a constant factor: the self-append loop `acc ~= [i]` is O(n²) before COW
//! (each `~` copies the whole left list) and O(n) after. Watch how the time scales as `n` doubles.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use lang_ast::Program;
use lang_backend::Backend;
use lang_eval::TreeWalkBackend;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

/// Source → parsed `Program`. Panics on a parse error — bench programs must parse cleanly.
fn program(src: &str) -> Program {
    let source = Source::new(SourceId::FIRST, "bench.lang", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "bench program must parse without diagnostics: {:?}",
        parsed.diagnostics
    );
    parsed.program
}

/// Dispatch loop: recursive Fibonacci — the canonical interpreter dispatch stressor.
fn dispatch_src() -> String {
    "fn fib(n: int): int {\n    \
        if n < 2 { return n; }\n    \
        return fib(n - 1) + fib(n - 2);\n\
     }\n\
     echo fib(24);\n"
        .to_string()
}

/// The self-append accumulator: `acc ~= [i]` n times. The P-COW target — O(n²) before, O(n) after.
fn accumulate_src(n: usize) -> String {
    format!(
        "mut acc = [];\n\
         for i in 0..{n} {{\n    \
            acc ~= [i];\n\
         }}\n\
         echo acc.count();\n"
    )
}

/// A struct-heavy loop: construct a 4-field struct, read every field, and functionally update it
/// each iteration. This is the field-storage stressor — every iteration does a slot fill (struct
/// construction), four field reads, and a copy-on-write update. It is the benchmark for the
/// representation change from a name-keyed `BTreeMap` to a slot-ordered `Vec` (Phase 1 of P-PACK).
fn struct_fields_src(n: usize) -> String {
    format!(
        "struct Point {{ x: int; y: int; z: int; w: int }}\n\
         mut p = Point {{ x: 0, y: 0, z: 0, w: 0 }}\n\
         mut sum = 0\n\
         for i in 0..{n} {{\n    \
            p = Point {{ ...p, x: p.x + 1, y: p.y + i }}\n    \
            sum = sum + p.x + p.y + p.z + p.w\n\
         }}\n\
         echo sum\n"
    )
}

/// A `List<packed>` workload: build an `n`-element list literal of a `Vec3`, then index every
/// element and sum its three fields. When the element type is `@packed` the list is stored as one
/// flat raw-primitive buffer (P-PACK 2.3); the plain-`struct` variant stores `n` boxed objects. Each
/// `data[i].field` read **fuses** to a single field decode (P-PACK 2.5+) — the packed list reads one
/// word with no element object materialized, so this measures the scalar-access win the flat layout
/// otherwise left on the table (2.3/2.4 were ~10% *slower* here than boxed; with fusion the gap is
/// largely closed on eval and the VM flips to a win — see `crates/lang-vm/benches/vm.rs`). The peak
/// memory win is measured separately by the `peak_memory` residency test in `lang-conformance`.
fn packed_list_src(n: usize, packed: bool) -> String {
    let kw = if packed { "@packed struct" } else { "struct" };
    let mut elems = String::with_capacity(n * 32);
    for i in 0..n {
        if i > 0 {
            elems.push_str(", ");
        }
        elems.push_str("Vec3 { x: 1.0, y: 2.0, z: 3.0 }");
    }
    format!(
        "{kw} Vec3 {{ x: float; y: float; z: float }}\n\
         data = [{elems}]\n\
         mut sum = 0.0\n\
         for i in 0..{n} {{\n    \
            sum = sum + data[i].x + data[i].y + data[i].z\n\
         }}\n\
         echo sum\n"
    )
}

/// Sizes for the parameterized accumulator. Each doubles the previous, so the per-step time ratio
/// reveals the complexity class (≈4× per doubling ⇒ O(n²); ≈2× ⇒ O(n)).
const ACC_SIZES: &[usize] = &[1000, 2000, 4000, 8000];

/// Sizes for the packed-list workload — a couple of points to confirm the per-element cost is flat.
const PACKED_SIZES: &[usize] = &[1000, 4000];

/// Sizes for the struct-field loop. A constant-factor change, so a single mid-size point suffices;
/// a couple of sizes confirm the per-iteration cost is flat in `n`.
const STRUCT_SIZES: &[usize] = &[2000, 8000];

fn eval_hot_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("eval");
    let dispatch = program(&dispatch_src());
    group.bench_function("dispatch_fib", |b| {
        b.iter(|| black_box(TreeWalkBackend::new().run(black_box(&dispatch))));
    });
    group.finish();

    let mut acc = c.benchmark_group("eval_accumulate");
    for &n in ACC_SIZES {
        let prog = program(&accumulate_src(n));
        acc.bench_with_input(BenchmarkId::from_parameter(n), &prog, |b, prog| {
            b.iter(|| black_box(TreeWalkBackend::new().run(black_box(prog))));
        });
    }
    acc.finish();

    let mut sf = c.benchmark_group("eval_struct_fields");
    for &n in STRUCT_SIZES {
        let prog = program(&struct_fields_src(n));
        sf.bench_with_input(BenchmarkId::from_parameter(n), &prog, |b, prog| {
            b.iter(|| black_box(TreeWalkBackend::new().run(black_box(prog))));
        });
    }
    sf.finish();

    let mut pl = c.benchmark_group("eval_packed_list");
    for &n in PACKED_SIZES {
        let packed = program(&packed_list_src(n, true));
        pl.bench_with_input(BenchmarkId::new("packed", n), &packed, |b, prog| {
            b.iter(|| black_box(TreeWalkBackend::new().run(black_box(prog))));
        });
        let boxed = program(&packed_list_src(n, false));
        pl.bench_with_input(BenchmarkId::new("boxed", n), &boxed, |b, prog| {
            b.iter(|| black_box(TreeWalkBackend::new().run(black_box(prog))));
        });
    }
    pl.finish();
}

criterion_group!(benches, eval_hot_paths);
criterion_main!(benches);
