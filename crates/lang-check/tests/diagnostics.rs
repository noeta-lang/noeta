//! A snapshot "gallery" of the type checker's *rendered* diagnostics. Like the M0 runtime
//! gallery in `lang-eval`, this asserts the full rendered text (caret placement, code, help
//! line) of each checker diagnostic, not just the code enum — diagnostic quality is a product
//! feature. Color is disabled in the renderer, so output is deterministic.

use lang_check::check;
use lang_diagnostics::render;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

fn render_checks(src: &str) -> String {
    let source = Source::new(SourceId::FIRST, "snippet.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "gallery snippet must lex/parse cleanly so only checker diagnostics show"
    );
    check(&parsed.program)
        .iter()
        .map(|d| render(&source, d))
        .collect()
}

#[test]
fn checker_diagnostic_gallery() {
    // One representative program per checker diagnostic. E0013 (unknown type) is deferred to
    // M1.9 and intentionally absent.
    let cases = [
        ("E0007 arithmetic type mismatch", "echo 1 + true;"),
        (
            "E0011 non-exhaustive match",
            "enum E { A; B; C; }\necho match E.A { E.A => 1, E.B => 2 };",
        ),
        (
            "E0012 `?` on a non-fallible value",
            "fn f(): int { return 5?; }",
        ),
        (
            "E0014 `impl` of an unknown trait",
            "class W {\n  impl Frob {\n    fn frob(other: W): W { return other; }\n  }\n}",
        ),
        (
            "E0015 `impl` missing the trait's required method",
            "class M {\n  amount: int\n  impl Add {\n    fn plus(other: M): M { return other; }\n  }\n}",
        ),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_checks(src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}
