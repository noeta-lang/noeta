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

/// Sizes for the parameterized accumulator. Each doubles the previous, so the per-step time ratio
/// reveals the complexity class (≈4× per doubling ⇒ O(n²); ≈2× ⇒ O(n)).
const ACC_SIZES: &[usize] = &[1000, 2000, 4000, 8000];

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
}

criterion_group!(benches, eval_hot_paths);
criterion_main!(benches);
