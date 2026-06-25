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

/// A blind-overwrite record accumulator: an 8-field class whose global accumulator has one field
/// overwritten each iteration (`acc = Wide { ...acc, f0: i }`). The accumulator is read only after
/// the loop, so it stays uniquely owned (a global, stored via the consuming `StoreGlobal`). Phase
/// 5.1b's global record reuse (`TakeGlobal` + `MakeRecordInPlace`) now overwrites the single field
/// in place instead of allocating a fresh object and copying all 8 every step — a ~2.6× constant-
/// factor cut (both were already O(n) in `n`; the win is the per-step work, alloc + 8 copies → 1
/// in-place overwrite).
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
/// returned after the loop. The `acc.f0` read keeps the accumulator uniquely owned at the construct
/// (the IR's `Drop` after the `LoadField` plus 3.3b's no-`Move` local declaration), so once
/// Phase-5 reuse lands it can mutate in place; today it still copies all 8 fields per step. Distinct
/// from `record_update_src` in that the new field value depends on a field read of the old value.
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

/// The self-append accumulator `acc ~= [i]` n times. Was O(n²) (each `~` copied the whole left
/// list); Phase 5.1b's `ConcatInPlace` (with a `TakeGlobal` exposing the global accumulator's unique
/// ownership) extends the backing buffer in place, bringing it to O(n) — measured ~31× at n=8000,
/// and the gap widens with n. Parameterized over n so the scaling — not just a constant factor — is
/// visible.
fn accumulate_src(n: usize) -> String {
    format!(
        "mut acc = [];\n\
         for i in 0..{n} {{\n    \
            acc ~= [i];\n\
         }}\n\
         echo acc.count();\n"
    )
}

/// A function-local map accumulator built by repeated index-assignment `m[k] = i` (desugaring to
/// `m = m.set(k, i)`) n times. Was O(n²) (each `set` copied the whole map); Phase 5.1c's in-place
/// reuse (the receiver register is consumed and the uniquely-owned backing map mutated) brings it to
/// O(n) — measured ~295× at n=8000 (791 ms → 2.7 ms), the gap widening with n. Local (in a function)
/// because the VM's method-receiver reuse covers directly-held locals this slice; parameterized over
/// n so the scaling is visible.
fn map_accumulate_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut m = {{}};\n    for i in 0..{n} {{\n        m[\"k${{i}}\"] = i;\n    }}\n    return m.count();\n}}\necho build();\n"
    )
}

/// A function-local list index-write accumulator: build an n-element list once, then overwrite every
/// slot via `xs[i] = …` (desugaring to `xs = xs.set(i, …)`) n times. Was O(n²) (each `set` copied the
/// whole list); the in-place reuse (the receiver register is consumed and the uniquely-owned backing
/// list's slot overwritten, O(1)) brings it to O(n) — measured ~54× at n=8000, the gap widening with
/// n. Parameterized over n so the scaling is visible.
fn list_index_write_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut xs = 0..{n};\n    for i in 0..{n} {{\n        xs[i] = i * 2;\n    }}\n    return xs.count();\n}}\necho build();\n"
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

// --- Memory-management stress benches (architecture §0.3) — the pre-migration baseline the
// memory-management track (`plans/memory-management/`) compares against. Each isolates a reclamation
// cost: allocation churn, destructor firing, and deep-structure teardown. The cyclic-garbage bench is
// deferred to Phase 6 (it needs a live cycle collector; today it would leak each iteration).

/// **Allocation churn:** build and drop a short-lived record on every iteration. Each step allocates
/// a fresh `Pair` and lets it die immediately — pure allocator + refcount-to-zero throughput, the
/// path prompt reclamation (Phase 3) and reuse (Phase 5) most affect.
fn mm_alloc_churn_src(n: usize) -> String {
    format!(
        "class Pair {{\n    a: int\n    b: int\n}}\n\
         mut total = 0;\n\
         for i in 0..{n} {{\n    \
            p = Pair {{ a: i, b: i }};\n    \
            total = total + p.a;\n\
         }}\n\
         echo total;\n"
    )
}

