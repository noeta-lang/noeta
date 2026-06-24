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

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use lang_bytecode::Module;
use lang_compiler::ReuseMode;
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
    // A parse error would otherwise yield a near-empty `program` that compiles to a trivial module
    // and benches in nanoseconds — silently measuring nothing. Fail loudly instead.
    assert!(
        parsed.diagnostics.is_empty(),
        "bench program must parse without diagnostics: {:?}",
        parsed.diagnostics
    );
    lang_compiler::compile(&parsed.program).expect("bench program must be in the VM subset")
}

/// Like [`compile`] but under an explicit [`ReuseMode`] — used by the record-update reuse matrix to
/// compile the *same* source three ways (no reuse / runtime-checked / statically-elided check).
fn compile_reuse(src: &str, mode: ReuseMode) -> Module {
    let source = Source::new(SourceId::FIRST, "bench.lang", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "bench program must parse without diagnostics: {:?}",
        parsed.diagnostics
    );
    lang_compiler::compile_with_options(&parsed.program, mode)
        .expect("bench program must be in the VM subset")
}

/// A blind-overwrite record accumulator: an 8-field class whose global accumulator has one field
/// overwritten each iteration (`acc = Wide { ...acc, f0: i }`). The plain path (`ReuseMode::Off`)
/// allocates a fresh object and copies all 8 fields every step — O(n·fields); reuse mutates the one
/// changed slot in place — O(n). The accumulator is read only after the loop, so it stays uniquely
/// owned (a global, stored via the consuming `StoreGlobal`), letting both reuse modes fire.
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

/// A **read-update** record accumulator inside a function (`acc = Wide { ...acc, f0: acc.f0 + 1 }`),
/// returned after the loop. Before drop insertion this could not reuse — the `acc.f0` read retained
/// the accumulator into a temporary the register machine never freed, so reuse fell back to a copy.
/// With drop insertion (`Drop` after the `LoadField`, plus no declaration `Move` for the local) the
/// accumulator stays uniquely owned, so `runtime` reuses in place. `off` vs `runtime` is the win drop
/// insertion unlocked. (Static stays off — the analysis keeps read-updates on the runtime path.)
fn record_update_read_src(n: usize) -> String {
    let fields: Vec<String> = (0..8).map(|i| format!("f{i}: int")).collect();
    let inits: Vec<String> = (0..8).map(|i| format!("f{i}: 0")).collect();
    format!(
        "class Wide {{\n    {}\n}}\n\
         fn build(n: int): int {{\n    \
            mut acc = Wide {{ {} }};\n    \
            for i in 0..n {{\n        \
                acc = Wide {{ ...acc, f0: acc.f0 + 1 }};\n    \
            }}\n    \
            return acc.f0;\n\
         }}\n\
         echo build({n});\n",
        fields.join("\n    "),
        inits.join(", "),
    )
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

/// The self-append accumulator `acc ~= [i]` n times. O(n²) today (each `~` copies the whole left
/// list); the P-COW VM half (folded into P-GC) brings it to O(n). Parameterized over n so the
/// scaling — not just a constant factor — is visible.
fn accumulate_src(n: usize) -> String {
    format!(
        "mut acc = [];\n\
         for i in 0..{n} {{\n    \
            acc ~= [i];\n\
         }}\n\
         echo acc.count();\n"
    )
}

/// A hot method-call + field-read site on the same receiver every iteration — the monomorphic
/// dispatch/property pattern P-IC (inline caches) targets beyond the existing `property_access`.
fn member_dispatch_src(n: usize) -> String {
    format!(
        "class Counter {{\n    n: int\n    \
            fn bump(): int {{ return n + 1; }}\n\
         }}\n\
         mut total = 0;\n\
         c = Counter {{ n: 1 }};\n\
         for i in 0..{n} {{\n    \
            total = total + c.bump() + c.n + i;\n\
         }}\n\
         echo total;\n"
    )
}

/// Sizes for the parameterized loops. Each doubles the previous, so the per-step time ratio reveals
/// the complexity class (≈4× per doubling ⇒ O(n²); ≈2× ⇒ O(n)).
const LOOP_SIZES: &[usize] = &[1000, 2000, 4000, 8000];

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

    let mut acc = c.benchmark_group("vm_accumulate");
    for &n in LOOP_SIZES {
        let module = compile(&accumulate_src(n));
        acc.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    acc.finish();

    let mut disp = c.benchmark_group("vm_member_dispatch");
    for &n in LOOP_SIZES {
        let module = compile(&member_dispatch_src(n));
        disp.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    disp.finish();

    // Record-update reuse matrix: the same blind-overwrite accumulator compiled three ways isolates
    // the two reuse axes — `off` vs `runtime` is the generalization win (allocation+copy elimination,
    // O(n·fields)→O(n)); `runtime` vs `static` is the compile-time-hoist win (the elided refcount
    // check). Parameterized over n so the complexity-class change is visible, not just a constant.
    let modes = [
        ("off", ReuseMode::Off),
        ("runtime", ReuseMode::Runtime),
        ("static", ReuseMode::Static),
    ];
    let mut reuse = c.benchmark_group("vm_record_update");
    for (label, mode) in modes {
        for &n in LOOP_SIZES {
            let module = compile_reuse(&record_update_src(n), mode);
            reuse.bench_with_input(BenchmarkId::new(label, n), &module, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))))
            });
        }
    }
    reuse.finish();

    // Read-update reuse, unlocked by drop insertion: `off` (copy) vs `runtime` (in-place). Without
    // the `Drop` after `acc.f0`'s `LoadField`, `runtime` would fall back to a copy (refcount > 1), so
    // this gap is exactly what drop insertion bought.
    let mut read = c.benchmark_group("vm_record_update_read");
    for (label, mode) in [("off", ReuseMode::Off), ("runtime", ReuseMode::Runtime)] {
        for &n in LOOP_SIZES {
            let module = compile_reuse(&record_update_read_src(n), mode);
            read.bench_with_input(BenchmarkId::new(label, n), &module, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))))
            });
        }
    }
    read.finish();
}

criterion_group!(benches, vm_hot_paths);
criterion_main!(benches);
