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

use noeta_alloc_probe::{TrackingAlloc, peak_during};
use noeta_bytecode::Module;
use noeta_eval::IrRefBackend;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

/// The tracking allocator must be registered as this test binary's global allocator for
/// [`peak_during`] to see any allocations.
#[global_allocator]
static GLOBAL: TrackingAlloc = TrackingAlloc(std::alloc::System);

/// The allocator's peak counter is process-wide, so two peak tests running on parallel test threads
/// would corrupt each other's high-water measurement. Each test holds this lock for its whole body
/// (compilation allocates too), serializing the measurements. Poison is ignored — a panic in one
/// test must not wedge the rest.
static PEAK_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    PEAK_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

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

fn parse_program(src: &str) -> noeta_ast::Program {
    noeta_conformance::ensure_std_registry();
    let source = Source::new(SourceId::FIRST, "peak.noe", src);
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
    noeta_compiler::compile(&parse_program(src))
        .expect("measurement program must be in the VM subset")
}

/// Lower + run a program through the Core-IR interpreter, but with **all compilation done outside**
/// the returned closure — only `run_ir` (pure execution) is measured. The VM side is naturally
/// execution-only (`run_module` takes a pre-built `Module`), so this keeps the two backends'
/// residency figures apples-to-apples: we weigh the *list's* heap, not the cost of compiling an
/// 8000-element literal (which on the eval side would otherwise dominate and hide the win).
fn eval_runner(program: noeta_ast::Program) -> impl FnOnce() -> noeta_backend::RunResult {
    let checked = noeta_check::check_all(&program);
    let ir = noeta_ir::lower_with_sites(&program, noeta_ir::lowering_sites!(checked.sites))
        .expect("Core-IR lowering is total over the parsed language");
    let relevance = noeta_ir_passes::Relevance {
        locals: checked.sites.destructor_relevance.locals.clone(),
        params: checked.sites.destructor_relevance.params.clone(),
    };
    let ir = noeta_ir_passes::insert_drops(&ir, Some(&relevance));
    let ir = noeta_ir_passes::thread_reuse(&ir, &std::collections::HashSet::new());
    let packed_type_layouts = checked
        .sites
        .packed_type_layouts
        .iter()
        .map(|l| (l.type_name.clone(), l.clone()))
        .collect();
    let sites = checked.sites.type_of_sites;
    let deserialize_recipes = checked.sites.deserialize_recipes.iter().cloned().collect();
    move || {
        IrRefBackend::new().run_ir(
            &program,
            &ir,
            sites,
            deserialize_recipes,
            packed_type_layouts,
        )
    }
}

/// Build an `n`-element packed/boxed `Vec3` list, run it through a flat-preserving **producer**
/// (`reverse`/`slice`/`filter`), hold the *result* (sum a field over it), and keep the input alive
/// too. With `@packed` and a producer that stays flat (P-PACK 2.6, VM) both the input and the result
/// are flat `Vec<u64>` buffers; if the producer silently demoted, the result would balloon to `n`
/// boxed objects and the residency would jump toward the boxed figure — which the test's ratio guard
/// then catches. `op` is a `.noe` expression over `data` producing the result list.
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
         for i in 0..result.len() {{\n    \
            sum = sum + result[i].x\n\
         }}\n\
         echo data.len()\n\
         echo sum\n"
    )
}

