//! Unit tests for the checker, driven through the real lexer/parser so the AST shapes are
//! exactly what the pipeline produces. Conformance `.lang` cases (positive + negative) carry
//! the end-to-end coverage; these pin specific rules in isolation.

use super::{check, resolve_type_of_sites};
use lang_ast::reflect::TypeRepr;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

/// Parse `text` and return the resolved full-fidelity `TypeRepr`s for its `type_of` sites, in no
/// particular order (one program under test has a single site, so order is irrelevant).
fn type_of_reprs(text: &str) -> Vec<TypeRepr> {
    let source = Source::new(SourceId::FIRST, "test.lang", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    resolve_type_of_sites(&parsed.program)
        .into_values()
        .collect()
}

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
fn exhaustive_union_type_pattern_match_is_clean() {
    // A union is a closed domain: covering every member with `is T` arms is exhaustive, no `_`.
    let src = "fn f(x: int | string): int { return match x { is int => 1, is string => 2 }; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn non_exhaustive_union_type_pattern_match_is_reported() {
    // Omitting a union member from a type-pattern match is E0011.
    let src =
        "fn f(x: int | string | bool): int { return match x { is int => 1, is string => 2 }; }\n";
    assert_eq!(codes(src), ["E0011"]);
}

#[test]
fn dyn_type_pattern_match_without_wildcard_is_reported() {
    // `dyn` is the open top — a finite set of `is T` arms cannot exhaust it, so a `_` is required.
    let src = "fn f(x: dyn): int { return match x { is int => 1, is string => 2 }; }\n";
    assert_eq!(codes(src), ["E0011"]);
}

#[test]
fn type_pattern_arm_narrows_the_scrutinee() {
    // Inside an `is int` arm the identifier scrutinee is seen as `int`, so an `int`-only use type-
    // checks; a `string` member used as `int` would not. Clean here proves the narrowing applies.
    let src =
        "fn f(x: int | string): int { return match x { is int => x + 1, is string => 0 }; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn statement_if_is_guard_narrows_the_then_body() {
    // A union is not numeric, so `x + 1` is E0007 on the bare `x` (the baseline) but clean inside
    // an `if x is int { … }` guard, where `x` is narrowed to `int`.
    let bare = "fn f(x: int | string): int { return x + 1; }\n";
    assert_eq!(codes(bare), ["E0007"]);
    let guarded = "fn f(x: int | string): int { if x is int { return x + 1; } return 0; }\n";
    assert!(codes(guarded).is_empty());
}

#[test]
fn statement_if_narrowing_does_not_escape_the_guard() {
    // The narrowing is scoped to the then-body: after the `if`, `x` is the union again, so the
    // `x + 1` outside the guard is E0007.
    let src = "fn f(x: int | string): int { if x is int { return 0; } return x + 1; }\n";
    assert_eq!(codes(src), ["E0007"]);
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
fn generic_function_with_bound_is_clean() {
    // A free generic function `<T: Comparable>`: `T` is in scope for the parameters and return,
    // the bound names a real built-in trait, and operations on `T` defer to runtime. Clean.
    let src = "fn max<T: Comparable>(a: T, b: T): T {\n  if a > b { return a; }\n  return b;\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn unknown_bound_on_type_parameter_is_reported() {
    // A bound must name a built-in trait; `Ordered` is not one, so it is `E0014`.
    let src = "fn f<T: Ordered>(a: T): T { return a; }\n";
    assert_eq!(codes(src), ["E0014"]);
}

#[test]
fn multiple_bounds_on_a_type_parameter_are_accepted() {
    // `<T: Comparable + Display>` — both bounds name built-in traits, so no diagnostic.
    let src = "fn f<T: Comparable + Display>(a: T): T { return a; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn generic_function_parameter_is_out_of_scope_outside_its_body() {
    // `T` is erased and scoped to `f`; naming it in a sibling function's annotation is `E0013`.
    let src = "fn f<T: Comparable>(a: T): T { return a; }\nfn g(x: T): T { return x; }\n";
    assert_eq!(codes(src), ["E0013", "E0013"]);
}

#[test]
fn generic_call_instantiates_return_type() {
    // `id(x)` returns the substituted `T`, not `dyn`: passing a `string` result where an `int` is
    // expected is a concrete mismatch (proving instantiation, since `dyn` would defer silently).
    let src = "fn id<T>(x: T): T { return x; }\n\
               fn need_int(n: int): int { return n; }\n\
               echo need_int(id(\"x\"));\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn associated_call_is_typed_precisely() {
    // `Box.new(1)` resolves to `Box` (not a hole), so passing it where an `int` is expected is a
    // concrete mismatch — proving the associated-call result is the constructor's return type.
    let src = "class Box<T> {\n  value: T\n  fn new(v: T): Box<T> { return Box { value: v }; }\n}\n\
               fn need_int(n: int): int { return n; }\n\
               echo need_int(Box.new(1));\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn literal_infers_its_type_arguments_from_fields() {
    // `Box { value: <v> }` infers `T` from the field value, so the element type is tracked: a
    // string-built box's field is a `string` (mismatch against `int`), an int-built one is clean.
    let cls = "class Box<T> { value: T }\nfn need_int(n: int): int { return n; }\n";
    let bad = format!("{cls}b = Box {{ value: \"hi\" }};\necho need_int(b.value);\n");
    assert_eq!(codes(&bad), ["E0007"]);
    let ok = format!("{cls}b = Box {{ value: 5 }};\necho need_int(b.value);\n");
    assert!(codes(&ok).is_empty());
}

#[test]
fn instance_keeps_its_type_argument() {
    // `Box<int>` tracks its element type through the instance: `b.get()` is `int` (passes where an
    // `int` is wanted), while `Box<string>.get()` is `string` (a mismatch against `int`).
    let cls = "class Box<T> { value: T\n  fn new(v: T): Box<T> { return Box { value: v }; }\n  fn get(): T { return value; } }\n\
               fn need_int(n: int): int { return n; }\n";
    let ok = format!("{cls}b = Box.new(1);\necho need_int(b.get());\n");
    assert!(codes(&ok).is_empty());
    let bad = format!("{cls}b = Box.new(\"hi\");\necho need_int(b.get());\n");
    assert_eq!(codes(&bad), ["E0007"]);
}

#[test]
fn generic_class_enforces_its_bound_at_construction() {
    // `Pair<T: Comparable>` constructed with a non-`Comparable` record is `E0025`; with an `int`,
    // clean. The class's bound is instantiated from the constructor argument.
    let cls =
        "class Pair<T: Comparable> { a: T\n  fn new(x: T): Pair<T> { return Pair { a: x }; } }\n";
    let bad = format!("type Bad = {{ v: int }};\n{cls}p = Pair.new(Bad {{ v: 1 }});\n");
    assert_eq!(codes(&bad), ["E0025"]);
    let good = format!("{cls}p = Pair.new(7);\n");
    assert!(codes(&good).is_empty());
}

#[test]
fn ordering_on_an_unbounded_type_parameter_is_reported() {
    // Body-side: `<` on an unbounded `T` is rejected at the definition (one diagnostic for the
    // comparison); adding `: Comparable` licenses it.
    let unbounded = "fn less<T>(a: T, b: T): bool { return a < b; }\n";
    assert_eq!(codes(unbounded), ["E0025"]);
    let bounded = "fn less<T: Comparable>(a: T, b: T): bool { return a < b; }\n";
    assert!(codes(bounded).is_empty());
}

#[test]
fn arithmetic_on_an_unbounded_type_parameter_is_reported() {
    // The operator-trait check is not ordering-only: `+` on an unbounded `T` needs `Add`.
    let unbounded = "fn sum<T>(a: T, b: T): T { return a + b; }\n";
    assert_eq!(codes(unbounded), ["E0025"]);
    let bounded = "fn sum<T: Add>(a: T, b: T): T { return a + b; }\n";
    assert!(codes(bounded).is_empty());
}

#[test]
fn arithmetic_on_a_concrete_type_without_the_trait_is_reported() {
    // A concrete user type that does not `impl Add` cannot be used with `+`: `E0007` (the runtime's
    // "cannot apply"), now caught statically. A type that *does* `impl Add` is accepted.
    let bad = "type P = { x: int };\na = P { x: 1 };\nb = P { x: 2 };\necho a + b;\n";
    assert_eq!(codes(bad), ["E0007"]);
    let good = "class M { n: int\n  impl Add { fn add(o: M): M { return o; } } }\n\
                a = M { n: 1 };\nb = M { n: 2 };\necho a + b;\n";
    assert!(codes(good).is_empty());
}

#[test]
fn ordering_on_a_concrete_non_comparable_type_is_reported() {
    // Ordering now checks concrete types too: a record that does not derive/`impl Comparable` is
    // `E0007`; a `@derive(Comparable)` type is accepted.
    let bad = "type P = { x: int };\na = P { x: 1 };\nb = P { x: 2 };\necho a < b;\n";
    assert_eq!(codes(bad), ["E0007"]);
    let good = "@derive(Comparable)\nclass V { n: int }\n\
                a = V { n: 1 };\nb = V { n: 2 };\necho a < b;\n";
    assert!(codes(good).is_empty());
}

#[test]
fn generic_call_with_satisfied_primitive_bound_is_clean() {
    let src = "fn max<T: Comparable>(a: T, b: T): T { if a > b { return a; } return b; }\n\
               echo max(1, 2);\n";
    assert!(codes(src).is_empty());
}

#[test]
fn generic_call_violating_a_bound_is_reported() {
    // A record literal has the concrete type `P`, which does not satisfy `Comparable`: `E0025`.
    let src = "type P = { x: int };\n\
               fn max<T: Comparable>(a: T, b: T): T { return a; }\n\
               echo max(P { x: 1 }, P { x: 2 });\n";
    assert_eq!(codes(src), ["E0025"]);
}

#[test]
fn generic_call_argument_mismatch_after_binding_is_reported() {
    // The first argument pins `T = int`; the second (`string`) is checked against `int`: `E0007`.
    let src = "fn max<T: Comparable>(a: T, b: T): T { return a; }\n\
               echo max(3, \"x\");\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn user_type_deriving_the_bound_satisfies_it() {
    let src = "@derive(Comparable)\nclass B { n: int }\n\
               fn max<T: Comparable>(a: T, b: T): T { return a; }\n\
               echo max(B { n: 1 }, B { n: 2 });\n";
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
fn deriving_the_same_trait_twice_is_conflicting() {
    // Coherence: a trait may be implemented at most once per type.
    let src = "@derive(Comparable, Comparable)\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0027"]);
}

#[test]
fn deriving_and_impl_of_same_trait_conflict() {
    // `@derive(Display)` synthesizes `to_string`; an `impl Display` writes one by hand — two
    // competing implementations of one trait.
    let src = "@derive(Display)\nclass P {\n  x: int\n  impl Display {\n    fn to_string(): string { return \"P\"; }\n  }\n}\n";
    assert_eq!(codes(src), ["E0027"]);
}

#[test]
fn two_impl_blocks_for_same_trait_conflict() {
    let src = "class P {\n  x: int\n  impl Add {\n    fn add(other: P): P { return other; }\n  }\n  impl Add {\n    fn add(other: P): P { return other; }\n  }\n}\n";
    assert_eq!(codes(src), ["E0027"]);
}

#[test]
fn standalone_marker_impl_is_accepted() {
    // A bodiless record declares a capability via a same-module standalone `impl` — the mechanism
    // that lets a record (which has no body) participate in a trait. No diagnostics.
    let src = "type Route = { path: string };\nimpl Attribute for Route {}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn standalone_impl_for_undeclared_type_is_orphan() {
    // The orphan rule: a standalone `impl` may only target a type declared in this module.
    let src = "impl Attribute for Widget {}\n";
    assert_eq!(codes(src), ["E0013"]);
}

#[test]
fn standalone_impl_counts_toward_coherence() {
    // Coherence spans all three implementation forms: a `@derive` and a standalone `impl` of the
    // same trait for one type conflict, just like two derives or two in-body impls.
    let src = "@derive(Clone)\ntype Route = { path: string };\nimpl Clone for Route {}\n";
    assert_eq!(codes(src), ["E0027"]);
}

#[test]
fn standalone_impl_with_methods_is_unsupported() {
    // Pass 1 supports only empty-body capability impls; a body with methods is rejected (E0015).
    let src = "type Route = { path: string };\nimpl Attribute for Route {\n  fn extra(): int { return 1; }\n}\n";
    assert_eq!(codes(src), ["E0015"]);
}

#[test]
fn attributes_of_on_an_attribute_type_checks_clean() {
    // `attributes_of::<Route>()` is `List<Attributed<Route>>`, so `r.target` is a string and
    // `r.value` is a `Route` whose `.path` is a string — all resolve without diagnostics.
    let src = "type Route = { path: string };\nimpl Attribute for Route {}\n#[Route(\"/x\")]\ntype Users = { id: int };\nfor r in attributes_of::<Route>() {\n  echo r.target;\n  echo r.value.path;\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_function_checks_clean() {
    // A `#[...]` on a function is validated like one on a type: the capability gate plus the
    // all-fields construction check, both satisfied here.
    let src = "type Route = { method: string };\nimpl Attribute for Route {}\n#[Route(\"GET\")]\nfn greet(): string { return \"hi\"; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_method_checks_clean() {
    // The same validation reaches a class method's attributes (through `check_fn`).
    let src = "type Route = { method: string };\nimpl Attribute for Route {}\nclass Api {\n  id: int\n  #[Route(\"GET\")]\n  fn list(): string { return \"[]\"; }\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_function_must_be_an_attribute() {
    // The E0029 gate applies on a function too: `Plain` is a record but not an `Attribute`.
    let src = "type Plain = { method: string };\n#[Plain(\"GET\")]\nfn greet(): string { return \"hi\"; }\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attribute_on_record_and_class_fields_checks_clean() {
    // A `#[...]` on a record field and on a class field is validated like any other attribute use.
    let src = "type Column = { name: string };\nimpl Attribute for Column {}\ntype User = {\n  #[Column(\"uid\")]\n  id: int,\n};\nclass Account {\n  #[Column(\"bal\")]\n  balance: int\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_field_must_be_an_attribute() {
    // The E0029 gate reaches field attributes too.
    let src = "type Plain = { name: string };\ntype User = {\n  #[Plain(\"x\")]\n  id: int,\n};\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attachable_to_permits_a_listed_kind() {
    // `@attachableTo(Function)` allows the attribute on a top-level function.
    let src = "@attachableTo(Function)\ntype Route = { method: string };\nimpl Attribute for Route {}\n#[Route(\"GET\")]\nfn greet(): string { return \"hi\"; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attachable_to_rejects_an_unlisted_kind() {
    // `@attachableTo(Method)` forbids the attribute on a type declaration → E0030.
    let src = "@attachableTo(Method)\ntype Route = { method: string };\nimpl Attribute for Route {}\n#[Route(\"GET\")]\ntype User = { id: int };\n";
    assert_eq!(codes(src), ["E0030"]);
}

#[test]
fn attachable_to_with_an_unknown_kind_is_rejected() {
    // The kind vocabulary is closed; an unknown name in the directive is E0030.
    let src =
        "@attachableTo(Bogus)\ntype Route = { method: string };\nimpl Attribute for Route {}\n";
    assert_eq!(codes(src), ["E0030"]);
}

#[test]
fn attachable_to_field_only_attribute_rejects_a_method() {
    // A field-only attribute (`@attachableTo(Field)`) on a method is E0030 — exercising the
    // method/function target axis added in P2.4.
    let src = "@attachableTo(Field)\ntype Column = { name: string };\nimpl Attribute for Column {}\nclass Api {\n  #[Column(\"x\")]\n  fn list(): int { return 0; }\n}\n";
    assert_eq!(codes(src), ["E0030"]);
}

#[test]
fn attribute_on_an_enum_variant_checks_clean() {
    // A `#[...]` on an enum variant (plain or algebraic) is validated like any other attribute use.
    let src = "type Note = { text: string };\nimpl Attribute for Note {}\nenum Status {\n  Active;\n  #[Note(\"gone\")]\n  Archived;\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_variant_must_be_an_attribute() {
    // The E0029 gate reaches enum-variant attributes too.
    let src = "type Plain = { text: string };\nenum Status {\n  #[Plain(\"x\")]\n  Active;\n}\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attributes_of_on_a_non_attribute_is_rejected() {
    // The capability gate, mirroring a `#[Foo]` use: the type argument must implement `Attribute`.
    let src = "type Plain = { path: string };\nrs = attributes_of::<Plain>();\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn type_of_resolves_concrete_operand_to_full_fidelity_repr() {
    // A list literal's element type is statically known, so the site resolves to the precise
    // recursive `TypeRepr` (List(Int)) the backends bake as a constant (fidelity A).
    assert_eq!(
        type_of_reprs("x = type_of([1, 2, 3]);\n"),
        vec![TypeRepr::List(Box::new(TypeRepr::Int))]
    );
}

#[test]
fn type_of_on_a_dyn_operand_has_no_static_resolution() {
    // A `dyn`-typed operand carries no fixed head constructor to bake, so the site is absent from
    // the map and falls back to the runtime head-constructor path (fidelity B).
    let src = "fn as_dyn(v: dyn): dyn { return v; }\nx = type_of(as_dyn(5));\n";
    assert!(type_of_reprs(src).is_empty());
}

#[test]
fn type_of_synthesizes_the_prelude_type_enum() {
    // `type_of(v)` is the prelude `Type` enum, pattern-matchable; its payload bindings carry `Type`
    // (here `Type.List(e)` binds `e: Type`, matched again against `Type.Dyn`) — all check clean.
    let src = "x = type_of(5);\nlabel = match x {\n  Type.Int => \"int\",\n  Type.List(e) => match e { Type.Dyn => \"dyn\", _ => \"?\" },\n  _ => \"other\",\n};\necho label;\n";
    assert!(codes(src).is_empty());
}

#[test]
fn deriving_distinct_traits_with_an_impl_is_coherent() {
    // Different traits never conflict — only a repeated one does.
    let src = "@derive(Equatable, Comparable)\nclass P {\n  x: int\n  impl Add {\n    fn add(other: P): P { return other; }\n  }\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn narrowing_a_dyn_value_is_clean() {
    // `x.as<int>()` on a `dyn` value is the sanctioned way out of the open top; it types as `?int`.
    let src = "fn f(x: dyn): ?int {\n  return x.as<int>();\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn narrowing_a_concrete_value_is_rejected() {
    // Narrowing only makes sense out of an open type; an already-concrete `int` has nothing to narrow.
    let src = "fn f(n: int): ?int {\n  return n.as<int>();\n}\n";
    assert_eq!(codes(src), ["E0028"]);
}

#[test]
fn narrowing_out_of_a_union_is_clean() {
    // A union is a closed `dyn`, so narrowing a member back out is allowed (not E0028).
    let src = "fn f(x: int | string): ?int {\n  return x.as<int>();\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn union_member_argument_is_accepted() {
    // A value of any member widens into the union (`int <: int | string`), so both calls are clean.
    let src = "fn f(x: int | string): string { return \"v\"; }\nf(1);\nf(\"a\");\n";
    assert!(codes(src).is_empty());
}

#[test]
fn union_non_member_argument_is_rejected() {
    // `bool` is not a member of `int | string`, so the argument does not widen in (E0007).
    let src = "fn f(x: int | string): string { return \"v\"; }\nf(true);\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn union_with_an_unknown_member_is_reported() {
    // Each member of a union is validated like any annotation — an undeclared one is E0013.
    let src = "fn f(x: int | Bogus): string { return \"v\"; }\n";
    assert_eq!(codes(src), ["E0013"]);
}

#[test]
fn narrowing_to_an_unknown_type_is_reported() {
    // The narrowing target is validated like any type annotation — an undeclared name is E0013.
    let src = "fn f(x: dyn): void {\n  x.as<Bogus>();\n}\n";
    assert_eq!(codes(src), ["E0013"]);
}

#[test]
fn type_test_synthesizes_bool() {
    use lang_types::Type;
    // `x is T` is a `bool` regardless of the source — it satisfies a `bool` expectation and
    // violates a non-`bool` one (the same E0007 path). A concrete source is fine (no E0028).
    assert!(check_value_against("5 is int", Type::Bool).is_empty());
    assert!(check_value_against("\"hi\" is int", Type::Bool).is_empty());
    assert_eq!(check_value_against("5 is int", Type::Int), ["E0007"]);
}

#[test]
fn type_test_against_an_unknown_type_is_reported() {
    // The tested type is validated like any annotation — an undeclared name is E0013.
    let src = "fn f(x: dyn): bool {\n  return x is Bogus;\n}\n";
    assert_eq!(codes(src), ["E0013"]);
}

#[test]
fn old_derive_attribute_spelling_is_reported() {
    // `#[derive(...)]` is the old codegen spelling; it is now `@derive(...)`.
    let src = "#[derive(Equatable)]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0017"]);
}

