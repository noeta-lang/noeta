//! **P-PACK Phase 2.5a — peak-residency measurement.**
//!
//! Phases 2.3 (eval) and 2.4 (VM) made a `List<@packed struct>` store its elements as one
//! contiguous raw-primitive `Vec<u64>` instead of N boxed objects behind N pointers. The
//! per-element *time* benchmarks (`eval_packed_list` / `vm_packed_list`) showed scalar access is no
//! faster — pack-at-build + materialize-on-read costs as much as it saves. The real win is **memory
//! density**, which those time benches cannot see. This test measures it directly.
//!
//! A process-wide tracking allocator records the heap high-water mark during a run. We run the exact
//! same Vec3-list workload twice — once with `@packed struct Vec3` (flat buffer) and once with plain
//! `struct Vec3` (boxed) — on **both** backends, and assert the packed peak is a fraction of the
//! boxed peak. This is both a number to report and a regression guard: if a future change silently
//! demotes the literal to boxed, the packed peak jumps to the boxed peak and this test fails.
//!
//! The whole measurement lives in a single `#[test]` so the shared atomic counters are never touched
//! by a concurrent test thread in this (dedicated) test binary.

use lang_alloc_probe::{TrackingAlloc, peak_during};
use lang_bytecode::Module;
use lang_eval::TreeWalkBackend;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};
use lang_vm::VmBackend;

/// The tracking allocator must be registered as this test binary's global allocator for
/// [`peak_during`] to see any allocations.
#[global_allocator]
static GLOBAL: TrackingAlloc = TrackingAlloc;

/// Build + hold an `n`-element list of `Vec3 { x, y, z }`, then sum a field over it. The full list is
/// resident at the moment the loop starts, so the run's peak is dominated by the list's storage:
/// `@packed` ⇒ one `Vec<u64>` of `3n` words; plain ⇒ `n` boxed objects + the outer pointer vec.
fn vec3_list_src(n: usize, packed: bool) -> String {
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
            sum = sum + data[i].x\n\
         }}\n\
         echo sum\n"
    )
}

fn parse_program(src: &str) -> lang_ast::Program {
    let source = Source::new(SourceId::FIRST, "peak.lang", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "measurement program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    parsed.program
}

fn compile_program(src: &str) -> Module {
    lang_compiler::compile(&parse_program(src))
        .expect("measurement program must be in the VM subset")
}

/// Lower + run a program through the Core-IR interpreter, but with **all compilation done outside**
/// the returned closure — only `run_ir` (pure execution) is measured. The VM side is naturally
/// execution-only (`run_module` takes a pre-built `Module`), so this keeps the two backends'
/// residency figures apples-to-apples: we weigh the *list's* heap, not the cost of compiling an
/// 8000-element literal (which on the eval side would otherwise dominate and hide the win).
fn eval_runner(program: lang_ast::Program) -> impl FnOnce() -> lang_backend::RunResult {
    let checked = lang_check::check_all(&program);
    let ir = lang_ir::lower_with_sites(
        &program,
        &checked.packed_list_sites,
        &checked.index_field_sites,
    )
    .expect("Core-IR lowering is total over the parsed language");
    let relevance = lang_ir_passes::Relevance {
        locals: checked.destructor_relevance.locals.clone(),
        params: checked.destructor_relevance.params.clone(),
    };
    let ir = lang_ir_passes::insert_drops(&ir, Some(&relevance));
    let ir = lang_ir_passes::thread_reuse(&ir);
    let sites = checked.type_of_sites;
    move || TreeWalkBackend::new().run_ir(&program, &ir, sites)
}

/// Build an `n`-element packed/boxed `Vec3` list, run it through a flat-preserving **producer**
/// (`reverse`/`slice`/`filter`), hold the *result* (sum a field over it), and keep the input alive
/// too. With `@packed` and a producer that stays flat (P-PACK 2.6, VM) both the input and the result
/// are flat `Vec<u64>` buffers; if the producer silently demoted, the result would balloon to `n`
/// boxed objects and the residency would jump toward the boxed figure — which the test's ratio guard
/// then catches. `op` is a `.lang` expression over `data` producing the result list.
fn producer_vec3_src(n: usize, packed: bool, op: &str) -> String {
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
         echo data.count()\n\
         echo sum\n"
    )
}