#[test]
fn packed_producers_keep_the_list_flat() {
    let _serial = serial();
    // VM-only this slice (eval's list dispatch materializes its receiver, so its producers still
    // demote — a deliberate, RunResult-invisible asymmetry). Each producer holds its result *and* the
    // input; if it stayed flat the VM peak is two flat buffers, well under half the boxed peak. A
    // silent demote of the result would push the packed peak toward boxed and trip the `* 2 <` guard.
    const N: usize = 2_000;
    let cases = [
        ("reverse", "data.reverse()"),
        ("slice", "data.slice(0, data.len())"),
        ("filter", "data.filter(fn(v) => v.x > 0.0)"),
        ("set", "data.set(0, Vec3 { x: 9.0, y: 9.0, z: 9.0 })"),
        // `map` to a packed struct keeps the result flat too (P-PACK 2.6 category B): each mapped
        // element is packed straight into the buffer, so only one input + one output element are live
        // at a time and the held result is a flat buffer. (Method form since P1.2 — the free
        // `map`/`filter` left the prelude; the methods route to the same builtin impls.)
        (
            "map",
            "data.map(fn(v) => Vec3 { x: v.x + 1.0, y: v.y, z: v.z })",
        ),
        // `concat` (`~`) also stays flat (see `packed_set_concat.noe` + the `packed_concat` /
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
        // A demotion puts packed ≈ boxed (ratio ~1×); a healthy flat producer sits near 2×. The
        // guard threshold is 1.8× rather than a clean 2×: extern-types X3 removed the inline
        // `Payload::FileHandle` variant (80 B), shrinking EVERY boxed heap object's payload from
        // 88 to 56 bytes — the boxed baseline improved, so packed's *relative* margin narrowed
        // (slice measured 1.97× after) while its absolute footprint is unchanged.
        assert!(
            packed_peak * 9 < boxed_peak * 5,
            "{label}: packed peak ({packed_peak} B) should be < boxed/1.8 ({boxed_peak} B) — \
             did the producer silently demote the packed list to boxed?"
        );
    }
}

/// Build + hold an `n`-element list of `Vec3` whose fields are `f32` (4 bytes each) or `float`
/// (f64, 8 bytes), then sum a field. With the VM's byte-addressed packed buffer (P-PACK 3.2b) the
/// f32 list is ~half the float list's residency — the density win of the narrowed slot.
fn vec3_typed_src(n: usize, f32_fields: bool) -> String {
    let ty = if f32_fields { "f32" } else { "float" };
    let lit = if f32_fields { "1.0f32" } else { "1.0" };
    let mut elems = String::with_capacity(n * 40);
    for i in 0..n {
        if i > 0 {
            elems.push_str(", ");
        }
        elems.push_str(&format!("Vec3 {{ x: {lit}, y: {lit}, z: {lit} }}"));
    }
    format!(
        "@packed struct Vec3 {{ x: {ty}; y: {ty}; z: {ty} }}\n\
         data = [{elems}]\n\
         mut sum = 0.0{}\n\
         for i in 0..{n} {{\n    sum = sum + data[i].x\n}}\n\
         echo data.len()\n",
        if f32_fields { "f32" } else { "" }
    )
}

/// Build + hold an `n`-element list of an 8-field `@packed` struct whose fields are all `bool` (1
/// byte each) or all `int` (8 bytes). With the byte-addressed buffer, the bool struct is 8 bytes/
/// element vs 64 — so the bool list saves 7 bytes/field.
fn flags8_src(n: usize, bool_fields: bool) -> String {
    let ty = if bool_fields { "bool" } else { "int" };
    let lit = if bool_fields { "true" } else { "0" };
    let decl: Vec<String> = (0..8).map(|i| format!("f{i}: {ty}")).collect();
    let init: Vec<String> = (0..8).map(|i| format!("f{i}: {lit}")).collect();
    let one = format!("F8 {{ {} }}", init.join(", "));
    let elems = vec![one; n].join(", ");
    format!(
        "@packed struct F8 {{ {} }}\n\
         data = [{elems}]\n\
         echo data.len()\n",
        decl.join("; ")
    )
}

