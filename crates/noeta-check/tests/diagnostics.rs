//! A snapshot "gallery" of the type checker's *rendered* diagnostics. Like the M0 runtime
//! gallery in `noeta-eval`, this asserts the full rendered text (caret placement, code, help
//! line) of each checker diagnostic, not just the code enum — diagnostic quality is a product
//! feature. Color is disabled in the renderer, so output is deterministic.

use noeta_check::check;
use noeta_diagnostics::render;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

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
    // This test is its own assembling driver (audit-6 F2): seed the std units first.
    noeta_stdlib::registry::default_seeded();
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

/// The `match`-arm gallery. E0066 is the first diagnostic in the catalog to carry **two** labels —
/// the arm that died and the pattern that killed it — so its rendered shape (both carets in one
/// snippet, in source order) is worth pinning rather than assumed. The bare-variant cases ride
/// along because they are what an author now hits *instead* of the retired E0067: a bare identifier
/// that resolved is a case test (so the only complaint left is the cases it does NOT cover, E0011),
/// and one that did not resolve is the plain catch-all it always was.
#[test]
fn match_arm_gallery() {
    noeta_stdlib::registry::default_seeded();
    let type_enum = "enum Type { String; Int; List(inner: string) }\n";
    let cases = [
        (
            "E0066 unreachable arm after a wildcard",
            "fn rank(n: int): string {\n    return match n {\n        _ => \"many\",\n        1 => \"one\",\n    }\n}".to_string(),
        ),
        (
            "E0066 unreachable arm after a bare `none` the scrutinee did not resolve",
            "fn show(d: dyn): string {\n    return match d {\n        none => \"empty\",\n        some(v) => \"${v}\",\n    }\n}".to_string(),
        ),
        (
            "E0011 a bare payload-free variant covers only its own case",
            format!("{type_enum}fn describe(t: Type): string {{\n    return match t {{\n        String => \"string\",\n    }}\n}}"),
        ),
        (
            "clean: every payload-free variant spelled bare, no `_` needed",
            format!("{type_enum}fn describe(t: Type): string {{\n    return match t {{\n        String => \"string\",\n        Int => \"int\",\n        List(i) => i,\n    }}\n}}"),
        ),
    ];
    let mut out = String::new();
    for (label, src) in cases {
        out.push_str(&format!("================ {label} ================\n"));
        out.push_str(&render_checks(&src));
        out.push('\n');
    }
    insta::assert_snapshot!(out);
}
