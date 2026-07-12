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
/// verdict, apply it. Returns the plan (for bookkeeping assertions) and the swap entry's output
/// (a re-running swap's re-executed top level lands its stdout here).
fn apply(
    session: &mut VmSession,
    old_src: &str,
    new_src: &str,
) -> (SwapPlan, noeta_vm::SessionOutput) {
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
            (plan, out)
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

/// H3 — the swap lands while a **live force-JIT engine** is armed and `main`'s native frame is on
/// the machine stack (the await suspension is where the scheduler tick applies the pre-deposited
/// plan). Exercises retire→re-arm end to end: the retired engine's pages must stay executable
/// (main returns into them), the mirror tables clear, and the re-armed engine compiles the
/// swapped module — the post-swap call observes the new body, natively.
#[cfg(feature = "jit")]
#[test]
fn a_swap_lands_under_a_live_force_jit_engine() {
    use noeta_vm::{HotChannel, HotSwapMailbox, VmBackend};

    // The tick that applies the swap is `NativeCtx::advance_tasks` — reached from ctx-driven
    // loops (`task.all`, the serve loop), not from a bare top-level await.
    let v = |ret: i64| {
        format!(
            "use std.task.{{sleep, all}}\n\
             fn f(): int {{ return {ret}; }}\n\
             async fn probe(): int {{\n\
             \x20   sleep(1).await\n\
             \x20   return f()\n\
             }}\n\
             echo f()\n\
             results = all([probe()])\n\
             echo results[0]\n"
        )
    };
    let (v1, v2) = (v(1), v(2));
    let program = parse(&v1);
    let checked = noeta_check::check_all(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let (module, compiler) =
        noeta_compiler::compile_with_sites_session(&program, checked.sites, false, false)
            .expect("compiles");

    // Pre-deposit the body swap: the first scheduler tick (inside the await) applies it.
    let SwapDiff::Swap(plan) = verdict(&v1, &v2) else {
        panic!("a body edit must be swappable");
    };
    let mailbox: HotSwapMailbox = std::sync::Arc::new(HotChannel::default());
    mailbox.plans.lock().unwrap().push(plan);

    let (result, trace) = VmBackend::new().run_module_hot_forced_jit(
        &module,
        compiler,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
        mailbox,
    );
    assert!(trace.is_empty(), "no abort under the armed swap: {trace:?}");
    assert_eq!(
        result.stdout, "1\n2\n",
        "pre-swap call sees v1, the post-tick call sees the swapped body — under tier 1"
    );
    assert_eq!(result.exit_code, 0);
}

#[test]
fn a_changed_embedded_packed_struct_blocks_transitively() {
    // H2's transitive claim: `Outer` embeds `Inner` in flat packed storage, so a change to
    // Inner's fields is a layout change for BOTH. The differ blocks on Inner's own declaration —
    // transitivity is free because the embedded type's decl is where the edit lives; the restart
    // covers every container.
    let v1 = "@packed struct Inner { x: int }\n@packed struct Outer { i: Inner; y: int }\n";
    let v2 = "@packed struct Inner { x: int; z: int }\n@packed struct Outer { i: Inner; y: int }\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("an embedded packed layout change must block");
    };
    assert_eq!(
        blockers,
        vec![SwapBlocker::LayoutChanged {
            type_name: "Inner".into()
        }]
    );
}

#[test]
fn an_enum_variant_change_blocks() {
    let v1 = "enum Shape { Dot; Line(int) }\n";
    let v2 = "enum Shape { Dot; Line(int, int) }\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a variant payload change must block");
    };
    assert_eq!(
        blockers,
        vec![SwapBlocker::LayoutChanged {
            type_name: "Shape".into()
        }]
    );
}

#[test]
fn a_field_default_change_blocks() {
    // A per-field default is compiled into construction sites — layout-level, not a body edit.
    let v1 = "struct P { x: int = 1 }\n";
    let v2 = "struct P { x: int = 2 }\n";
    assert!(
        matches!(verdict(v1, v2), SwapDiff::NeedsRestart(_)),
        "a field-default change must block"
    );
}

#[test]
fn a_comment_edit_inside_a_type_does_not_restart() {
    // H2: the residual is compared as TOKENS, so trivia edits between fields — a doc tweak, a
    // reflowed comment — no longer read as layout changes (H0's raw-text residual forced a
    // state-losing restart here).
    let v1 = "struct P {\n    // the x coordinate\n    x: int\n\n    fn get(): int { return self.x; }\n}\n";
    let v2 = "struct P {\n    // the horizontal coordinate\n    x: int\n\n    fn get(): int { return self.x; }\n}\n";
    assert!(
        matches!(verdict(v1, v2), SwapDiff::Unchanged),
        "a comment-only edit is no behavioral change at all"
    );
    // …and with a method edit riding along, it swaps rather than restarts.
    let v3 = "struct P {\n    // the horizontal coordinate\n    x: int\n\n    fn get(): int { return self.x + 0; }\n}\n";
    let SwapDiff::Swap(plan) = verdict(v1, v3) else {
        panic!("comment + method-body edits must swap");
    };
    assert_eq!(plan.changed, vec!["P.get".to_string()]);
}

