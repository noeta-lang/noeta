//! Hot-swap oracle + behavior pins (server-hmr H0).
//!
//! The spine is the **swap differential**: for a program without retained state, hot-swapping
//! v1 → v2 into a live session and then probing must be observationally identical to cold-starting
//! v2 and probing — byte-equal stdout and echoed values. This makes the differ and the rebinding
//! path refactorable rather than risky.
//!
//! Around it, behavior pins for the semantics the differential deliberately can't see:
//! restart-blocking verdicts, removed definitions staying live for in-flight callers, and the one
//! intended divergence — a function *value* captured before the swap keeps the old body.

use noeta_ast::Program;
use noeta_compiler::hotswap::{SwapBlocker, SwapDiff, SwapPlan, diff_programs};
use noeta_span::{Source, SourceId};
use noeta_vm::VmSession;

fn parse(src: &str) -> Program {
    let source = Source::new(SourceId::FIRST, "<hotswap>", src);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "test source should parse cleanly: {src:?}\n{:?}\n{:?}",
        lexed.diagnostics,
        parsed.diagnostics
    );
    parsed.program
}

fn factory() -> noeta_vm::HostFactory {
    Box::new(|| {
        (
            Box::new(noeta_stdlib::SandboxHost::new()),
            Box::new(noeta_stdlib::SandboxExecutor::new()),
        )
    })
}

/// Launch `src` the way the CLI launch path does: checked compile, session adopted from it,
/// entry 0 run to completion.
fn boot(src: &str) -> VmSession {
    let program = parse(src);
    let checked = noeta_check::check_all(&program);
    assert!(
        checked.diagnostics.is_empty(),
        "boot source should check cleanly: {:?}",
        checked.diagnostics
    );
    let (module, compiler) =
        noeta_compiler::compile_with_sites_session(&program, checked.sites, false, true)
            .expect("a checked program compiles");
    let (session, out) = VmSession::adopted(&module, compiler, factory());
    assert!(out.trace.is_empty(), "boot must not panic: {:?}", out.trace);
    session
}

fn verdict(old_src: &str, new_src: &str) -> SwapDiff {
    diff_programs(&parse(old_src), old_src, &parse(new_src), new_src)
}

/// The full driver dance: gate on the NEW version's check (transactional), require a swappable
/// verdict, apply it. Returns the plan for assertions on its bookkeeping.
fn apply(session: &mut VmSession, old_src: &str, new_src: &str) -> SwapPlan {
    let checked = noeta_check::check_all(&parse(new_src));
    assert!(
        checked.diagnostics.is_empty(),
        "the new version must check green before a swap: {:?}",
        checked.diagnostics
    );
    match verdict(old_src, new_src) {
        SwapDiff::Swap(plan) => {
            let out = session.hot_swap(&plan);
            assert!(
                out.diagnostics.is_empty() && out.trace.is_empty(),
                "the swap fragment must run cleanly: {:?} {:?}",
                out.diagnostics,
                out.trace
            );
            plan
        }
        other => panic!("expected a swappable diff, got {other:?}"),
    }
}

fn probe(session: &mut VmSession, src: &str) -> String {
    let out = session.eval(&parse(src));
    assert!(
        out.trace.is_empty(),
        "probe must not panic: {:?}",
        out.trace
    );
    out.stdout
}

/// The swap differential: swap(v1→v2) + probe ≡ cold-start(v2) + probe.
fn oracle(v1: &str, v2: &str, probe_src: &str) {
    let mut swapped = boot(v1);
    apply(&mut swapped, v1, v2);
    let via_swap = probe(&mut swapped, probe_src);
    swapped.teardown();

    let mut cold = boot(v2);
    let via_cold = probe(&mut cold, probe_src);
    cold.teardown();

    assert_eq!(
        via_swap, via_cold,
        "hot-swap must be observationally identical to a cold start of the new version"
    );
}

// ---------------------------------------------------------------- the differential corpus

#[test]
fn a_changed_fn_body_swaps() {
    oracle(
        "fn version(): string { return \"v1\"; }\n",
        "fn version(): string { return \"v2\"; }\n",
        "echo version();",
    );
}

