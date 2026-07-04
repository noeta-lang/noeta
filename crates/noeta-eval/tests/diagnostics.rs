//! A snapshot "gallery" of every diagnostic code's *rendered* form. Diagnostic quality is
//! a product feature (errors-as-data, one ariadne renderer), so the rendered text — caret
//! placement, the code, the help line — is asserted here, not just the code enum. Color is
//! disabled in the renderer, so the output is deterministic.
//!
//! Rendering is staged exactly as the CLI compiles: stop at the earliest stage that
//! produces diagnostics (lexer → parser → evaluator), so each case shows only its own code
//! rather than cascading downstream noise.

use noeta_diagnostics::render;
use noeta_eval::{Backend, TreeWalkBackend};
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

fn render_first_failing_stage(src: &str) -> String {
    let source = Source::new(SourceId::FIRST, "snippet.noe", src);
    let lexed = lex(&source);
    if !lexed.diagnostics.is_empty() {
        return lexed
            .diagnostics
            .iter()
            .map(|d| render(&source, d))
            .collect();
    }
    let parsed = parse(&source, &lexed.tokens);
    if !parsed.diagnostics.is_empty() {
        return parsed
            .diagnostics
            .iter()
            .map(|d| render(&source, d))
            .collect();
    }
    TreeWalkBackend::new()
        .run(&parsed.program)
        .diagnostics
        .iter()
        .map(|d| render(&source, d))
        .collect()
}

#[test]
fn diagnostic_gallery() {
    // One representative program per code, E0001..=E0010.
    let cases = [
        ("E0001 unexpected character", "echo $;"),
        ("E0002 unterminated string", "echo \"oops;"),
        ("E0003 unexpected token", "echo ;"),
        ("E0004 unexpected end of input", "x = 1 +"),
        ("E0005 unknown name", "echo missing;"),
        ("E0006 immutable assignment", "x = 1;\nx = 2;"),
        ("E0007 type mismatch", "echo 1 + true;"),
        ("E0008 division by zero", "echo 1 / 0;"),
        (
            "E0009 missing field",
            "class P { x: int y: int }\np = P { x: 1 };",
        ),
        ("E0010 panic", "panic(\"boom\");"),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_first_failing_stage(src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}
