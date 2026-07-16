//! Property tests for the pipeline's robustness and determinism, plus a no-panic sweep
//! over the whole conformance corpus.
//!
//! Two invariants the differential-oracle design leans on:
//! 1. **Totality** — lexing, parsing, and evaluating any input *returns* (errors are data
//!    in the [`RunResult`], never a Rust panic or `process::exit`). A panic here would mean
//!    a backend can crash instead of producing a comparable result.
//! 2. **Determinism** — the same source always yields the same `RunResult` and the same
//!    pretty-printed AST (seeded ids, `BTreeMap` ordering, no wall-clock). Without this the
//!    corpus expectations and the future VM differential could flake.
//!
//! The §14 pretty-printer is an S-expression form, not re-parsable source, so a literal
//! parse→print→parse round-trip is not applicable in M0 (a source printer is the M2
//! formatter's job). The round-trip's *intent* — that parse-then-print is a stable function
//! — is captured by the determinism properties below.

use std::path::PathBuf;

use noeta_ast::Pretty;
use noeta_eval::{Backend, RunResult, IrRefBackend};
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};
use proptest::prelude::*;

/// Drive a source string all the way through the pipeline. This must never panic, whatever
/// the input — malformed programs surface as diagnostics in the result.
fn run_pipeline(src: &str) -> RunResult {
    let source = Source::new(SourceId::FIRST, "prop.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    IrRefBackend::new().run(&parsed.program)
}

/// The pretty-printed AST of a source string (the snapshot form).
fn pretty_of(src: &str) -> String {
    let source = Source::new(SourceId::FIRST, "prop.noe", src);
    let lexed = lex(&source);
    parse(&source, &lexed.tokens).program.to_pretty_string()
}

/// Random token soup over the real vocabulary: far more likely to reach the parser's
/// recovery paths and the evaluator than arbitrary bytes, so panics surface faster.
fn arb_token_soup() -> impl Strategy<Value = String> {
    let token = prop_oneof![
        Just("echo"),
        Just("fn"),
        Just("return"),
        Just("if"),
        Just("else"),
        Just("for"),
        Just("in"),
        Just("match"),
        Just("enum"),
        Just("class"),
        Just("type"),
        Just("use"),
        Just("namespace"),
        Just("mut"),
        Just("("),
        Just(")"),
        Just("{"),
        Just("}"),
        Just("["),
        Just("]"),
        Just(";"),
        Just(","),
        Just(":"),
        Just("."),
        Just("="),
        Just("=>"),
        Just("|>"),
        Just("+"),
        Just("*"),
        Just("~"),
        Just("?"),
        Just("??"),
        Just("1"),
        Just("2.5"),
        Just("\"s\""),
        Just("true"),
        Just("v0"),
        Just("Ok"),
        Just("some"),
        Just("none"),
        Just("panic"),
    ];
    proptest::collection::vec(token, 0..24).prop_map(|toks| toks.join(" "))
}

/// A generator of *valid, type-correct* programs: `echo <expr>;` lines over pure integer
/// arithmetic (non-negative literals, `+`/`*`, parens). M0 has no type checker, so to assert
/// a clean exit the generated programs must be type-correct by construction — integer-only
/// arithmetic always evaluates to an int with exit 0 (wrapping arithmetic never panics, and
/// no `/` means no division-by-zero).
fn arb_valid_program() -> impl Strategy<Value = String> {
    let leaf = (0u32..1000).prop_map(|n| n.to_string());
    let expr = leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} + {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a} * {b})")),
            inner.prop_map(|a| format!("({a})")),
        ]
    });
    proptest::collection::vec(expr, 1..6)
        .prop_map(|exprs| exprs.into_iter().map(|e| format!("echo {e};")).collect())
}

proptest! {
    /// Totality: no input, however malformed, panics the pipeline.
    #[test]
    fn pipeline_never_panics_on_token_soup(src in arb_token_soup()) {
        let _ = run_pipeline(&src);
    }

    /// Totality over arbitrary bytes — stresses the lexer specifically.
    #[test]
    fn pipeline_never_panics_on_arbitrary_text(src in ".{0,120}") {
        let _ = run_pipeline(&src);
    }

    /// Determinism: the same source yields identical results and identical ASTs on repeat
    /// runs (the stable-function property that stands in for parse→print→parse).
    #[test]
    fn pipeline_is_deterministic(src in arb_token_soup()) {
        prop_assert_eq!(run_pipeline(&src), run_pipeline(&src));
        prop_assert_eq!(pretty_of(&src), pretty_of(&src));
    }

    /// Valid programs parse without diagnostics and evaluate cleanly (exit 0), and their
    /// pretty-print is stable.
    #[test]
    fn valid_programs_parse_and_evaluate_cleanly(src in arb_valid_program()) {
        let source = Source::new(SourceId::FIRST, "prop.noe", src.clone());
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        prop_assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "generated program failed to parse: {src:?} -> {:?}",
            parsed.diagnostics
        );
        prop_assert_eq!(pretty_of(&src), pretty_of(&src));
        let result = IrRefBackend::new().run(&parsed.program);
        prop_assert_eq!(result.exit_code, 0);
    }
}

/// Collect every `.noe` file under the conformance corpus.
fn corpus_files() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance");
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("corpus directory is readable");
        for entry in entries {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "noe") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn corpus_evaluates_without_panic_and_deterministically() {
    // The tree-walker recurses to the depth of the program's nesting, and real corpus cases — a
    // reactive `@html` LiveView template compiling every hole into a `computed`, for one — go deep
    // enough to blow the default ~2 MiB test-thread stack. The production harness always runs eval
    // inside a 64 MiB worker (`noeta_conformance::on_deep_stack`, matched to `noeta_parser`'s deep
    // stack); this sweep must do the same, or the binding constraint is the harness's stack, not the
    // interpreter's real depth. Inlined (rather than depending on noeta-conformance) to avoid a
    // dev-dependency cycle — that crate depends on this one.
    const DEEP_STACK: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, || {
                let files = corpus_files();
                assert!(!files.is_empty(), "the conformance corpus is empty");
                for path in files {
                    let text = std::fs::read_to_string(&path).expect("corpus file is readable");
                    let first = run_pipeline(&text);
                    let second = run_pipeline(&text);
                    assert_eq!(
                        first,
                        second,
                        "non-deterministic evaluation for {}",
                        path.display()
                    );
                }
            })
            .expect("spawn deep-stack corpus worker")
            .join()
            .expect("corpus worker panicked");
    });
}