#[test]
fn an_unchanged_caller_dispatches_to_the_swapped_callee() {
    // THE HMR property: `describe` is byte-identical across versions (never recompiled), yet its
    // `Op::CallGlobal` picks up the new `version` closure from the rebound slot.
    oracle(
        "fn version(): string { return \"v1\"; }\n\
         fn describe(): string { return \"running ${version()}\"; }\n",
        "fn version(): string { return \"v2\"; }\n\
         fn describe(): string { return \"running ${version()}\"; }\n",
        "echo describe();",
    );
}

#[test]
fn a_swapped_recursive_fn_recurses_into_its_new_body() {
    oracle(
        "fn total(n: int): int { if n <= 0 { return 0; } return n + total(n - 1); }\n",
        "fn total(n: int): int { if n <= 0 { return 100; } return n + total(n - 1); }\n",
        "echo total(3);",
    );
}

#[test]
fn an_added_fn_is_callable_from_a_swapped_body() {
    oracle(
        "fn calc(): int { return 1; }\n",
        "fn helper(): int { return 41; }\n\
         fn calc(): int { return helper() + 1; }\n",
        "echo calc();",
    );
}

#[test]
fn an_added_import_binds_for_a_swapped_body() {
    oracle(
        "fn root(x: float): float { return x; }\n",
        "use std.math.{sqrt}\n\
         fn root(x: float): float { return sqrt(x); }\n",
        "echo root(9.0);",
    );
}

#[test]
fn a_method_body_swap_reaches_instances_created_before_the_swap() {
    // The instance is constructed at boot (under the OLD class declaration on the swapped path,
    // the NEW one on the cold path); layout is unchanged, so both sides hold the same
    // content-interned shape and dispatch by name into the new body.
    let v1 = "class Counter {\n\
              n: int\n\
              fn new(): Counter { return Counter { n: 7 }; }\n\
              fn describe(): string { return \"v1 ${self.n}\"; }\n\
              }\n\
              mut c = Counter.new()\n";
    let v2 = "class Counter {\n\
              n: int\n\
              fn new(): Counter { return Counter { n: 7 }; }\n\
              fn describe(): string { return \"v2 ${self.n}\"; }\n\
              }\n\
              mut c = Counter.new()\n";
    oracle(v1, v2, "echo c.describe();");
}

// ---------------------------------------------------------------- differ verdicts

#[test]
fn an_identical_program_is_unchanged() {
    let src = "fn f(): int { return 1; }\necho f()\n";
    assert!(matches!(verdict(src, src), SwapDiff::Unchanged));
}

#[test]
fn a_formatting_only_edit_is_unchanged() {
    // Whitespace between declarations moves every span but no definition's text.
    let v1 = "fn a(): int { return 1; }\nfn b(): int { return 2; }\n";
    let v2 = "fn a(): int { return 1; }\n\n\nfn b(): int { return 2; }\n";
    assert!(matches!(verdict(v1, v2), SwapDiff::Unchanged));
}

#[test]
fn a_signature_change_blocks() {
    let v1 = "fn f(n: int): int { return n; }\n";
    let v2 = "fn f(n: int, m: int): int { return n + m; }\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a signature change must block");
    };
    assert_eq!(
        blockers,
        vec![SwapBlocker::SignatureChanged { name: "f".into() }]
    );
}

#[test]
fn a_field_layout_change_blocks() {
    let v1 = "struct P { x: int }\n";
    let v2 = "struct P { x: int; y: int }\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a layout change must block");
    };
    assert_eq!(
        blockers,
        vec![SwapBlocker::LayoutChanged {
            type_name: "P".into()
        }]
    );
}

#[test]
fn a_changed_top_level_statement_blocks() {
    let v1 = "fn f(): int { return 1; }\necho f()\n";
    let v2 = "fn f(): int { return 1; }\necho f() + 1\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a top-level statement change must block");
    };
    assert!(matches!(blockers[0], SwapBlocker::TopLevelChanged { .. }));
}

