//! Criterion benchmarks over the VM hot paths named in `plans/roadmap.md` and
//! `plans/m2/slice-00-benchmarks.md`: the **dispatch loop**, **property access
//! through inline caches**, and **allocation**. Each program is compiled to a
//! `Module` once in setup; the timed closure runs the already-compiled module,
//! so the measurement is execution (the dispatch loop / IC / allocator), not the
//! lexer/parser/compiler front end.
//!
//! This is the M2.0 baseline captured *before* the M2.1+ host/async indirection
//! lands, so later slices can prove they introduced no dispatch/property/
//! allocation regression (implementation-plan §6.6/§6.7).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use lang_bytecode::Module;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};
use lang_vm::VmBackend;

/// Source → compiled `Module`. Panics if the program falls outside the VM
/// subset — bench programs must stay compilable so they exercise real opcodes.
fn compile(src: &str) -> Module {
    let source = Source::new(SourceId::FIRST, "bench.lang", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    lang_compiler::compile(&parsed.program).expect("bench program must be in the VM subset")
}

/// A list literal `[0, 1, 2, …, n-1]`, generated here so the iteration count is
/// high without a giant source line in this file.
fn int_list(n: usize) -> String {
    let mut s = String::with_capacity(n * 4 + 2);
    s.push('[');
    for i in 0..n {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&i.to_string());
    }
    s.push(']');
    s
}

/// Number of loop iterations for the property/allocation benches. Large enough
/// that the hot path dominates per-run VM startup, small enough to keep CI fast.
const ITERS: usize = 5_000;

/// Dispatch loop: recursive Fibonacci — dense calls, comparisons, arithmetic,
/// and returns. The canonical interpreter-dispatch stressor (~150k calls).
fn dispatch_src() -> String {
    "fn fib(n: int): int {\n    \
        if n < 2 { return n; }\n    \
        return fib(n - 1) + fib(n - 2);\n\
     }\n\
     echo fib(24);\n"
        .to_string()
}

/// Property access through inline caches: read the same object's fields on every
/// iteration. After the first hit the `p.x`/`p.y` sites go monomorphic, so this
/// measures the cached LOAD path.
fn property_src() -> String {
    format!(
        "class Point {{\n    x: int\n    y: int\n}}\n\
         mut total = 0;\n\
         p = Point {{ x: 3, y: 4 }};\n\
         for i in {list} {{\n    \
            total = total + p.x + p.y + i;\n\
         }}\n\
         echo total;\n",
        list = int_list(ITERS)
    )
}

/// Allocation: build and drop a small list on every iteration (refcount churn
/// through the heap allocator).
fn allocation_src() -> String {
    format!(
        "mut total = 0;\n\
         for i in {list} {{\n    \
            xs = [i, i, i];\n    \
            total = total + xs.count();\n\
         }}\n\
         echo total;\n",
        list = int_list(ITERS)
    )
}

fn vm_hot_paths(c: &mut Criterion) {
    let programs = [
        ("dispatch_fib", dispatch_src()),
        ("property_access", property_src()),
        ("allocation_list", allocation_src()),
    ];

    let mut group = c.benchmark_group("vm");
    for (name, src) in &programs {
        let module = compile(src);
        group.bench_function(*name, |b| {
            b.iter(|| {
                let result = VmBackend::new().run_module(black_box(&module));
                black_box(result);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, vm_hot_paths);
criterion_main!(benches);
