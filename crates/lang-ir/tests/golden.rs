//! Golden snapshots of the lowered Core IR for representative programs. The textual dump is
//! stable, so a lowering change shows up as a reviewable snapshot diff. These complement the
//! faithfulness differential (which proves the IR *runs* identically to the tree-walker): the
//! goldens make the *shape* of the lowering — its `let`-sequencing and ANF temporaries —
//! visible and regression-checked.

use lang_span::{Source, SourceId};

/// Lex, parse, and lower a source program, returning the Core-IR dump.
fn lower_dump(src: &str) -> String {
    let source = Source::new(SourceId::FIRST, "golden", src);
    let lexed = lang_lexer::lex(&source);
    assert!(
        lexed.diagnostics.is_empty(),
        "lex errors: {:?}",
        lexed.diagnostics
    );
    let parsed = lang_parser::parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse errors: {:?}",
        parsed.diagnostics
    );
    let ir = lang_ir::lower(&parsed.program).expect("program lowers");
    lang_ir::dump(&ir)
}

#[test]
fn anf_flattens_nested_arithmetic() {
    // The headline ANF property: `(a + 1) * (b + 2)` becomes a flat `let` sequence over atoms,
    // every intermediate explicitly named.
    insta::assert_snapshot!(lower_dump("x = 3;\ny = 4;\necho (x + 1) * (y + 2);\n"));
}

#[test]
fn control_flow_and_accumulator() {
    insta::assert_snapshot!(lower_dump(
        "mut acc = 0;\nfor i in 0..5 {\n  if i % 2 == 0 {\n    acc = acc + i;\n  }\n}\necho acc;\n"
    ));
}

#[test]
fn function_with_default_and_call() {
    insta::assert_snapshot!(lower_dump(
        "fn add(a: int, b: int = 10): int {\n  return a + b;\n}\necho add(5);\n"
    ));
}

#[test]
fn class_method_and_object_literal() {
    insta::assert_snapshot!(lower_dump(
        "class Point {\n  x: int\n  y: int\n  fn sum(): int { return x + y; }\n}\np = Point { x: 1, y: 2 };\necho p.sum();\n"
    ));
}

#[test]
fn match_and_closure() {
    insta::assert_snapshot!(lower_dump(
        "f = fn(n) => n + 1;\nr = match f(2) {\n  3 => \"three\",\n  _ => \"other\",\n};\necho r;\n"
    ));
}
