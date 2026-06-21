//! Unit tests for the checker, driven through the real lexer/parser so the AST shapes are
//! exactly what the pipeline produces. Conformance `.lang` cases (positive + negative) carry
//! the end-to-end coverage; these pin specific rules in isolation.

use super::check;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

/// Parse `text` and return the checker's diagnostic codes (wire form), in order.
fn codes(text: &str) -> Vec<String> {
    let source = Source::new(SourceId::FIRST, "test.lang", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    check(&parsed.program)
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn well_typed_program_is_clean() {
    assert!(codes("echo 1 + 2;\necho \"hi\" ~ \"there\";\n").is_empty());
}

#[test]
fn arithmetic_on_bool_is_type_mismatch() {
    assert_eq!(codes("echo 1 + true;\n"), ["E0007"]);
}

#[test]
fn mixed_numeric_arithmetic_is_fine() {
    // int + float is valid in M0 (promotes to float); the checker must not flag it.
    assert!(codes("echo 1 + 2.5;\n").is_empty());
}

#[test]
fn concat_accepts_any_operands() {
    // `~` is display-based concatenation, never a type error.
    assert!(codes("echo 1 ~ true;\n").is_empty());
}

#[test]
fn non_exhaustive_enum_match_is_reported() {
    let src = "enum E { A; B; C; }\necho match E.A { E.A => 1, E.B => 2 };\n";
    assert_eq!(codes(src), ["E0011"]);
}

#[test]
fn exhaustive_enum_match_is_clean() {
    let src = "enum E { A; B; }\necho match E.A { E.A => 1, E.B => 2 };\n";
    assert!(codes(src).is_empty());
}

#[test]
fn wildcard_makes_match_exhaustive() {
    let src = "enum E { A; B; C; }\necho match E.A { E.A => 1, _ => 0 };\n";
    assert!(codes(src).is_empty());
}

#[test]
fn match_on_unknown_scrutinee_is_not_flagged() {
    // A gradual scrutinee (here an unannotated parameter) has an unknown domain: never flagged.
    let src = "fn f(x) { return match x { 1 => \"a\", 2 => \"b\" }; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn try_on_int_is_invalid() {
    let src = "fn f(): int { return 5?; }\n";
    assert_eq!(codes(src), ["E0012"]);
}

#[test]
fn try_on_result_is_clean() {
    let src = "fn g(): Result<int, string> { return Ok(1); }\n\
               fn f(): Result<int, string> { return Ok(g()?); }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn undeclared_type_annotation_is_not_flagged_yet() {
    // Unknown-type checking (E0013) is deferred to M1.9: until module resolution exists, an
    // undeclared annotation cannot be told from a valid-but-unresolved one, so the checker must
    // stay silent (M0 runs such programs fine — see results/coalesce_default.lang's `?User`).
    assert!(codes("fn f(x: Nope): int { return 0; }\n").is_empty());
    assert!(codes("fn find(hit): ?User { return none; }\n").is_empty());
}

#[test]
fn annotations_do_not_produce_false_positives() {
    let src = "type Item = { price: float };\n\
               fn f(xs: List<Item>): Result<void, string> { return Ok(); }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn valid_operator_impl_is_clean() {
    let src = "class M {\n  amount: int\n  impl Add {\n    fn add(other: M): M { return other; }\n  }\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn impl_of_unknown_trait_is_reported() {
    let src = "class W {\n  impl Frob {\n    fn frob(other: W): W { return other; }\n  }\n}\n";
    assert_eq!(codes(src), ["E0014"]);
}

#[test]
fn impl_missing_required_method_is_reported() {
    // `impl Add` without an `add` method does not satisfy the trait.
    let src = "class M {\n  amount: int\n  impl Add {\n    fn plus(other: M): M { return other; }\n  }\n}\n";
    assert_eq!(codes(src), ["E0015"]);
}

#[test]
fn impl_with_wrong_arity_is_reported() {
    // `add` must take exactly one parameter besides the receiver.
    let src = "class M {\n  amount: int\n  impl Add {\n    fn add(): M { return M { amount: 0 }; }\n  }\n}\n";
    assert_eq!(codes(src), ["E0015"]);
}

#[test]
fn derivable_traits_are_accepted() {
    let src = "@derive(Equatable, Comparable, Display, Clone)\nclass P {\n  x: int\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn deriving_a_non_derivable_trait_is_reported() {
    // `Add` is an operator trait, implemented not derived.
    let src = "@derive(Add)\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0014"]);
}

#[test]
fn deriving_an_unknown_trait_is_reported() {
    let src = "@derive(Bogus)\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0014"]);
}

#[test]
fn old_derive_attribute_spelling_is_reported() {
    // `#[derive(...)]` is the old codegen spelling; it is now `@derive(...)`.
    let src = "#[derive(Equatable)]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0017"]);
}

#[test]
fn data_attribute_is_accepted() {
    // A non-`derive` `#[...]` attribute attaches as data and is not (yet) validated.
    let src = "#[Route]\nclass P {\n  x: int\n}\n";
    assert!(codes(src).is_empty());
}