#[test]
fn packed_producers_keep_the_list_flat() {
    // VM-only this slice (eval's list dispatch materializes its receiver, so its producers still
    // demote — a deliberate, RunResult-invisible asymmetry). Each producer holds its result *and* the
    // input; if it stayed flat the VM peak is two flat buffers, well under half the boxed peak. A
    // silent demote of the result would push the packed peak toward boxed and trip the `* 2 <` guard.
    const N: usize = 2_000;
    let cases = [
        ("reverse", "data.reverse()"),
        ("slice", "data.slice(0, data.count())"),
        ("filter", "filter(data, fn(v) => v.x > 0.0)"),
        ("set", "data.set(0, Vec3 { x: 9.0, y: 9.0, z: 9.0 })"),
        // `map` to a packed struct keeps the result flat too (P-PACK 2.6 category B): each mapped
        // element is packed straight into the buffer, so only one input + one output element are live
        // at a time and the held result is a flat buffer.
        (
            "map",
            "map(data, fn(v) => Vec3 { x: v.x + 1.0, y: v.y, z: v.z })",
        ),
        // `concat` (`~`) also stays flat (see `packed_set_concat.lang` + the `packed_concat` /
        // `packed_extend_in_place` miri tests); it is omitted here because its result-size growth and
        // `Vec` capacity doubling make the residency ratio too noisy for this half-of-boxed guard.
    ];
    for (label, op) in cases {
        let vm_packed = compile_program(&producer_vec3_src(N, true, op));
        let vm_boxed = compile_program(&producer_vec3_src(N, false, op));
        let (r_p, packed_peak) = peak_during(|| VmBackend::new().run_module(&vm_packed));
        let (r_b, boxed_peak) = peak_during(|| VmBackend::new().run_module(&vm_boxed));
        assert!(!r_p.stdout.is_empty(), "{label}: packed produced no output");
        assert_eq!(r_p.stdout, r_b.stdout, "{label}: packed vs boxed stdout");
        let kib = |b: usize| b as f64 / 1024.0;
        println!(
            "  vm producer {label:<8} packed {:>8.1} KiB  boxed {:>8.1} KiB  ({:.2}× smaller)",
            kib(packed_peak),
            kib(boxed_peak),
            boxed_peak as f64 / packed_peak.max(1) as f64
        );
        assert!(
            packed_peak * 2 < boxed_peak,
            "{label}: packed peak ({packed_peak} B) should be < half boxed ({boxed_peak} B) — \
             did the producer silently demote the packed list to boxed?"
        );
    }
}

#[test]
fn packed_list_peak_residency_is_a_fraction_of_boxed() {
    // The memory ratio is independent of `n` (both representations scale linearly), so a moderate
    // size shows the win while keeping this test (which runs under a tracking global allocator, so
    // every allocation is instrumented) reasonably quick. A list literal also builds each element
    // into its own register, and `Reg` is a `u16`, capping the literal size well above this.
    const N: usize = 2_000;

    // Parse/compile/lower OUTSIDE the measured region so we weigh only execution residency.
    let eval_packed = eval_runner(parse_program(&vec3_list_src(N, true)));
    let eval_boxed = eval_runner(parse_program(&vec3_list_src(N, false)));
    let vm_packed = compile_program(&vec3_list_src(N, true));
    let vm_boxed = compile_program(&vec3_list_src(N, false));

    let (r_ep, eval_packed_peak) = peak_during(eval_packed);
    let (r_eb, eval_boxed_peak) = peak_during(eval_boxed);
    let (r_vp, vm_packed_peak) = peak_during(|| VmBackend::new().run_module(&vm_packed));
    let (r_vb, vm_boxed_peak) = peak_during(|| VmBackend::new().run_module(&vm_boxed));

    // Layout is invisible to `RunResult`: all four runs must print byte-identical output (and have run
    // at all). This guards against a silently-broken program distorting the memory figure.
    assert!(!r_ep.stdout.is_empty(), "eval packed produced no output");
    assert_eq!(r_ep.stdout, r_eb.stdout, "eval packed vs boxed stdout");
    assert_eq!(r_ep.stdout, r_vp.stdout, "eval vs vm packed stdout");
    assert_eq!(r_vp.stdout, r_vb.stdout, "vm packed vs boxed stdout");

    let kib = |b: usize| b as f64 / 1024.0;
    println!("\nP-PACK peak heap residency, List<Vec3> n={N}");
    println!("  eval   packed {:>9.1} KiB", kib(eval_packed_peak));
    println!("  eval   boxed  {:>9.1} KiB", kib(eval_boxed_peak));
    println!(
        "  eval   ratio  {:>9.2}× smaller",
        eval_boxed_peak as f64 / eval_packed_peak.max(1) as f64
    );
    println!("  vm     packed {:>9.1} KiB", kib(vm_packed_peak));
    println!("  vm     boxed  {:>9.1} KiB", kib(vm_boxed_peak));
    println!(
        "  vm     ratio  {:>9.2}× smaller",
        vm_boxed_peak as f64 / vm_packed_peak.max(1) as f64
    );

    // The flat buffer is ~3n words; the boxed form is n objects + pointers. On the **VM** (register
    // allocation coalesces the streaming accumulator into one slot) the win is clean — packed is well
    // under *half* the boxed peak. On **eval** the win is real but muted: the Core-IR interpreter
    // sizes its activation frame to the raw temp count (no coalescing), and streaming construction
    // emits ~2× the temps (the `acc` push-chain), so a fat `Option<Value>` slot per extra temp offsets
    // part of the list saving. Both must still come out smaller; the VM additionally clears the 2× bar.
    // (Observed n=2000: VM 98 vs 350 KiB ≈ 3.6×; eval 256 vs 458 KiB ≈ 1.8×; ratios hold at any n.)
    assert!(
        eval_packed_peak < eval_boxed_peak,
        "eval packed peak ({eval_packed_peak} B) should be < boxed ({eval_boxed_peak} B) — \
         did the literal silently demote to boxed?"
    );
    assert!(
        vm_packed_peak * 2 < vm_boxed_peak,
        "vm packed peak ({vm_packed_peak} B) should be < half boxed ({vm_boxed_peak} B) — \
         did the literal silently demote to boxed?"
    );
}