#[test]
fn a_changed_top_level_statement_makes_a_rerunning_swap() {
    let v1 = "fn f(): int { return 1; }\necho f()\n";
    let v2 = "fn f(): int { return 1; }\necho f() + 1\n";
    let SwapDiff::Swap(plan) = verdict(v1, v2) else {
        panic!("a top-level statement change must produce a re-running swap");
    };
    assert!(plan.rerun_top_level);
    // The changed statement rides in the fragment; `f` is unchanged and does not.
    assert_eq!(plan.fragment.stmts.len(), 1);
}

#[test]
fn a_changed_namespace_blocks() {
    let v1 = "namespace App.One;\nfn f(): int { return 1; }\n";
    let v2 = "namespace App.Two;\nfn f(): int { return 1; }\n";
    let SwapDiff::NeedsRestart(blockers) = verdict(v1, v2) else {
        panic!("a namespace change must block");
    };
    assert!(matches!(blockers[0], SwapBlocker::NamespaceChanged { .. }));
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
    let (plan, _) = apply(&mut session, v1, v2);
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

// ------------------------------------------- H1: re-running swaps & the reactive state rule

#[test]
fn a_rerunning_swap_matches_cold_start_for_stateless_programs() {
    // Both the fn body and the top level changed; the re-run recomputes `r` exactly as a cold
    // start would.
    oracle(
        "fn f(): int { return 1; }\nr = f()\n",
        "fn f(): int { return 2; }\nr = f() + 10\n",
        "echo r;",
    );
}

#[test]
fn plain_bindings_reinitialize_on_a_rerunning_swap() {
    // `v = version()` is byte-identical across versions, but the swap re-runs the top level
    // (because `marker` changed), so `v` re-initializes against the NEW `version` body — plain
    // state behaves as if the program restarted.
    let v1 = "fn version(): string { return \"v1\"; }\nv = version()\nmarker = 1\n";
    let v2 = "fn version(): string { return \"v2\"; }\nv = version()\nmarker = 2\n";
    let mut session = boot(v1);
    let (plan, _) = apply(&mut session, v1, v2);
    assert!(plan.rerun_top_level);
    assert_eq!(probe(&mut session, "echo v;"), "v2\n");
    assert_eq!(probe(&mut session, "echo marker;"), "2\n");
    session.teardown();
}

#[test]
fn signal_state_survives_a_swap_and_the_effect_reruns_with_its_new_body() {
    // THE state rule: `count` (unchanged reactive anchor) is withheld from the re-run — its
    // value survives — while the edited effect is disposed and re-created, firing once with the
    // new body over the preserved value. A post-swap set fires the new effect exactly once (the
    // old one is gone — no duplicate subscription).
    let v1 = "use std.reactive.{signal, effect}\n\
              count = signal(0)\n\
              effect(fn() {\n    echo \"v1:${count.get()}\"\n})\n";
    let v2 = "use std.reactive.{signal, effect}\n\
              count = signal(0)\n\
              effect(fn() {\n    echo \"v2:${count.get()}\"\n})\n";
    let mut session = boot(v1);
    assert_eq!(probe(&mut session, "count.set(5);"), "v1:5\n");
    let (plan, out) = apply(&mut session, v1, v2);
    assert!(plan.rerun_top_level);
    assert_eq!(plan.preserved, vec!["count".to_string()]);
    assert_eq!(
        out.stdout, "v2:5\n",
        "the re-created effect runs once, with the new body, over the PRESERVED signal value"
    );
    assert_eq!(
        probe(&mut session, "count.set(6);"),
        "v2:6\n",
        "exactly one effect fires — the old epoch was disposed"
    );
    assert_eq!(probe(&mut session, "echo count.get();"), "6\n");
    session.teardown();
}

#[test]
fn a_view_rebuilt_by_a_swap_diffs_against_a_fresh_baseline() {
    // The L1∘H1 seam: a `view` is PLAIN state (rebuilt by the re-run) over PRESERVED reactive
    // state. After a swap edits an unrelated binding, the re-run re-creates the view and
    // re-exposes the surviving signal — the fresh baseline means diff() is quiet until a real
    // change, and the first post-swap change diffs against the preserved (not initial) value.
    let v1 = "use std.reactive.{signal, view}\n\
              count = signal(0)\n\
              v = view()\n\
              v.expose(\"count\", count)\n\
              marker = 1\n";
    let v2 = "use std.reactive.{signal, view}\n\
              count = signal(0)\n\
              v = view()\n\
              v.expose(\"count\", count)\n\
              marker = 2\n";
    let mut session = boot(v1);
    assert_eq!(
        probe(&mut session, "count.set(7); echo v.diff() ?? \"none\";"),
        "{\"type\":\"patch\",\"changes\":{\"count\":7}}\n"
    );
    let (plan, _) = apply(&mut session, v1, v2);
    assert_eq!(plan.preserved, vec!["count".to_string()]);
    assert_eq!(
        probe(&mut session, "echo v.diff() ?? \"none\";"),
        "none\n",
        "the rebuilt view baselines at expose — the preserved value is not re-pushed"
    );
    assert_eq!(
        probe(&mut session, "count.set(8); echo v.diff() ?? \"none\";"),
        "{\"type\":\"patch\",\"changes\":{\"count\":8}}\n",
        "post-swap changes flow through the rebuilt view over the preserved signal"
    );
    session.teardown();
}

#[test]
fn a_changed_signal_binding_resets_its_state() {
    // The developer redefined the signal itself — it is NOT preserved: the re-run creates the
    // replacement (initial 100) and the old node is disposed pre-run.
    let v1 = "use std.reactive.{signal}\ncount = signal(0)\n";
    let v2 = "use std.reactive.{signal}\ncount = signal(100)\n";
    let mut session = boot(v1);
    assert_eq!(
        probe(&mut session, "count.set(5); echo count.get();"),
        "5\n"
    );
    let (plan, _) = apply(&mut session, v1, v2);
    assert!(plan.preserved.is_empty());
    assert_eq!(probe(&mut session, "echo count.get();"), "100\n");
    session.teardown();
}

#[test]
fn a_preserved_computed_keeps_deriving_after_a_swap() {
    let v1 = "use std.reactive.{signal, computed}\n\
              count = signal(2)\n\
              double = computed(fn() => count.get() * 2)\n\
              marker = 1\n";
    let v2 = "use std.reactive.{signal, computed}\n\
              count = signal(2)\n\
              double = computed(fn() => count.get() * 2)\n\
              marker = 2\n";
    let mut session = boot(v1);
    assert_eq!(probe(&mut session, "echo double.get();"), "4\n");
    let (plan, _) = apply(&mut session, v1, v2);
    assert_eq!(
        plan.preserved,
        vec!["count".to_string(), "double".to_string()]
    );
    assert_eq!(
        probe(&mut session, "count.set(10); echo double.get();"),
        "20\n",
        "the preserved computed re-derives from the preserved signal after the swap"
    );
    session.teardown();
}

#[test]
fn a_user_disposed_effect_does_not_break_a_rerunning_swap() {
    // The dispose arm prunes the epoch registry, so the swap's disposal pass must not
    // double-release the already-disposed effect's body.
    let v1 = "use std.reactive.{signal, effect}\n\
              count = signal(0)\n\
              e = effect(fn() {\n    echo \"fx:${count.get()}\"\n})\n\
              marker = 1\n";
    let v2 = "use std.reactive.{signal, effect}\n\
              count = signal(0)\n\
              e = effect(fn() {\n    echo \"fx:${count.get()}\"\n})\n\
              marker = 2\n";
    let mut session = boot(v1);
    assert_eq!(probe(&mut session, "e.dispose(); count.set(1);"), "");
    let (_, out) = apply(&mut session, v1, v2);
    assert_eq!(
        out.stdout, "fx:1\n",
        "the re-run re-creates the effect over the preserved signal"
    );
    session.teardown();
}

#[test]
fn residency_returns_to_baseline_across_rerunning_swaps_with_reactivity() {
    // The leak-oracle shape for the H1 disposal paths: swap back and forth between two versions
    // that re-run the top level with signals + effects live, then teardown to baseline.
    let before = noeta_value::live_count();
    let v_a = "use std.reactive.{signal, effect}\n\
               count = signal(0)\n\
               effect(fn() {\n    echo \"a:${count.get()}\"\n})\n\
               marker = 1\n";
    let v_b = "use std.reactive.{signal, effect}\n\
               count = signal(0)\n\
               effect(fn() {\n    echo \"b:${count.get()}\"\n})\n\
               marker = 2\n";
    let mut session = boot(v_a);
    for round in 0..6 {
        let (from, to) = if round % 2 == 0 {
            (v_a, v_b)
        } else {
            (v_b, v_a)
        };
        apply(&mut session, from, to);
    }
    probe(&mut session, "count.set(9);");
    session.teardown();
    assert_eq!(
        noeta_value::live_count(),
        before,
        "teardown after re-running swap churn returns residency to baseline"
    );
}
