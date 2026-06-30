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

/// A blind-overwrite struct accumulator: an 8-field class whose global accumulator has one field
/// overwritten each iteration (`acc = Wide { ...acc, f0: i }`). The accumulator is read only after
/// the loop, so it stays uniquely owned (a global, stored via the consuming `StoreGlobal`). Phase
/// 5.1b's global struct reuse (`TakeGlobal` + `MakeStructInPlace`) now overwrites the single field
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

/// A **read-update** struct accumulator inside a function (`acc = Wide { ...acc, f0: acc.f0 + 1 }`),
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

/// A **`mut` field-assignment** accumulator inside a function (`acc.f0 = acc.f0 + i`, Phase 5.2),
/// returned after the loop. An 8-field class whose `mut f0` is overwritten each iteration. With the
/// in-place field-set reuse it overwrites the single slot in place (O(1)); without it (the copy
/// path) it clones all 8 slots per step — so the win is a constant factor proportional to the field
/// count, like `record_update`. The field read keeps the accumulator uniquely owned at the write.
fn field_assign_src(n: usize) -> String {
    let fields: Vec<String> = (0..8)
        .map(|i| {
            if i == 0 {
                "mut f0: int".to_string()
            } else {
                format!("f{i}: int")
            }
        })
        .collect();
    let inits: Vec<String> = (0..8).map(|i| format!("f{i}: 0")).collect();
    format!(
        "class Wide {{\n    {}\n}}\n\
         fn build(n: int): int {{\n    \
            mut acc = Wide {{ {} }};\n    \
            for i in 0..n {{\n        \
                acc.f0 = acc.f0 + i;\n    \
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

/// A `List<packed>` workload (P-PACK 2.4): build an `n`-element list literal of a `Vec3`, then index
/// every element and sum its three fields. With `@packed` the VM stores the list as one flat
/// raw-primitive buffer; the plain-`struct` variant stores `n` boxed objects. Each `data[i].field`
/// read **fuses** to a single `Op::IndexField` (P-PACK 2.5+): the packed list decodes one word with
/// no element object materialized. This was the scalar-access cost the flat layout otherwise paid —
/// 2.4 was ~1.55× *slower* here than boxed; fusion eliminated that, making packed scalar access ~3–4%
/// *faster* than boxed (and the boxed path itself ~7–10% faster, from one op replacing index+load).
/// The memory win (2.5 streaming keeps the build peak at one element + the buffer, ~3.6× under boxed)
/// is measured separately by the `peak_memory` residency test.
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

/// A `List<packed>` **producer** workload (P-PACK 2.6): build the list, run it through a selection
/// producer (`op` over `data`), then index + sum a field over the result. With `@packed` the VM keeps
/// the result flat — copying the kept elements' words — instead of allocating `n` boxed objects; the
/// plain-`struct` variant materializes a boxed result. The producer + downstream scalar read are timed
/// together. `reverse`/`slice`/`set`/`concat` are word copies — ~12–16% faster than boxed (no N
/// allocations or retains); `filter` must materialize each element for the predicate (a ~5% time
/// cost); `map` (category B — builds N output objects either way, then packs each) is ~time-neutral
/// but a 3.8× memory win since it streams both the input read and the output pack. All keep the result
/// flat (a 2.3–3.8× memory win — see the `peak_memory` residency test).
fn packed_producer_src(n: usize, packed: bool, op: &str) -> String {
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
         result = {op}\n\
         mut sum = 0.0\n\
         for i in 0..result.count() {{\n    \
            sum = sum + result[i].x\n\
         }}\n\
         echo sum\n"
    )
}

/// A serialization workload (P-PACK 4.4): build an `n`-element packed `List<Vec3<f32>>` and serialize
/// it, either as raw `bytes` (`.to_bytes()` — an O(n) copy of the flat buffer) or as JSON
/// (`json.stringify` — per-element formatting + a growing string). Timed side by side: the binary
/// path is ~7× faster end-to-end (192µs vs 1.47ms at n=1k, 782µs vs 5.66ms at n=4k) — and the
/// serialize step alone far more, since both share the list-build cost.
fn serialize_src(n: usize, binary: bool) -> String {
    let mut elems = String::with_capacity(n * 36);
    for i in 0..n {
        if i > 0 {
            elems.push_str(", ");
        }
        elems.push_str("V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }");
    }
    let op = if binary {
        "data.to_bytes().count()"
    } else {
        "json.stringify(data).count()"
    };
    format!(
        "use std.{{json}}\n\
         @packed struct V3 {{ x: f32; y: f32; z: f32 }}\n\
         data = [{elems}]\n\
         echo {op}\n"
    )
}

/// A bulk `vec.add_all` workload (P-PACK Phase 4.2): build two `n`-element `Vec3<f32>` lists and add
/// them component-wise. With `@packed` the operands are flat `f32` byte buffers and the kernel runs an
/// autovectorized loop over them (`lang_stdlib::vec3::add_buffers`); the plain-`struct` variant misses
/// the packed fast path and takes the scalar fallback (materialize each element, add, rebuild). The
/// two are timed side by side: the flat-buffer kernel is ~1.8–2× faster (383µs vs 733µs at n=1k,
/// 1.52ms vs 3.10ms at n=4k, with the P-PACK 4.3 byte-direct kernel) — it avoids the N element
/// materializations and runs an autovectorizable `f32` loop over contiguous data.
fn vec_add_all_src(n: usize, packed: bool) -> String {
    let kw = if packed { "@packed struct" } else { "struct" };
    let mut elems = String::with_capacity(n * 36);
    for i in 0..n {
        if i > 0 {
            elems.push_str(", ");
        }
        elems.push_str("V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }");
    }
    format!(
        "use std.{{vec}}\n\
         {kw} V3 {{ x: f32; y: f32; z: f32 }}\n\
         xs = [{elems}]\n\
         ys = [{elems}]\n\
         r = vec.add_all(xs, ys)\n\
         echo r.count()\n"
    )
}

/// A fused lazy-iterator pipeline vs the eager `map`/`filter` equivalent (Track I.1c). Both compute
/// `sum(map(xs, *2) |> filter(even))`; the `sum` terminal means neither materializes a *result* list,
/// so the difference is purely the **intermediate** lists. The eager form allocates two full
/// `n`-element lists (one from `map`, one from `filter`); the lazy form streams one element at a time
/// through `map → filter → sum` with no intermediate list at all (O(1) extra space). This isolates the
/// allocation the closure adapters eliminate.
fn iter_pipeline_src(n: usize, lazy: bool) -> String {
    let pipeline = if lazy {
        "xs.iter().map(fn(v) => v * 2).filter(fn(v) => v % 2 == 0).sum()".to_string()
    } else {
        "sum(filter(map(xs, fn(v) => v * 2), fn(v) => v % 2 == 0))".to_string()
    };
    format!("xs = 0..{n}\necho {pipeline}\n")
}

/// The early-stop case (Track I.1c): take only the first 10 results of `map → filter`. The lazy form
/// stops pulling once `take(10)` is satisfied — it touches ~20 source elements regardless of `n`; the
/// eager form must build the *entire* mapped and filtered lists before slicing the first 10, so its
/// work grows with `n`. This is where laziness wins on **time**, not just memory.
fn iter_take_pipeline_src(n: usize, lazy: bool) -> String {
    let pipeline = if lazy {
        "xs.iter().map(fn(v) => v * 2).filter(fn(v) => v % 2 == 0).take(10).collect().count()"
            .to_string()
    } else {
        "filter(map(xs, fn(v) => v * 2), fn(v) => v % 2 == 0).slice(0, 10).count()".to_string()
    };
    format!("xs = 0..{n}\necho {pipeline}\n")
}

/// Sizes for the packed-list workload — a couple of points to confirm the per-element cost is flat.
const PACKED_SIZES: &[usize] = &[1000, 4000];

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

/// A function-local set accumulator: insert n distinct elements via `s = s.add(i)`. Was O(n² log n)
/// (each `add` cloned the whole set and re-sorted); the in-place reuse (the receiver register is
/// consumed and the uniquely-owned canonical buffer binary-search-inserts one element) brings it to
/// O(n). Parameterized over n so the scaling is visible.
fn set_accumulate_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut s = #{{}};\n    for i in 0..{n} {{\n        s = s.add(i);\n    }}\n    return s.count();\n}}\necho build();\n"
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

/// **Allocation churn:** build and drop a short-lived struct on every iteration. Each step allocates
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

/// **Cyclic garbage** (Phase 6.4): each `make_cycle` call ties a closure↔cell reference cycle (a
/// self-recursive nested `fn`) that is unreachable once the call returns — `n` cycles built and
/// abandoned, the workload that *only* the cycle collector reclaims. Stresses collection: the trace
/// must walk the whole live heap, while trial-deletion examines only the buffered candidates.
fn mm_cyclic_garbage_src(n: usize) -> String {
    format!(
        "fn make_cycle(): int {{\n    \
            fn rec(k: int): int {{ if k <= 0 {{ return 0; }} return rec(k - 1); }}\n    \
            return rec(1);\n\
         }}\n\
         mut total = 0;\n\
         for i in 0..{n} {{\n    total = total + make_cycle();\n}}\n\
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

    let mut setacc = c.benchmark_group("vm_set_accumulate");
    for &n in LOOP_SIZES {
        let module = compile(&set_accumulate_src(n));
        setacc.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    setacc.finish();

    let mut fieldset = c.benchmark_group("vm_field_assign");
    for &n in LOOP_SIZES {
        let module = compile(&field_assign_src(n));
        fieldset.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    fieldset.finish();

    let mut pl = c.benchmark_group("vm_packed_list");
    for &n in PACKED_SIZES {
        let packed = compile(&packed_list_src(n, true));
        pl.bench_with_input(BenchmarkId::new("packed", n), &packed, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let boxed = compile(&packed_list_src(n, false));
        pl.bench_with_input(BenchmarkId::new("boxed", n), &boxed, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    pl.finish();

    let mut prod = c.benchmark_group("vm_packed_producer");
    for (op_label, op) in [
        ("reverse", "data.reverse()"),
        ("filter", "filter(data, fn(v) => v.x > 0.0)"),
        ("set", "data.set(0, Vec3 { x: 9.0, y: 9.0, z: 9.0 })"),
        ("concat", "data ~ data.slice(0, 1)"),
        (
            "map",
            "map(data, fn(v) => Vec3 { x: v.x + 1.0, y: v.y, z: v.z })",
        ),
    ] {
        for &n in PACKED_SIZES {
            let packed = compile(&packed_producer_src(n, true, op));
            prod.bench_with_input(
                BenchmarkId::new(format!("{op_label}-packed"), n),
                &packed,
                |b, module| b.iter(|| black_box(VmBackend::new().run_module(black_box(module)))),
            );
            let boxed = compile(&packed_producer_src(n, false, op));
            prod.bench_with_input(
                BenchmarkId::new(format!("{op_label}-boxed"), n),
                &boxed,
                |b, module| b.iter(|| black_box(VmBackend::new().run_module(black_box(module)))),
            );
        }
    }
    prod.finish();

    let mut vadd = c.benchmark_group("vm_vec_add_all");
    for &n in PACKED_SIZES {
        let packed = compile(&vec_add_all_src(n, true));
        vadd.bench_with_input(BenchmarkId::new("packed", n), &packed, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let boxed = compile(&vec_add_all_src(n, false));
        vadd.bench_with_input(BenchmarkId::new("boxed", n), &boxed, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    vadd.finish();

    let mut ser = c.benchmark_group("vm_serialize");
    for &n in PACKED_SIZES {
        let binary = compile(&serialize_src(n, true));
        ser.bench_with_input(BenchmarkId::new("to_bytes", n), &binary, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let json = compile(&serialize_src(n, false));
        ser.bench_with_input(BenchmarkId::new("json", n), &json, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    ser.finish();

    // Fused lazy pipeline vs eager map/filter (Track I.1c): `lazy` streams with no intermediate list,
    // `eager` allocates two full n-element lists. Timed side by side over n so both the constant-factor
    // and the allocation difference are visible.
    let mut pipe = c.benchmark_group("vm_iter_pipeline");
    for &n in LOOP_SIZES {
        let lazy = compile(&iter_pipeline_src(n, true));
        pipe.bench_with_input(BenchmarkId::new("lazy", n), &lazy, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let eager = compile(&iter_pipeline_src(n, false));
        pipe.bench_with_input(BenchmarkId::new("eager", n), &eager, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    pipe.finish();

    // Early-stop pipeline (Track I.1c): `take(10)` after map→filter. The lazy form's work is constant
    // in n (it stops after 10 results); the eager form builds the full lists first, so it grows with n.
    let mut takepipe = c.benchmark_group("vm_iter_take_pipeline");
    for &n in LOOP_SIZES {
        let lazy = compile(&iter_take_pipeline_src(n, true));
        takepipe.bench_with_input(BenchmarkId::new("lazy", n), &lazy, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let eager = compile(&iter_take_pipeline_src(n, false));
        takepipe.bench_with_input(BenchmarkId::new("eager", n), &eager, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    takepipe.finish();

    let mut disp = c.benchmark_group("vm_member_dispatch");
    for &n in LOOP_SIZES {
        let module = compile(&member_dispatch_src(n));
        disp.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    disp.finish();

    // Blind-overwrite struct accumulator (`acc = Wide { ...acc, f0: i }`), parameterized over n so
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

    // Phase 6.4 head-to-head: the two cycle collectors on a cycle-heavy workload (collection time)
    // and on acyclic allocation churn (the per-allocation / per-release overhead each imposes on code
    // that forms no cycles). The lower-overhead one is the default; this is the data behind that call.
    use lang_value::CollectorMode::{Trace, TrialDeletion};
    let mut col = c.benchmark_group("vm_collector");
    for &n in LOOP_SIZES {
        let cyclic = compile(&mm_cyclic_garbage_src(n));
        let churn = compile(&mm_alloc_churn_src(n));
        for (mode, tag) in [(Trace, "trace"), (TrialDeletion, "trial")] {
            col.bench_with_input(
                BenchmarkId::new(format!("cyclic_{tag}"), n),
                &cyclic,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module_with_collector(module, mode)))
                },
            );
            col.bench_with_input(
                BenchmarkId::new(format!("churn_{tag}"), n),
                &churn,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module_with_collector(module, mode)))
                },
            );
        }
    }
    col.finish();
}

criterion_group!(benches, vm_hot_paths);
criterion_main!(benches);
