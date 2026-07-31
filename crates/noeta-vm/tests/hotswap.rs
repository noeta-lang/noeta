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
    noeta_stdlib::registry::default_seeded();
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
/// verdict, apply it **with that check's whole-program sites** — exactly what the hot watcher
/// deposits (server-hmr H5), so the fragment compiles with the same site-keyed codegen and precise
/// destructor relevance a cold start of the new version gets. Returns the plan (for bookkeeping
/// assertions) and the swap entry's output (a re-running swap's re-executed top level lands its
/// stdout here).
fn apply(
    session: &mut VmSession,
    old_src: &str,
    new_src: &str,
) -> (SwapPlan, noeta_vm::SessionOutput) {
    noeta_stdlib::registry::default_seeded();
    let checked = noeta_check::check_all(&parse(new_src));
    assert!(
        checked.diagnostics.is_empty(),
        "the new version must check green before a swap: {:?}",
        checked.diagnostics
    );
    match verdict(old_src, new_src) {
        SwapDiff::Swap(plan) => {
            let out = session.hot_swap(&plan, Some(&checked.sites));
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

/// A swapped body containing an `@tier` **expression** block (`@html { … }`) must still resolve its
/// handler. The declaration that makes the tier a value (`@tier(html, …, expr: Doc)`) lives outside
/// the fragment — in a real app, inside the imported package — and lowering reads that table off the
/// program in hand, so a fragment lowered alone found no handler and emitted a
/// "`@html` is not an expression tier" panic *where the template should be*: a LiveView page that
/// swapped cleanly and then failed every request. The session carries the table across entries.
#[test]
fn a_swapped_body_keeps_an_expression_tier_from_the_program_that_declared_it() {
    let app = |tag: &str| {
        format!(
            "struct Doc {{ text: string }}\n\
             @tier(html, text: \"html\", expr: Doc)\n\
             fn render(statics: List<string>, holes: List<() -> string>): Doc {{\n\
             \x20   mut out = \"\"\n\
             \x20   for s in statics {{ out = out ~ s }}\n\
             \x20   return Doc {{ text: out }}\n\
             }}\n\
             fn page(): string {{\n\
             \x20   doc = @html {{ <h1>{tag}</h1> }}\n\
             \x20   return doc.text\n\
             }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo page();");
}

/// The sibling of the tier case, for the table built from the program's **`use` statements**: only
/// a *new or changed* import rides in a fragment, so a body swapped under an UNCHANGED import
/// lowered with no alias for it — and `v is Uuid` quietly answered `false` where a cold start
/// answers `true`. A narrowing that silently flips is the worst shape this bug class takes.
#[test]
fn a_swapped_body_narrows_against_an_unchanged_native_import() {
    let app = |tag: &str| {
        format!(
            "use std.id\n\
             use std.id.Uuid\n\
             fn kind(): string {{\n\
             \x20   v = id.uuid()\n\
             \x20   return \"{tag} ${{v is Uuid}}\"\n\
             }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo kind();");
}

/// The same table read the other way: `type_of` on a value whose type came in through an unchanged
/// import must report the qualified identity the reflection artifact registers, not the bare local
/// name the fragment happens to see.
#[test]
fn a_swapped_body_reflects_an_unchanged_native_imports_qualified_name() {
    let app = |tag: &str| {
        format!(
            "use std.id\n\
             use std.id.Uuid\n\
             fn describe(): string {{\n\
             \x20   v = id.uuid()\n\
             \x20   return \"{tag} ${{type_of(v).name()}}\"\n\
             }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo describe();");
}

/// Module globals: the async/generator state-machine desugar needs the program's top-level names so
/// a bare assignment to one stays a global store instead of being hoisted into a state cell. A
/// fragment holds only the changed declarations, so the set was all but empty and a swapped async
/// body that touched a global **panicked** — where the same code cold-starts fine.
#[test]
fn a_swapped_async_body_still_sees_the_module_globals() {
    let app = |tag: &str| {
        format!(
            "mut count = 0\n\
             async fn bump() use (count): int {{\n\
             \x20   count = count + 1\n\
             \x20   return count + {tag}\n\
             }}\n"
        )
    };
    oracle(&app("0"), &app("10"), "echo bump().await\necho count\n");
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

// ------------------------------------------- H5: the swap compiles against the check's own sites
//
// Everything below is a body edit whose swapped code needs a **span-keyed checker site** to behave
// the way a cold start behaves. Every one of them diverged from its own cold start under the
// checkerless install — a call-site-typed decode lost its recipe and aborted, a packed list came
// back boxed, named arguments bound positionally, a bare variant pattern matched everything, `i8`
// arithmetic stopped wrapping — which is what made a long editing session drift away from the
// program a restart would run.

/// Call-site-typed native decode (`json.parse::<T>`): the turbofish `T` is resolved by the CHECKER
/// into a `TypeRecipe` keyed by the call's span (`Sites::typed_module_call_sites`). The swapped
/// body's call is a fresh span, so a checkerless install lowers it as an ordinary module call with
/// no recipe at all.
#[test]
fn a_swapped_body_decodes_through_a_call_site_typed_json_parse() {
    let app = |tag: &str| {
        format!(
            "use std.{{json}}\n\
             struct Point {{ x: int  y: int }}\n\
             fn decode(text: string): string {{\n\
             \x20   p = json.parse::<Point>(text)\n\
             \x20   return \"{tag} ${{p.x}},${{p.y}}\"\n\
             }}\n"
        )
    };
    oracle(
        &app("v1"),
        &app("v2"),
        "echo decode(\"{\\\"x\\\": 1, \\\"y\\\": 2}\");",
    );
}

/// `@derive(Deserialize<Json>)` + `json.decode_typed(name, text)` — the router-facing decode. Two
/// sites carry it: the per-type recipe registry the derive produces and the call span lowering
/// turns into a `DecodeTyped`. A checkerless install recognizes neither.
#[test]
fn a_swapped_body_decodes_by_runtime_type_name() {
    let app = |tag: &str| {
        format!(
            "use std.json\n\
             @derive(Deserialize<Json>)\n\
             struct User {{ name: string  age: int }}\n\
             fn describe(text: string): string {{\n\
             \x20   return match json.decode_typed(\"User\", text) {{\n\
             \x20       Ok(u) => \"{tag} ${{u.name}}/${{u.age}}\",\n\
             \x20       Err(e) => \"{tag} err: ${{e}}\",\n\
             \x20   }}\n\
             }}\n"
        )
    };
    oracle(
        &app("v1"),
        &app("v2"),
        "echo describe(\"{\\\"name\\\": \\\"Ada\\\", \\\"age\\\": 36}\");",
    );
}

/// A `List<@packed struct>` literal: the checker records the element's flat layout at the
/// constructing span (`Sites::packed_list_sites`), and only then does the list get contiguous raw
/// storage. Boxed vs flat is invisible to most of the language on purpose — `to_bytes` is where it
/// surfaces, since a boxed list has no canonical buffer at all (E0007).
#[test]
fn a_swapped_body_builds_its_packed_list_flat() {
    let app = |tag: &str| {
        format!(
            "@packed struct Pt {{ x: i32  y: i32 }}\n\
             fn layout(): string {{\n\
             \x20   pts = [Pt {{ x: 1i32, y: 2i32 }}, Pt {{ x: 3i32, y: 4i32 }}]\n\
             \x20   return \"{tag} ${{pts.to_bytes().len()}}\"\n\
             }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo layout();");
}

// (A `type_of` case is deliberately absent: both an annotated empty-list reflection
// (`xs: List<string> = []` → `type_of(xs)`) and a generic one (`type_of(wrap(1))`) were measured
// IDENTICAL across the two install paths — the runtime tag the annotation-driven construction
// already carries answers them, so neither distinguishes checked from checkerless and a test on
// one would prove nothing about this seam.)

/// **Named arguments**: the checker resolves a call's label binding into an argument permutation
/// keyed by the call span (`Sites::arg_orders`) — it is the only pass that knows the callee's
/// parameter names. Without it the swapped body binds the arguments POSITIONALLY: not an abort, a
/// silently wrong answer (`2-1` where a restart says `1-2`).
#[test]
fn a_swapped_body_binds_its_named_arguments_by_label() {
    let app = |tag: &str| {
        format!(
            "fn mk(a: int, b: int): string {{ return \"${{a}}-${{b}}\" }}\n\
             fn call(): string {{ return \"{tag} ${{mk(b: 2, a: 1)}}\" }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo call();");
}

/// **Bare payload-free variant patterns**: `Red =>` is a variant test only because the checker
/// resolved that name against the scrutinee's enum and recorded the span
/// (`Sites::variant_pattern_sites`); otherwise it is an ordinary binding pattern, which matches
/// EVERYTHING. The swapped body took its first arm for every colour — again silently, with no
/// diagnostic anywhere.
#[test]
fn a_swapped_body_matches_bare_variant_patterns_as_variants() {
    let app = |tag: &str| {
        format!(
            "enum Color {{ Red; Green }}\n\
             fn name(c: Color): string {{\n\
             \x20   return match c {{ Red => \"{tag} red\", Green => \"{tag} green\" }}\n\
             }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo name(Color.Green);");
}

/// **Fixed-width arithmetic** (Tier W): a same-width `i8` multiplication masks to 8 bits only where
/// the checker recorded the site. The checkerless swap computed `100 * 3 = 300` where a restart
/// wraps it to `44`.
#[test]
fn a_swapped_body_masks_fixed_width_arithmetic() {
    let app = |tag: &str| {
        format!(
            "fn wrapmul(): string {{\n\
             \x20   x: i8 = 100i8\n\
             \x20   return \"{tag} ${{x * 3i8}}\"\n\
             }}\n"
        )
    };
    oracle(&app("v1"), &app("v2"), "echo wrapmul();");
}

/// The **live-VM** half of H5, end to end through the mailbox the CLI actually uses: a deposit
/// carrying its sites is drained at a scheduler tick and installed via `Vm::install_fragment` →
/// `FragmentCompiler::extend_checked`. Same named-argument probe as above, so a checkerless install
/// prints `2-1` on the post-swap line; with the bundle both lines bind by label. (The oracle tests
/// above drive `VmSession::hot_swap`; only this one proves the *mailbox* carries the bundle across
/// to the worker that drains it.)
#[test]
fn a_mailbox_deposit_installs_the_fragment_with_its_sites() {
    use noeta_vm::{HotChannel, HotSwapMailbox, VmBackend};

    let v = |tag: &str| {
        format!(
            "use std.task.{{sleep, all}}\n\
             fn mk(a: int, b: int): string {{ return \"${{a}}-${{b}}\" }}\n\
             fn f(): string {{ return \"{tag} ${{mk(b: 2, a: 1)}}\" }}\n\
             async fn probe(): string {{\n\
             \x20   sleep(1).await\n\
             \x20   return f()\n\
             }}\n\
             echo f()\n\
             results = all([probe()])\n\
             echo results[0]\n"
        )
    };
    let (v1, v2) = (v("one"), v("two"));
    noeta_stdlib::registry::default_seeded();
    let program = parse(&v1);
    let checked = noeta_check::check_all(&program);
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let (module, compiler) =
        noeta_compiler::compile_with_sites_session(&program, checked.sites, false, false)
            .expect("compiles");

    // The watcher's deposit: the swappable plan plus the whole-program sites of the check that
    // admitted it. The first scheduler tick (inside the await) drains and applies it.
    let SwapDiff::Swap(plan) = verdict(&v1, &v2) else {
        panic!("a body edit must be swappable");
    };
    let new_checked = noeta_check::check_all(&parse(&v2));
    assert!(
        new_checked.diagnostics.is_empty(),
        "{:?}",
        new_checked.diagnostics
    );
    let mailbox: HotSwapMailbox = std::sync::Arc::new(HotChannel::default());
    mailbox.deposit(noeta_vm::HotFragment {
        fragment: plan.fragment,
        rerun_top_level: plan.rerun_top_level,
        added: plan.added,
        changed: plan.changed,
        sites: Some(std::sync::Arc::new(new_checked.sites)),
    });

    let (result, trace) = VmBackend::new().run_module_hot(
        &module,
        compiler,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
        mailbox,
    );
    assert!(trace.is_empty(), "no abort across the swap: {trace:?}");
    assert_eq!(
        result.stdout, "one 1-2\ntwo 1-2\n",
        "the swapped body binds its named arguments by label — the deposit's sites reached the \
         install"
    );
    assert_eq!(result.exit_code, 0);
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
    noeta_stdlib::registry::default_seeded();
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
    // The mailbox queues `HotFragment`s (server-hmr F5): the watcher owns the compiler and hands the
    // VM only the applied-swap payload, mirroring `noeta_cli::watch`'s deposit — the gate's own
    // whole-program sites included (H5).
    let new_checked = noeta_check::check_all(&parse(&v2));
    mailbox.deposit(noeta_vm::HotFragment {
        fragment: plan.fragment,
        rerun_top_level: plan.rerun_top_level,
        added: plan.added,
        changed: plan.changed,
        sites: Some(std::sync::Arc::new(new_checked.sites)),
    });

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

// ------------------------------------------- broadcast-queue retention (server-hmr H5 retention)

/// The versions the two retention tests below swap through: one function body, one distinct tag
/// each. `probe`'s await is the scheduler tick the drain happens at, so the second echo reports
/// whatever the fleet installed.
fn tagged(tag: usize) -> String {
    format!(
        "use std.task.{{sleep, all}}\n\
         fn f(): string {{ return \"v{tag}\" }}\n\
         async fn probe(): string {{\n\
         \x20   sleep(1).await\n\
         \x20   return f()\n\
         }}\n\
         echo f()\n\
         results = all([probe()])\n\
         echo results[0]\n"
    )
}

/// Deposit the `generation` → `generation + 1` body edit exactly as the watcher does — the swappable plan plus the
/// whole-program `Sites` of the check that admitted it — and hand back a `Weak` on that bundle, so a
/// caller can observe its real liveness rather than the queue's opinion of it.
fn deposit_edit(
    mailbox: &noeta_vm::HotSwapMailbox,
    generation: usize,
) -> std::sync::Weak<noeta_compiler::Sites> {
    let (old, new) = (tagged(generation), tagged(generation + 1));
    let SwapDiff::Swap(plan) = verdict(&old, &new) else {
        panic!("a body edit must be swappable");
    };
    let checked = noeta_check::check_all(&parse(&new));
    assert!(checked.diagnostics.is_empty(), "{:?}", checked.diagnostics);
    let sites = std::sync::Arc::new(checked.sites);
    let weak = std::sync::Arc::downgrade(&sites);
    mailbox.deposit(noeta_vm::HotFragment {
        fragment: plan.fragment,
        rerun_top_level: plan.rerun_top_level,
        added: plan.added,
        changed: plan.changed,
        sites: Some(sites),
    });
    weak
}

/// Run one hot worker isolate over `mailbox` from the v1 baseline: its own compile, its own
/// session, its own cursor on the shared queue — what `serve_parallel_hot` spawns per core.
fn run_hot_worker(mailbox: &noeta_vm::HotSwapMailbox) -> noeta_vm::RunResult {
    let program = parse(&tagged(1));
    let checked = noeta_check::check_all(&program);
    let (module, compiler) =
        noeta_compiler::compile_with_sites_session(&program, checked.sites, false, false)
            .expect("compiles");
    let (result, trace) = noeta_vm::VmBackend::new().run_module_hot(
        &module,
        compiler,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
        std::sync::Arc::clone(mailbox),
    );
    assert!(trace.is_empty(), "no abort across the swaps: {trace:?}");
    result
}

/// **A deposit's payload is reclaimed once — and only once — every worker has installed it.**
///
/// The queue is append-only and a `--parallel N` fleet drains it by N independent cursors, so
/// nothing may be dropped until the slowest worker has passed it. Since H5 made each deposit carry
/// the whole-program `Sites` of the check that admitted it, "when the process exits" stopped being
/// an acceptable answer: measured on a 293 KB app, that bundle is 204 KiB against 14 KiB for the
/// one-function fragment beside it, and it scales with the PROGRAM where the fragment scales with
/// the EDIT. A day of saves would retain hundreds of MB of superseded site maps.
///
/// This drives the real path — `HotChannel::deposit` from the watcher's side, `Vm::apply_pending_hotswap`
/// → `HotChannel::drain` → `Vm::install_fragment` from each worker's — with a three-consumer channel
/// and three real hot VMs, and pins both directions at each step:
///
/// * after one and after two workers, all `N` plans are still resident and every `Sites` bundle is
///   still reachable — a worker that has not drained cannot lose a swap;
/// * after the third, residency is zero and every bundle has actually been *freed*, asserted through
///   `Weak` handles to the deposited `Arc`s rather than through the queue's own bookkeeping.
///
/// The workers run in sequence rather than concurrently: the cursor arithmetic is identical (the
/// queue serializes either way) and the frontier then moves deterministically. Retirement
/// deliberately does not collect, so a queue that only shrank at teardown would fail the middle
/// assertions instead of passing this test by accident. The concurrent fleet is covered by
/// `noeta-cli`'s `parallel_hot` integration test.
#[test]
fn a_deposited_plan_is_reclaimed_when_the_last_worker_has_installed_it() {
    const WORKERS: usize = 3;
    const DEPOSITS: usize = 6;

    noeta_stdlib::registry::default_seeded();
    let mailbox: noeta_vm::HotSwapMailbox = std::sync::Arc::new(noeta_vm::HotChannel::new(WORKERS));
    let bundles: Vec<std::sync::Weak<noeta_compiler::Sites>> = (1..=DEPOSITS)
        .map(|generation| deposit_edit(&mailbox, generation))
        .collect();
    assert_eq!(mailbox.deposited(), DEPOSITS);
    assert_eq!(mailbox.resident_plans(), DEPOSITS, "nothing drained yet");

    let live = |bundles: &[std::sync::Weak<noeta_compiler::Sites>]| {
        bundles.iter().filter(|w| w.upgrade().is_some()).count()
    };

    for worker in 1..=WORKERS {
        let result = run_hot_worker(&mailbox);
        assert_eq!(
            result.stdout,
            format!("v1\nv{}\n", DEPOSITS + 1),
            "worker {worker} drains the whole queue at its tick and ends on the last deposit"
        );
        // The generation numbering is untouched by reclamation: index IS the generation, the queue
        // never shifts — only a passed plan's payload is released out of its slot.
        assert_eq!(mailbox.deposited(), DEPOSITS);
        if worker < WORKERS {
            assert_eq!(
                mailbox.resident_plans(),
                DEPOSITS,
                "worker {worker} of {WORKERS} has drained, but the rest have not — nothing may be \
                 dropped while a consumer could still need it"
            );
            assert_eq!(
                live(&bundles),
                DEPOSITS,
                "every whole-program `Sites` bundle is still reachable after worker {worker}"
            );
        }
    }
    assert_eq!(
        mailbox.resident_plans(),
        0,
        "every worker has installed every deposit — the queue holds no plan payloads, so an \
         editing session costs O(1) in bundles, not O(saves x program size)"
    );
    assert_eq!(
        live(&bundles),
        0,
        "and the bundles are actually freed, not merely unreferenced by the queue"
    );
    assert_eq!(
        mailbox.deposited(),
        DEPOSITS,
        "generation = index survives reclamation"
    );
}

/// The negative half, isolated: a **declared consumer that never registers** — a worker still
/// compiling its session when the first edit lands — holds the whole queue back. The fleet size is
/// declared at `HotChannel::new` and an unclaimed cursor sits at generation 0, so reclamation cannot
/// begin before every worker has armed. Without that gate a fast worker's drain would tombstone
/// plans the slow one has never seen, and it would serve a program missing swaps.
#[test]
fn an_unregistered_consumer_holds_the_whole_queue() {
    noeta_stdlib::registry::default_seeded();
    // Two workers declared; only one ever runs.
    let mailbox: noeta_vm::HotSwapMailbox = std::sync::Arc::new(noeta_vm::HotChannel::new(2));
    let bundle = deposit_edit(&mailbox, 1);

    let result = run_hot_worker(&mailbox);
    assert_eq!(
        result.stdout, "v1\nv2\n",
        "the one live worker did install it"
    );
    assert_eq!(
        mailbox.resident_plans(),
        1,
        "the second declared worker has not drained — the plan stays"
    );
    assert!(
        bundle.upgrade().is_some(),
        "and its `Sites` bundle stays with it"
    );
}