#[test]
fn data_attribute_marked_with_capability_is_accepted() {
    // `#[Route(...)]` is valid when `Route` is a record/class marked `impl Attribute for Route`,
    // and the arguments construct it (the positional value fills `path`).
    let src = "type Route = { path: string };\nimpl Attribute for Route {}\n#[Route(\"/x\")]\nclass P {\n  x: int\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn unmarked_attribute_is_rejected() {
    // The capability gate: a `#[Foo]` whose `Foo` does not implement `Attribute` is E0029.
    let src = "#[Route]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attribute_missing_field_is_reported() {
    // The construction check: `#[Route]` with no argument leaves `path` unset (E0009).
    let src = "type Route = { path: string };\nimpl Attribute for Route {}\n#[Route]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0009"]);
}

#[test]
fn attribute_argument_type_mismatch_is_reported() {
    // The construction check: a literal whose type does not match its field is E0007.
    let src = "type Route = { path: string };\nimpl Attribute for Route {}\n#[Route(42)]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0007"]);
}

// ----- bidirectional check-mode (white-box) -----
//
// Production callers feed real expectations through `Checker::check` (declared returns, argument
// types, declared element types). These white-box tests drive it directly with concrete
// expectations to pin down subsumption and inward propagation in isolation.

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

// ----- stdlib type knowledge (S3a) -----
//
// Each of these only fails (E0007) if the method/prelude/module/index/user-method result is typed
// concretely; if it were `Unknown` the return-type check would be suppressed. So they double as a
// proof that the type flows, against the return-type expectation from S2.

