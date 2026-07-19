//! **Bounded-peak-residency regression** for in-run safepoint cycle collection
//! (memory-management 6.x).
//!
//! A program building reference cycles in a loop used to grow its heap without bound until the
//! exit-time reapers ran. With safepoint collection armed, peak residency must stay bounded — on
//! **both** backends, each measured with its own live-aggregate meter (`noeta_value::live_peak`
//! for the VM heap, `noeta_eval::live_peak` for the tree-walker's counted aggregates). Each case
//! also runs the same program with the trigger disarmed and asserts the peak then grows with the
//! iteration count — proving the bound comes from the safepoint collections, not the program.
//!
//! The differential/leak oracles separately prove observable behavior and exit residency are
//! unchanged; this test is the *bounding* proof.

use noeta_conformance::reference::reference_run;
use noeta_vm::{RunOptions, VmBackend};

/// Iterations each cycle-building loop runs. Every iteration strands at least one reference
/// cycle, so the disarmed peak grows past `ITERS` while the armed peak stays near the threshold.
const ITERS: usize = 3_000;

/// The safepoint step the armed runs pin: small enough that collections fire many times over
/// `ITERS` iterations, large enough to stay out of the noise floor.
const STEP: usize = 256;

/// The armed-peak ceiling: generous headroom over `STEP` (the geometric re-arm allows up to
/// ~`2·live + step` growth between collections plus per-iteration transients), while staying far
/// below the disarmed peak (≥ `ITERS`).
const BOUND: i64 = 2_000;

/// A loop stranding one closure↔cell capture cycle per iteration (the self-recursive nested
/// `fn`), plus a class two-cycle per iteration — the two cycle shapes the language can tie.
fn cycle_loop_program() -> String {
    format!(
        "class Node {{\n\
         \x20   pub mut next: ?Node\n\
         \x20   fn new(): Node {{ return Node {{ next: none }} }}\n\
         }}\n\
         mut i = 0\n\
         while i < {ITERS} {{\n\
         \x20   fn rec(n: int): int {{\n\
         \x20       if n <= 0 {{ return 0 }}\n\
         \x20       return rec(n - 1)\n\
         \x20   }}\n\
         \x20   a = Node.new()\n\
         \x20   b = Node.new()\n\
         \x20   a.next = some(b)\n\
         \x20   b.next = some(a)\n\
         \x20   i = i + 1\n\
         }}\n\
         echo i\n"
    )
}

/// Parse + check the program through the same salsa graph the differential drives, handing back
/// what each backend needs.
fn build(
    text: &str,
) -> (
    noeta_ast::Program,
    noeta_check::Sites,
    noeta_bytecode::Module,
) {
    noeta_conformance::ensure_std_registry();
    let db = noeta_db::LangDatabase::default();
    let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "cycle_loop.noe", text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let parsed = noeta_db::ast(&db, src);
    assert!(parsed.0.diagnostics.is_empty(), "program parses");
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked.diagnostics.is_empty(),
        "program checks: {:?}",
        checked.diagnostics
    );
    let module = noeta_db::bytecode(&db, src)
        .0
        .clone()
        .expect("program compiles");
    (parsed.0.program.clone(), checked.sites.clone(), module)
}

/// Run the program on the VM with the given safepoint step, returning the peak live-object delta.
fn vm_peak(module: &noeta_bytecode::Module, step: usize) -> i64 {
    let baseline = noeta_value::live_count() as i64;
    noeta_value::reset_peak();
    let result = VmBackend::new()
        .run_module_with(
            module,
            RunOptions {
                gc_threshold: Some(step),
                ..RunOptions::default()
            },
        )
        .result;
    assert_eq!(result.exit_code, 0, "clean exit: {:?}", result.diagnostics);
    assert_eq!(result.stdout, format!("{ITERS}\n"));
    let peak = noeta_value::live_peak() as i64 - baseline;
    assert_eq!(
        noeta_value::live_count() as i64,
        baseline,
        "exit residency unchanged"
    );
    peak
}

/// Run the program on the reference interpreter with the given safepoint step override, returning
/// the peak live-aggregate delta.
fn eval_peak(program: &noeta_ast::Program, sites: &noeta_check::Sites, step: Option<i64>) -> i64 {
    noeta_eval::set_safepoint_threshold(step);
    let baseline = noeta_eval::live_count();
    noeta_eval::reset_peak();
    let result = reference_run(program, sites.clone());
    noeta_eval::set_safepoint_threshold(None);
    assert_eq!(result.exit_code, 0, "clean exit: {:?}", result.diagnostics);
    assert_eq!(result.stdout, format!("{ITERS}\n"));
    let peak = noeta_eval::live_peak() - baseline;
    assert_eq!(
        noeta_eval::live_count(),
        baseline,
        "exit residency unchanged"
    );
    peak
}

#[test]
fn vm_peak_residency_is_bounded_by_safepoint_collection() {
    let (_, _, module) = build(&cycle_loop_program());
    let armed = vm_peak(&module, STEP);
    // Disarmed control: `usize::MAX` saturates the watermark, so no mid-run collection ever runs
    // and the stranded cycles accumulate until exit.
    let disarmed = vm_peak(&module, usize::MAX);
    assert!(
        disarmed > ITERS as i64,
        "control run must grow with the loop (peak {disarmed}, iters {ITERS})"
    );
    eprintln!("vm: armed peak {armed}, disarmed peak {disarmed}");
    assert!(
        armed < BOUND,
        "armed peak must stay bounded: {armed} (bound {BOUND}, disarmed control {disarmed})"
    );
}

#[test]
fn eval_peak_residency_is_bounded_by_safepoint_collection() {
    let (program, sites, _) = build(&cycle_loop_program());
    let armed = eval_peak(&program, &sites, Some(STEP as i64));
    let disarmed = eval_peak(&program, &sites, Some(i64::MAX));
    assert!(
        disarmed > ITERS as i64,
        "control run must grow with the loop (peak {disarmed}, iters {ITERS})"
    );
    eprintln!("eval: armed peak {armed}, disarmed peak {disarmed}");
    assert!(
        armed < BOUND,
        "armed peak must stay bounded: {armed} (bound {BOUND}, disarmed control {disarmed})"
    );
}
