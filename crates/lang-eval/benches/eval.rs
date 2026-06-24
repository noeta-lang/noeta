//! Criterion benchmarks over the **tree-walker** (`TreeWalkBackend`) hot paths.
//!
//! The VM benches (`crates/lang-vm/benches/vm.rs`) can't see optimizations that land on the
//! tree-walker first — notably P-COW (copy-on-write list append, `plans/perf/p-cow-list-append.md`),
//! which ships eval-side before the VM. This harness gives those a measurement surface.
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

/// Dispatch loop: recursive Fibonacci — the canonical tree-walk dispatch stressor.
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

/// Sizes for the parameterized accumulator. Each doubles the previous, so the per-step time ratio
/// reveals the complexity class (≈4× per doubling ⇒ O(n²); ≈2× ⇒ O(n)).
const ACC_SIZES: &[usize] = &[1000, 2000, 4000, 8000];

/// A blind-overwrite record accumulator (`acc = Wide { ...acc, f0: i }`) — the tree-walker analogue
/// of the VM `vm_record_update` bench. Reuse mutates the uniquely-owned object's field map in place
/// instead of cloning the whole `BTreeMap` + every field reference each step. Eight fields so the
/// per-step copy cost is visible.
fn record_update_src(n: usize) -> String {
    let fields: Vec<String> = (0..8).map(|i| format!("f{i}: int")).collect();
    let inits: Vec<String> = (0..8).map(|i| format!("f{i}: 0")).collect();
    format!(
        "class Wide {{\n    {}\n}}\n\
         mut acc = Wide {{ {} }};\n\
         for i in 0..{n} {{\n    \
            acc = Wide {{ ...acc, f0: i }};\n\
         }}\n\
         echo acc.f0;\n",
        fields.join("\n    "),
        inits.join(", "),
    )
}

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

    // Record-update reuse (tree-walker): `off` disables the fast path (clone the whole field map
    // each step), `on` mutates the uniquely-owned object in place. Unlike the VM, the tree-walker
    // reuses even when the update *reads* the accumulator, because Rust drops the read's temporary
    // promptly — the very last-use behavior the register VM lacks.
    let mut reuse = c.benchmark_group("eval_record_update");
    for &n in ACC_SIZES {
        let prog = program(&record_update_src(n));
        reuse.bench_with_input(BenchmarkId::new("off", n), &prog, |b, prog| {
            b.iter(|| black_box(lang_eval::run_without_record_reuse(0, black_box(prog))));
        });
        reuse.bench_with_input(BenchmarkId::new("on", n), &prog, |b, prog| {
            b.iter(|| black_box(TreeWalkBackend::new().run(black_box(prog))));
        });
    }
    reuse.finish();
}

criterion_group!(benches, eval_hot_paths);
criterion_main!(benches);