#[test]
fn string_and_list_methods_are_typed() {
    assert_eq!(codes("fn f(): int { return \"ab\".upper(); }\n"), ["E0007"]); // upper -> string
    assert!(codes("fn f(): string { return \"ab\".upper(); }\n").is_empty());
    assert_eq!(
        codes("fn f(): int { return [1, 2].sorted(); }\n"),
        ["E0007"]
    ); // sorted -> List<int>
    assert_eq!(codes("fn f(): int { return [1].first(); }\n"), ["E0007"]); // first -> Option<int>
    // Chaining flows the type through: split -> List<string>, count -> int.
    assert!(codes("fn f(): int { return \"a b\".split(\" \").count(); }\n").is_empty());
}

#[test]
fn prelude_functions_are_typed() {
    assert!(codes("fn f(): int { return len([1, 2, 3]); }\n").is_empty()); // len -> int
    assert_eq!(codes("fn f(): string { return len([1]); }\n"), ["E0007"]);
    assert!(codes("fn f(): int { return sum([1, 2]); }\n").is_empty()); // sum(List<int>) -> int
}

#[test]
fn module_calls_are_typed() {
    // math.sqrt -> float, not int.
    let bad = "use std.{math};\nfn f(): int { return math.sqrt(4.0); }\n";
    assert_eq!(codes(bad), ["E0007"]);
    let good = "use std.{math};\nfn f(): float { return math.sqrt(4.0); }\n";
    assert!(codes(good).is_empty());
    // fs.read -> string.
    let fs = "use std.{fs};\nfn f(): int { return fs.read(\"p\"); }\n";
    assert_eq!(codes(fs), ["E0007"]);
}