#[test]
fn a_removed_type_blocks() {
    let v1 = "struct P { x: int }\nfn f(): int { return 1; }\n";
    let v2 = "fn f(): int { return 1; }\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a removed type must block");
    };
    assert_eq!(
        blockers,
        vec![SwapBlocker::TypeRemoved {
            type_name: "P".into()
        }]
    );
}

#[test]
fn a_method_signature_change_blocks_qualified() {
    let v1 = "class C {\nn: int\nfn get(): int { return self.n; }\n}\n";
    let v2 = "class C {\nn: int\nfn get(d: int): int { return self.n + d; }\n}\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a method signature change must block");
    };
    assert_eq!(
        blockers,
        vec![SwapBlocker::SignatureChanged {
            name: "C.get".into()
        }]
    );
}

#[test]
fn the_plan_records_changed_added_and_removed() {
    let v1 = "fn stays(): int { return 1; }\n\
              fn edited(): int { return 1; }\n\
              fn goes(): int { return 1; }\n";
    let v2 = "fn stays(): int { return 1; }\n\
              fn edited(): int { return 2; }\n\
              fn fresh(): int { return 3; }\n";
    let SwapDiff::Swap(plan) = verdict(v1, v2) else {
        panic!("body-level changes must produce a plan");
    };
    assert_eq!(plan.changed, vec!["edited".to_string()]);
    assert_eq!(plan.added, vec!["fresh".to_string()]);
    assert_eq!(plan.removed, vec!["goes".to_string()]);
    assert_eq!(
        plan.fragment.stmts.len(),
        2,
        "only edited + fresh are re-evaluated"
    );
}

// ---------------------------------------------------------------- semantic pins

#[test]
fn a_removed_fn_stays_live_for_old_callers() {
    let v1 = "fn gone(): int { return 7; }\nfn keep(): int { return gone(); }\n";
    let v2 = "fn keep(): int { return 1; }\n";
    let mut session = boot(v1);
    let plan = apply(&mut session, v1, v2);
    assert_eq!(plan.removed, vec!["gone".to_string()]);
    // The stale global stays bound: an in-flight caller compiled against v1 would still resolve
    // it. (Freshly checked v2 code can no longer name it — the checker never saw it.)
    assert_eq!(probe(&mut session, "echo gone();"), "7\n");
    // And `keep` itself was swapped — it no longer calls `gone`.
    assert_eq!(probe(&mut session, "echo keep();"), "1\n");
    session.teardown();
}

#[test]
fn a_captured_fn_value_keeps_the_old_body() {
    // The one intended divergence from cold-start equivalence: a closure VALUE holds its proto
    // directly, so a capture taken before the swap pins the old behavior; slot-routed calls (the
    // bare `f()` form) rebind. This is the documented semantics, not a bug.
    let v1 = "fn f(): int { return 1; }\n";
    let v2 = "fn f(): int { return 2; }\n";
    let mut session = boot(v1);
    assert_eq!(probe(&mut session, "mut h = f; echo h();"), "1\n");
    apply(&mut session, v1, v2);
    assert_eq!(
        probe(&mut session, "echo f();"),
        "2\n",
        "slot-routed calls rebind"
    );
    assert_eq!(
        probe(&mut session, "echo h();"),
        "1\n",
        "captured values pin the old body"
    );
    session.teardown();
}

#[test]
fn residency_returns_to_baseline_across_repeated_swaps() {
    // The arena/tables grow per swap by design; the LIVE-VALUE residency must not. Ten swap
    // rounds, then teardown returns to the pre-session baseline (the leak-oracle shape).
    let before = noeta_value::live_count();
    let v_even = "fn f(): string { return \"even\"; }\n";
    let v_odd = "fn f(): string { return \"odd\"; }\n";
    let mut session = boot(v_even);
    for round in 0..10 {
        let (from, to) = if round % 2 == 0 {
            (v_even, v_odd)
        } else {
            (v_odd, v_even)
        };
        apply(&mut session, from, to);
    }
    assert_eq!(probe(&mut session, "echo f();"), "even\n");
    session.teardown();
    assert_eq!(
        noeta_value::live_count(),
        before,
        "teardown after swap churn returns residency to baseline"
    );
}
