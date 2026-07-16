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

use noeta_bytecode::Module;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

/// Source → compiled `Module`. Panics if the program falls outside the VM
/// subset — bench programs must stay compilable so they exercise real opcodes.
fn compile(src: &str) -> Module {
    let source = Source::new(SourceId::FIRST, "bench.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    // A parse error would otherwise yield a near-empty `program` that compiles to a trivial module
    // and benches in nanoseconds — silently measuring nothing. Fail loudly instead.
    assert!(
        parsed.diagnostics.is_empty(),
        "bench program must parse without diagnostics: {:?}",
        parsed.diagnostics
    );
    noeta_compiler::compile(&parsed.program).expect("bench program must be in the VM subset")
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
         for i in 0..result.len() {{\n    \
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
        "data.to_bytes().len()"
    } else {
        "json.stringify(data).len()"
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
/// autovectorized loop over them (`noeta_stdlib::vec3::add_buffers`); the plain-`struct` variant misses
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
         echo r.len()\n"
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
        "xs.map(fn(v) => v * 2).filter(fn(v) => v % 2 == 0).slice(0, 10).len()".to_string()
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

/// S2 (P-VMT-FRAME): call-heavy recursion. `fib(n)` performs ~2·fib(n) calls. Before this slice each
/// call heap-allocated its register file (`vec![Value::unit(); num_registers]`) and freed it on
/// return; now every frame is a base offset into one contiguous per-run register stack that a call
/// extends and a return truncates — so an ordinary call allocates nothing once the stack has grown
/// to the run's deepest depth. Parameterized over depth so the per-call cost, not just a constant, is
/// visible.
fn fib_src(n: usize) -> String {
    format!(
        "fn fib(n: int): int {{\n    \
            if n < 2 {{ return n; }}\n    \
            return fib(n - 1) + fib(n - 2);\n\
         }}\n\
         echo fib({n});\n"
    )
}

const FIB_DEPTHS: &[usize] = &[20, 24, 28];

/// S3 (P-VMT-DISP): a tight arithmetic loop with no per-iteration heap work (ints are NaN-boxed),
/// so its cost is dominated by the raw dispatch floor — the per-op work the interpreter does before
/// any real computation. Before S3 the loop head re-derived the current frame (`frames.len() - 1`,
/// `module.protos[..]`) and re-bounds-checked `frames[top].pc` and every operand on every op; S3
/// hoists the frame window into loop-locals re-derived only on a call/return, so a straight-line op
/// is just `pc += 1`. Parameterized so the per-iteration cost is visible, not just a constant.
fn loop_sum_src(n: usize) -> String {
    format!(
        "fn sum(n: int): int {{\n    \
            mut acc = 0;\n    \
            for i in 0..n {{\n        \
                acc = acc + i;\n    \
            }}\n    \
            return acc;\n\
         }}\n\
         echo sum({n});\n"
    )
}

/// The same tight arithmetic loop as `loop_sum_src` but with **top-level** `mut` accumulators (the
/// natural scripting shape) rather than function locals — so `i`/`total` are globals. Before
/// P-VMT-GSLOT each `LoadGlobal`/`StoreGlobal` hashed the name against a `HashMap` every iteration
/// (6 hash+probe ops per step here); slot indexing makes each a direct `Vec` index. Measured ~2.3×
/// end to end. The contrast with `loop_sum_src` (function locals, already register-fast) isolates the
/// global-access cost.
fn global_loop_src(n: usize) -> String {
    format!(
        "mut total = 0;\nmut i = 0;\nwhile i < {n} {{\n    total = total + (i % 7);\n    i = i + 1;\n}}\necho total;\n"
    )
}

const LOOP_ITERS: &[usize] = &[100_000, 1_000_000];

/// J1 (P-JIT integer fast path): a pure-integer `while` loop in a function (register-local `i`/`total`,
/// no globals, no calls) — the shape the JIT compiles to native machine code. The `while` form (not
/// `for i in 0..n`, which materializes a range list) keeps the whole body in the J1 op set:
/// `LoadConst`/`Binary`/`CondBranch`/`Move`/`Drop`/`Jump`. Benched interpreter vs forced-JIT so the
/// native win is directly visible.
#[cfg(feature = "jit")]
fn jit_loop_src(n: usize) -> String {
    format!(
        "fn run(n: int): int {{\n    mut total = 0;\n    mut i = 0;\n    while i < n {{\n        total = total + (i % 7);\n        i = i + 1;\n    }}\n    return total;\n}}\necho run({n});\n"
    )
}

/// S1 (P-JIT bitwise): an xorshift-style loop of `^ & << >>` whose intermediates provably fit the
/// 48-bit immediate range (36-bit state, shifts of 11/7), so the whole body stays native — the
/// Tier-B ops this slice made JITable. Interp-vs-JIT pair like `loop_*`.
#[cfg(feature = "jit")]
fn jit_bitwise_loop_src(n: usize) -> String {
    format!(
        "fn run(n: int): int {{\n    mut h = 123456789;\n    mut i = 0;\n    while i < n {{\n        h = h ^ (h << 11);\n        h = h & 68719476735;\n        h = h ^ (h >> 7);\n        i = i + 1;\n    }}\n    return h;\n}}\necho run({n});\n"
    )
}

/// S2 (P-JIT mixed lane): the canonical float-accumulator loop — `total + i` pairs an f64 with the
/// int counter every iteration, the shape that previously bailed per iteration. Interp-vs-JIT pair.
#[cfg(feature = "jit")]
fn jit_mixed_loop_src(n: usize) -> String {
    format!(
        "fn run(n: int): float {{\n    mut total = 0.0;\n    mut i = 0;\n    while i < n {{\n        total = total + i;\n        i = i + 1;\n    }}\n    return total;\n}}\necho run({n});\n"
    )
}

/// J2 (P-JIT float fast path): the same loop shape with an f64 accumulator — a float `Binary` (`*`,
/// `+`) each iteration alongside the integer counter, so the JIT's runtime int-vs-float dispatch and
/// the native f64 arithmetic both get exercised.
#[cfg(feature = "jit")]
fn jit_float_loop_src(n: usize) -> String {
    format!(
        "fn run(n: int): float {{\n    mut total = 0.0;\n    mut i = 0;\n    while i < n {{\n        total = total + 1.5;\n        i = i + 1;\n    }}\n    return total;\n}}\necho run({n});\n"
    )
}

/// J4 slice 2 (P-JIT field access): a hot loop reading (`LoadField`) and writing (`SetField`) the
/// fields of a mutable `struct`, so the leaf-op helper's field paths get exercised on the fast path
/// while the surrounding loop stays native.
#[cfg(feature = "jit")]
fn field_loop_src(n: usize) -> String {
    format!(
        "struct Point {{\n    mut x: int\n    mut y: int\n}}\nfn run(n: int): int {{\n    mut p = Point {{ x: 0, y: 0 }};\n    mut i = 0;\n    while i < n {{\n        p.x = p.x + i;\n        p.y = p.y + p.x;\n        i = i + 1;\n    }}\n    return p.x + p.y;\n}}\necho run({n});\n"
    )
}

/// P-JIT field-read floor: a **read-only** hot loop over a WIDE struct, reading the last field
/// (worst-case `slot_of` scan) each iteration — no `SetField`, so the loop is pure `LoadField` +
/// native arithmetic. This isolates the per-`LoadField` leaf-helper-call cost: the JIT stays *below*
/// the tier-0 interpreter here (whose `LoadField` is a call-free match arm with its own inline
/// cache), which is why a *helper-internal* inline cache was found not to help — only a call-free
/// native field read (guard + slot load emitted as machine code, needing a layout-stable object
/// representation) would cross this floor. Kept as the bench that will show that win when it lands.
#[cfg(feature = "jit")]
fn wide_field_read_src(n: usize) -> String {
    let fields: Vec<String> = (0..8).map(|i| format!("f{i}: int")).collect();
    let inits: Vec<String> = (0..8).map(|i| format!("f{i}: {i}")).collect();
    format!(
        "struct Wide {{ {} }}\n\
         fn run(n: int): int {{\n    \
            w = Wide {{ {} }};\n    \
            mut acc = 0;\n    \
            mut i = 0;\n    \
            while i < n {{\n        \
                acc = acc + w.f7;\n        \
                i = i + 1;\n    \
            }}\n    \
            return acc;\n\
         }}\n\
         echo run({n});\n",
        fields.join("; "),
        inits.join(", "),
    )
}

/// J4 slice 3 (P-JIT indexing): a hot loop indexing a list (`xs[i]`) and a map (`m[key]`) — the
/// `Op::Index` list/map paths run through the leaf-op helper while the loop stays native.
#[cfg(feature = "jit")]
fn index_loop_src(n: usize) -> String {
    format!(
        "fn run(n: int): int {{\n    xs = [10, 20, 30, 40, 50];\n    m = {{ \"a\": 1, \"b\": 2, \"c\": 3 }};\n    keys = [\"a\", \"b\", \"c\"];\n    mut total = 0;\n    mut i = 0;\n    while i < n {{\n        total = total + xs[i % 5];\n        total = total + m[keys[i % 3]];\n        i = i + 1;\n    }}\n    return total;\n}}\necho run({n});\n"
    )
}

/// S5 (P-VMT-STR): string-interpolation throughput. Before S5 the compiler lowered a `"…${x}…"` to
/// `LoadConst "" + N×(Stringify + Concat)` — an intermediate `String` per part, the accumulator
/// reallocated on every step. S5 lowers it to a single `Op::BuildString` (one pass, one output
/// allocation). `single_hole` is the wordcount-style hot key `"word${i}"`; `multi_hole` stresses the
/// old fold's O(k²) copying with three holes.
fn interp_single_hole_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    \
            mut total = 0;\n    \
            for i in 0..{n} {{\n        \
                total = total + \"word${{i}}\".len();\n    \
            }}\n    \
            return total;\n\
         }}\n\
         echo build();\n"
    )
}

fn interp_multi_hole_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    \
            mut total = 0;\n    \
            for i in 0..{n} {{\n        \
                total = total + \"${{i}}-${{i}}-${{i}}\".len();\n    \
            }}\n    \
            return total;\n\
         }}\n\
         echo build();\n"
    )
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
         echo acc.len();\n"
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

/// A function-local **read-modify-write** map histogram: `m[k] = (m[k] ?? 0) + 1` over n updates with
/// a bounded key space (the wordcount idiom). Reading `m` before the write extends its live range, so
/// `insert_drops` puts a `DropVar` between the `m.set(...)` and its rebind — which denied the reuse
/// token until P-VMT-RMW made the pairing tolerant of intervening drops. Before: O(n²) (each `set`
/// copied the whole map); after: O(n) in-place — measured ~33× end to end at n=200000. The contrast
/// with `map_accumulate_src` (write-only, always reused) isolates exactly the RMW case.
fn map_rmw_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut m: Map<string, int> = {{}};\n    mut i = 0;\n    while i < {n} {{\n        k = \"w${{i % 500}}\";\n        prev = if m.has(k) then m[k] else 0;\n        m[k] = prev + 1;\n        i = i + 1;\n    }}\n    return m.count();\n}}\necho build();\n"
    )
}

/// The wordcount histogram via `get_or` — the built-in-method dispatch shape. Unlike
/// `map_rmw_src` (whose `m[k] = …` write takes the reuse fast path, the first rung of the
/// `CallMethod` dispatch), `m.get_or(k, 0)` is an ordinary built-in method call and pays the
/// full receiver-classification dispatch on every iteration. Gauges the dispatch cost itself.
fn map_get_or_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut m: Map<string, int> = {{}};\n    mut i = 0;\n    while i < {n} {{\n        k = \"w${{i % 500}}\";\n        m[k] = m.get_or(k, 0) + 1;\n        i = i + 1;\n    }}\n    return m.count();\n}}\necho build();\n"
    )
}

/// A function-local list index-write accumulator: build an n-element list once, then overwrite every
/// slot via `xs[i] = …` (desugaring to `xs = xs.set(i, …)`) n times. Was O(n²) (each `set` copied the
/// whole list); the in-place reuse (the receiver register is consumed and the uniquely-owned backing
/// list's slot overwritten, O(1)) brings it to O(n) — measured ~54× at n=8000, the gap widening with
/// n. Parameterized over n so the scaling is visible.
fn list_index_write_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut xs = 0..{n};\n    for i in 0..{n} {{\n        xs[i] = i * 2;\n    }}\n    return xs.len();\n}}\necho build();\n"
    )
}

/// A function-local set accumulator: insert n distinct elements via `s = s.add(i)`. Was O(n² log n)
/// (each `add` cloned the whole set and re-sorted); the in-place reuse (the receiver register is
/// consumed and the uniquely-owned canonical buffer binary-search-inserts one element) brings it to
/// O(n). Parameterized over n so the scaling is visible.
fn set_accumulate_src(n: usize) -> String {
    format!(
        "fn build(): int {{\n    mut s = #{{}};\n    for i in 0..{n} {{\n        s = s.add(i);\n    }}\n    return s.len();\n}}\necho build();\n"
    )
}

/// A **top-level (global)** map accumulator built by `m[k] = i` n times — the idiomatic
/// script-at-module-scope shape. Was O(n²) even after Phase 5.1c (which reused only function-local
/// receivers): the compiler dropped the reuse token for a global receiver, so every `set` deep-copied
/// the whole map. S1 (P-VMT-GACC) moves the global out with `TakeGlobal` so the in-place op sees
/// refcount 1, bringing it to O(n) — ~850× at n=40000 (33.5 s → 40 ms). The local twin is
/// `map_accumulate_src`; this is its global counterpart, parameterized so the scaling is visible.
fn map_accumulate_global_src(n: usize) -> String {
    format!("mut m = {{}};\nfor i in 0..{n} {{\n    m[\"k${{i}}\"] = i;\n}}\necho m.count();\n")
}

/// A top-level (global) list index-write accumulator — the global twin of `list_index_write_src`.
/// O(n²)→O(n) under S1 (global-receiver reuse via `TakeGlobal`).
fn list_index_write_global_src(n: usize) -> String {
    format!("mut xs = 0..{n};\nfor i in 0..{n} {{\n    xs[i] = i * 2;\n}}\necho xs.count();\n")
}

/// A top-level (global) set accumulator — the global twin of `set_accumulate_src`.
/// O(n² log n)→O(n) under S1 (global-receiver reuse via `TakeGlobal`).
fn set_accumulate_global_src(n: usize) -> String {
    format!("mut s = #{{}};\nfor i in 0..{n} {{\n    s = s.add(i);\n}}\necho s.count();\n")
}

/// A hot method-call + field-read site on the same receiver every iteration — the monomorphic
/// dispatch/property pattern P-IC (inline caches) targets beyond the existing `property_access`.
fn member_dispatch_src(n: usize) -> String {
    format!(
        "class Counter {{\n    n: int\n    \
            fn bump(): int {{ return self.n + 1; }}\n\
         }}\n\
         mut total = 0;\n\
         c = Counter {{ n: 1 }};\n\
         for i in 0..{n} {{\n    \
            total = total + c.bump() + c.n + i;\n\
         }}\n\
         echo total;\n"
    )
}

/// A hot method-call site on ENUM receivers (audit-1 finding 7): the enum arm historically
/// bypassed the shape-pointer inline cache and cloned `(type_name, method)` into owned `String`
/// keys per call. Two receivers of the same enum type keep the site monomorphic on the shape.
fn enum_method_src(n: usize) -> String {
    format!(
        "enum Status {{\n    Pending;\n    Paid;\n    \
            fn score(): int {{\n        \
                return match self {{\n            \
                    Status.Pending => 1,\n            \
                    Status.Paid => 2,\n        \
                }};\n    \
            }}\n\
         }}\n\
         fn run(n: int): int {{\n    \
            s = Status.Pending;\n    \
            p = Status.Paid;\n    \
            mut total = 0;\n    \
            mut i = 0;\n    \
            while i < n {{\n        \
                total = total + s.score() + p.score();\n        \
                i = i + 1;\n    \
            }}\n    \
            return total;\n\
         }}\n\
         echo run({n});\n"
    )
}

/// A hot operator-overload site (same finding): `+` on a class with `impl Add` dispatches through
/// the `(type_name, method)` table — historically two `String` allocations per executed operator.
fn operator_overload_src(n: usize) -> String {
    format!(
        "class Vec2 {{\n    x: int\n    y: int\n    \
            impl Add {{\n        \
                fn add(other: Vec2): Vec2 {{\n            \
                    return Vec2 {{ x: self.x + other.x, y: self.y + other.y }};\n        \
                }}\n    \
            }}\n\
         }}\n\
         fn run(n: int): int {{\n    \
            mut acc = Vec2 {{ x: 0, y: 0 }};\n    \
            step = Vec2 {{ x: 1, y: 2 }};\n    \
            mut i = 0;\n    \
            while i < n {{\n        \
                acc = acc + step;\n        \
                i = i + 1;\n    \
            }}\n    \
            return acc.x + acc.y;\n\
         }}\n\
         echo run({n});\n"
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

    let mut maprmw = c.benchmark_group("vm_map_rmw");
    for &n in LOOP_SIZES {
        let module = compile(&map_rmw_src(n));
        maprmw.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    maprmw.finish();

    let mut mapgetor = c.benchmark_group("vm_map_get_or");
    for &n in LOOP_SIZES {
        let module = compile(&map_get_or_src(n));
        mapgetor.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    mapgetor.finish();

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

    // S1 (P-VMT-GACC): the same collection accumulators built in a **top-level global** — O(n²)
    // before this slice (the compiler dropped the reuse token for a global receiver), O(n) after.
    let mut gacc = c.benchmark_group("vm_global_accumulate");
    for &n in LOOP_SIZES {
        let map = compile(&map_accumulate_global_src(n));
        gacc.bench_with_input(BenchmarkId::new("map", n), &map, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let list = compile(&list_index_write_global_src(n));
        gacc.bench_with_input(BenchmarkId::new("list", n), &list, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let set = compile(&set_accumulate_global_src(n));
        gacc.bench_with_input(BenchmarkId::new("set", n), &set, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    gacc.finish();

    // S2 (P-VMT-FRAME): call-heavy recursion — every call previously heap-allocated its register
    // file; the contiguous per-run register stack removes the per-call alloc after warm-up.
    let mut recur = c.benchmark_group("vm_recursion");
    for &n in FIB_DEPTHS {
        let module = compile(&fib_src(n));
        recur.bench_with_input(BenchmarkId::new("fib", n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    recur.finish();

    // S3 (P-VMT-DISP): the dispatch floor — a tight arithmetic loop whose per-iteration cost is
    // the interpreter's per-op overhead, not real work. Hoisting the frame window out of the loop
    // head (re-derived only on call/return) is what this measures.
    let mut disp = c.benchmark_group("vm_dispatch");
    for &n in LOOP_ITERS {
        let module = compile(&loop_sum_src(n));
        disp.bench_with_input(BenchmarkId::new("loop_sum", n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        // P-VMT-GSLOT: the same loop with top-level (global) accumulators — measures the
        // per-iteration global-access cost slot indexing removed.
        let g = compile(&global_loop_src(n));
        disp.bench_with_input(BenchmarkId::new("global_loop", n), &g, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    disp.finish();

    // S5 (P-VMT-STR): interpolation throughput — one `BuildString` vs the old N-way concat fold.
    let mut interp = c.benchmark_group("vm_interp");
    for &n in LOOP_ITERS {
        let single = compile(&interp_single_hole_src(n));
        interp.bench_with_input(BenchmarkId::new("single_hole", n), &single, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
        let multi = compile(&interp_multi_hole_src(n));
        interp.bench_with_input(BenchmarkId::new("multi_hole", n), &multi, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    interp.finish();

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

    // Enum-method + operator-overload dispatch loops (audit-1 finding 7): the enum inline cache
    // and the alloc-free method-table probe.
    let mut edisp = c.benchmark_group("vm_enum_method");
    for &n in LOOP_SIZES {
        let module = compile(&enum_method_src(n));
        edisp.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    edisp.finish();

    let mut odisp = c.benchmark_group("vm_operator_overload");
    for &n in LOOP_SIZES {
        let module = compile(&operator_overload_src(n));
        odisp.bench_with_input(BenchmarkId::from_parameter(n), &module, |b, module| {
            b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
        });
    }
    odisp.finish();

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
    use noeta_value::CollectorMode::{Trace, TrialDeletion};
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

    // J1 (P-JIT): the integer fast path. A register-local integer `while` loop, run through the
    // interpreter (tier 0) and the forced JIT (tier 1) so the native speedup is directly visible.
    #[cfg(feature = "jit")]
    {
        let mut jit = c.benchmark_group("vm_jit");
        for &n in LOOP_ITERS {
            let module = compile(&jit_loop_src(n));
            jit.bench_with_input(BenchmarkId::new("loop_interp", n), &module, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
            });
            jit.bench_with_input(BenchmarkId::new("loop_jit", n), &module, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
            });
            let bmodule = compile(&jit_bitwise_loop_src(n));
            jit.bench_with_input(
                BenchmarkId::new("bitwise_interp", n),
                &bmodule,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(BenchmarkId::new("bitwise_jit", n), &bmodule, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
            });
            let mmodule = compile(&jit_mixed_loop_src(n));
            jit.bench_with_input(
                BenchmarkId::new("mixed_interp", n),
                &mmodule,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(BenchmarkId::new("mixed_jit", n), &mmodule, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
            });
            let fmodule = compile(&jit_float_loop_src(n));
            jit.bench_with_input(
                BenchmarkId::new("float_interp", n),
                &fmodule,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(BenchmarkId::new("float_jit", n), &fmodule, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
            });
            // Native globals: the same arithmetic loop but **top-level** (global `i`/`total`) — the
            // scripting shape. Per-op bail compiles proto 0 (LoadGlobal/StoreGlobal inlined), bailing
            // only at the trailing `echo`.
            let gmodule = compile(&global_loop_src(n));
            jit.bench_with_input(
                BenchmarkId::new("global_interp", n),
                &gmodule,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(BenchmarkId::new("global_jit", n), &gmodule, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
            });
            // J4 (heap/collections): a `for i in 0..n` range loop — its MakeRange/IterSnapshot/
            // ListLen/ListGet internals now run through the leaf-op helper, so the loop is native.
            let rmodule = compile(&loop_sum_src(n));
            jit.bench_with_input(
                BenchmarkId::new("forrange_interp", n),
                &rmodule,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(
                BenchmarkId::new("forrange_jit", n),
                &rmodule,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
                },
            );
            // J4 slice 2 (field access): a loop reading/writing `struct` fields — LoadField/SetField
            // run through the leaf-op helper, keeping the surrounding loop native.
            let field_module = compile(&field_loop_src(n));
            jit.bench_with_input(
                BenchmarkId::new("field_interp", n),
                &field_module,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(
                BenchmarkId::new("field_jit", n),
                &field_module,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
                },
            );
            // P-JIT field-read floor: a read-only loop over a wide struct's last field — the JIT stays
            // below the tier-0 interpreter here (per-LoadField leaf-helper-call cost), the floor a
            // future call-free native field read would cross.
            let wide_module = compile(&wide_field_read_src(n));
            jit.bench_with_input(
                BenchmarkId::new("widefield_interp", n),
                &wide_module,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(
                BenchmarkId::new("widefield_jit", n),
                &wide_module,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
                },
            );
            // J4 slice 3 (indexing): a loop indexing a list and a map — Op::Index runs through the
            // leaf-op helper, keeping the surrounding loop native.
            let index_module = compile(&index_loop_src(n));
            jit.bench_with_input(
                BenchmarkId::new("index_interp", n),
                &index_module,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
                },
            );
            jit.bench_with_input(
                BenchmarkId::new("index_jit", n),
                &index_module,
                |b, module| {
                    b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
                },
            );
        }
        // Native calls (J3): recursive `fib`, interpreter vs forced JIT. Each frame's pre-call region
        // and both recursive subtrees run native (the callee enters at pc 0); the caller's tail after
        // its first call resumes in tier 0.
        for &d in FIB_DEPTHS {
            let module = compile(&fib_src(d));
            jit.bench_with_input(BenchmarkId::new("fib_interp", d), &module, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
            });
            jit.bench_with_input(BenchmarkId::new("fib_jit", d), &module, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit(black_box(module))));
            });
        }
        // OSR (J5): the same top-level global loop, but through **ordinary hot-counter promotion**
        // (`run_module_jit_hot`, not forced) — the real production path. Before OSR, `main` (entered
        // once) never crossed the entry threshold, so this loop ran entirely in tier 0 in production;
        // OSR counts its back-edges and jumps into native code mid-flight. Benched interp vs hot-JIT
        // so the win from the promotion actually reaching the loop is visible.
        for &n in LOOP_ITERS {
            let gmodule = compile(&global_loop_src(n));
            jit.bench_with_input(BenchmarkId::new("osr_interp", n), &gmodule, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module(black_box(module))));
            });
            jit.bench_with_input(BenchmarkId::new("osr_hot", n), &gmodule, |b, module| {
                b.iter(|| black_box(VmBackend::new().run_module_jit_hot(black_box(module))));
            });
        }
        jit.finish();
    }
}

criterion_group!(benches, vm_hot_paths);
criterion_main!(benches);