#[test]
fn indexing_is_typed() {
    // xs[0] is a string element, not an int.
    assert_eq!(
        codes("fn f(xs: List<string>): int { return xs[0]; }\n"),
        ["E0007"]
    );
    assert!(codes("fn f(xs: List<int>): int { return xs[0]; }\n").is_empty());
}

#[test]
fn user_method_returns_are_typed() {
    let src = "class C {\n  x: int\n  fn label(): string { return \"c\"; }\n}\n\
               fn f(c: C): int { return c.label(); }\n";
    assert_eq!(codes(src), ["E0007"]); // label() -> string, not int
}

// ----- strict checks now possible with stdlib types (S3b) -----

#[test]
fn heterogeneous_list_literal_is_rejected() {
    // Concretely-incompatible elements in synthesis position are a static error.
    assert_eq!(codes("echo [1, \"two\", 3];\n"), ["E0007"]);
    // Homogeneous and numeric-promoting lists are fine.
    assert!(codes("echo [1, 2, 3];\n").is_empty());
    assert!(codes("echo [1, 2.5];\n").is_empty());
    assert!(codes("echo [\"a\", \"b\"];\n").is_empty());
    // An empty list has no conflicting elements.
    assert!(codes("echo [];\n").is_empty());
    // A mixed list is allowed when checked against an explicit `List<dyn>`.
    assert!(codes("fn f(): List<dyn> { return [1, \"two\"]; }\n").is_empty());
}

