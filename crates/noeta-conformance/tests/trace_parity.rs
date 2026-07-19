//! **Backend trace parity**: the abort traceback the VM captures (from its frame stack + line
//! table) and the one the tree-walker oracle captures (from its call-site shadow stack) must agree
//! on the story they tell — the same function names, at the same source lines, innermost first.
//!
//! Compared as `(name, line)` — not raw spans — deliberately: the two backends resolve a caller
//! frame's location through different artifacts (the VM through the covering *statement* entry of
//! the line table at the call op's pc; the oracle through the call *expression*'s own span), which
//! agree on the line without being byte-identical spans. The trace rides beside [`RunResult`], so
//! this is its own oracle rather than part of the main differential; if the two shapes ever
//! converge span-exactly, it can be promoted there.

use std::collections::HashMap;

use noeta_backend::TraceFrame;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

/// Run `src` on both backends and return both tracebacks.
fn both_traces(src: &str) -> (Vec<TraceFrame>, Vec<TraceFrame>, Source) {
    noeta_conformance::ensure_std_registry();
    let source = Source::new(SourceId::FIRST, "trace.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "must parse: {:?}",
        parsed.diagnostics
    );
    let checked = noeta_check::check_all(&parsed.program);
    assert!(
        checked.diagnostics.is_empty(),
        "must check: {:?}",
        checked.diagnostics
    );

    let module = noeta_compiler::compile_with_sites(
        &parsed.program,
        checked.sites.clone(),
        false,
        false, // a PRODUCTION compile — traces must not need the debug tier
    )
    .expect("compiles");
    let (vm_result, vm_trace) = VmBackend::new().run_module_traced(&module);
    assert_ne!(vm_result.exit_code, 0, "the program should abort");

    let (eval_result, eval_trace) =
        noeta_conformance::reference::reference_run_traced(&parsed.program, checked.sites);
    assert_ne!(eval_result.exit_code, 0, "the program should abort");

    (vm_trace, eval_trace, source)
}

/// Project a traceback to the comparable `(name, line)` story.
fn story(trace: &[TraceFrame], source: &Source) -> Vec<(Option<String>, Option<u32>)> {
    trace
        .iter()
        .map(|f| {
            (
                f.name.clone(),
                f.span.map(|s| source.line_col(s.start).line),
            )
        })
        .collect()
}

/// A map of a HashMap type used by both pipelines — kept for signature clarity above.
#[allow(dead_code)]
type Sites = HashMap<noeta_span::Span, noeta_ast::reflect::TypeRepr>;

#[test]
fn nested_function_panic_traces_identically() {
    let (vm, eval, source) = both_traces(
        "fn inner(n: int): int {\n    panic(\"boom\")\n}\nfn outer(): int {\n    return inner(1)\n}\nmut r = outer()\necho r\n",
    );
    let vm_story = story(&vm, &source);
    assert_eq!(vm_story, story(&eval, &source), "vm={vm:#?} eval={eval:#?}");
    // And the shared story is the expected one.
    let line = |l| Some(l);
    assert_eq!(
        vm_story,
        vec![
            (Some("inner".into()), line(2)),
            (Some("outer".into()), line(5)),
            (Some("main".into()), line(7)),
        ]
    );
}

#[test]
fn method_panic_traces_identically() {
    // `check` reads `self` so it derives as an *instance* method (prelude-redesign EX.2).
    let (vm, eval, source) = both_traces(
        "struct Acct {\n    v: int\n    fn check(): int {\n        panic(\"no funds: ${self.v}\")\n    }\n}\nfn charge(a: Acct): int {\n    return a.check()\n}\nmut a = Acct { v: 1 }\nmut r = charge(a)\necho r\n",
    );
    let vm_story = story(&vm, &source);
    assert_eq!(vm_story, story(&eval, &source), "vm={vm:#?} eval={eval:#?}");
    assert_eq!(
        vm_story,
        vec![
            (Some("Acct.check".into()), Some(4)),
            (Some("charge".into()), Some(8)),
            (Some("main".into()), Some(11)),
        ]
    );
}

#[test]
fn async_fn_panic_traces_under_the_functions_name() {
    // An async body lowers to a synthesized step closure; it inherits the enclosing function's
    // name (`Func::name`, set at lowering), so the frame is `fetch`, not `<anonymous>` — and both
    // backends read the same IR field, so they agree by construction.
    let (vm, eval, source) = both_traces(
        "async fn fetch(n: int): int {\n    panic(\"async boom\")\n}\nconcurrent {\n    h = spawn fetch(1)\n    echo h.await\n}\n",
    );
    let vm_story = story(&vm, &source);
    let eval_story = story(&eval, &source);
    // The *names* agree in full. The awaiting frame's location is a known structural asymmetry:
    // the VM reaches an async panic through a re-entrant run, whose outer segment's top frame
    // carries no span (its saved pc is stale — only synced at calls), while the oracle's shadow
    // stack still knows the await site. So spans are compared on the innermost (async) frame,
    // where both are precise.
    let names = |st: &[(Option<String>, Option<u32>)]| {
        st.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        names(&vm_story),
        names(&eval_story),
        "vm={vm:#?} eval={eval:#?}"
    );
    assert_eq!(
        vm_story.first(),
        eval_story.first(),
        "innermost frames should agree exactly"
    );
    assert_eq!(
        vm_story.first().map(|(n, l)| (n.as_deref(), *l)),
        Some((Some("fetch"), Some(2))),
        "the async frame should carry the function's name: {vm_story:?}"
    );
}

#[test]
fn anonymous_closure_panic_traces_identically() {
    // The named fn imports the closure binding with `use (f)` — sealed named fns.
    let (vm, eval, source) = both_traces(
        "mut f = fn(x: int) => panic(\"in closure\")\nfn call_it() use (f): int {\n    return f(1)\n}\nmut r = call_it()\necho r\n",
    );
    let vm_story = story(&vm, &source);
    assert_eq!(vm_story, story(&eval, &source), "vm={vm:#?} eval={eval:#?}");
    // The closure frame is anonymous on both sides.
    assert_eq!(vm_story[0].0, None, "closure frame should be nameless");
    assert_eq!(vm_story.last().unwrap().0, Some("main".into()));
}
