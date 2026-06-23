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
fn match_on_open_domain_scrutinee_is_not_flagged() {
    // A `dyn` scrutinee has an open domain (not a closed enum / Result / Option), so the
    // exhaustiveness check does not fire — no false positive.
    let src = "fn f(x: dyn): string { return match x { 1 => \"a\", 2 => \"b\" }; }\n";
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
fn undeclared_type_annotation_is_e0013() {
    // M1.9 lit up unknown-type checking: an annotation naming nothing declared, imported, or
    // built-in is now a hard error, on the offending name.
    assert_eq!(codes("fn f(x: Nope): int { return 0; }\n"), ["E0013"]);
    assert_eq!(
        codes("fn find(hit: bool): ?User { return none; }\n"),
        ["E0013"]
    );
    // The unknown name inside a generic argument is flagged too.
    assert_eq!(
        codes("fn f(xs: List<Ghost>): int { return 0; }\n"),
        ["E0013"]
    );
}

#[test]
fn imported_type_annotation_is_not_flagged() {
    // A name brought in by `use` is a legal referent — the linker either merged its real
    // declaration or left an opaque stub, but either way the annotation resolves.
    let src = "use App.Models.User;\nfn find(): ?User { return none; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn generic_parameter_is_a_legal_type() {
    // A class's `<T>` is an in-scope type within its own field and method annotations, but is
    // erased — unknown outside the declaration.
    let src = "class Box<T> {\n  value: T\n  fn get(): T { return value; }\n}\n";
    assert!(codes(src).is_empty());
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

// ----- bidirectional check-mode (white-box) -----
//
// Production callers pass an open (`Unknown`) expectation in this slice, so the check path is a
// no-op end-to-end — these drive `Checker::check` directly with concrete expectations to prove
// subsumption and inward propagation are wired (the machinery later slices feed real types into).

/// Parse `__probe = <expr>;`, then check the binding's value against `expected`, returning the
/// resulting diagnostic codes.
fn check_value_against(expr: &str, expected: lang_types::Type) -> Vec<String> {
    let text = format!("__probe = {expr};");
    let source = Source::new(SourceId::FIRST, "test.lang", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "probe must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let value = match &parsed.program.stmts[0] {
        lang_ast::Stmt::Binding { value, .. } => value,
        other => panic!("expected a binding, got {other:?}"),
    };
    let mut checker = super::Checker::default();
    checker.collect(&parsed.program);
    let mut env: super::Env = vec![std::collections::HashMap::new()];
    checker.check(value, &expected, &mut env);
    checker.diags.iter().map(|d| d.code.to_string()).collect()
}

#[test]
fn subsumption_passes_on_identity_and_into_dyn() {
    use lang_types::Type;
    assert!(check_value_against("5", Type::Int).is_empty());
    assert!(check_value_against("\"hi\"", Type::String).is_empty());
    // Every type widens into the explicit top.
    assert!(check_value_against("5", Type::Dyn).is_empty());
}

#[test]
fn subsumption_fires_on_a_concrete_violation() {
    use lang_types::Type;
    // int is not a subtype of string → the same code the arithmetic mismatch path uses.
    assert_eq!(check_value_against("5", Type::String), ["E0007"]);
    assert_eq!(check_value_against("true", Type::Int), ["E0007"]);
}

#[test]
fn subsumption_is_a_no_op_against_an_open_expectation() {
    use lang_types::Type;
    // The production default: an `Unknown` expectation never reports — the parity guarantee.
    assert!(check_value_against("5", Type::Unknown).is_empty());
    assert!(check_value_against("true", Type::Unknown).is_empty());
}

#[test]
fn list_expectation_propagates_to_elements() {
    use lang_types::Type;
    // A `List<int>` expectation checks each element against `int`; the string element violates it.
    assert_eq!(
        check_value_against("[1, \"two\", 3]", Type::List(Box::new(Type::Int))),
        ["E0007"]
    );
    // A homogeneous list satisfies the element expectation.
    assert!(check_value_against("[1, 2, 3]", Type::List(Box::new(Type::Int))).is_empty());
    // And every element widens into a `List<dyn>` expectation.
    assert!(check_value_against("[1, \"two\"]", Type::List(Box::new(Type::Dyn))).is_empty());
}

#[test]
fn closure_expectation_propagates_param_and_return_types() {
    use lang_types::Type;
    let fn_int_to_int = Type::Fn {
        params: vec![Type::Int],
        ret: Box::new(Type::Int),
    };
    // `|x| x` against `fn(int) -> int`: the param adopts `int`, the body (`x`) checks against the
    // expected `int` return — well typed.
    assert!(check_value_against("fn(x) => x", fn_int_to_int).is_empty());
    // Same closure against `fn(int) -> string`: the body `x` is `int`, not `string`.
    let fn_int_to_string = Type::Fn {
        params: vec![Type::Int],
        ret: Box::new(Type::String),
    };
    assert_eq!(
        check_value_against("fn(x) => x", fn_int_to_string),
        ["E0007"]
    );
}

// ----- signature requirement + return checking (S2) -----

#[test]
fn unannotated_parameter_requires_a_signature() {
    assert_eq!(codes("fn double(n): int { return n; }\n"), ["E0022"]);
}

#[test]
fn missing_return_type_requires_a_signature() {
    assert_eq!(codes("fn greet(name: string) { echo name; }\n"), ["E0022"]);
}

#[test]
fn a_fully_annotated_named_fn_is_clean() {
    assert!(codes("fn add(a: int, b: int): int { return a + b; }\n").is_empty());
}

#[test]
fn closures_and_locals_do_not_require_annotations() {
    // A closure parameter and a local binding stay inferred — only named boundaries are mandatory.
    let src = "f = fn(x) => x + 1;\ng = 41;\necho f(g);\n";
    assert!(codes(src).is_empty());
}

#[test]
fn return_value_is_checked_against_the_declared_type() {
    // A concrete return-type violation is `E0007`; a matching return is clean.
    assert_eq!(codes("fn f(): int { return \"x\"; }\n"), ["E0007"]);
    assert!(codes("fn f(): int { return 7; }\n").is_empty());
    // A `dyn` return absorbs anything (the escape): no mismatch.
    assert!(codes("fn f(): dyn { return \"x\"; }\n").is_empty());
}

#[test]
fn a_nested_fn_return_does_not_clobber_the_enclosing_one() {
    // The inner `fn inner(): string` and the outer `fn outer(): int` each check their own return;
    // neither bleeds into the other (saved/restored `current_ret`).
    let src = "fn outer(): int {\n  fn inner(): string { return \"s\"; }\n  return 1;\n}\n";
    assert!(codes(src).is_empty());
}