#[test]
fn indexing_a_primitive_is_rejected() {
    assert_eq!(codes("echo 42[0];\n"), ["E0007"]);
    assert_eq!(codes("echo true[0];\n"), ["E0007"]);
    // Indexable receivers are fine.
    assert!(codes("echo [1, 2][0];\n").is_empty());
    assert!(codes("echo \"hi\"[0];\n").is_empty());
}

#[test]
fn calling_an_unknown_method_on_a_primitive_is_rejected() {
    // Parallel to the non-indexable check: a concrete primitive has a closed method set.
    assert_eq!(codes("echo (42).upper();\n"), ["E0007"]);
    assert_eq!(codes("echo true.foo();\n"), ["E0007"]);
    // `compare` is defined on every value (Comparable), so it resolves.
    assert!(codes("echo (1).compare(2);\n").is_empty());
    // Receivers with method tables and deferred receivers stay lenient here.
    assert!(codes("echo \"hi\".upper();\n").is_empty());
    assert!(codes("fn f(x: dyn): dyn { return x.whatever(); }\n").is_empty());
}

#[test]
fn argument_arity_is_checked() {
    assert_eq!(codes("echo \"hi\".upper(\"extra\");\n"), ["E0007"]); // upper takes 0
    assert!(codes("echo \"hi\".upper();\n").is_empty());
    // User functions are arity-checked too.
    assert_eq!(
        codes("fn add(a: int, b: int): int { return a + b; }\necho add(1);\n"),
        ["E0007"]
    );
    assert!(codes("fn add(a: int, b: int): int { return a + b; }\necho add(1, 2);\n").is_empty());
}

#[test]
fn argument_types_are_checked() {
    assert_eq!(codes("echo [1, 2].join(5);\n"), ["E0007"]); // join wants a string
    assert!(codes("echo [1, 2].join(\", \");\n").is_empty());
    // Numeric widening: an int argument is accepted for a float parameter.
    let m = "use std.{math};\n";
    assert!(codes(&format!("{m}echo math.sqrt(4);\n")).is_empty());
    assert_eq!(codes(&format!("{m}echo math.sqrt(\"x\");\n")), ["E0007"]);
}