#[test]
fn packed_bool_field_is_one_byte() {
    let _serial = serial();
    // P-PACK bool narrowing: a `bool` packed field is 1 byte, an `int` 8 — so an 8-`bool` element is
    // 8 bytes vs an 8-`int` element's 64. The held bool list's peak is far under the int list's; the
    // delta is ~7 bytes/field × 8 fields × N, proving each bool is 1 byte. Measured on the VM.
    const N: usize = 4_000;
    let vm_bool = compile_program(&flags8_src(N, true));
    let vm_int = compile_program(&flags8_src(N, false));
    let (r_a, bool_peak) = peak_during(|| VmBackend::new().run_module(&vm_bool));
    let (r_b, int_peak) = peak_during(|| VmBackend::new().run_module(&vm_int));
    assert_eq!(r_a.stdout, "4000\n");
    assert_eq!(r_b.stdout, "4000\n");
    let kib = |b: usize| b as f64 / 1024.0;
    let saved = int_peak.saturating_sub(bool_peak);
    println!(
        "\nP-PACK bool narrowing, List<F8> n={N}: bool {:.1} KiB vs int {:.1} KiB (saved {:.1} KiB)",
        kib(bool_peak),
        kib(int_peak),
        kib(saved)
    );
    let expected = N * 8 * 7; // 8 fields, 7 bytes narrower each (8→1)
    assert!(
        saved as f64 >= expected as f64 * 0.9,
        "bool narrowing should save ~{expected} B (8 fields × 7 B × {N}); saved only {saved} B \
         (bool {bool_peak} vs int {int_peak})"
    );
}

#[test]
fn packed_f32_list_is_roughly_half_of_float() {
    let _serial = serial();
    // Byte-addressed narrowing (P-PACK 3.2b VM + the eval follow-up): an f32 `Vec3` is 12 bytes/
    // element, a float `Vec3` is 24 — so the held f32 packed list saves the narrowed bytes vs the
    // float one. Both backends are byte-addressed now, so both are measured.
    const N: usize = 4_000;
    let kib = |b: usize| b as f64 / 1024.0;
    let expected = N * 3 * 4; // 3 fields, 4 bytes narrower each

    // Each closure precompiles/lowers outside the measured region (see `compile_program`/`eval_runner`).
    let vm_f32 = compile_program(&vec3_typed_src(N, true));
    let vm_float = compile_program(&vec3_typed_src(N, false));
    let (r_a, vm_f32_peak) = peak_during(|| VmBackend::new().run_module(&vm_f32));
    let (r_b, vm_float_peak) = peak_during(|| VmBackend::new().run_module(&vm_float));
    assert_eq!(r_a.stdout, "4000\n");
    assert_eq!(r_b.stdout, "4000\n");

    let (r_c, ev_f32_peak) = peak_during(eval_runner(parse_program(&vec3_typed_src(N, true))));
    let (r_d, ev_float_peak) = peak_during(eval_runner(parse_program(&vec3_typed_src(N, false))));
    assert_eq!(r_c.stdout, "4000\n");
    assert_eq!(r_d.stdout, "4000\n");

    println!("\nP-PACK 3.2b f32 narrowing, List<Vec3> n={N}");
    println!(
        "  vm    f32 {:>8.1} KiB  float {:>8.1} KiB  (saved {:.1} KiB)",
        kib(vm_f32_peak),
        kib(vm_float_peak),
        kib(vm_float_peak - vm_f32_peak)
    );
    println!(
        "  eval  f32 {:>8.1} KiB  float {:>8.1} KiB  (saved {:.1} KiB)",
        kib(ev_f32_peak),
        kib(ev_float_peak),
        kib(ev_float_peak - ev_f32_peak)
    );
    // On each backend the f32 buffer is exactly half the float buffer (12 vs 24 bytes/element); the
    // rest of the peak is fixed run overhead, so assert on the *delta* — it must be ~the narrowed
    // bytes (3 fields × 4 B × N = 48 KiB), proving the f32 slot is 4 bytes, not 8, on *both* backends.
    for (label, f32_peak, float_peak) in [
        ("vm", vm_f32_peak, vm_float_peak),
        ("eval", ev_f32_peak, ev_float_peak),
    ] {
        let saved = float_peak.saturating_sub(f32_peak);
        assert!(
            saved as f64 >= expected as f64 * 0.9,
            "{label}: f32 narrowing should save ~{expected} B (3 fields × 4 B × {N}); saved only \
             {saved} B (f32 {f32_peak} vs float {float_peak})"
        );
    }
}

#[test]
fn packed_list_peak_residency_is_a_fraction_of_boxed() {
    let _serial = serial();
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