/// **Destructor-heavy:** a `mut` global holding a `destruct`-bearing instance, reassigned every
/// iteration. The reassignment destroys the displaced instance immediately (spec §5), so the
/// destructor fires `n` times — the deterministic-destruction path Phase 4 generalizes to all scopes.
/// The `destruct` body is side-effect-free (no `echo`) so the bench measures destructor *dispatch*,
/// not output.
fn mm_destructor_heavy_src(n: usize) -> String {
    format!(
        "class Res {{\n    id: int\n    \
            fn new(id: int): Res {{ return Res {{ id: id }}; }}\n    \
            destruct {{ x = id + 1; }}\n\
         }}\n\
         mut r = Res.new(0);\n\
         for i in 0..{n} {{\n    \
            r = Res.new(i);\n\
         }}\n\
         echo r.id;\n"
    )
}

/// **Deep-structure free:** build one deeply-nested list `[[…[0]…]]` of the given depth, then let it
/// fall out of scope at program end — a single recursive teardown through `free`'s child-release walk
/// (spec §4, container-before-contained). Stresses the depth of reclamation rather than its rate.
fn mm_deep_free_src(depth: usize) -> String {
    let mut s = String::from("x = ");
    s.push_str(&"[".repeat(depth));
    s.push('0');
    s.push_str(&"]".repeat(depth));
    s.push_str(";\necho x.count();\n");
    s
}

/// Nesting depth for the deep-free bench — deep enough that the recursive teardown dominates, but
/// safely under the point where `free`'s recursive child-release overflows a small (2 MiB) thread
/// stack (~200 levels). The recursion depth is a real reclamation limitation noted in the Phase-0
/// baseline (a candidate for an iterative teardown later).
const DEEP_FREE_DEPTH: usize = 100;

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

    let mut mapacc = c.benchmark_group("vm_map_accumulate");
    for &n in LOOP_SIZES {
        let module = compile(&map_accumulate_src(n));
        mapacc.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    mapacc.finish();

    let mut listidx = c.benchmark_group("vm_list_index_write");
    for &n in LOOP_SIZES {
        let module = compile(&list_index_write_src(n));
        listidx.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    listidx.finish();

    let mut disp = c.benchmark_group("vm_member_dispatch");
    for &n in LOOP_SIZES {
        let module = compile(&member_dispatch_src(n));
        disp.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    disp.finish();

    // Blind-overwrite record accumulator (`acc = Wide { ...acc, f0: i }`), parameterized over n so
    // the complexity class is visible. Today's copying lowering is O(n·fields); the anchor for the
    // Phase-5 in-place reuse that will cut it to O(n). (Was a three-mode `ReuseMode` matrix; the modes
    // were retired with the inert P-REUSE machinery in memory-management Phase 3.3c — a single
    // canonical compile remains.)
    let mut reuse = c.benchmark_group("vm_record_update");
    for &n in LOOP_SIZES {
        let module = compile(&record_update_src(n));
        reuse.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))))
        });
    }
    reuse.finish();

    // Read-update accumulator (`acc = Wide { ...acc, f0: acc.f0 + 1 }`): the new value depends on a
    // field read of the old, the distinct workload Phase-5 reuse must keep uniquely owned through the
    // read. Same copying baseline today.
    let mut read = c.benchmark_group("vm_record_update_read");
    for &n in LOOP_SIZES {
        let module = compile(&record_update_read_src(n));
        read.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))))
        });
    }
    read.finish();

    // MM-stress baseline: allocation churn and destructor firing parameterized over n (so the later
    // track can show prompt reclamation / reuse changed the constant or the slope), plus a single
    // deep-structure teardown.
    let mut mm = c.benchmark_group("vm_mm");
    for &n in LOOP_SIZES {
        let churn = compile(&mm_alloc_churn_src(n));
        mm.bench_with_input(BenchmarkId::new("alloc_churn", n), &churn, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))))
        });
        let dtor = compile(&mm_destructor_heavy_src(n));
        mm.bench_with_input(
            BenchmarkId::new("destructor_heavy", n),
            &dtor,
            |b, module| b.iter(|| black_box(VmBackend::new().run_module(black_box(module)))),
        );
    }
    let deep = compile(&mm_deep_free_src(DEEP_FREE_DEPTH));
    mm.bench_function(BenchmarkId::new("deep_free", DEEP_FREE_DEPTH), |b| {
        b.iter(|| black_box(VmBackend::new().run_module(black_box(&deep))))
    });
    mm.finish();
}

criterion_group!(benches, vm_hot_paths);
criterion_main!(benches);