#[test]
fn generic_method_arguments_are_not_false_positives() {
    // A generic parameter is erased to `dyn`, so any concrete argument is accepted.
    let src = "class Box<T> {\n  value: T\n  fn set(v: T): void { value = v; }\n}\n\
               fn f(b: Box<int>): void { b.set(5); }\n";
    assert!(codes(src).is_empty());
}

// ----- list concatenation via `~` (L1) -----

#[test]
fn concat_of_two_lists_is_a_list() {
    use lang_types::Type;
    // `~` on two lists yields a list of the unified element type, not a string.
    assert!(check_value_against("[1, 2] ~ [3]", Type::List(Box::new(Type::Int))).is_empty());
    assert_eq!(check_value_against("[1, 2] ~ [3]", Type::String), ["E0007"]);
    // Element types unify (int/float promote); a concrete clash widens to `List<dyn>`.
    assert!(check_value_against("[1] ~ [2.5]", Type::List(Box::new(Type::Float))).is_empty());
    // Display-concatenation is unchanged for non-list operands.
    assert!(check_value_against("\"a\" ~ 1", Type::String).is_empty());
}

#[test]
fn concat_result_flows_through_a_signature() {
    assert!(codes("fn f(): List<int> { return [1] ~ [2]; }\n").is_empty());
    assert_eq!(codes("fn f(): string { return [1] ~ [2]; }\n"), ["E0007"]);
}

// ----- E0023 cannot-infer endpoint (S3c.4) -----

#[test]
fn immutable_context_free_literal_binding_is_e0023() {
    // An immutable binding to a zero-information literal has no way to fix its type and is not
    // annotated — `E0023`.
    assert_eq!(codes("xs = [];\n"), ["E0023"]);
    assert_eq!(codes("m = {};\n"), ["E0023"]);
    assert_eq!(codes("x = none;\n"), ["E0023"]);
    // `Ok(x)`/`Err(e)` leave the opposite `Result` slot a hole, so an immutable, un-annotated
    // binding to one is undeterminable just like the empties.
    assert_eq!(codes("r = Ok(5);\n"), ["E0023"]);
    assert_eq!(codes("r = Err(\"boom\");\n"), ["E0023"]);
    // `some(x)` fully determines its `Option`, so it is not flagged.
    assert!(codes("o = some(5);\n").is_empty());
    // An annotation fixes the open slot.
    assert!(codes("r: Result<int, string> = Ok(5);\n").is_empty());
}

#[test]
fn never_reassigned_mut_context_free_literal_is_e0023() {
    // A `mut` binding is exempt from E0023 only because a later write can supply its type. When no
    // such write exists, its type stays an undeterminable hole — the `mut` analogue of the error.
    assert_eq!(codes("mut acc = [];\necho acc;\n"), ["E0023"]);
    assert_eq!(codes("mut r = Ok(5);\necho r;\n"), ["E0023"]);
    // A later reassignment (here, inside a loop) resolves the type — exempt again.
    assert!(codes("mut acc = [];\nfor x in [1, 2] { acc = acc ~ [x]; }\necho acc;\n").is_empty());
    // A reassignment in a nested `if` body also counts.
    assert!(codes("mut acc = [];\nif true { acc = [1]; }\necho acc;\n").is_empty());
    // ...and one inside a `while` body — the gap-3 walk descends `while` like `if`/`for`.
    assert!(
        codes("mut acc = [];\nmut i = 0;\nwhile i < 3 { acc = acc ~ [i]; i += 1; }\necho acc;\n")
            .is_empty()
    );
    // An annotation resolves it without any reassignment.
    assert!(codes("mut acc: List<int> = [];\necho acc;\n").is_empty());
}

#[test]
fn e0023_is_fixed_by_an_annotation_or_a_mut_accumulator() {
    // An annotation resolves the element/payload type.
    assert!(codes("xs: List<int> = [];\n").is_empty());
    assert!(codes("m: Map<string, int> = {};\n").is_empty());
    assert!(codes("x: ?int = none;\n").is_empty());
    // A `mut` accumulator is exempt — its later writes supply the type.
    assert!(codes("mut acc = [];\nfor i in [1, 2] { acc = acc ~ [i]; }\necho acc;\n").is_empty());
}

#[test]
fn e0023_does_not_fire_in_expression_position_or_on_typed_values() {
    // Empty collections in expression position are fine — only a *binding* commits to a type.
    assert!(codes("echo [];\necho len([]);\necho [].first();\n").is_empty());
    // A non-empty literal infers its elements; a typed value carries its type — neither is E0023.
    assert!(codes("xs = [1, 2, 3];\nm = {\"a\": 1};\n").is_empty());
}

// ----- assignment updates the declaring scope; accumulators infer (L3) -----

#[test]
fn accumulator_element_type_infers_and_persists_past_the_loop() {
    // `mut acc = []` starts as a list of an unknown element; the loop-body reassignment
    // `acc = acc ~ [x]` refines it to `List<int>` in acc's *declaring* scope, so the post-loop
    // `return acc` satisfies a `List<int>` signature and violates a `List<string>` one.
    let ok = "fn build(xs: List<int>): List<int> {\n  mut acc = [];\n  for x in xs { acc = acc ~ [x]; }\n  return acc;\n}\n";
    assert!(codes(ok).is_empty());
    let bad = "fn build(xs: List<int>): List<string> {\n  mut acc = [];\n  for x in xs { acc = acc ~ [x]; }\n  return acc;\n}\n";
    assert_eq!(codes(bad), ["E0007"]);
}

#[test]
fn compound_assignment_accumulator_infers_like_plain_concat() {
    // `acc ~= [x]` desugars to `acc = acc ~ [x]`, so it threads the same accumulator inference:
    // `acc` resolves to `List<int>` and is checked against the declared return.
    let ok = "fn build(xs: List<int>): List<int> {\n  mut acc = [];\n  for x in xs { acc ~= [x]; }\n  return acc;\n}\n";
    assert!(codes(ok).is_empty());
    let bad = "fn build(xs: List<int>): List<string> {\n  mut acc = [];\n  for x in xs { acc ~= [x]; }\n  return acc;\n}\n";
    assert_eq!(codes(bad), ["E0007"]);
}

#[test]
fn nested_reassignment_updates_the_outer_binding() {
    // A reassignment inside an `if` updates the outer binding's type, not a block-local shadow.
    let src = "mut x = 1;\nif true { x = \"now a string\"; }\ns: string = x;\n";
    assert!(codes(src).is_empty());
}

// ----- list spread `[...xs, x]` (L2, desugars to `~`) -----

#[test]
fn list_spread_types_as_the_unified_list() {
    use lang_types::Type;
    let li = |t| Type::List(Box::new(t));
    // `[...xs, x]` desugars to `[] ~ xs ~ [x]`, so it types as the unified element list.
    assert!(codes("fn f(xs: List<int>): List<int> { return [...xs, 99]; }\n").is_empty());
    // A spread element of the wrong type is caught through the concat result.
    assert_eq!(
        codes("fn f(xs: List<int>): List<string> { return [...xs]; }\n"),
        ["E0007"]
    );
    // Spread + literal element of disagreeing types widens to `List<dyn>`.
    assert!(check_value_against("[...[1, 2], \"x\"]", li(Type::Dyn)).is_empty());
}

#[test]
fn range_types_as_a_list_of_int() {
    use lang_types::Type;
    let li = |t| Type::List(Box::new(t));
    // A range is a `List<int>`, so it satisfies a `List<int>` annotation and drives a for-loop.
    assert!(codes("xs: List<int> = 0..5;\n").is_empty());
    assert!(check_value_against("0..=10", li(Type::Int)).is_empty());
    // Non-int bounds are a static error; `dyn` bounds defer.
    assert_eq!(codes("echo 1.5..3;\n"), ["E0007"]);
    assert!(codes("fn f(a: dyn, b: int): List<int> { return a..b; }\n").is_empty());
}

#[test]
fn break_continue_outside_a_loop_is_e0024() {
    // Inside a loop (including a nested `if`) they are fine.
    assert!(codes("for i in 0..3 { if i == 1 { continue; } echo i; }\n").is_empty());
    assert!(codes("mut n = 0;\nwhile n < 3 { n += 1; break; }\n").is_empty());
    // Outside any loop, each is E0024.
    assert_eq!(codes("break;\n"), ["E0024"]);
    assert_eq!(codes("continue;\n"), ["E0024"]);
    // A loop does not leak across a function boundary: `break` in a nested `fn` is still outside a
    // loop, even when the `fn` is declared inside one.
    assert_eq!(
        codes("for i in 0..3 { fn f(): int { break; return 0; } }\n"),
        ["E0024"]
    );
}

#[test]
fn spreading_a_non_list_is_rejected() {
    // `...` requires a list operand; a concrete non-list is an error (not display-concatenation).
    // It still resolves list-shaped, so there is no cascading second diagnostic.
    assert_eq!(codes("echo [...42];\n"), ["E0007"]);
    assert_eq!(codes("echo [...\"hi\"];\n"), ["E0007"]);
    // A `dyn` operand defers (its membership is unknown until runtime) and a list passes through.
    assert!(codes("fn f(x: dyn): List<dyn> { return [...x]; }\n").is_empty());
    assert!(codes("fn f(xs: List<int>): List<int> { return [...xs]; }\n").is_empty());
}

// ----- optional binding annotations (S3c.2) -----

#[test]
fn annotated_binding_checks_its_value_against_the_annotation() {
    assert!(codes("x: int = 5;\n").is_empty());
    assert_eq!(codes("x: int = \"s\";\n"), ["E0007"]);
    // The annotation resolves an otherwise context-free literal: `[]` against `List<int>`.
    assert!(codes("xs: List<int> = [];\n").is_empty());
    assert!(codes("mut acc: List<string> = [];\n").is_empty());
}

#[test]
fn binding_annotation_is_resolved_like_any_type() {
    // An unknown type in a binding annotation is the same `E0013` as anywhere else; the value is
    // then also checked against it (an int is not assignable to an unknown `Ghost`).
    assert_eq!(codes("x: Ghost = 5;\n"), ["E0013", "E0007"]);
}

#[test]
fn annotated_binding_type_flows_to_later_uses() {
    // The binding is bound at its annotated type, so a later concrete misuse is caught: a
    // `string`-typed binding flowing into an `int`-annotated one is a mismatch.
    assert_eq!(codes("s: string = \"hi\";\nn: int = s;\n"), ["E0007"]);
    assert!(codes("s: string = \"hi\";\nt: string = s;\n").is_empty());
    // The method result still flows: `upper()` keeps it a string.
    assert!(codes("s: string = \"hi\";\nu: string = s.upper();\n").is_empty());
    assert_eq!(
        codes("s: string = \"hi\";\nu: int = s.upper();\n"),
        ["E0007"]
    );
}

// ----- contextual propagation + map inference (S3c.1) -----

#[test]
fn option_constructors_check_their_payload_against_the_expectation() {
    // `some(x)` adopts an expected `Option<T>` and checks `x` against `T`.
    assert_eq!(
        codes("fn f(): Option<int> { return some(\"x\"); }\n"),
        ["E0007"]
    );
    assert!(codes("fn f(): Option<int> { return some(1); }\n").is_empty());
    // `?T` is the same expectation.
    assert!(codes("fn f(): ?int { return some(1); }\n").is_empty());
}

#[test]
fn none_resolves_against_an_option_expectation() {
    // `none` adopts `?T`/`Option<T>` instead of leaking a hole — no mismatch.
    assert!(codes("fn f(): ?int { return none; }\n").is_empty());
    assert!(codes("fn f(): Option<string> { return none; }\n").is_empty());
}

#[test]
fn result_constructors_check_each_slot_against_the_expectation() {
    // `Ok(x)` checks `x` against the ok slot; `Err(e)` against the err slot.
    assert_eq!(
        codes("fn f(): Result<int, string> { return Ok(\"x\"); }\n"),
        ["E0007"]
    );
    assert!(codes("fn f(): Result<int, string> { return Ok(1); }\n").is_empty());
    assert_eq!(
        codes("fn f(): Result<int, string> { return Err(1); }\n"),
        ["E0007"]
    );
    assert!(codes("fn f(): Result<int, string> { return Err(\"e\"); }\n").is_empty());
    // `Ok()` carries a unit payload — well typed against `Result<void, E>`.
    assert!(codes("fn f(): Result<void, string> { return Ok(); }\n").is_empty());
}

#[test]
fn map_literal_infers_its_element_types() {
    use lang_types::Type;
    let map = |k, v| Type::Map(Box::new(k), Box::new(v));
    // `{"a": 1}` synthesizes `Map<string, int>` — it satisfies that expectation and violates others.
    assert!(check_value_against("{\"a\": 1}", map(Type::String, Type::Int)).is_empty());
    assert_eq!(
        check_value_against("{\"a\": 1}", map(Type::String, Type::String)),
        ["E0007"]
    );
}

#[test]
fn heterogeneous_map_values_are_rejected() {
    // Concretely-disagreeing values are a static error (the map analogue of heterogeneous lists).
    assert_eq!(codes("echo {\"a\": 1, \"b\": \"two\"};\n"), ["E0007"]);
    // Homogeneous and empty maps are fine.
    assert!(codes("echo {\"a\": 1, \"b\": 2};\n").is_empty());
    assert!(codes("echo {};\n").is_empty());
}

#[test]
fn pipeline_threads_the_piped_value_as_first_arg() {
    // `5 |> add(10)` is `add(5, 10)` — not a one-argument call, so no arity error.
    let src = "fn add(a: int, b: int): int { return a + b; }\necho 5 |> add(10);\n";
    assert!(codes(src).is_empty());
    // `5 |> inc` is `inc(5)`.
    let src2 = "fn inc(n: int): int { return n + 1; }\necho 5 |> inc;\n";
    assert!(codes(src2).is_empty());
}

#[test]
fn parameter_default_omitted_at_call_is_clean() {
    // A trailing default makes its argument optional: the call may omit it or supply it.
    let src = "fn greet(name: string, greeting: string = \"Hi\"): string { return greeting ~ name; }\n\
               echo greet(\"a\");\necho greet(\"a\", \"Yo\");\n";
    assert!(codes(src).is_empty());
}

#[test]
fn required_parameter_after_optional_is_e0026() {
    // Defaults must be trailing-only — a required parameter after a defaulted one is rejected.
    let src = "fn f(a: int = 1, b: int): int { return a + b; }\necho f(1, 2);\n";
    assert_eq!(codes(src), ["E0026"]);
}

#[test]
fn default_value_type_must_match_parameter() {
    // A `string` parameter defaulted to an `int` is a static `E0007`.
    let src = "fn f(x: string = 5): string { return x; }\necho f();\n";
    assert_eq!(codes(src), ["E0007"]);
    // A matching default is clean.
    assert!(codes("fn g(x: int = 5): int { return x; }\necho g();\n").is_empty());
}

#[test]
fn call_below_minimum_arity_is_rejected() {
    // `f` requires `a` and defaults `b`, so it accepts 1 or 2 arguments; zero is too few.
    let src = "fn f(a: int, b: int = 1): int { return a + b; }\necho f();\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn call_above_maximum_arity_is_rejected() {
    // The same `f` accepts at most 2 arguments; three is too many.
    let src = "fn f(a: int, b: int = 1): int { return a + b; }\necho f(1, 2, 3);\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn method_default_omitted_at_call_is_clean() {
    // An instance method may carry a default; omitting it at the call site is well-typed.
    let src = "class C {\n  start: int\n  fn from(start: int): C { return C { start: start }; }\n  \
               fn bump(by: int = 1): int { return start + by; }\n}\n\
               d = C.from(10);\necho d.bump();\necho d.bump(5);\n";
    assert!(codes(src).is_empty());
}

#[test]
fn closure_parameter_default_is_clean() {
    // A closure may default a trailing parameter; omitting it at the call is well-typed.
    let src = "g = fn(n: int, bump: int = 10) => n + bump;\necho g(5);\necho g(5, 1);\n";
    assert!(codes(src).is_empty());
}

#[test]
fn closure_required_after_optional_is_e0026() {
    // The trailing-only rule applies to closures too.
    let src = "g = fn(a: int = 1, b: int) => a + b;\necho g(1, 2);\n";
    assert_eq!(codes(src), ["E0026"]);
}

#[test]
fn closure_default_may_reference_a_captured_variable() {
    // A closure default is checked in the captured (enclosing) scope, so referencing a captured
    // binding is clean (capture-aware) — unlike a named function's globals-only default.
    let src = "fn make(tag: string): dyn {\n  return fn(s: string, label: string = tag) => label ~ s;\n}\necho 1;\n";
    assert!(codes(src).is_empty());
}
