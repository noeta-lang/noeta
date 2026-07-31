//! Unit tests for the checker, driven through the real lexer/parser so the AST shapes are
//! exactly what the pipeline produces. Conformance `.noe` cases (positive + negative) carry
//! the end-to-end coverage; these pin specific rules in isolation.

use super::{check, resolve_type_of_sites};

/// Seed the process-default registry with the std units. Production drivers do this before the
/// checker runs (audit-6 F2 — the checker consumes the registry as data and no longer links the
/// std units); these tests are their own driver, so every funnel below seeds first. Idempotent.
fn seed_std() {
    noeta_stdlib::registry::default_seeded();
}
use noeta_ast::reflect::TypeRepr;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

/// Parse `text` and return the resolved full-fidelity `TypeRepr`s for its `type_of` sites, in no
/// particular order (one program under test has a single site, so order is irrelevant).
fn type_of_reprs(text: &str) -> Vec<TypeRepr> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    resolve_type_of_sites(&parsed.program)
        .into_values()
        .collect()
}

/// Parse `text` and return the checker's diagnostics themselves, for the assertions that pin a
/// message, a help line, or a diagnostic's secondary labels rather than just its code.
fn diagnostics(text: &str) -> Vec<noeta_diagnostics::Diagnostic> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    check(&parsed.program)
}

/// Parse `text` and return the checker's diagnostic codes (wire form), in order.
fn codes(text: &str) -> Vec<String> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
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

// ----- E0012: the `?` POSITION rule, both halves -----
//
// `?` is an early return, so the declared return has to carry what it returns: `?T` for an `Option`'s
// `none`, `Result<T, E>` for a `Result`'s `Err`. The `Result` half was missing, which made
// `fn work(): void { fallible()? }` check clean, discard the error, and exit 0.

#[test]
fn try_on_result_in_a_void_function_is_invalid() {
    let src = "fn g(): Result<int, string> { return Err(\"no\"); }\n\
               fn f(): void { g()?; }\n";
    assert_eq!(codes(src), ["E0012"]);
}

#[test]
fn try_on_result_in_a_typed_function_is_invalid() {
    // A concrete non-`Result` return is the same rule as `void`: an `int` slot cannot hold an `Err`.
    let src = "fn g(): Result<int, string> { return Err(\"no\"); }\n\
               fn f(): int { return g()?; }\n";
    assert_eq!(codes(src), ["E0012"]);
}

#[test]
fn try_on_option_in_a_void_function_is_invalid() {
    // The absence half, asserted beside its twin so the symmetry is visible here and not just in prose.
    let src = "fn f(xs: List<int>): void { xs.first()?; }\n";
    assert_eq!(codes(src), ["E0012"]);
}

#[test]
fn try_on_result_in_a_deferring_function_still_defers() {
    // `dyn` is the gradual escape: the checker cannot say what the early return must fit, so it does
    // not pretend to. The judgement lands at runtime (E0069 if the `Err` reaches the top).
    let src = "fn g(): Result<int, string> { return Err(\"no\"); }\n\
               fn f(): dyn { return g()?; }\n";
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
    // A user-module name brought in by `use` (a non-extension root) is a legal referent in a
    // single-file check: the linker resolves it later (or, in a complete link, flags it — see
    // `noeta-loader`'s `unresolved_user_module_is_an_error`), but an isolated file stays lenient.
    let src = "use App.Models.User;\nfn find(): ?User { return none; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn use_of_unknown_std_target_is_an_error() {
    // An extension root is fully enumerable, so a `use` that resolves to no module / namespace /
    // member / type is a hard error at check time — not an opaque stub that only fails at run or
    // `--native` (the check/run divergence). This is the handoff's `use std.{http}` repro's sibling:
    // a genuinely nonexistent target.
    assert_eq!(codes("use std.bogus;\n"), ["E0019"]);
    // A selective import naming a real module but an unknown member.
    assert_eq!(codes("use std.math.bogus;\n"), ["E0019"]);
}

#[test]
fn namespaced_type_resolves_in_type_position() {
    // A type reached through a namespace group — `http.Response` from `use std.http` — resolves in
    // annotation and `is` position (the type-position analog of `http.client.get`), identical to the
    // leaf import `use std.http.Response`. A nonexistent one is a hard `unknown type` error.
    assert!(
        codes("use std.http\nfn f(r: http.Response): bool { return r is http.Response; }\n")
            .is_empty()
    );
    assert_eq!(
        codes("use std.http\nfn f(x: http.Bogus): int { return 0; }\n"),
        ["E0013"]
    );
}

#[test]
fn namespace_group_binds_and_resolves() {
    // `use std.http` binds `http` as a navigable group; `http.client.get(...)` resolves the client
    // submodule and type-checks clean (identical to the leaf form `use std.http.client`). The `?`
    // is load-bearing, not decoration: a verb returns `Result<Response, HttpError>`, so without it
    // `r` is the Result and `status()` is a method it does not have. This fixture asserted the
    // unspent form was clean until closed-type method resolution started catching that.
    assert!(
        codes("use std.http;\nr = http.client.get(\"https://x\")?;\necho r.status();\n").is_empty()
    );
}

#[test]
fn bad_member_on_namespace_group_is_an_error() {
    // The handoff's core repro: `use std.http` is now valid, but `http.get(...)` names no member of
    // the `http` group (`get` lives on `http.client`). A group is fully enumerable, so this is a
    // hard error at check time — `check` and `run` now agree (both reject it) instead of `check`
    // passing and `run`/`--native` failing.
    assert_eq!(
        codes("use std.http;\nr = http.get(\"https://x\");\necho \"unreachable\";\n"),
        ["E0005"]
    );
    // The same miss when the member is read, not called.
    assert_eq!(codes("use std.http;\nx = http.nope;\n"), ["E0005"]);
}

#[test]
fn generic_parameter_is_a_legal_type() {
    // A class's `<T>` is an in-scope type within its own field and method annotations, but is
    // erased — unknown outside the declaration.
    let src = "class Box<T> {\n  value: T\n  fn get(): T { return self.value; }\n}\n";
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
    let cls = "class Box<T> { pub value: T }\nfn need_int(n: int): int { return n; }\n";
    let bad = format!("{cls}b = Box {{ value: \"hi\" }};\necho need_int(b.value);\n");
    assert_eq!(codes(&bad), ["E0007"]);
    let ok = format!("{cls}b = Box {{ value: 5 }};\necho need_int(b.value);\n");
    assert!(codes(&ok).is_empty());
}

#[test]
fn instance_keeps_its_type_argument() {
    // `Box<int>` tracks its element type through the instance: `b.get()` is `int` (passes where an
    // `int` is wanted), while `Box<string>.get()` is `string` (a mismatch against `int`).
    let cls = "class Box<T> { value: T\n  fn new(v: T): Box<T> { return Box { value: v }; }\n  fn get(): T { return self.value; } }\n\
               fn need_int(n: int): int { return n; }\n";
    let ok = format!("{cls}b = Box.new(1);\necho need_int(b.get());\n");
    assert!(codes(&ok).is_empty());
    let bad = format!("{cls}b = Box.new(\"hi\");\necho need_int(b.get());\n");
    assert_eq!(codes(&bad), ["E0007"]);
}

#[test]
fn generic_class_enforces_its_bound_at_construction() {
    // `Pair<T: Comparable>` constructed with a non-`Comparable` struct is `E0025`; with an `int`,
    // clean. The class's bound is instantiated from the constructor argument.
    let cls =
        "class Pair<T: Comparable> { a: T\n  fn new(x: T): Pair<T> { return Pair { a: x }; } }\n";
    let bad = format!("struct Bad {{ v: int }}\n{cls}p = Pair.new(Bad {{ v: 1 }});\n");
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
    let bad = "struct P { x: int }\na = P { x: 1 };\nb = P { x: 2 };\necho a + b;\n";
    assert_eq!(codes(bad), ["E0007"]);
    let good = "class M { pub n: int\n  impl Add { fn add(o: M): M { return o; } } }\n\
                a = M { n: 1 };\nb = M { n: 2 };\necho a + b;\n";
    assert!(codes(good).is_empty());
}

#[test]
fn ordering_on_a_concrete_non_comparable_type_is_reported() {
    // Ordering now checks concrete types too: a struct that does not derive/`impl Comparable` is
    // `E0007`; a `@derive(Comparable)` type is accepted.
    let bad = "struct P { x: int }\na = P { x: 1 };\nb = P { x: 2 };\necho a < b;\n";
    assert_eq!(codes(bad), ["E0007"]);
    let good = "@derive(Comparable)\nclass V { pub n: int }\n\
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
    // A struct literal has the concrete type `P`, which does not satisfy `Comparable`: `E0025`.
    let src = "struct P { x: int }\n\
               fn max<T: Comparable>(a: T, b: T): T { return a; }\n\
               echo max(P { x: 1 }, P { x: 2 });\n";
    assert_eq!(codes(src), ["E0025"]);
}

// --- `Mergeable` is no longer a built-in ------------------------------------------------------
//
// It was a closed `BuiltinTrait` satisfied only by the CRDT extern types, with `impl`/`@derive`
// rejected outright, so that a value with no runtime merge could not pass the checker. That closed
// the door on the legitimate case too: an app cannot define its own CRDT. It is now an ordinary
// native `ExtTrait` declared by the para-p2p package (`para.crdt.Mergeable`) with a REQUIRED
// `merge` method, so the checker enforces that an implementor actually supplies one — the failure
// mode the closure existed to prevent — while a user type may join the bound like any other trait.
//
// Nothing about it is checkable against this std-only registry any more (the trait ships with the
// package), so its tests live in that repo's conformance corpus:
// `tests/conformance/crdt/{mergeable_bound_satisfied,user_mergeable}.noe`.

#[test]
fn a_name_no_built_in_trait_claims_is_not_a_bound() {
    // With `Mergeable` gone from the built-in set and no extension installed, the name is simply
    // unknown — the generic path reports the bad bound rather than silently accepting it.
    let src = "fn store<T: Mergeable>(v: T): T { return v; }\n\
               echo store(42);\n";
    assert!(!codes(src).is_empty(), "an unknown bound must be reported");
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
    let src = "@derive(Comparable)\nclass B { pub n: int }\n\
               fn max<T: Comparable>(a: T, b: T): T { return a; }\n\
               echo max(B { n: 1 }, B { n: 2 });\n";
    assert!(codes(src).is_empty());
}

#[test]
fn annotations_do_not_produce_false_positives() {
    let src = "struct Item { price: float }\n\
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
fn deriving_comparable_over_an_unorderable_field_is_e0050() {
    // A `List` field has no ordering under any values — statically rejected at the derive.
    let src = "@derive(Comparable)\nstruct B {\n  items: List<int>\n}\n";
    assert_eq!(codes(src), ["E0050"]);
    // Maps and tuples likewise.
    let src = "@derive(Comparable)\nstruct C {\n  m: Map<string, int>\n}\n";
    assert_eq!(codes(src), ["E0050"]);
}

#[test]
fn deriving_comparable_over_orderable_fields_is_clean() {
    // Every orderable field kind: primitives (incl. bool/f32), a nested struct of primitives,
    // and `?int` (the prelude Option enum orders by variant then payload).
    let src = "@derive(Comparable)\nstruct Inner {\n  a: int\n}\n\
               @derive(Comparable)\nstruct S {\n  n: int\n  f: f32\n  b: bool\n  s: string\n  \
               inner: Inner\n  opt: ?int\n}\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn deriving_comparable_on_an_enum_with_unorderable_payload_is_e0050() {
    // The payload struct itself derives nothing (legal); the DERIVED enum's payload recursion
    // finds the unorderable `List` field inside it.
    let src = "struct Bag {\n  items: List<int>\n}\n\
               @derive(Comparable)\nenum E {\n  A(Bag)\n  B\n}\n";
    assert_eq!(codes(src), ["E0050"]);
}

#[test]
fn deriving_serialize_over_a_function_field_is_e0050() {
    let src = "@derive(Serialize<Json>)\nstruct H {\n  callback: (int) -> int\n}\n";
    assert_eq!(codes(src), ["E0050"]);
    // Deeply nested inside a container still rejects.
    let src = "@derive(Serialize<Json>)\nstruct H {\n  callbacks: List<(int) -> int>\n}\n";
    assert_eq!(codes(src), ["E0050"]);
}

#[test]
fn derive_field_constraint_defers_generic_params_to_instantiation() {
    // A field typed by the type's own parameter is checked at the use site (conditional derive),
    // not at the declaration.
    let src = "@derive(Comparable)\nstruct Box<T> {\n  value: T\n}\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

const MAX_FN: &str =
    "fn max<T: Comparable>(a: T, b: T): T {\n  if a > b { return a; }\n  return b;\n}\n";

#[test]
fn generic_derive_is_conditional_on_instantiated_fields() {
    // `Box<int>` satisfies the bound (the instantiated field is orderable) …
    let ok = format!(
        "@derive(Comparable)\nstruct Box<T> {{\n  value: T\n}}\n{MAX_FN}\
         echo max(Box {{ value: 1 }}, Box {{ value: 2 }}).value\n"
    );
    assert!(codes(&ok).is_empty(), "{:?}", codes(&ok));
    // … while `Box<List<int>>` does not — the bound fails at the call site, not at runtime.
    let bad = format!(
        "@derive(Comparable)\nstruct Box<T> {{\n  value: T\n}}\n{MAX_FN}\
         echo max(Box {{ value: [1] }}, Box {{ value: [2] }}).value\n"
    );
    assert_eq!(
        codes(&bad),
        ["E0025"],
        "Box<List<int>> must not satisfy Comparable"
    );
}

#[test]
fn generic_hand_written_impl_stays_unconditional() {
    // An `impl Comparable` with a hand-written `compare` is the author's contract — no field
    // constraint applies, whatever the instantiation.
    let src = format!(
        "struct Box<T> {{\n  value: T\n  impl Comparable {{\n    fn compare(other: Box<T>): \
         Ordering {{ return Ordering.Less; }}\n  }}\n}}\n{MAX_FN}\
         echo max(Box {{ value: [1] }}, Box {{ value: [2] }}).value\n"
    );
    assert!(codes(&src).is_empty(), "{:?}", codes(&src));
}

#[test]
fn map_keys_without_a_runtime_key_form_are_rejected_statically() {
    // A plain (non-packed) user struct cannot key a map — literal and annotation forms.
    let src = "struct P { x: int }\nm = { P { x: 1 }: \"one\" }\necho m.len()\n";
    assert_eq!(codes(src), ["E0007"]);
    let src = "struct P { x: int }\nfn f(m: Map<P, int>): int { return m.len() }\necho 1\n";
    assert_eq!(codes(src), ["E0007"]);
    // String and int-family keys are key-capable (P-PKEY S4).
    let src = "m = { \"a\": 1 }\necho m.len()\n";
    assert!(codes(src).is_empty());
    let src = "fn f(m: Map<int, string>): int { return m.len() }\necho 1\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
    for width in ["i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64"] {
        let src = format!("fn f(m: Map<{width}, string>): int {{ return m.len() }}\necho 1\n");
        assert!(codes(&src).is_empty(), "{width}: {:?}", codes(&src));
    }
    // The whole float family is barred as a key — including `f64`, which the stringly
    // predecessor of this gate did not know, so `Map<f64, _>` slipped through while
    // `Map<float, _>` was rejected (the funnel-drift bug class).
    for float in ["float", "f32", "f64"] {
        let src = format!("fn f(m: Map<{float}, int>): int {{ return m.len() }}\necho 1\n");
        assert_eq!(codes(&src), ["E0007"], "float-family key `{float}`");
    }
}

#[test]
fn set_members_follow_the_set_order_rule_statically() {
    // A `Set<T>` annotation admits value kinds (structs/enums, ordering structurally) and
    // rejects classes — matching the runtime `set_order` (a class reference could be mutated
    // after insertion, breaking the canonical-order snapshot).
    let ok = "struct P { x: int }\nfn f(s: Set<P>): int { return s.len() }\necho 1\n";
    assert!(codes(ok).is_empty(), "{:?}", codes(ok));
    let ok = "enum Dir { north; south }\nfn f(s: Set<Dir>): int { return s.len() }\necho 1\n";
    assert!(codes(ok).is_empty(), "{:?}", codes(ok));
    let bad = "class C { pub x: int }\nfn f(s: Set<C>): int { return s.len() }\necho 1\n";
    assert_eq!(codes(bad), ["E0007"]);
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
    // A bodiless struct declares a capability via a same-module standalone `impl` — the mechanism
    // that lets a struct (which has no body) participate in a trait. No diagnostics.
    let src = "struct Route { path: string }\nimpl Clone for Route {}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn standalone_impl_for_undeclared_type_is_orphan() {
    // The orphan rule: a standalone `impl` may only target a type declared in this module.
    let src = "impl Clone for Widget {}\n";
    assert_eq!(codes(src), ["E0013"]);
}

#[test]
fn generic_serialize_derive_checks_clean() {
    // `@derive(Serialize<Json>)` is the format-parameterized serializer; a valid format checks clean.
    let src = "@derive(Serialize<Json>)\nstruct Point { x: int }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn serialize_derive_requires_a_format() {
    // `Serialize` is generic, so a bare `@derive(Serialize)` is an arity error (E0014).
    let src = "@derive(Serialize)\nstruct Point { x: int }\n";
    assert_eq!(codes(src), ["E0014"]);
}

#[test]
fn serialize_derive_rejects_an_unknown_format() {
    // A `Serialize` format argument must be a blessed format; `Xml` is unknown (E0013).
    let src = "@derive(Serialize<Xml>)\nstruct Point { x: int }\n";
    assert_eq!(codes(src), ["E0013"]);
}

#[test]
fn nullary_derive_rejects_type_arguments() {
    // A non-generic derivable trait takes no type arguments (E0014).
    let src = "@derive(Comparable<int>)\nstruct Point { x: int }\n";
    assert_eq!(codes(src), ["E0014"]);
}

#[test]
fn standalone_impl_counts_toward_coherence() {
    // Coherence spans all three implementation forms: a `@derive` and a standalone `impl` of the
    // same trait for one type conflict, just like two derives or two in-body impls.
    let src = "@derive(Clone)\nstruct Route { path: string }\nimpl Clone for Route {}\n";
    assert_eq!(codes(src), ["E0027"]);
}

#[test]
fn a_coherence_conflict_labels_both_implementations() {
    // E0027 must be *locatable*: it labels the offending site AND the one it collides with, so a
    // reader can find both. It used to name only the later occurrence and describe the other as
    // "above" — false the moment the two live in different modules (or packages), which is the
    // common case now that coherence runs over the whole linked program.
    let src = "@derive(Clone)\nstruct Route { path: string }\nimpl Clone for Route {}\n";
    let diagnostics = diagnostics(src);
    let conflict = diagnostics
        .iter()
        .find(|d| d.code == noeta_diagnostics::DiagnosticCode::ConflictingTraitImpl)
        .expect("the duplicate implementation is E0027");
    assert_eq!(
        conflict.labels.len(),
        2,
        "both sites are labelled: {:?}",
        conflict.labels
    );
    // The primary span is the offending (later) site, and it is also the first label — so the
    // rendered report's header and its first group agree with every non-rendered consumer.
    assert_eq!(conflict.labels[0].span, conflict.span);
    assert!(
        conflict.labels[0].message.contains("standalone `impl"),
        "the later site names its own spelling: {:?}",
        conflict.labels[0]
    );
    assert!(
        conflict.labels[1].message.contains("`@derive`"),
        "the first site names its own spelling: {:?}",
        conflict.labels[1]
    );
    let help = conflict.help.as_deref().unwrap_or_default();
    assert!(
        !help.contains("above"),
        "the positional wording is gone — the other site may be in another file: {help}"
    );
}

#[test]
fn standalone_impl_with_methods_is_unsupported() {
    // Pass 1 supports only empty-body capability impls; a body with methods is rejected (E0015).
    let src = "struct Route { path: string }\nimpl Clone for Route {\n  fn extra(): int { return 1; }\n}\n";
    assert_eq!(codes(src), ["E0015"]);
}

#[test]
fn attribute_on_a_class_is_rejected() {
    // Attributes are structs only — `@attribute` on a class is a misplaced directive, E0054. (It
    // reported E0029 until the codes were unified; E0029 is about *using* a non-attribute struct as
    // an attribute, a different fault.)
    let src = "@attribute\nclass Route {\n  path: string\n}\n";
    assert_eq!(codes(src), ["E0054"]);
}

#[test]
fn bare_attribute_record_is_usable_anywhere() {
    // A bare `@attribute` (no kinds) opts the struct in with no placement restriction, so a use on
    // any site — here a top-level function — is accepted.
    let src = "@attribute\nstruct Tag { name: string }\n#[Tag(\"x\")]\nfn f(): int { return 0; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn packed_struct_of_primitives_is_clean() {
    // P-PACK Phase 0: a `@packed` struct whose fields are all primitives (or other packed structs)
    // checks clean — and a packed struct may name another packed struct (order-independent).
    let src = "@packed struct Vec3 { x: float; y: float; z: float }\n@packed struct Segment { a: Vec3; b: Vec3 }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn packed_struct_non_primitive_field_is_e0038() {
    // A heap-shaped field (string/list/class/…) cannot lay out flat — E0038.
    let src = "@packed struct Bad { name: string }\n";
    assert!(
        codes(src).contains(&"E0038".to_string()),
        "{:?}",
        codes(src)
    );
}

#[test]
fn packed_on_a_non_struct_is_misplaced() {
    // `@packed` is a struct-only layout marker; on a class or enum it is a misplacement, E0054 —
    // not E0038, which is reserved for a packed struct's own field constraints.
    assert!(
        codes("@packed class Boxed { v: int }\n").contains(&"E0054".to_string()),
        "{:?}",
        codes("@packed class Boxed { v: int }\n")
    );
    assert!(
        codes("@packed enum E { A }\n").contains(&"E0054".to_string()),
        "{:?}",
        codes("@packed enum E { A }\n")
    );
}

#[test]
fn defaulted_attribute_field_is_optional() {
    // An `@attribute` field with a default (`ttl: int = 60`) is optional in a construction (slice
    // 6i): `#[Cache]` may omit it, and a mandatory field may still be supplied. No diagnostics.
    let src = "@attribute\nstruct Cache { ttl: int = 60  eager: bool }\n#[Cache(eager: true)]\nstruct A { id: int }\n#[Cache(ttl: 5, eager: false)]\nstruct B { id: int }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn mandatory_attribute_field_is_still_required() {
    // A field *without* a default is still mandatory — omitting it is E0009, even when a sibling
    // field has a default.
    let src =
        "@attribute\nstruct Cache { ttl: int = 60  eager: bool }\n#[Cache]\nstruct A { id: int }\n";
    assert!(
        codes(src).contains(&"E0009".to_string()),
        "{:?}",
        codes(src)
    );
}

#[test]
fn builtin_skip_reason_is_optional() {
    // The built-in `Skip` attribute's `reason` defaults to `""`, so both `#[Skip]` and
    // `#[Skip("…")]` construct it (slice 6i). A `@test` fn is stripped on a normal check, so this
    // exercises the construction gate via an ordinary declaration. `Skip` lives under `std.test`
    // (D2b — no global attribute namespace), so it is imported like any attribute.
    let src = "use std.test.{Skip}\n#[Skip]\nstruct A { id: int }\n#[Skip(\"flaky\")]\nstruct B { id: int }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn attributes_of_on_an_attribute_type_checks_clean() {
    // `attributes_of::<Route>()` is `List<Attributed<Route>>`, so `r.target` is a string and
    // `r.value` is a `Route` whose `.path` is a string — all resolve without diagnostics.
    let src = "@attribute\nstruct Route { path: string }\n#[Route(\"/x\")]\nstruct Users { id: int }\nfor r in attributes_of::<Route>() {\n  echo r.target;\n  echo r.value.path;\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_function_checks_clean() {
    // A `#[...]` on a function is validated like one on a type: the capability gate plus the
    // all-fields construction check, both satisfied here.
    let src = "@attribute\nstruct Route { method: string }\n#[Route(\"GET\")]\nfn greet(): string { return \"hi\"; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn concrete_type_is_assignable_to_its_abstract_kind() {
    // A declared enum/struct/class widens into the matching abstract kind-type at a boundary.
    let src = "enum E { A; B; }\nstruct R { x: int }\nfn takeE(e: Enum): int { return 1; }\nfn takeR(r: Struct): int { return 1; }\necho takeE(E.A);\nv = R { x: 1 };\necho takeR(v);\n";
    assert!(codes(src).is_empty());
}

#[test]
fn wrong_kind_is_a_type_error() {
    // The kinds are distinct: a struct is not an enum (E0007 at the call).
    let src = "struct R { x: int }\nfn takeE(e: Enum): int { return 1; }\nv = R { x: 1 };\necho takeE(v);\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn dyn_narrows_to_an_abstract_kind() {
    // `dyn` → a kind via `is`/`.as<>()` is a valid (runtime) narrow, not E0028.
    let src = "enum E { A; }\nx: dyn = E.A;\necho x is Enum;\ny = x.as<Struct>();\n";
    assert!(codes(src).is_empty());
}

#[test]
fn role_tag_on_an_attribute_checks_clean() {
    // A `@role(Semantic.EntryPoint)` tag on a struct that is also `@attribute` is well-formed;
    // `roles_of()` then type-checks as `List<RoleBinding>` whose `.role` is an `Enum` and `.target`
    // a string.
    let src = "@attribute\n@role(Semantic.EntryPoint)\nstruct Route { path: string }\nfor b in roles_of() {\n  echo b.target;\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn role_must_be_a_known_variant() {
    // `@role(Enum.Variant)` must name an existing variant of the `@semantic` enum — an unknown
    // variant is E0031.
    let src = "@attribute\n@role(Semantic.Bogus)\nstruct Route { path: string }\n";
    assert_eq!(codes(src), ["E0031"]);
}

#[test]
fn role_must_name_a_semantic_enum() {
    // A `@role` whose enum is not `@semantic` is E0031 — only a promoted enum's variants are roles.
    let src = "enum Plain { A; B; }\n@attribute\n@role(Plain.A)\nstruct Route { path: string }\n";
    assert_eq!(codes(src), ["E0031"]);
}

#[test]
fn role_on_a_user_semantic_enum_checks_clean() {
    // A user enum marked `@semantic` makes its fieldless variants role-eligible (declared after the
    // attribute that references it — the validation pass runs after all types are collected).
    let src = "@attribute\n@role(WebRole.Controller)\nstruct Route { path: string }\n@semantic\nenum WebRole { Controller; Middleware; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn role_must_be_qualified() {
    // A bare `@role(Variant)` with no enum qualifier is E0031 — a role is always `Enum.Variant`.
    let src = "@attribute\n@role(EntryPoint)\nstruct Route { path: string }\n";
    assert_eq!(codes(src), ["E0031"]);
}

#[test]
fn role_variant_must_be_fieldless() {
    // A payload-carrying variant cannot be a role (its payload would need comptime per use site).
    let src = "@semantic\nenum WebRole { Tagged(name: string); }\n@attribute\n@role(WebRole.Tagged)\nstruct Route { path: string }\n";
    assert_eq!(codes(src), ["E0031"]);
}

#[test]
fn semantic_on_a_struct_is_misplaced() {
    // `@semantic` marks enums; on a struct it is a misplacement, E0054. E0031 stays the code for a
    // malformed *role* — the subject it actually names.
    let src = "@semantic\nstruct Route { path: string }\n";
    assert_eq!(codes(src), ["E0054"]);
}

#[test]
fn role_requires_the_record_be_an_attribute() {
    // A role rides on an attribute, so `@role` without `@attribute` is E0031.
    let src = "@role(Semantic.EntryPoint)\nstruct Route { path: string }\n";
    assert_eq!(codes(src), ["E0031"]);
}

#[test]
fn multiple_roles_on_one_declaration_check_clean() {
    // A declaration may carry several roles; each becomes its own binding.
    let src = "@attribute\n@role(Semantic.EntryPoint, Semantic.TrustBoundary)\nstruct Route { path: string }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn role_match_is_exhaustive_over_a_wildcard() {
    // `b.role` is the abstract `Enum` kind, an open domain matchable by `Enum.Variant`; a `_` arm
    // covers the rest.
    let src = "@attribute\n@role(Semantic.Sink)\nstruct Db { table: string }\n#[Db(\"users\")]\nfn w(): int { return 1; }\nfor b in roles_of() {\n  echo match b.role { Semantic.Sink => \"s\", _ => \"o\" };\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_method_checks_clean() {
    // The same validation reaches a class method's attributes (through `check_fn`).
    let src = "@attribute\nstruct Route { method: string }\nclass Api {\n  id: int\n  #[Route(\"GET\")]\n  fn list(): string { return \"[]\"; }\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn structured_attribute_args_check_clean() {
    // A composite literal tree (list of enums, map, set, nested struct) type-checks recursively
    // against the attribute's field types.
    let src = "enum Method { Get; Post; }\nstruct Limits { rps: int }\n@attribute\nstruct Endpoint { methods: List<Method> limits: Map<string, int> tags: Set<string> fallback: Limits }\n#[Endpoint(methods: [Method.Get, Method.Post], limits: { \"r\": 1 }, tags: #{\"a\"}, fallback: Limits { rps: 1 })]\nstruct Users { id: int }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn structured_attribute_arg_nested_mismatch() {
    // A wrong element type inside a composite argument is caught recursively (E0007): a string in a
    // `List<int>` field.
    let src = "@attribute\nstruct Nums { xs: List<int> }\n#[Nums(xs: [1, \"two\"])]\nstruct Page { id: int }\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn structured_attribute_arg_struct_field_mismatch() {
    // A nested struct literal's field value is checked against the declared struct field type.
    let src = "struct Limits { rps: int }\n@attribute\nstruct Endpoint { fallback: Limits }\n#[Endpoint(fallback: Limits { rps: \"x\" })]\nstruct Page { id: int }\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn invoke_checks_clean_and_yields_a_result() {
    // `invoke(recv, name, args)` synthesizes `Result<dyn, dyn>`, so its value matches `Ok`/`Err`
    // arms without diagnostics. The name/args are runtime-checked (no static constraint here).
    let src = "class Shape {\n  w: int\n  fn new(w: int): Shape { return Shape { w: w }; }\n  fn area(): int { return self.w; }\n}\nr = invoke(Shape.new(2), \"area\", []);\necho match r { Ok(_) => \"y\", Err(_) => \"n\" };\n";
    assert!(codes(src).is_empty());
}

#[test]
fn invoke_result_is_a_concrete_result_not_a_hole() {
    // The result is a concrete `Result<dyn, dyn>`, not a deferring hole, so passing it where an
    // `int` is expected is a static error (E0007) — proof the synth is precise, not gradual.
    let src = "class Shape {\n  w: int\n  fn new(w: int): Shape { return Shape { w: w }; }\n  fn area(): int { return self.w; }\n}\nfn need_int(n: int): int { return n; }\necho need_int(invoke(Shape.new(1), \"area\", []));\n";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn attribute_on_a_function_must_be_an_attribute() {
    // The E0029 gate applies on a function too: `Plain` is a struct but not marked `@attribute`.
    let src = "struct Plain { method: string }\n#[Plain(\"GET\")]\nfn greet(): string { return \"hi\"; }\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attribute_on_record_and_class_fields_checks_clean() {
    // A `#[...]` on a struct field and on a class field is validated like any other attribute use.
    let src = "@attribute\nstruct Column { name: string }\nstruct User { #[Column(\"uid\")] id: int }\nclass Account {\n  #[Column(\"bal\")]\n  balance: int\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_field_must_be_an_attribute() {
    // The E0029 gate reaches field attributes too.
    let src = "struct Plain { name: string }\nstruct User { #[Plain(\"x\")] id: int }\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attachable_to_permits_a_listed_kind() {
    // `@attribute(Function)` allows the attribute on a top-level function.
    let src = "@attribute(Function)\nstruct Route { method: string }\n#[Route(\"GET\")]\nfn greet(): string { return \"hi\"; }\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attachable_to_rejects_an_unlisted_kind() {
    // `@attribute(Method)` forbids the attribute on a type declaration → E0030.
    let src = "@attribute(Method)\nstruct Route { method: string }\n#[Route(\"GET\")]\nstruct User { id: int }\n";
    assert_eq!(codes(src), ["E0030"]);
}

#[test]
fn attachable_to_with_an_unknown_kind_is_rejected() {
    // The kind vocabulary is closed; an unknown name in the directive is E0030.
    let src = "@attribute(Bogus)\nstruct Route { method: string }\n";
    assert_eq!(codes(src), ["E0030"]);
}

#[test]
fn attachable_to_field_only_attribute_rejects_a_method() {
    // A field-only attribute (`@attribute(Field)`) on a method is E0030 — exercising the
    // method/function target axis added in P2.4.
    let src = "@attribute(Field)\nstruct Column { name: string }\nclass Api {\n  #[Column(\"x\")]\n  fn list(): int { return 0; }\n}\n";
    assert_eq!(codes(src), ["E0030"]);
}

#[test]
fn attribute_on_an_enum_variant_checks_clean() {
    // A `#[...]` on an enum variant (plain or algebraic) is validated like any other attribute use.
    let src = "@attribute\nstruct Note { text: string }\nenum Status {\n  Active;\n  #[Note(\"gone\")]\n  Archived;\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn attribute_on_a_variant_must_be_an_attribute() {
    // The E0029 gate reaches enum-variant attributes too.
    let src = "struct Plain { text: string }\nenum Status {\n  #[Plain(\"x\")]\n  Active;\n}\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attributes_of_on_a_non_attribute_is_rejected() {
    // The capability gate, mirroring a `#[Foo]` use: the type argument must be marked `@attribute`.
    let src = "struct Plain { path: string }\nrs = attributes_of::<Plain>();\n";
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

/// Every prelude enum is a *declared* enum to the checker, registered from the one shared table
/// both backends seed their runtime type environments from — so naming a case checks clean and a
/// `match` on it is exhaustiveness-checked. `Ordering` was the hole on this side: the two backends
/// knew it and the checker did not, so a non-exhaustive `match o { Ordering.Less => … }` passed
/// E0011 and then aborted with "no match arm matched the value Ordering.Greater" at run time.
#[test]
fn every_prelude_enum_is_registered_and_nameable() {
    for decl in noeta_ast::reflect::prelude_enums() {
        let variant = &decl.variants[0].name;
        // A payload-carrying first variant would need arguments; every prelude enum's is fieldless
        // today, and this says so rather than silently skipping if that ever changes.
        assert!(
            decl.variants[0].fields.is_empty(),
            "`{}.{variant}` grew a payload — the naming probe below needs updating",
            decl.name
        );
        let src = format!("x = {}.{variant};\necho x;\n", decl.name);
        assert!(
            codes(&src).is_empty(),
            "`{}.{variant}` must name a prelude enum case: {:?}",
            decl.name,
            codes(&src)
        );
    }
}

#[test]
fn a_non_exhaustive_match_on_a_prelude_enum_is_reported() {
    // `Ordering` has three cases; two arms and no catch-all is E0011 — the same rule every other
    // enum gets, now that the checker registers it from the shared prelude table.
    let src = "o = 5.compare(2);\nlabel = match o { Ordering.Less => \"l\", Ordering.Equal => \"e\" };\necho label;\n";
    assert_eq!(codes(src), vec!["E0011"]);
    // With the third arm it checks clean.
    let src = "o = 5.compare(2);\nlabel = match o { Ordering.Less => \"l\", Ordering.Equal => \"e\", Ordering.Greater => \"g\" };\necho label;\n";
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
    use noeta_types::Type;
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
    // `#[Route(...)]` is valid when `Route` is a struct marked `@attribute`,
    // and the arguments construct it (the positional value fills `path`).
    let src =
        "@attribute\nstruct Route { path: string }\n#[Route(\"/x\")]\nclass P {\n  x: int\n}\n";
    assert!(codes(src).is_empty());
}

#[test]
fn unmarked_attribute_is_rejected() {
    // The capability gate: a `#[Foo]` whose `Foo` is not marked `@attribute` is E0029.
    let src = "#[Route]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0029"]);
}

#[test]
fn attribute_missing_field_is_reported() {
    // The construction check: `#[Route]` with no argument leaves `path` unset (E0009).
    let src = "@attribute\nstruct Route { path: string }\n#[Route]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0009"]);
}

#[test]
fn attribute_argument_type_mismatch_is_reported() {
    // The construction check: a literal whose type does not match its field is E0007.
    let src = "@attribute\nstruct Route { path: string }\n#[Route(42)]\nclass P {\n  x: int\n}\n";
    assert_eq!(codes(src), ["E0007"]);
}

// ----- bidirectional check-mode (white-box) -----
//
// Production callers feed real expectations through `Checker::check` (declared returns, argument
// types, declared element types). These white-box tests drive it directly with concrete
// expectations to pin down subsumption and inward propagation in isolation.

/// Parse `__probe = <expr>;`, then check the binding's value against `expected`, returning the
/// resulting diagnostic codes.
fn check_value_against(expr: &str, expected: noeta_types::Type) -> Vec<String> {
    seed_std();
    let text = format!("__probe = {expr};");
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "probe must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let value = match &parsed.program.stmts[0] {
        noeta_ast::Stmt::Binding { value, .. } => value,
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
    use noeta_types::Type;
    assert!(check_value_against("5", Type::Int).is_empty());
    assert!(check_value_against("\"hi\"", Type::String).is_empty());
    // Every type widens into the explicit top.
    assert!(check_value_against("5", Type::Dyn).is_empty());
}

#[test]
fn subsumption_fires_on_a_concrete_violation() {
    use noeta_types::Type;
    // int is not a subtype of string → the same code the arithmetic mismatch path uses.
    assert_eq!(check_value_against("5", Type::String), ["E0007"]);
    assert_eq!(check_value_against("true", Type::Int), ["E0007"]);
}

#[test]
fn subsumption_is_a_no_op_against_an_open_expectation() {
    use noeta_types::Type;
    // The production default: an `Unknown` expectation never reports — the parity guarantee.
    assert!(check_value_against("5", Type::Unknown).is_empty());
    assert!(check_value_against("true", Type::Unknown).is_empty());
}

#[test]
fn list_expectation_propagates_to_elements() {
    use noeta_types::Type;
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
    use noeta_types::Type;
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
    assert!(codes("fn f(): int { return \"a b\".split(\" \").len(); }\n").is_empty());
}

#[test]
fn every_reserved_prelude_name_rejects_binding() {
    // P3's guard, name by name: each of the six reserved prelude names is E0046 in a plain
    // binding. (The runtime divergence this closes: the tree-walker pre-declared these as
    // immutable globals while the VM shadowed them as fresh locals.)
    for name in ["Ok", "Err", "some", "none", "panic", "assert"] {
        assert_eq!(
            codes(&format!("{name} = 5;\n")),
            ["E0046"],
            "binding `{name}` must be reserved"
        );
    }
}

#[test]
fn reserved_native_type_names_reject_type_declarations() {
    // E0049 now reserves only the **language-level** built-ins (`Iterator`/`Future`/`Sender`/
    // `Receiver`), whose values are backend builtins dispatched by name.
    assert_eq!(codes("class Iterator { x: int }\n"), ["E0049"]);
    assert_eq!(codes("enum Future { A }\n"), ["E0049"]);
    // A registered **extern** type's name is no longer reserved — extern types are namespace-scoped
    // and `use`-imported, so a user may freely declare one (it carries a distinct qualified
    // identity and never conflates with the native type).
    assert_eq!(
        codes("struct FileHandle { x: int }\n"),
        Vec::<String>::new()
    );
    assert_eq!(codes("class Response { x: int }\n"), Vec::<String>::new());
    // An unreserved name stays declarable.
    assert_eq!(codes("struct Handle2 { x: int }\n"), Vec::<String>::new());
}

#[test]
fn reserved_names_reject_every_declaration_form() {
    // The reservation is uniform across declaration forms, not just plain bindings.
    assert_eq!(codes("mut some = 1;\n"), ["E0046"]); // mut binding
    assert_eq!(codes("(Ok, b) = (1, 2);\necho b;\n"), ["E0046"]); // tuple destructuring
    assert_eq!(codes("fn panic(): void { return; }\n"), ["E0046"]); // fn name
    assert_eq!(
        codes("fn f(assert: int): int { return assert; }\necho f(1);\n"),
        ["E0046"]
    ); // parameter
    assert_eq!(codes("for none in [1] { echo 1; }\n"), ["E0046"]); // for binder
    assert_eq!(codes("struct Ok { x: int }\n"), ["E0046"]); // type name
    assert_eq!(codes("f = fn(Err: int) => Err;\necho f(1);\n"), ["E0046"]); // closure parameter
}

#[test]
fn none_in_match_pattern_position_stays_legal() {
    // The one exemption: bare `none` in a match arm is the Option-none CONSTRUCTOR pattern
    // (represented as a binding, matched by name), not a fresh binding.
    let src = "x = some(1);\necho match x { some(v) => v, none => 0 };\n";
    assert!(codes(src).is_empty(), "none-pattern must stay legal");
}

#[test]
fn prelude_functions_are_typed() {
    // `len`/`sum` left the prelude (P1.2) — they are collection methods now, typed as such.
    assert!(codes("fn f(): int { return [1, 2, 3].len(); }\n").is_empty()); // len -> int
    assert_eq!(codes("fn f(): string { return [1].len(); }\n"), ["E0007"]);
    assert!(codes("fn f(): int { return [1, 2].sum(); }\n").is_empty()); // sum(List<int>) -> int
    // The remaining prelude free functions stay typed.
    assert!(codes("use std.id.{next_id}\nfn f(): int { return next_id(); }\n").is_empty()); // next_id -> int
    assert_eq!(
        codes("use std.id.{next_id}\nfn f(): string { return next_id(); }\n"),
        ["E0007"]
    );
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
    let src = "class C {\n  x: int\n  fn label(): string { return \"c${self.x}\"; }\n}\n\
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
    // Strict numeric fit: an int argument is NOT accepted where a float is expected — write `4.0`,
    // not `4` (matching a binding / return / element, and Rust's no-implicit-widening rule).
    let m = "use std.{math};\n";
    assert_eq!(codes(&format!("{m}echo math.sqrt(4);\n")), ["E0007"]);
    assert!(codes(&format!("{m}echo math.sqrt(4.0);\n")).is_empty());
    assert_eq!(codes(&format!("{m}echo math.sqrt(\"x\");\n")), ["E0007"]);
}

#[test]
fn generic_method_arguments_are_not_false_positives() {
    // A generic parameter is erased to `dyn`, so any concrete argument is accepted.
    let src = "class Box<T> {\n  mut value: T\n  fn set(v: T): void { self.value = v; }\n}\n\
               fn f(b: Box<int>): void { b.set(5); }\n";
    assert!(codes(src).is_empty());
}

// ----- list concatenation via `~` (L1) -----

#[test]
fn concat_of_two_lists_is_a_list() {
    use noeta_types::Type;
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
    // (`.len()` is the method form; the free `len([])` was never valid — it left the prelude in
    // P1.2 and is now caught at check time by the F1 unknown-name gate.)
    assert!(codes("echo [];\necho [].len();\necho [].first();\n").is_empty());
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
fn a_resolved_mut_binding_rejects_an_incompatible_reassignment() {
    // Stable typing: a reassignment must match the binding's established type — even nested in an
    // `if`, and even for an *inferred* type. `x` is `int`, so assigning a `string` is E0007 (use a
    // declared union or `dyn` for a genuinely multi-type binding). This reverses the old flow-typed
    // `mut`, where a reassignment silently retyped the binding.
    let bad = "mut x = 1;\nif true { x = \"now a string\"; }\n";
    assert_eq!(codes(bad), ["E0007"]);
    // A *compatible* reassignment (same type) in a nested scope is fine and stays in scope.
    let ok = "mut x = 1;\nif true { x = 2; }\ns: int = x;\n";
    assert!(codes(ok).is_empty());
}

// ----- list spread `[...xs, x]` (L2, desugars to `~`) -----

#[test]
fn list_spread_types_as_the_unified_list() {
    use noeta_types::Type;
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
    use noeta_types::Type;
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
    use noeta_types::Type;
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
fn pipeline_binds_named_arguments() {
    // A label on the right of a pipe binds to the parameter it names; the piped value takes the
    // first parameter no label claimed. Both orders type-check, and both are the SAME call.
    let add = "fn add(a: int, b: string): string { return b ~ a; }\n";
    assert!(codes(&format!("{add}echo 5 |> add(b: \"x\");\n")).is_empty());
    assert!(codes(&format!("{add}echo \"x\" |> add(a: 5);\n")).is_empty());
    // The types follow the binding, not the written position: piping a `string` into the call whose
    // label already claimed `b` leaves it bound to `a: int`, which is the ordinary mismatch. This
    // is what silently passed while `|>` discarded labels.
    assert_eq!(
        codes(&format!("{add}echo \"x\" |> add(b: \"y\");\n")),
        ["E0007"]
    );
    // A label naming no parameter is caught through a pipe, exactly as in a direct call.
    assert_eq!(
        codes(&format!("{add}echo 5 |> add(zzz: \"x\");\n")),
        ["E0061"]
    );
    // …as is naming the same parameter twice.
    assert_eq!(
        codes(&format!("{add}echo 5 |> add(b: \"x\", b: \"y\");\n")),
        ["E0061"]
    );
    // A label may skip a defaulted parameter through a pipe: the piped value supplies `a` and `b`
    // keeps its default.
    let f = "fn f(a: int, b: int = 2, c: int = 3): int { return a + b + c; }\n";
    assert!(codes(&format!("{f}echo 1 |> f(c: 9);\n")).is_empty());
    // The piped value counts toward arity — `sub` is fully supplied by the pipe plus its label, so
    // the extra positional is one argument too many and reports as an ordinary arity error.
    assert_eq!(
        codes("fn sub(a: int, b: int): int { return a - b; }\necho 1 |> sub(2, a: 3);\n"),
        ["E0007"]
    );
}

#[test]
fn pipeline_passes_argument_expressions_for_literal_adaptation() {
    // A bare literal adapts into a fixed-width parameter through a pipe, on both sides of the `|>`
    // — `takes(200, 5)` always did, but the piped form reported TWO spurious `E0007`s because the
    // pipeline handed the checker no argument expressions to adapt.
    let src = "fn takes(x: u8, y: u8): u8 { return x + y; }\necho 200 |> takes(5);\n";
    assert!(codes(src).is_empty());
}

#[test]
fn a_label_that_cannot_bind_is_rejected_not_ignored() {
    // A callee that declares no parameter names has nothing for a label to name. Every one of these
    // used to check CLEAN with the label silently discarded, which is the same silent-wrongness
    // that labelled user calls were fixed for.
    //
    // A built-in METHOD — resolved from the receiver's type rather than a module signature, so it
    // has no `ExtFn::param_names` to consult. Including a label naming nothing at all.
    assert_eq!(
        codes("echo \"abc\".replace(zzz: \"a\", \"b\");\n"),
        ["E0061"]
    );
    assert_eq!(codes("echo [1, 2, 3].map(f: fn(n) => n * 2);\n"), ["E0061"]);
    // A function VALUE: the closure had parameter names, but `Type::Fn` carries only types, so the
    // call site cannot see them. No registry entry can ever fix this one.
    assert_eq!(
        codes("g = fn(a: int, b: int): int => a - b;\necho g(b: 1, a: 10);\n"),
        ["E0061", "E0061"]
    );
}

/// A **skipping** named call carries a supplied mask — one `u64`, shifted up by one for a method's
/// receiver — so it can only name parameters within `MASKED_PARAM_LIMIT`.
///
/// Regression: the bound was checked over the position of the first *hole*, so `f(1, z: 5)` over 66
/// parameters (hole at 1, `z` at 65) checked clean, lowering dropped `z`'s out-of-range bit from the
/// mask, and the argument landed on whichever parameter the shortened bit-count pointed at — where
/// that parameter's own default then overwrote it. An explicitly written argument vanished with
/// nothing said, which is the exact failure named arguments exist to remove.
///
/// Only skipping is bounded: a dense prefix carries no mask, and neither does a pure reordering.
#[test]
fn a_skipping_named_call_is_bounded_by_the_parameter_it_names() {
    let wide = |call: &str| {
        let mut params = vec!["a: int".to_string()];
        params.extend((1..65).map(|i| format!("p{i}: int = {i}")));
        params.push("z: int = 999".to_string());
        format!(
            "fn f({}): int {{ return a }}\necho {call};\n",
            params.join(", ")
        )
    };
    // Skips `p1..`, then names parameter 65 — refused rather than silently dropped.
    assert_eq!(codes(&wide("f(1, z: 5)")), ["E0061"]);
    // The same wide signature is fine when the call does not skip: a dense prefix, and a pure
    // reordering of one.
    assert!(codes(&wide("f(1, 7)")).is_empty());
    assert!(codes(&wide("f(p1: 7, a: 1)")).is_empty());
    // A skip within the limit is exactly what the named form is for.
    assert!(codes(&wide("f(1, p2: 7)")).is_empty());
}

#[test]
fn a_named_native_signature_binds_labels() {
    // `math.pow` declares `param_names: &["base", "exp"]`, so it accepts labels and REORDERS by
    // them — through exactly the binding a declared Noeta function uses. Written positionally it is
    // unchanged, and the reordered form used to compute 3² because the labels were discarded.
    let math = "use std.math;\n";
    assert!(codes(&format!("{math}echo math.pow(2.0, 3.0);\n")).is_empty());
    assert!(codes(&format!("{math}echo math.pow(base: 2.0, exp: 3.0);\n")).is_empty());
    assert!(codes(&format!("{math}echo math.pow(exp: 3.0, base: 2.0);\n")).is_empty());
    // And through a pipe: the label claims `exp`, so the piped value fills `base`.
    assert!(codes(&format!("{math}echo 2.0 |> math.pow(exp: 3.0);\n")).is_empty());
    // A label naming no parameter is caught precisely, and ONLY once — the recovery path must not
    // also report "does not take named arguments" for a signature that plainly does.
    assert_eq!(
        codes(&format!("{math}echo math.pow(zzz: 2.0, exp: 3.0);\n")),
        ["E0061"]
    );
}

#[test]
fn kernel_operands_are_typed_against_the_implementor() {
    // `SigType::SelfTy` resolves to the CONCRETE implementor at the call site, so a component-wise
    // operand must be the same shape and a bulk one a list of it. Both were `Dyn` before, so both of
    // these reached the runtime to fail there.
    let prelude = "use std.vec;\n\
                   @packed struct V3 { x: f32; y: f32; z: f32 }\n\
                   impl vec.Kernels for V3 {}\n\
                   a = V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 };\n\
                   b = V3 { x: 4.0f32, y: 5.0f32, z: 6.0f32 };\n";
    assert!(codes(&format!("{prelude}echo a.add(b).x;\n")).is_empty());
    assert!(codes(&format!("{prelude}echo [a].add_all([b])[0].x;\n")).is_empty());
    assert_eq!(codes(&format!("{prelude}echo a.add(5).x;\n")), ["E0007"]);
    assert_eq!(
        codes(&format!("{prelude}echo [a].add_all(a)[0].x;\n")),
        ["E0007"]
    );
    // `scale` is `SigType::Numeric`, NOT `Self::Elem`: the factor is width-agnostic on purpose, so
    // every numeric kind is accepted — including one that is not the element's own width — while a
    // non-number is refused. Typing it as the element would reject the `f32` factor on a `u8` shape
    // that the corpus relies on.
    assert!(codes(&format!("{prelude}echo a.scale(2.0f32).x;\n")).is_empty());
    assert!(codes(&format!("{prelude}echo a.scale(2).x;\n")).is_empty());
    assert!(codes(&format!("{prelude}echo a.scale(2.0).x;\n")).is_empty());
    assert!(codes(&format!("{prelude}echo a.scale(2i32).x;\n")).is_empty());
    assert_eq!(codes(&format!("{prelude}echo a.scale(a).x;\n")), ["E0007"]);
    assert_eq!(
        codes(&format!("{prelude}echo a.scale(\"x\").x;\n")),
        ["E0007"]
    );
}

#[test]
fn kernel_methods_bind_labels() {
    // A kernel method's names carry the ROLE its type cannot: `Self` says `other` must be the same
    // shape, but only the name says which of `scale`'s two readings is meant. This path used to pass
    // no argument expressions at all, so a label was dropped before anything could bind or refuse it.
    let prelude = "use std.vec;\n\
                   @packed struct V3 { x: f32; y: f32; z: f32 }\n\
                   impl vec.Kernels for V3 {}\n\
                   a = V3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 };\n\
                   b = V3 { x: 4.0f32, y: 5.0f32, z: 6.0f32 };\n";
    assert!(codes(&format!("{prelude}echo a.add(other: b).x;\n")).is_empty());
    assert!(codes(&format!("{prelude}echo a.scale(factor: 2.0f32).x;\n")).is_empty());
    assert_eq!(
        codes(&format!("{prelude}echo a.add(zzz: b).x;\n")),
        ["E0061"]
    );
}

#[test]
fn binding_consumes_labels_so_a_bound_call_is_not_rejected() {
    // The rejection above keys on "a label survived binding", so the callees that DO bind must come
    // out of `order_arguments` label-free. This is the guard on that: a declared function and
    // method still accept labels, positionally-out-of-order, skipping a default, and through a pipe.
    let src = "fn sub(a: int, b: int): int { return a - b; }\n\
               fn f(a: int, b: int = 2, c: int = 3): int { return a + b + c; }\n\
               echo sub(b: 1, a: 10);\necho f(1, c: 9);\necho 1 |> sub(a: 10);\n";
    assert!(codes(src).is_empty());
    let method = "class Box { pub v: int\n  fn scale(k: int, off: int): int { return self.v * k + off } }\n\
                  b = Box { v: 3 };\necho b.scale(off: 1, k: 2);\necho 2 |> b.scale(off: 1);\n";
    assert!(codes(method).is_empty());
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
               fn bump(by: int = 1): int { return self.start + by; }\n}\n\
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

/// Parse `text` and return the checker's per-binding destructor-relevance (Phase 3.2b).
fn relevance(text: &str) -> super::DestructorRelevance {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    super::check_all(&parsed.program).sites.destructor_relevance
}

#[test]
fn no_destructors_means_no_relevant_bindings() {
    // With no `destruct` block anywhere, no value's drop can run one, so nothing is relevant.
    let r = relevance("x = 5;\nfn f(a: int) {\n  echo a;\n}\nf(1);\n");
    assert!(r.locals.is_empty(), "locals: {:?}", r.locals);
    assert!(r.params.is_empty(), "params: {:?}", r.params);
}

#[test]
fn a_binding_of_a_destructor_bearing_type_is_relevant() {
    // `x` is an `R` (which has a `destruct` block) and `use_it`'s parameter is an `R`, so both are
    // recorded relevant; the `int` parameter `n` is not.
    let src = "class R {\n  n: int\n  fn new(n: int): R { return R { n: n }; }\n  destruct { echo n; }\n}\nfn use_it(r: R) {\n  echo r.n;\n}\nx = R.new(1);\nuse_it(x);\n";
    let r = relevance(src);
    assert_eq!(
        r.locals.len(),
        1,
        "the R-typed local `x` is relevant: {:?}",
        r.locals
    );
    assert_eq!(
        r.params.len(),
        1,
        "only the R-typed parameter `r` is relevant: {:?}",
        r.params
    );
}

#[test]
fn a_primitive_binding_is_not_relevant_even_when_destructors_exist() {
    // `R` has a destructor, but no binding/parameter here is `R`-typed, so relevance stays empty —
    // relevance is per-value, not per-program.
    let src = "class R {\n  n: int\n  destruct { echo n; }\n}\nfn g(a: int) {\n  echo a;\n}\nm = 5;\ng(1);\n";
    let r = relevance(src);
    assert!(
        r.locals.is_empty(),
        "primitive local not relevant: {:?}",
        r.locals
    );
    assert!(
        r.params.is_empty(),
        "primitive param not relevant: {:?}",
        r.params
    );
}

#[test]
fn a_list_of_a_destructor_bearing_type_is_relevant() {
    // Dropping a `List<R>` releases its elements, which run `R`'s destructor — transitive reach.
    let src = "class R {\n  n: int\n  destruct { echo n; }\n}\nfn collect(items: List<R>) {\n  echo len(items);\n}\necho 1;\n";
    let r = relevance(src);
    assert_eq!(
        r.params.len(),
        1,
        "the List<R> parameter is relevant: {:?}",
        r.params
    );
}

// --- P-PACK Phase 2: packed-list construction-site channel (`resolve_packed_list_sites`) ---

use super::resolve_packed_list_sites;
use noeta_ast::reflect::{PackedKind, PackedLayout};

/// Parse `text` and return the `PackedLayout`s recorded at its list-construction sites.
fn packed_layouts(text: &str) -> Vec<PackedLayout> {
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    resolve_packed_list_sites(&parsed.program)
        .into_values()
        .collect()
}

#[test]
fn packed_list_literal_is_recorded() {
    let layouts = packed_layouts(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         xs = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo xs.len()\n",
    );
    assert_eq!(layouts.len(), 1);
    let l = &layouts[0];
    assert_eq!(l.type_name, "Vec3");
    assert_eq!(l.fields.len(), 3);
    assert!(l.fields.iter().all(|f| f.kind == PackedKind::Float));
    assert_eq!(
        l.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        ["x", "y", "z"]
    );
    assert_eq!(l.word_count(), 3);
}

#[test]
fn packed_list_with_annotation_is_recorded() {
    // The check-position arm (`xs: List<Vec3> = [...]`) records too.
    let layouts = packed_layouts(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         xs: List<Vec3> = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo xs.len()\n",
    );
    assert_eq!(layouts.len(), 1);
    assert_eq!(layouts[0].type_name, "Vec3");
}

#[test]
fn nested_packed_list_flattens() {
    let layouts = packed_layouts(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         @packed struct Segment { start: Vec3; end: Vec3 }\n\
         v = Vec3 { x: 1.0, y: 2.0, z: 3.0 }\n\
         xs = [Segment { start: v, end: v }]\n\
         echo xs.len()\n",
    );
    assert_eq!(layouts.len(), 1);
    let l = &layouts[0];
    assert_eq!(l.type_name, "Segment");
    assert_eq!(l.fields.len(), 2);
    // Each field is a nested packed Vec3 (3 words), so a Segment is 6 words flat.
    assert!(matches!(l.fields[0].kind, PackedKind::Struct(_)));
    assert_eq!(l.word_count(), 6);
}

#[test]
fn packed_list_with_int_and_bool_kinds() {
    let layouts = packed_layouts(
        "@packed struct Cell { n: int; on: bool }\n\
         xs = [Cell { n: 1, on: true }]\n\
         echo xs.len()\n",
    );
    assert_eq!(layouts.len(), 1);
    assert_eq!(layouts[0].fields[0].kind, PackedKind::Int);
    assert_eq!(layouts[0].fields[1].kind, PackedKind::Bool);
}

#[test]
fn non_packed_struct_list_is_not_recorded() {
    // An ordinary (non-`@packed`) value struct stays on the boxed representation.
    let layouts = packed_layouts(
        "struct Vec3 { x: float; y: float; z: float }\n\
         xs = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo xs.len()\n",
    );
    assert!(layouts.is_empty());
}

#[test]
fn primitive_list_is_not_recorded() {
    assert!(packed_layouts("xs = [1, 2, 3]\necho xs.len()\n").is_empty());
}

// --- P-PACK 2.5+: fused `list[i].field` site channel (`Checked::index_field_sites`) ---

/// Parse + check `text` and return how many `list[i].field` reads the checker marked fusable.
fn index_field_count(text: &str) -> usize {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    super::check_all(&parsed.program)
        .sites
        .index_field_sites
        .len()
}

#[test]
fn packed_indexed_field_read_is_fusable() {
    let n = index_field_count(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         ps = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo ps[0].x\n",
    );
    assert_eq!(n, 1);
}

#[test]
fn boxed_struct_indexed_field_read_is_also_fusable() {
    // Fusion is not packed-only: a `List<struct>[i].field` read fuses too (the backends' fast path is
    // keyed on the runtime packed representation; a boxed list takes the equivalent fallback).
    let n = index_field_count(
        "struct P { x: int; y: int }\n\
         ps = [P { x: 1, y: 2 }]\n\
         echo ps[0].x\n",
    );
    assert_eq!(n, 1);
}

#[test]
fn map_indexed_field_read_is_not_fusable() {
    // A `map[key].field` indexes a `Map`, not a `List`, so it stays on the generic index+field path
    // (the fused op's fallback is list-only).
    let n = index_field_count(
        "struct P { x: int; y: int }\n\
         m = { \"a\": P { x: 1, y: 2 } }\n\
         echo m[\"a\"].x\n",
    );
    assert_eq!(n, 0);
}

#[test]
fn indexed_method_call_is_not_fusable() {
    // `list[i].method()` is a method call (the member is a call callee), never a field read, so no
    // fuse site is recorded.
    let n = index_field_count(
        "struct P { x: int\n  fn get_x(self): int { return self.x } }\n\
         ps = [P { x: 1 }]\n\
         echo ps[0].get_x()\n",
    );
    assert_eq!(n, 0);
}

// --- P-PACK 2.6 category B: `map(...)` packed-result site channel (`Checked::map_packed_sites`) ---

/// Parse + check `text` and return how many `map(...)` calls produce a packed result.
fn map_packed_count(text: &str) -> usize {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    super::check_all(&parsed.program)
        .sites
        .map_packed_sites
        .len()
}

#[test]
fn map_to_packed_struct_is_recorded() {
    let n = map_packed_count(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         ps = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo ps.map(fn(v) => Vec3 { x: v.x + 1.0, y: v.y, z: v.z }).len()\n",
    );
    assert_eq!(n, 1);
}

#[test]
fn map_to_primitive_is_not_recorded() {
    // `ps.map(fn(v) => v.x)` produces `List<float>` — not a packed struct, so it stays boxed.
    let n = map_packed_count(
        "@packed struct Vec3 { x: float; y: float; z: float }\n\
         ps = [Vec3 { x: 1.0, y: 2.0, z: 3.0 }]\n\
         echo ps.map(fn(v) => v.x).len()\n",
    );
    assert_eq!(n, 0);
}

#[test]
fn map_to_non_packed_struct_is_not_recorded() {
    let n = map_packed_count(
        "struct P { x: int; y: int }\n\
         ps = [P { x: 1, y: 2 }]\n\
         echo ps.map(fn(v) => P { x: v.x + 1, y: v.y }).len()\n",
    );
    assert_eq!(n, 0);
}

// --- `Checked::packed_layouts` — the name-keyed IDE storage-fact index ---

/// Parse + check `text` and return the name→layout table the IDE reads.
fn packed_layout_table(text: &str) -> std::collections::HashMap<String, PackedLayout> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    super::check_all(&parsed.program).packed_layouts
}

#[test]
fn packed_layouts_index_every_packed_struct_by_name() {
    let table = packed_layout_table(
        "@packed struct Vec3 { x: f32; y: f32; z: f32 }\n\
         @packed(Layout.Column) struct Particle { n: int; alive: bool }\n\
         struct Boxed { s: string }\n\
         echo 1\n",
    );
    assert_eq!(table.len(), 2, "non-packed `Boxed` is absent: {table:?}");
    let vec3 = &table["Vec3"];
    assert!(!vec3.column);
    assert_eq!(vec3.byte_size(), 12); // 3 × f32
    let particle = &table["Particle"];
    assert!(particle.column);
    assert_eq!(particle.byte_size(), 9); // int(8) + bool(1)
}

#[test]
fn packed_layouts_empty_without_packed_structs() {
    let table = packed_layout_table("struct P { x: int; y: int }\necho 1\n");
    assert!(table.is_empty());
}

#[test]
fn plain_field_read_is_not_fusable() {
    // A field read whose receiver is not an index expression is untouched.
    let n = index_field_count(
        "struct P { x: int; y: int }\n\
         p = P { x: 1, y: 2 }\n\
         echo p.x\n",
    );
    assert_eq!(n, 0);
}

#[test]
fn non_void_function_that_can_fall_through_is_e0048() {
    // Falls off the end: the body's last statement is a bare expression, not a `return`, so the
    // function would implicitly yield `unit` where `int` was promised.
    assert_eq!(codes("fn f(n: int): int { n + 1 }\n"), ["E0048"]);
    // An `if` without an `else` leaves the false path open to the end.
    assert_eq!(
        codes("fn g(n: int): int { if n > 0 { return 1 } }\n"),
        ["E0048"]
    );
    // Only one arm of an `if`/`else` returns.
    assert_eq!(
        codes("fn h(n: int): int { if n > 0 { return 1 } else { echo \"no\" } }\n"),
        ["E0048"]
    );
    // Methods are checked the same way.
    assert_eq!(
        codes("class Box { mut v: int\n  fn get(): int { echo \"peek\" } }\n"),
        ["E0048"]
    );
}

#[test]
fn bare_return_in_a_non_void_function_is_a_type_mismatch() {
    // A bare `return;` yields unit, which a non-`void` return type does not admit — the
    // statement escapes the function without the promised value. (`can-fall-through` E0048 is a
    // *separate* check for reaching the end with no `return` at all; here the `return` is present
    // but valueless, so it is a plain E0007 `expected int, found void`.)
    assert_eq!(codes("fn f(n: int): int { return }\n"), ["E0007"]);
    // Reachable behind a branch, too — the mismatch is at the `return`, not the fall-through.
    assert_eq!(
        codes("fn g(n: int): int { if n > 0 { return } return n }\n"),
        ["E0007"]
    );
    // A `void` function's bare `return` is exactly right; `dyn` admits unit as well.
    assert!(codes("fn v(): void { return }\n").is_empty());
    assert!(codes("fn d(): dyn { return }\n").is_empty());
}

#[test]
fn function_returning_or_diverging_on_every_path_is_clean() {
    // An explicit `return`, or an `if`/`else` where both arms return.
    assert!(codes("fn r(n: int): int { return n + 1 }\n").is_empty());
    assert!(codes("fn c(n: int): int { if n > 0 { return 1 } else { return 2 } }\n").is_empty());
    // A `void` function may fall off the end (only `void` may).
    assert!(codes("fn v(n: int): void { echo \"hi\" }\n").is_empty());
    // `panic` never returns; an infinite `while true` (no `break`) never falls through.
    assert!(codes("fn p(n: int): int { panic(\"no\") }\n").is_empty());
    assert!(codes("fn w(n: int): int { while true { if n > 0 { return 1 } } }\n").is_empty());
    // `dyn` admits `unit`, so falling through is well-typed and not flagged.
    assert!(codes("fn d(n: int): dyn { echo \"hi\" }\n").is_empty());
}

#[test]
fn exhaustive_match_whose_arms_all_return_is_a_return() {
    // `Ok`/`Err` is the whole of `Result` and `some`/`none` the whole of `Option`, so a two-arm
    // `match` over either is exhaustive: control always enters an arm, and every arm returns.
    // Blocks never yield values (E0055), so a bailing arm MUST be a block with a `return` — this
    // is the idiomatic fallible pipeline and needs no unreachable trailing `return`.
    assert!(
        codes(
            "fn f(r: Result<int, string>): int { match r { Ok(v) => { return v }, \
             Err(_) => { return 0 }, } }\n"
        )
        .is_empty()
    );
    assert!(
        codes(
            "fn g(o: ?int): int { match o { some(v) => { return v }, none => { return 0 }, } }\n"
        )
        .is_empty()
    );
    // A user enum with every variant covered.
    assert!(
        codes(
            "enum C { A; B }\n\
             fn f(c: C): int { match c { C.A => { return 1 }, C.B => { return 2 }, } }\n"
        )
        .is_empty()
    );
    // A `_` catch-all is irrefutable, so even an open `int` domain is exhaustive.
    assert!(
        codes("fn f(n: int): int { match n { 1 => { return 1 }, _ => { return 0 }, } }\n")
            .is_empty()
    );
    // An arm that `panic`s diverges just as a returning one does.
    assert!(
        codes(
            "fn f(r: Result<int, string>): int { match r { Ok(v) => { return v }, \
             Err(_) => { panic(\"no\") }, } }\n"
        )
        .is_empty()
    );
    // Expression-bodied arms that all diverge.
    assert!(
        codes(
            "fn f(r: Result<int, string>): int { match r { Ok(_) => panic(\"a\"), \
             Err(_) => panic(\"b\"), } }\n"
        )
        .is_empty()
    );
    // Nested: a `match` inside a block arm.
    assert!(
        codes(
            "enum C { A; B }\n\
             fn f(c: C, r: Result<int, string>): int { match c { \
             C.A => { match r { Ok(v) => { return v }, Err(_) => { return 0 }, } }, \
             C.B => { return 9 }, } }\n"
        )
        .is_empty()
    );
    // A `match` in the tail position of an `if` branch: the `if` diverges because both blocks do.
    assert!(
        codes(
            "fn f(b: bool, r: Result<int, string>): int { if b { \
             match r { Ok(v) => { return v }, Err(_) => { return 0 }, } } else { return 9 } }\n"
        )
        .is_empty()
    );
    // Inside a `while true`, which never exits normally on its own.
    assert!(
        codes(
            "fn f(r: Result<int, string>): int { while true { \
             match r { Ok(v) => { return v }, Err(_) => { return 0 }, } } }\n"
        )
        .is_empty()
    );
    // The type domain rides the same judgement: `is` arms covering every member of a closed union
    // are exhaustive, so an all-returning type-pattern `match` returns too.
    assert!(
        codes(
            "fn f(x: int | string): int { match x { is int => { return 1 }, \
             is string => { return 2 }, } }\n"
        )
        .is_empty()
    );
    // …but `dyn` is the open top: no finite set of `is` arms exhausts it, so E0048 stands
    // (alongside the E0011 the open domain already earns).
    assert_eq!(
        codes(
            "fn f(x: dyn): int { match x { is int => { return 1 }, \
             is string => { return 2 }, } }\n"
        ),
        ["E0011", "E0048"]
    );
}

#[test]
fn match_that_is_not_provably_exhaustive_still_falls_through_e0048() {
    // The rule is gated on EXHAUSTIVENESS — the checker's own E0011 judgement, not a second
    // approximation. Counting a `match` that can fail would let a function fall off its end.
    //
    // A guarded arm proves nothing (the guard may be false), so `C.A` stays uncovered: E0011 for
    // the coverage hole, E0048 for the path it leaves open to the end of the body.
    assert_eq!(
        codes(
            "enum C { A; B }\n\
             fn f(c: C, hot: bool): int { match c { C.A if hot => { return 1 }, \
             C.B => { return 2 }, } }\n"
        ),
        ["E0011", "E0048"]
    );
    // An `int` scrutinee has an open domain and there is no `_`. E0011 stays silent (open domains
    // are the runtime backstop's job) but the path to the end is real, so E0048 fires.
    assert_eq!(
        codes("fn f(n: int): int { match n { 1 => { return 1 }, 2 => { return 2 }, } }\n"),
        ["E0048"]
    );
    // Exhaustive, but one arm falls out of its own block instead of returning.
    assert_eq!(
        codes(
            "enum C { A; B }\n\
             fn f(c: C): int { match c { C.A => { return 1 }, C.B => { echo \"b\" }, } }\n"
        ),
        ["E0048"]
    );
    // Exhaustive with expression arms that produce values rather than diverging: the `match` is a
    // discarded statement, so the body still reaches its end.
    assert_eq!(
        codes("fn f(r: Result<int, string>): int { match r { Ok(v) => v, Err(_) => 0, } }\n"),
        ["E0048"]
    );
    // A guarded arm followed by an irrefutable `_` IS total — the `_` covers what the guard
    // declines — so this one is clean, and the two cases above are not a blanket "guards lose".
    assert!(
        codes("fn f(n: int, hot: bool): int { match n { 1 if hot => { return 1 }, _ => { return 0 }, } }\n")
            .is_empty()
    );
}

// --- SessionChecker (session-checker C0/C1): per-entry checking against an accumulated session ---

/// Parse one entry with its own `SourceId` (as the REPL/console assigns them) and check it against
/// `session`, returning this entry's diagnostic codes.
fn entry_codes(session: &mut super::SessionChecker, id: u32, text: &str) -> Vec<String> {
    seed_std();
    let source = Source::new(SourceId(id), format!("<entry:{id}>"), text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "entry must parse cleanly: {:?}",
        parsed.diagnostics
    );
    session
        .check_entry(&parsed.program)
        .iter()
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn a_session_entry_sees_what_earlier_entries_committed() {
    seed_std();
    let mut session = super::SessionChecker::new();
    // Entry 1: a fn, a type, a binding — all clean.
    assert!(
        entry_codes(
            &mut session,
            0,
            "fn twice(n: int): int { return n * 2 }\nstruct P { x: int }\nmut total = 1\n",
        )
        .is_empty()
    );
    // Entry 2: uses all three across the entry boundary.
    assert!(
        entry_codes(
            &mut session,
            1,
            "mut p = P { x: twice(total) }\ntotal = p.x\n",
        )
        .is_empty()
    );
}

#[test]
fn forward_references_work_within_an_entry_and_unknown_names_stay_runtime_deferred() {
    seed_std();
    let mut session = super::SessionChecker::new();
    // Within one entry, a call may precede the declaration (collect runs first) — like any file.
    assert!(
        entry_codes(
            &mut session,
            0,
            "fn a(): int { return b() }\nfn b(): int { return 1 }\n",
        )
        .is_empty()
    );
    // A not-yet-defined name is NOT a static error — the checker's deliberate unknown-ident
    // tolerance defers it to the runtime E0005, in a session entry exactly as in a file. (So a
    // cross-entry "forward reference" fails at run time, not at the prompt's check.)
    assert!(entry_codes(&mut session, 1, "echo later()\n").is_empty());
    // Defining it in a later entry makes subsequent uses statically KNOWN (typed, not deferred).
    assert!(entry_codes(&mut session, 2, "fn later(): int { return 3 }\n").is_empty());
    assert!(entry_codes(&mut session, 3, "mut n: int = later()\n").is_empty());
}

#[test]
fn mut_stability_rules_apply_across_entries() {
    seed_std();
    let mut session = super::SessionChecker::new();
    assert!(entry_codes(&mut session, 0, "mut n = 1\nfixed = 2\n").is_empty());
    // Compatible reassignment across the boundary: fine.
    assert!(entry_codes(&mut session, 1, "n = 5\n").is_empty());
    // Incompatible reassignment across the boundary: E0007, exactly as within a file.
    assert_eq!(entry_codes(&mut session, 2, "n = \"s\"\n"), vec!["E0007"]);
    // Reassigning an immutable binding from an earlier entry: E0006.
    assert_eq!(entry_codes(&mut session, 3, "fixed = 3\n"), vec!["E0006"]);
    // Re-`mut` re-declares — even retyped — because the language allows it.
    assert!(entry_codes(&mut session, 4, "mut n = \"now a string\"\n").is_empty());
    assert!(entry_codes(&mut session, 5, "n = \"still one\"\n").is_empty());
}

#[test]
fn an_erroring_entry_leaves_the_session_usable_and_diagnostics_do_not_leak() {
    seed_std();
    let mut session = super::SessionChecker::new();
    // A genuinely static error mid-entry (mut retype within the entry).
    assert_eq!(
        entry_codes(&mut session, 0, "mut oops = 1\noops = \"s\"\n"),
        vec!["E0007"]
    );
    // The session is still usable, and the failed entry's diagnostics did not leak forward.
    assert!(entry_codes(&mut session, 1, "mut ok = 1\necho ok\n").is_empty());
}

#[test]
fn reserved_names_refuse_in_a_session_entry() {
    seed_std();
    let mut session = super::SessionChecker::new();
    // A prelude value name (E0046) and a reserved language-level type name (E0049) refuse, as in a
    // file. (A registered extern type like `Uuid` is no longer reserved — see the file-level test.)
    assert_eq!(
        entry_codes(&mut session, 0, "fn panic(): int { return 1 }\n"),
        vec!["E0046"]
    );
    assert_eq!(
        entry_codes(&mut session, 1, "struct Iterator { x: int }\n"),
        vec!["E0049"]
    );
}

#[test]
fn destruct_reachability_refixpoints_over_the_accumulated_registry() {
    seed_std();
    let mut session = super::SessionChecker::new();
    // Entry 1: a plain container — nothing destructor-bearing yet.
    assert!(
        entry_codes(
            &mut session,
            0,
            "class Holder { r: Res }\nstruct Res { x: int }\n"
        )
        .is_empty()
    );
    // Entry 2 re-declares Res as a destructor class; the fixpoint over the ACCUMULATED registry
    // must now mark Holder reachable too.
    assert!(
        entry_codes(
            &mut session,
            1,
            "class Res { x: int\n    destruct { echo \"drop\" }\n}\n",
        )
        .is_empty()
    );
    assert!(
        session
            .sites_snapshot()
            .destructor_relevance
            .reachable_types
            .contains("Holder")
    );
}

#[test]
fn an_erroring_entry_is_transactional_and_commits_nothing() {
    seed_std();
    let mut session = super::SessionChecker::new();
    // The entry binds `fixed2` immutably AND then errors — the whole entry rolls back.
    assert_eq!(
        entry_codes(&mut session, 0, "fixed2 = 2\nmut boom: int = \"s\"\n"),
        vec!["E0007"]
    );
    // `fixed2` was never committed: binding it fresh (not E0006-reassigning) is clean.
    assert!(entry_codes(&mut session, 1, "fixed2 = 3\nfixed2 = 4\n") == vec!["E0006"]);
}

// ----- F1: unknown-name gate (a genuinely undefined name is a static E0005) -----

#[test]
fn unknown_names_are_caught_at_check_time_in_a_file() {
    // A call to an undefined function, and a bare reference to an undefined value, are both
    // static errors now — the gap that let a typo swap into a hot-reloaded server and fail at
    // request time instead of showing the diagnostic.
    assert_eq!(codes("x = nonexistent_fn()\n"), vec!["E0005"]);
    assert_eq!(codes("y = undefined_name\n"), vec!["E0005"]);
    assert_eq!(codes("echo also_missing()\n"), vec!["E0005"]);
    // Inside a closure body too.
    assert_eq!(codes("f = fn() => gone()\necho \"ok\"\n"), vec!["E0005"]);
}

#[test]
fn legitimate_forward_and_nested_references_stay_clean() {
    // Top-level fns forward-reference each other (two-pass collect).
    assert!(codes("fn a(): int { return b() }\nfn b(): int { return 1 }\necho a()\n").is_empty());
    // A fn body reaches a top-level global — declared later, even — through its `use (…)`
    // capture clause (sealed named fns: the clause is the explicit import; hoisting still
    // makes the forward declaration visible to it).
    assert!(codes("fn use_g() use (g): int { return g }\ng = 10\necho use_g()\n").is_empty());
    // Without the clause the global is out of scope — the sealed-fn E0005.
    assert_eq!(
        codes("fn use_g(): int { return g }\ng = 10\necho use_g()\n"),
        vec!["E0005"]
    );
    // A nested fn calls a sibling / itself / an enclosing global.
    assert!(
        codes(
            "fn outer(): int {\n  \
               fn inner(): int { return 2 }\n  \
               return inner() + inner()\n\
             }\n\
             echo outer()\n"
        )
        .is_empty()
    );
    // A local closure value is callable without being flagged.
    assert!(codes("f = fn(x: int): int => x + 1\necho f(5)\n").is_empty());
    // A `concurrent {}` binding leaks to the enclosing scope (transparent scope).
    assert!(
        codes(
            "use std.task.{sleep}\n\
             async fn run(): int {\n  \
               concurrent { w = 7 }\n  \
               return w\n\
             }\n\
             echo run().await\n"
        )
        .is_empty()
    );
    // The prelude and built-in enums are always known.
    assert!(codes("panic(\"x\")\n").is_empty());
    assert!(codes("echo Ordering.Less\n").is_empty());
}

#[test]
fn a_repl_session_defers_unknown_names_to_a_later_entry() {
    // A session is the ONE place an unknown name stays deferred — a later entry may define it.
    // Seed before construction: register_prelude resolves against the process-default registry.
    seed_std();
    let mut session = super::SessionChecker::new();
    assert!(entry_codes(&mut session, 0, "echo later()\n").is_empty());
    assert!(entry_codes(&mut session, 1, "fn later(): int { return 3 }\n").is_empty());
    assert!(entry_codes(&mut session, 2, "echo later()\n").is_empty());
}

#[test]
fn the_checker_resolves_native_names_against_the_injected_registry() {
    // Instance-registry F2 (IR2): a checker given an explicit `Registry` resolves every native
    // name against *that* registry, not the process-global default. Proven differentially — the
    // same std-using program is clean against a registry that has `std`, and unresolved against
    // one that does not.
    use noeta_stdlib::registry::Registry;

    // `math.sqrt` takes a `float`; calling it with a `string` is a type error (E0007) — but ONLY
    // if the checker can see `sqrt`'s signature, which it reads from the registry. This makes the
    // registry the sole cause of the diagnostic, so its presence/absence is a clean injection probe.
    let src = "use std.{math};\necho math.sqrt(\"x\");\n";
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "program must parse cleanly");
    let has_e0007 =
        |c: &super::Checked| c.diagnostics.iter().any(|d| d.code.to_string() == "E0007");

    // Against the default (std installed), the checker knows `sqrt(float)` and flags the string.
    let with_std = noeta_stdlib::registry::default_seeded();
    assert!(
        has_e0007(&super::check_all_with_registry(&parsed.program, with_std)),
        "the checker must read `sqrt`'s signature from a registry that has std"
    );

    // Against an EMPTY registry, `std.math` never resolves, so there is no signature to check the
    // argument against and the E0007 disappears — the checker consulted the injected registry, not
    // a global default (which, being installed, would still know `sqrt`).
    let empty: &'static Registry = Box::leak(Box::new(Registry::new(vec![])));
    assert!(
        !has_e0007(&super::check_all_with_registry(&parsed.program, empty)),
        "an empty registry leaves `math.sqrt` unresolved, so no signature-mismatch can fire"
    );
}

#[test]
fn directives_on_a_type_and_associated_method_are_clean() {
    // `@doc` on a type and a method, and `@test` on an associated method (no `self`), are all
    // permitted sites for the std tiers, so the program checks clean.
    let src = "@doc { A point. }\n\
               struct Point {\n    \
               x: int = 0\n    \
               @doc { Distance from origin. }\n    \
               fn manhattan(): int { return self.x }\n    \
               @test\n    \
               fn origin_is_zero(): void { assert(Point {}.manhattan() == 0, \"o\") }\n\
               }\n";
    assert!(codes(src).is_empty(), "expected clean: {:?}", codes(src));
}

#[test]
fn test_on_an_instance_method_is_invalid_site() {
    // A `@test` method that reads `self` is an instance method — the runner has no receiver to call
    // it on, so it is rejected (E0054, directive attachment-site model).
    let src = "class Counter {\n    \
               mut n: u64 = 0u64\n    \
               @test\n    \
               fn reads_self(): void { assert(self.n == 0u64, \"n\") }\n\
               }\n";
    assert_eq!(codes(src), ["E0054"]);
}

#[test]
fn attribute_target_kind_vocabulary_is_in_lockstep() {
    // The shared `ATTRIBUTE_TARGET_KINDS` vocabulary (diagnostics help + IDE completion) accepts
    // exactly what `TargetKind::from_name` parses.
    for name in noeta_ast::reflect::ATTRIBUTE_TARGET_KINDS {
        assert!(
            crate::TargetKind::from_name(name).is_some(),
            "`{name}` is in the vocabulary but not parsed"
        );
    }
    // The one historic drift: the old help text said `Record`, which was never accepted.
    assert!(crate::TargetKind::from_name("Record").is_none());
}

#[test]
fn derive_of_a_fully_defaulted_user_trait_checks_clean() {
    // UT5: deriving a user trait adopts its defaults; valid when every method has one.
    let src =
        "trait D {\n    fn label(): string { return \"x\" }\n}\n@derive(D)\nstruct P { n: int }\n";
    assert_eq!(codes(src), Vec::<String>::new());
}

#[test]
fn derive_of_a_user_trait_with_required_methods_is_e0050() {
    let src = "trait G {\n    fn greet(who: string): string\n    fn shout(): string { return \"HI\" }\n}\n@derive(G)\nstruct P { n: int }\n";
    assert_eq!(codes(src), ["E0050"]);
}

#[test]
fn derive_of_a_generic_user_trait_is_e0050() {
    let src = "trait C<T> {\n    fn nop(): int { return 0 }\n}\n@derive(C)\nstruct P { n: int }\n";
    assert_eq!(codes(src), ["E0050"]);
}

#[test]
fn derived_user_trait_satisfies_dyn_coercion_and_types_the_defaults() {
    // The derive registers trait membership (dyn coercion checks) and the default method's
    // signature (the member call types as `string`, not a hole).
    let src = "trait D {\n    fn label(): string { return \"x\" }\n    fn describe(): string { return self.label() }\n}\n@derive(D)\nstruct P { n: int }\nfn takes(d: dyn D): string { return d.describe() }\np = P { n: 1 }\nout: string = p.describe()\necho takes(p)\necho out\n";
    assert_eq!(codes(src), Vec::<String>::new());
}

#[test]
fn try_through_inference_seeds_the_success_arm() {
    // D1 (poly-deferrals): `o: Order = load(text)?` binds `T = Order` through the `?` unwrap; a
    // wrong expectation is caught as the ordinary subsumption mismatch (proving the seed flowed,
    // rather than the payload erasing to `dyn`).
    let ok = "struct Order { id: int }\nfn wrap<T>(v: T): Result<T, string> { return Ok(v) }\nfn go(o: Order): Result<int, string> {\n    r: Order = wrap(o)?\n    return Ok(r.id)\n}\n";
    assert_eq!(codes(ok), Vec::<String>::new());
    // The seed WINS over the argument (first-wins, as in every seeded position): `T` is pinned
    // to `Order` by the expectation, so the `int` argument fails assignability against it.
    let bad = "struct Order { id: int }\nfn wrap<T>(v: T): Result<T, string> { return Ok(v) }\nfn go(o: Order): Result<int, string> {\n    r: Order = wrap(5)?\n    return Ok(r.id)\n}\n";
    assert_eq!(codes(bad), ["E0007"]);
}

#[test]
fn coalesce_seeds_from_fallback_type() {
    // D1: `v = wrap(o) ?? fallback` — the fallback's type seeds the success arm, so the binding
    // is precisely typed (`.id` on the result checks; a bogus member would not).
    let src = "struct Order { id: int }\nfn wrap<T>(v: T): Result<T, string> { return Ok(v) }\no = Order { id: 1 }\npicked = wrap(o) ?? o\necho picked.id\n";
    assert_eq!(codes(src), Vec::<String>::new());
}

/// The plain `@derive(Error)` trait name is spelled in two crates: `BuiltinTrait::Error` in
/// `noeta-types`, and `noeta_ast::derive::ERROR_TRAIT` in the shared cascade (which cannot depend
/// on `noeta-types`). They drifted once already — the checker's cascade tested the enum's `name()`
/// while lowering's tested a bare `"Error"` — so pin them together.
#[test]
fn the_error_derive_name_agrees_across_crates() {
    assert_eq!(
        noeta_types::BuiltinTrait::Error.name(),
        noeta_ast::derive::ERROR_TRAIT
    );
}

/// Placement is checked for every declaration kind that can bear a directive — not for the three
/// somebody remembered. The walk is driven by `Stmt::decorated`, which is exhaustive over the
/// statement kinds, so this is a property of the code rather than of this list; the list is here to
/// notice if the wiring is ever cut.
#[test]
fn every_decorated_declaration_kind_is_placement_checked() {
    for src in [
        "@semantic\nstruct S { x: int }\n",
        "@semantic\nclass C { x: int }\n",
        "@packed\nenum E { A }\n",
        "@validated\ntrait T { fn f(): int }\n",
    ] {
        assert_eq!(codes(src), ["E0054"], "unchecked placement in: {src}");
    }
}

/// The diagnostic's noun comes from the site, not from a second hand-written string beside it.
/// Those two drifted: a misplaced directive on a struct said "a record" while its own help line,
/// computed from the same site, said "a struct".
#[test]
fn the_misplacement_noun_is_derived_from_the_site() {
    for (src, noun) in [
        ("@semantic\nstruct S { x: int }\n", "a struct"),
        ("@semantic\nclass C { x: int }\n", "a class"),
        ("@packed\nenum E { A }\n", "an enum"),
        ("@validated\ntrait T { fn f(): int }\n", "a trait"),
    ] {
        let msgs: Vec<String> = {
            seed_std();
            let source = Source::new(SourceId::FIRST, "test.noe", src);
            let lexed = lex(&source);
            let parsed = parse(&source, &lexed.tokens);
            check(&parsed.program)
                .iter()
                .map(|d| d.message.clone())
                .collect()
        };
        assert!(
            msgs.iter().any(|m| m.contains(noun)),
            "expected {noun:?} in {msgs:?}"
        );
    }
}

// ----- the body-coverage ledger (body-coverage arc) -----

/// A program exercising **one of every** [`BodyKind`], so the two tests below are talking about a
/// program that actually contains all six.
const EVERY_BODY_KIND: &str = r#"
trait Greeter {
    fn name(): string
    fn greet(): string { return "hi" }
}

class Person {
    tag: int
    fn new(tag: int): Person { return Person { tag: tag } }
    fn nested(): int {
        fn inner(): int { return 1 }
        return inner()
    }
    impl Display {
        fn to_string(): string { return "person" }
    }
    destruct { echo "gone" }
}

impl Greeter for Person {
    fn name(): string { return "person" }
}

fn free(): int { return 2 }
"#;

/// The walker sees every kind of body a program can declare.
///
/// This is the compile-time half of the guarantee's runtime companion: [`noeta_ast::bodies`]
/// matches `Stmt` exhaustively, so a new statement kind cannot be added without deciding whether it
/// owns bodies — and this pins that the decisions already made are actually wired up.
#[test]
fn the_ledger_enumerates_every_body_kind() {
    use noeta_ast::bodies::{BodyKind, body_sites};
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", EVERY_BODY_KIND);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "the sample program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let kinds: std::collections::HashSet<BodyKind> =
        body_sites(&parsed.program).iter().map(|s| s.kind).collect();
    for want in [
        BodyKind::Function,
        BodyKind::Method,
        BodyKind::StandaloneImplMethod,
        BodyKind::Destructor,
        BodyKind::TraitDefault,
    ] {
        assert!(
            kinds.contains(&want),
            "the ledger missed {want:?}; found {kinds:?}"
        );
    }
}

/// **The gate itself**: checking that program leaves no body unvisited.
///
/// The `debug_assert` in `verify_body_coverage` already fires on every checked program in every
/// debug build — including the whole conformance corpus — so this test's job is to state the
/// property explicitly and to fail with a readable message naming the missed sites, rather than
/// leaving the guarantee implicit in a panic somewhere else.
#[test]
fn the_checker_visits_every_body_it_is_given() {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", EVERY_BODY_KIND);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let missed = super::unchecked_bodies_for(&parsed.program);
    assert!(
        missed.is_empty(),
        "the checker never visited these bodies: {:?}",
        missed.iter().map(|s| s.describe()).collect::<Vec<_>>()
    );
}

// ----- E0063: unanswerable width `is` test (packed-widths slice 2) -----
//
// A bare-scalar `x is iN` / `x is f64` is the one test the *runtime* cannot answer: an erased width
// carries no tag on a scalar, so the shared matcher reaches no head and always says `false`. Where
// the scrutinee's static type settles the question the **checker** answers it instead and the
// constant is folded at lowering (`Sites::folded_type_tests`) — no warning, because a decided
// answer is not an erasure problem. E0063 is left for a scrutinee that leaves the width genuinely
// unrecoverable: a `dyn` launder, a union, an erased type parameter.
//
// The observable end of that (what the folded test *prints*, and the surviving warning) is
// corpus-pinned in `tests/conformance/narrowing/is_erased_width_static.noe`; what is pinned here is
// the checker-side rule — which scrutinees warn, which fold, and to what. (The reified `f32`
// narrowing is corpus-pinned in `tests/conformance/narrowing/f32_width_subtype.noe`.)

/// Return every warning-severity diagnostic's code for `text`, in order.
fn warn_codes(text: &str) -> Vec<String> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    check(&parsed.program)
        .iter()
        .filter(|d| d.severity == noeta_diagnostics::Severity::Warning)
        .map(|d| d.code.to_string())
        .collect()
}

#[test]
fn scalar_is_fixed_width_int_warns_erased() {
    for width in ["i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64"] {
        let src = format!("fn f(x: dyn): bool {{ return x is {width}; }}\n");
        assert_eq!(codes(&src), ["E0063"], "`is {width}` should warn E0063");
    }
}

#[test]
fn scalar_is_f64_warns_erased() {
    assert_eq!(
        codes("fn f(x: dyn): bool { return x is f64; }\n"),
        ["E0063"]
    );
}

#[test]
fn the_erased_width_diagnostic_is_a_warning_not_an_error() {
    // Advisory: the program is well-formed and still compiles (a warning, never an error), so the
    // erased-width test emits exactly one warning and no errors.
    let src = "fn f(x: dyn): bool { return x is i32; }\n";
    assert_eq!(warn_codes(src), ["E0063"]);
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        check(&parsed.program)
            .iter()
            .all(|d| d.severity != noeta_diagnostics::Severity::Error),
        "E0063 must be advisory, never an error"
    );
}

#[test]
fn scalar_is_f32_does_not_warn() {
    // `f32` is reified at runtime, so `x is f32` is a real test, not an always-false one.
    assert!(codes("fn f(x: dyn): bool { return x is f32; }\n").is_empty());
}

#[test]
fn scalar_is_base_types_do_not_warn() {
    for base in ["int", "float", "string", "bool", "bytes"] {
        let src = format!("fn f(x: dyn): bool {{ return x is {base}; }}\n");
        assert!(codes(&src).is_empty(), "`is {base}` should not warn");
    }
}

#[test]
fn container_of_erased_width_does_not_warn() {
    // A container target reifies its element width (packed storage, sibling slice), so
    // `x is List<i32>` is legitimate — only a *bare scalar* width target is unanswerable.
    assert!(codes("fn f(x: dyn): bool { return x is List<i32>; }\n").is_empty());
    assert!(codes("fn f(x: dyn): bool { return x is List<f64>; }\n").is_empty());
}

/// Every answer the checker folded for `text`, in span order — the `folded_type_tests` channel.
fn folded_answers(text: &str) -> Vec<bool> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    let mut folded: Vec<(noeta_span::Span, bool)> = super::check_all(&parsed.program)
        .sites
        .folded_type_tests
        .into_iter()
        .collect();
    folded.sort_by_key(|(span, _)| span.start);
    folded.into_iter().map(|(_, answer)| answer).collect()
}

#[test]
fn a_width_test_the_checker_can_answer_is_folded_not_warned() {
    // `a`'s declared type IS `i32`, so the test is decided — `true`, and silently. Answering
    // `false` (what the runtime does, having no width to look at) would be simply wrong, and
    // warning about it was noise on a program that had nothing wrong with it.
    let src = "fn f(): bool { a: i32 = 5; return a is i32; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
    assert_eq!(folded_answers(src), [true]);
}

#[test]
fn a_width_test_decided_false_is_folded_silently_too() {
    // A *different* width, or a different signedness, is equally decided — `i32` is not `i64` and
    // not `u32`, since widths have identity-only subtyping. This is a decided answer, not an
    // erasure problem, so it gets no E0063. (Whether an always-false test deserves a complaint of
    // its own is a separate question from erasure, and no code makes it today: `s is int` on a
    // `string` is just as impossible and just as silent.)
    for target in ["i64", "u32", "i8"] {
        let src = format!("fn f(): bool {{ a: i32 = 5; return a is {target}; }}\n");
        assert!(codes(&src).is_empty(), "`is {target}` should be silent");
        assert_eq!(folded_answers(&src), [false], "`is {target}` should fold false");
    }
}

#[test]
fn float_and_f64_are_distinct_static_types_so_both_directions_fold() {
    // `f64` is bit-identical to `float` at runtime, which is exactly why the runtime cannot tell
    // them apart. Statically they are distinct types that do not widen into each other, so both
    // tests are decided.
    assert_eq!(
        folded_answers("fn f(): bool { x: f64 = 1.5; return x is f64; }\n"),
        [true]
    );
    assert_eq!(
        folded_answers("fn f(): bool { x: float = 1.5; return x is f64; }\n"),
        [false]
    );
}

#[test]
fn a_scrutinee_that_does_not_fix_the_width_still_warns_and_folds_nothing() {
    // The genuinely unanswerable scrutinees: the `dyn` top, a union (`number` is one), and an
    // erased type parameter. In each the checker knows a *set* — or nothing — and the value cannot
    // be asked, so no answer exists to fold.
    for scrut in ["dyn", "number", "int | string"] {
        let src = format!("fn f(x: {scrut}): bool {{ return x is i32; }}\n");
        assert_eq!(codes(&src), ["E0063"], "`{scrut}` should warn");
        assert!(folded_answers(&src).is_empty(), "`{scrut}` must not fold");
    }
    let param = "fn f<T>(x: T): bool { return x is i32; }\n";
    assert_eq!(codes(param), ["E0063"]);
    assert!(folded_answers(param).is_empty());
}

#[test]
fn f32_needs_no_special_case_and_gets_none() {
    // `f32` is reified — it has a real runtime tag — so it is not an erased width, nothing folds,
    // and the runtime answers it. That holds whether or not the checker also knows the answer.
    assert!(folded_answers("fn f(): bool { x: f32 = 1.5f32; return x is f32; }\n").is_empty());
    assert!(codes("fn f(): bool { x: f32 = 1.5f32; return x is f32; }\n").is_empty());
}

#[test]
fn a_folded_true_test_still_narrows_its_branch() {
    // Folding must not cost the narrowing the test performs. The branch below binds a `dyn`-typed
    // field off `a` only because `a` is seen as an `i32` there; the E0007 that would follow a lost
    // narrowing is what this asserts the absence of.
    let src = "fn f(): int { a: i32 = 5; if a is i32 { b: i32 = a; return b.to_int(); } return 0; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

// ----- associated types on traits (ExtBundle→ExtTrait convergence, slice 1a) -----

#[test]
fn assoc_type_projects_per_impl() {
    // `Self::Item` in a trait method signature resolves, on a concrete receiver, to the impl's
    // bound associated type — independently per implementor (int for Boxx, string for Tagg).
    let src = "\
trait Container {
  type Item
  fn get(): Self::Item
}
class Boxx {
  v: int
  impl Container {
    type Item = int
    fn get(): Self::Item { return self.v; }
  }
}
class Tagg {
  s: string
  impl Container {
    type Item = string
    fn get(): Self::Item { return self.s; }
  }
}
fn f(b: Boxx): int { return b.get(); }
fn g(t: Tagg): string { return t.get(); }
";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn assoc_type_projection_is_concrete_not_a_hole() {
    // Proves the projection resolves to the impl's bound type (int) rather than degrading to a
    // gradual hole: using `Boxx::get()` where a string is expected is a real mismatch (E0007).
    let src = "\
trait Container {
  type Item
  fn get(): Self::Item
}
class Boxx {
  v: int
  impl Container {
    type Item = int
    fn get(): Self::Item { return self.v; }
  }
}
fn bad(b: Boxx): string { return b.get(); }
";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn assoc_type_under_dyn_degrades_to_hole() {
    // Under `dyn Container` the implementor is unknown, so `Self::Item` cannot resolve statically;
    // it degrades to a gradual hole (`Unknown`) — no error at either use, whatever the expected type.
    let as_int = "\
trait Container {
  type Item
  fn get(): Self::Item
}
fn h(c: dyn Container): int { return c.get(); }
";
    let as_string = "\
trait Container {
  type Item
  fn get(): Self::Item
}
fn h(c: dyn Container): string { return c.get(); }
";
    assert!(codes(as_int).is_empty(), "{:?}", codes(as_int));
    assert!(codes(as_string).is_empty(), "{:?}", codes(as_string));
}

#[test]
fn assoc_type_unbound_non_default_is_coherence_error() {
    // An impl that omits an associated type with NO default fails coherence (E0015).
    let src = "\
trait Container {
  type Item
  fn get(): Self::Item
}
class Boxx {
  v: int
  impl Container {
    fn get(): int { return self.v; }
  }
}
";
    assert_eq!(codes(src), ["E0015"]);
}

#[test]
fn assoc_type_defaulted_binding_is_omittable() {
    // A defaulted associated type may be left unbound — no coherence error, and the type still checks.
    let src = "\
trait Container {
  type Item = int
  fn get(): Self::Item
}
class Boxx {
  v: int
  impl Container {
    fn get(): int { return self.v; }
  }
}
fn f(b: Boxx): int { return b.get(); }
";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

// ----- E0065 / E0007: reified containers are not their payloads (dev-story sweep) -----
//
// `Option` and `Result` carry their own runtime head constructor (`some`/`none`, `Ok`/`Err`), never
// the payload's — so `x is P` on an `Option<P>` is statically always false. It used to type-check
// *and flow-narrow*, so the dead branch read the payload's fields and only the runtime disagreed
// (E0005, "no field `x` on enum"). Now the test warns (E0065, advisory like E0063) and, crucially,
// narrows nothing — which is what turns a misuse of the branch into a real error.

#[test]
fn is_payload_on_an_option_warns_impossible() {
    let src = "\
struct P { x: int }
fn f(p: ?P): bool { return p is P; }
";
    assert_eq!(warn_codes(src), ["E0065"]);
}

#[test]
fn is_payload_on_a_result_warns_impossible() {
    let src = "fn f(r: Result<int, string>): bool { return r is int; }\n";
    assert_eq!(warn_codes(src), ["E0065"]);
}

#[test]
fn the_impossible_test_diagnostic_is_a_warning_not_an_error() {
    // Advisory, exactly like E0063: the program is well-formed and still compiles.
    let src = "fn f(r: Result<int, string>): bool { return r is int; }\n";
    assert_eq!(codes(src), ["E0065"]);
}

#[test]
fn is_the_container_itself_does_not_warn() {
    // The true test — the value really is an `Option`/`Result`.
    assert!(codes("fn f(p: ?int): bool { return p is Option<int>; }\n").is_empty());
    assert!(
        codes("fn f(r: Result<int, string>): bool { return r is Result<int, string>; }\n")
            .is_empty()
    );
}

#[test]
fn is_a_kind_type_does_not_warn_because_containers_are_enums() {
    // `Option`/`Result` ARE enums at runtime, so `x is Enum` is genuinely `true`; flagging it
    // would be wrong, not merely noisy.
    assert!(codes("fn f(p: ?int): bool { return p is Enum; }\n").is_empty());
    assert!(codes("fn f(r: Result<int, string>): bool { return r is Enum; }\n").is_empty());
}

#[test]
fn is_an_open_target_does_not_warn() {
    // `dyn` and a `dyn Trait` membership test are the runtime's call, not a provable constant.
    assert!(codes("fn f(p: ?int): bool { return p is dyn; }\n").is_empty());
    let src = "\
trait Speaks { fn speak(): string }
fn f(p: ?int): bool { return p is dyn Speaks; }
";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn is_a_bare_type_parameter_does_not_warn() {
    // `T` is erased and may instantiate to the container itself.
    let src = "fn f<T>(p: ?T): bool { return p is T; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn an_impossible_test_narrows_nothing_in_an_if() {
    // The whole point: the dead branch must stop type-checking as the payload. Reading `p.x`
    // inside it is now the E0007 the member path reports, not silence.
    let src = "\
struct P { x: int }
fn f(p: ?P): int {
  if p is P { return p.x; }
  return 0;
}
";
    assert_eq!(codes(src), ["E0065", "E0007"]);
}

#[test]
fn an_impossible_test_narrows_nothing_in_a_match_arm() {
    let src = "\
struct P { x: int }
fn f(p: ?P): int {
  return match p {
    is P => p.x,
    _ => 0,
  };
}
";
    assert_eq!(codes(src), ["E0065", "E0007"]);
}

#[test]
fn a_real_is_narrowing_still_narrows() {
    // The guard must not disturb the ordinary `dyn`/union narrowing it sits beside.
    let src = "\
struct P { x: int }
fn f(d: dyn): int {
  if d is P { return d.x; }
  return 0;
}
";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn a_member_on_a_closed_builtin_is_an_error_in_value_position() {
    // The call path already caught `s.nope()`; the *member* path silently answered `Unknown`, so
    // `s.nope`, `[1].nope` and `p.x`-through-an-optional passed `check` and failed at run time.
    assert_eq!(
        codes("fn f(s: string): dyn { return s.nope; }\n"),
        ["E0007"]
    );
    assert_eq!(
        codes("fn f(xs: List<int>): dyn { return xs.nope; }\n"),
        ["E0007"]
    );
    let src = "\
struct P { x: int }
fn f(p: ?P): dyn { return p.x; }
";
    assert_eq!(codes(src), ["E0007"]);
}

#[test]
fn a_member_on_an_open_receiver_stays_lenient() {
    // `dyn` and a user `Named` type keep deferring — a trait impl or runtime dispatch this pass
    // cannot see may still supply the member.
    assert!(codes("fn f(d: dyn): dyn { return d.whatever; }\n").is_empty());
}

#[test]
fn a_real_member_on_a_closed_builtin_still_resolves() {
    // The guard fires only when nothing resolved; a genuine built-in method handle is untouched.
    assert!(codes("fn f(s: string): dyn { return s.upper; }\n").is_empty());
    assert!(codes("fn f(xs: List<int>): dyn { return xs.len; }\n").is_empty());
}

#[test]
fn an_optional_constructor_named_as_a_type_gets_the_idiom() {
    // `x is none` / `x is Ok` name a *value* constructor. It stays E0013 (there is no such type),
    // but the help points at the spelling that works instead of at the type catalog — and the
    // E0065 warning is suppressed, since a second diagnostic about a type that does not exist is
    // noise.
    assert_eq!(
        codes("fn f(p: ?int): bool { return p is none; }\n"),
        ["E0013"]
    );
    assert_eq!(
        codes("fn f(p: ?int): bool { return p is some; }\n"),
        ["E0013"]
    );
    assert_eq!(
        codes("fn f(r: Result<int, string>): bool { return r is Ok; }\n"),
        ["E0013"]
    );
}

// ----- E0065: comparing a reified container with its payload (dev-story sweep) -----
//
// The trap this closes is reached invisibly: `??=` deliberately *unwraps*, retyping its binding
// from `Option<int>` to `int` (the documented one-place exception to reassignment stability). A
// `mut column: ?int` that has been `??=`-assigned once therefore stops comparing equal to the
// `?int` it is tested against — silently, because `==` is universal and imposes no bound.

#[test]
fn comparing_an_option_to_its_payload_warns() {
    let src = "fn f(p: ?int, n: int): bool { return p == n; }\n";
    assert_eq!(warn_codes(src), ["E0065"]);
    let src = "fn f(p: ?int, n: int): bool { return n != p; }\n";
    assert_eq!(warn_codes(src), ["E0065"]);
}

#[test]
fn comparing_a_result_to_its_payload_warns() {
    let src = "fn f(r: Result<int, string>, n: int): bool { return r == n; }\n";
    assert_eq!(warn_codes(src), ["E0065"]);
}

#[test]
fn the_container_compare_diagnostic_is_a_warning_not_an_error() {
    assert_eq!(
        codes("fn f(p: ?int, n: int): bool { return p == n; }\n"),
        ["E0065"]
    );
}

#[test]
fn comparing_two_options_does_not_warn() {
    // Like with like — including `x == none`, which is *the* presence test.
    assert!(codes("fn f(a: ?int, b: ?int): bool { return a == b; }\n").is_empty());
    assert!(codes("fn f(p: ?int): bool { return p == none; }\n").is_empty());
    assert!(codes("fn f(p: ?int): bool { return p != none; }\n").is_empty());
    assert!(codes("fn f(p: ?int, n: int): bool { return p == some(n); }\n").is_empty());
}

#[test]
fn comparing_a_container_to_an_open_type_does_not_warn() {
    // A `dyn` operand really could hold the container at runtime.
    assert!(codes("fn f(p: ?int, d: dyn): bool { return p == d; }\n").is_empty());
}

#[test]
fn comparing_a_container_to_a_bare_type_parameter_does_not_warn() {
    let src = "fn f<T>(p: ?T, t: T): bool { return p == t; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn ordinary_cross_type_equality_is_still_unflagged() {
    // Deliberately narrow: this rule is about the container/payload confusion, not a general
    // "these types can never be equal" analysis, which `==`'s universality does not support.
    assert!(codes("fn f(n: int, s: string): bool { return n == s; }\n").is_empty());
}

// ----- Expectation propagation into `match` arms / `if…then…else` branches -----
//
// The bidirectional expectation a `match` expression is checked against now reaches its ARMS. It
// used to stop at the `match`, so every arm synthesized blind and a form that can only be typed
// against an expectation — a heterogeneous `Map<string, dyn>`, an empty `{}`/`[]`, a `.{ … }`, a
// bare numeric literal narrowing to a fixed width — worked after a bare `return` but not inside an
// arm. `if c then a else b` desugars to a `match`, so its branches ride the same path.

/// Parse `text` and return `(code, span)` for every checker diagnostic, in order.
fn coded_spans(text: &str) -> Vec<(String, noeta_span::Span)> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "test program must parse cleanly: {:?}",
        parsed.diagnostics
    );
    check(&parsed.program)
        .iter()
        .map(|d| (d.code.to_string(), d.span))
        .collect()
}

#[test]
fn return_position_mixed_map_literal_is_clean() {
    // The baseline the arms below have to match: a `return` already supplies the expectation.
    let src = "fn f(): Map<string, dyn> { return {\"type\": \"array\", \"n\": 1}; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn match_arm_absorbs_the_expected_map_value_type() {
    let src = "fn f(x: int): Map<string, dyn> {\n\
               \x20   return match x { 1 => {\"type\": \"array\", \"n\": 1}, _ => {\"t\": \"x\"} };\n\
               }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn if_then_else_branches_absorb_the_expected_map_value_type() {
    let src = "fn f(x: int): Map<string, dyn> {\n\
               \x20   return if x == 1 then {\"type\": \"array\", \"n\": 1} else {\"t\": \"x\"};\n\
               }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn an_empty_map_arm_absorbs_the_expected_map_type() {
    // The originating repro: the mixed arm needs the expectation to be a `Map<string, dyn>` and the
    // empty arm needs it to be a map at all.
    let src = "fn f(x: int): Map<string, dyn> {\n\
               \x20   return match x { 1 => {\"type\": \"array\", \"n\": 1}, _ => {} };\n\
               }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn an_empty_map_branch_of_an_if_then_else_absorbs_the_expected_map_type() {
    let src = "fn f(x: int): Map<string, dyn> {\n\
               \x20   return if x == 1 then {\"type\": \"array\", \"n\": 1} else {};\n\
               }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn match_arm_absorbs_the_expected_list_element_type() {
    let src = "fn f(x: int): List<dyn> { return match x { 1 => [1, \"two\", true], _ => [] }; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn match_arm_absorbs_a_fixed_width_numeric_literal() {
    // `try_adapt_literal` is reached through `check`, so it only fires once the arm has an
    // expectation — before, `200` synthesized as `int` and failed to subsume into `u8`.
    let src = "fn f(x: int): u8 { return match x { 1 => 200, _ => 0 }; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn match_arm_absorbs_a_target_typed_struct_literal() {
    let src = "struct Point { x: int; y: int }\n\
               fn f(k: int): Point { return match k { 1 => .{ x: 1, y: 2 }, _ => .{ x: 0, y: 0 } }; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn if_then_else_branches_absorb_a_target_typed_struct_literal() {
    let src = "struct Point { x: int; y: int }\n\
               fn f(b: bool): Point { return if b then .{ x: 1, y: 2 } else .{ x: 0, y: 0 }; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn match_arm_absorbs_an_expected_option() {
    // `none` and `some(…)` absorb their expected `Option<T>` in an arm exactly as at a `return`.
    let src = "fn f(x: int): ?int { return match x { 1 => some(1), _ => none }; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn an_ill_typed_match_arm_still_reports_on_the_arm() {
    // The expectation makes the arm the *reporting* site: E0007 lands on the offending arm body,
    // not on the whole `match`.
    let src = "fn f(x: int): Map<string, int> { return match x { 1 => {\"a\": 1}, _ => 5 }; }\n";
    let diags = coded_spans(src);
    assert_eq!(
        diags.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(),
        ["E0007"],
        "{diags:?}"
    );
    let (_, span) = &diags[0];
    let text = &src[span.start as usize..span.end as usize];
    assert_eq!(
        text, "5",
        "the diagnostic must point at the arm body, got {text:?}"
    );
}

#[test]
fn a_statement_position_match_still_synthesizes_its_arms() {
    // No expectation exists there, so nothing changes: a heterogeneous map literal in a
    // statement-position arm is the same E0007 it always was, reported by map synthesis.
    let src = "fn f(x: int): void {\n\
               \x20   match x { 1 => {\"a\": 1, \"b\": \"two\"}, _ => {\"a\": 2} };\n\
               }\n";
    assert_eq!(codes(src), ["E0007"], "{:?}", codes(src));
}

#[test]
fn a_block_bodied_arm_in_a_checked_position_is_still_not_a_value() {
    // Value position is unchanged by the expectation: a block arm produces no value (E0055), and it
    // is the ONLY diagnostic — the expectation is not additionally re-tested against the arm's
    // `unit`, which would bury the real message under a spurious E0007.
    let src = "fn f(x: int): int { return match x { 1 => { echo 1; }, _ => 0 }; }\n";
    assert_eq!(codes(src), ["E0055"], "{:?}", codes(src));
}

#[test]
fn an_open_expectation_leaves_match_arms_synthesizing() {
    // `dyn` is an open position with nothing to push down, so a mixed map in an arm is still the
    // synthesis-position error — the guard on the new arm keeps `Unknown`/`dyn` behavior identical.
    let src = "fn f(x: int): dyn { return match x { 1 => {\"a\": 1, \"b\": \"two\"}, _ => 0 }; }\n";
    assert_eq!(codes(src), ["E0007"], "{:?}", codes(src));
}

// ----- Bare variant patterns + E0066 arm reachability -----
//
// A bare identifier pattern resolves to a **payload-free variant of the scrutinee's own enum** when
// one of that name exists, and is a binding otherwise. So `String => …` on a `Type` scrutinee is the
// `Type.String` case (refutable, binding nothing, counting toward exhaustiveness), while `rest => …`
// on the same scrutinee — or `String` on an `int` — is the ordinary catch-all binding. E0066 still
// reports the arms a genuine catch-all swallows.

/// The rendered help lines, so a test can assert the suggestion actually names the fix.
fn helps(text: &str) -> Vec<String> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    check(&parsed.program)
        .iter()
        .filter_map(|d| d.help.clone())
        .collect()
}

const TYPE_ENUM: &str = "enum Type { String; Int; Bool; List(inner: string) }\n";

#[test]
fn a_bare_payload_free_variant_pattern_resolves_to_the_variant() {
    // The flip: the unambiguous-once-typed spelling is the short one. A lone `String` arm is now a
    // case test, so the `match` is NOT exhaustive — E0011 names exactly the cases still missing,
    // which is proof the arm covered `String` and nothing else.
    let src =
        format!("{TYPE_ENUM}fn f(t: Type): string {{ return match t {{ String => \"s\" }}; }}\n");
    assert_eq!(codes(&src), ["E0011"], "{:?}", codes(&src));
    assert!(
        !helps(&src).iter().any(|h| h.contains("Type.String")),
        "nothing is left to qualify: {:?}",
        helps(&src)
    );
}

#[test]
fn every_payload_free_variant_spelled_bare_is_exhaustive() {
    // No `_` needed: each bare name covers its own case, so naming them all closes the match.
    let src = format!(
        "{TYPE_ENUM}fn f(t: Type): string {{\n  return match t {{ String => \"s\", Int => \"i\", Bool => \"b\", List(i) => i }};\n}}\n"
    );
    assert!(codes(&src).is_empty(), "{:?}", codes(&src));
}

#[test]
fn a_resolved_variant_pattern_binds_nothing() {
    // It is the variant, not a name for the value — so the arm body cannot refer to it. (E0005 is
    // the unknown-name code; the point is that *some* error fires rather than the whole value
    // silently flowing through under the variant's name.)
    let src = format!(
        "{TYPE_ENUM}fn f(t: Type): string {{\n  return match t {{ String => \"${{String}}\", _ => \"o\" }};\n}}\n"
    );
    assert_eq!(codes(&src), ["E0005"], "{:?}", codes(&src));
}

#[test]
fn arms_below_a_resolved_variant_stay_reachable() {
    // What used to be E0067 + two E0066s is now simply a correct `match`.
    let src = format!(
        "{TYPE_ENUM}fn f(t: Type): string {{\n  return match t {{ String => \"s\", Int => \"i\", _ => \"o\" }};\n}}\n"
    );
    assert!(codes(&src).is_empty(), "{:?}", codes(&src));
}

#[test]
fn resolution_is_scrutinee_directed() {
    // A payload-free variant of a DIFFERENT enum is not this scrutinee's case, so it stays a
    // binding — and being irrefutable, it kills the arm after it (E0066).
    let other = "enum Other { String; Int }\n";
    let src = format!(
        "{TYPE_ENUM}{other}fn f(o: Other): string {{\n  return match o {{ Bool => \"b\", _ => \"o\" }};\n}}\n"
    );
    assert_eq!(codes(&src), ["E0066"], "{:?}", codes(&src));
    // …and a gradual scrutinee resolves nothing at all: `String` binds the whole `dyn` value.
    let dynamic = format!(
        "{TYPE_ENUM}fn f(d: dyn): string {{ return match d {{ String => \"s\", _ => \"o\" }}; }}\n"
    );
    assert_eq!(codes(&dynamic), ["E0066"], "{:?}", codes(&dynamic));
}

#[test]
fn a_nested_bare_variant_resolves_against_the_field_type() {
    // The inner pattern's scrutinee is the payload's type, not the outer one.
    let src = "fn f(r: Result<?int, string>): string {\n  return match r {\n    Ok(none) => \"empty\",\n    Ok(some(v)) => \"${v}\",\n    Err(e) => e,\n  };\n}\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn a_qualified_payload_free_variant_pattern_is_clean() {
    let src = format!(
        "{TYPE_ENUM}fn f(t: Type): string {{\n  return match t {{ Type.String => \"s\", _ => \"o\" }};\n}}\n"
    );
    assert!(codes(&src).is_empty(), "{:?}", codes(&src));
}

#[test]
fn a_payload_carrying_variant_pattern_needs_no_qualification() {
    // Call-shaped, so it can never be read as a binding — and it emits a real test, so the arms
    // below it stay reachable.
    let src = format!(
        "{TYPE_ENUM}fn f(t: Type): string {{\n  return match t {{ List(i) => i, _ => \"o\" }};\n}}\n"
    );
    assert!(codes(&src).is_empty(), "{:?}", codes(&src));
}

#[test]
fn a_binding_that_is_not_a_variant_of_the_scrutinee_is_clean() {
    // Deliberately narrow: the rule fires only on a payload-free variant of the SCRUTINEE's own
    // enum, never on an ordinary catch-all binding.
    let src = format!(
        "{TYPE_ENUM}fn f(t: Type): string {{\n  return match t {{ Type.Int => \"i\", rest => \"other\" }};\n}}\n"
    );
    assert!(codes(&src).is_empty(), "{:?}", codes(&src));
    assert!(
        codes("fn f(n: int): string { return match n { 0 => \"z\", rest => \"${rest}\" }; }\n")
            .is_empty()
    );
}

#[test]
fn an_arm_after_a_wildcard_is_unreachable() {
    let src = "fn f(n: int): string { return match n { _ => \"m\", 1 => \"o\" }; }\n";
    assert_eq!(codes(src), ["E0066"]);
    assert!(
        helps(src)[0].contains("last position"),
        "the help must say where the catch-all belongs: {:?}",
        helps(src)
    );
}

#[test]
fn an_arm_after_a_bare_binding_is_unreachable() {
    let src = "fn f(n: int): string { return match n { rest => \"${rest}\", 1 => \"o\" }; }\n";
    assert_eq!(codes(src), ["E0066"]);
}

#[test]
fn a_catch_all_in_last_position_is_clean() {
    assert!(
        codes("fn f(n: int): string { return match n { 0 => \"z\", _ => \"m\" }; }\n").is_empty()
    );
    assert!(
        codes("fn f(n: int): string { return match n { 0 => \"z\", rest => \"${rest}\" }; }\n")
            .is_empty()
    );
}

#[test]
fn a_guarded_catch_all_leaves_the_arms_below_it_reachable() {
    // The checker cannot prove a guard ever true, so a guarded arm closes nothing.
    let src = "fn f(n: int): string { return match n { big if big > 9 => \"b\", _ => \"m\" }; }\n";
    assert!(codes(src).is_empty(), "{:?}", codes(src));
}

#[test]
fn a_bare_none_arm_resolves_on_an_option_scrutinee() {
    // `none` is the correct bare spelling of the Option case, so it is a case test in EITHER order
    // — the ordering bug the old always-binds rule forced on every author.
    let none_first =
        "fn f(o: ?int): string { return match o { none => \"n\", some(v) => \"${v}\" }; }\n";
    assert!(codes(none_first).is_empty(), "{:?}", codes(none_first));
    let some_first =
        "fn f(o: ?int): string { return match o { some(v) => \"${v}\", none => \"n\" }; }\n";
    assert!(codes(some_first).is_empty(), "{:?}", codes(some_first));
    // A `none` arm alone covers only the none case — the some case is still missing.
    let lone = "fn f(o: ?int): string { return match o { none => \"n\" }; }\n";
    assert_eq!(codes(lone), ["E0011"], "{:?}", codes(lone));
}

#[test]
fn a_bare_none_on_a_non_option_scrutinee_is_still_a_binding() {
    // Resolution is scrutinee-directed, so on a `dyn` there is no Option case to resolve to and
    // `none` is an ordinary irrefutable binding — it swallows the arm below it.
    let src = "fn f(d: dyn): string { return match d { none => \"n\", some(v) => \"${v}\" }; }\n";
    assert_eq!(codes(src), ["E0066"], "{:?}", codes(src));
    assert!(
        helps(src)[0].contains("plain binding"),
        "the help must say why it did not resolve: {:?}",
        helps(src)
    );
}

/// The rendered diagnostic MESSAGES, for a rule whose identity lives in its wording rather than
/// only in its code — two diagnostics may share a code and mean different things.
fn messages(text: &str) -> Vec<String> {
    seed_std();
    let source = Source::new(SourceId::FIRST, "test.noe", text);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(parsed.diagnostics.is_empty(), "must parse cleanly");
    check(&parsed.program)
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

/// **Which** E0058 a structurally-unrecordable construction gets.
///
/// Two diagnostics share the code and say opposite things about the cause: "records no type
/// argument … nothing supplies an instantiation" (the type was *erased*, no instantiation reached
/// the call at all) and "`T` is a type parameter of the enclosing declaration" (an instantiation
/// exists, it just cannot travel to this position). Which one fires turns on
/// `open_only_by_erasure`, whose parameter case was spelled as "a `Named` head in the in-scope name
/// set" — a trigger that stopped firing the moment a parameter became its own lattice variant, and
/// silently downgraded every such site to the wrong explanation.
///
/// `generics/generic_in_generic_unrecordable.noe` covers this case in the corpus but pins only the
/// CODE and the span, so it stayed green through the regression. This is the test that would not
/// have.
#[test]
fn an_unrecordable_construction_names_the_type_parameter_not_erasure() {
    let src = "\
struct Todo { id: int }
class Repository<T> {
  pub tbl: string
  fn new(tbl: string): Repository<T> { return Repository { tbl: tbl }; }
  fn label(): string { return \"${self.tbl}\" ~ type_name::<T>(); }
  fn rebuild(): string {
    r: Repository<T> = Repository.new(self.tbl ~ \"2\");
    return r.label();
  }
}
r: Repository<Todo> = Repository::<Todo>.new(\"todos\");
echo r.rebuild();
";
    assert_eq!(codes(src), vec!["E0058"], "{:?}", codes(src));
    let m = &messages(src)[0];
    assert!(
        m.contains("is a type parameter of the enclosing declaration"),
        "an in-scope parameter is open BY THE PARAMETER, not by erasure — got: {m}"
    );
}

/// **The type walkers must be total over the lattice.** `erase_type_params`, `apply_subst` and
/// `bind_type_params` descended into every container the language has EXCEPT `Tuple` and `Union`,
/// so a parameter written inside one was neither erased nor instantiated — it survived as a
/// `Type::Param`, and a parameter is a subtype of nothing, so the argument check rejected a
/// perfectly ordinary call: `f((1, 2))` against `fn f<T>(p: (T, int))` reported *"argument of type
/// `(int, int)` is not assignable to `(T, int)`"*, naming a parameter the caller cannot even spell.
#[test]
fn a_type_parameter_inside_a_tuple_erases_like_one_inside_a_list() {
    let src = "fn f<T>(p: (T, int)): int { return 1; }\necho \"${f((1, 2))}\";\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}

/// The other half: a tuple element **instantiates** from the argument, so the call's result is the
/// caller's type and not a leaked parameter. The un-erased form escaped all the way into a
/// caller-visible type — `p = mk(3)` was `(T, int)` in a scope where `T` means nothing.
#[test]
fn a_type_parameter_inside_a_tuple_substitutes_from_the_argument() {
    let src = "fn mk<T>(v: T): (T, int) { return (v, 1); }\np = mk(3);\ns: string = p;\necho s;\n";
    assert_eq!(codes(src), ["E0007"], "{:?}", messages(src));
    assert!(
        messages(src)[0].contains("found `(int, int)`"),
        "the tuple element must be the instantiated `int`, not a leaked `T`: {:?}",
        messages(src)
    );
}

/// A union member is the same gap. `T | string` erases to `dyn | string` — which **is** `dyn`, so
/// the parameter accepts any argument, the honest answer when nothing determines `T`. It used to
/// stay `T | string` and reject every argument that was not already a `string`.
#[test]
fn a_type_parameter_inside_a_union_erases_to_dyn() {
    let src = "fn h<T>(x: T | string): int { return 1; }\necho \"${h(1)}\";\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}

/// And a union member instantiates like any other: a bound `T` is substituted inside the union, so
/// the returned type names the caller's `int` rather than the callee's parameter.
#[test]
fn a_type_parameter_inside_a_union_substitutes() {
    let src = "fn k<T>(v: T): T | string { return \"s\"; }\ns: string = k(1);\necho s;\n";
    assert_eq!(codes(src), ["E0007"], "{:?}", messages(src));
    assert!(
        messages(src)[0].contains("found `int | string`"),
        "the union member must be the instantiated `int`, not a leaked `T`: {:?}",
        messages(src)
    );
}

/// A **declaration-position** consequence of the same gap: a default value is checked against its
/// declared type with the enclosing parameters erased, so `(T, int) = (0, 0)` was rejected at the
/// declaration itself — no call site involved.
#[test]
fn a_tuple_typed_default_is_checked_against_the_erased_type() {
    let src =
        "struct Holder<T> {\n  pair: (T, int) = (0, 0)\n}\nh = Holder { };\necho \"${h.pair}\";\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}

/// **A method's `<T>` inside a `class Repo<T>` warns (E0075).** The shadowing is sound — the two
/// are different parameters and the inner one wins — but a reader of `Repo::<Todo>.label::<User>()`
/// cannot tell which `T` the body means, so the compiler says so and keeps going.
#[test]
fn a_method_type_parameter_that_shadows_its_class_warns() {
    let src = "class Repo<T> {\n  fn label<T>(): string { return \"x\"; }\n}\n";
    assert_eq!(codes(src), ["E0075"], "{:?}", messages(src));
    let d = &diagnostics(src)[0];
    assert_eq!(d.severity, noeta_diagnostics::Severity::Warning);
    assert!(
        d.message
            .contains("`T` shadows the enclosing `T` of `Repo`"),
        "the message must name both declarations: {}",
        d.message
    );
    // The second label points at the CLASS's `<T>`, so "where the outer one is declared" is in the
    // rendered report rather than left to the reader.
    assert_eq!(d.labels.len(), 2, "primary + the outer declaration: {d:?}");
    assert!(
        d.labels[1].message.contains("declared here"),
        "{:?}",
        d.labels
    );
    let outer = src.find("<T>").expect("the class's own `<T>`") as u32 + 1;
    assert_eq!(
        (d.labels[1].span.start, d.labels[1].span.end),
        (outer, outer + 1),
        "the label must span the CLASS's `T`, not the method's"
    );
}

/// It does not fire on a **different name** — the ordinary generic method, which reaches both
/// parameters and is exactly what the warning tells you to write.
#[test]
fn a_method_type_parameter_with_its_own_name_is_silent() {
    let src = "class Repo<T> {\n  fn label<U>(): string { return \"x\"; }\n}\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}

/// It does not fire on a **nominal type** of the same name. `struct T { }` beside a `class Repo<T>`
/// is legitimate — the parameter's author does not control what somebody named a type — and the
/// scope the check consults holds parameters only, so there is nothing there to hide.
#[test]
fn a_type_parameter_that_shares_a_name_with_a_declared_type_is_silent() {
    let src =
        "struct T {\n  id: int\n}\nclass Repo<T> {\n  fn label(): string { return \"x\"; }\n}\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}

/// It does not fire across **sibling scopes**: two methods may each declare `<T>`, because neither
/// is inside the other. The scope is saved and restored per declaration, so the second method's
/// `<T>` is compared against the class's — a non-generic class here — and not the first method's.
#[test]
fn two_sibling_methods_may_each_declare_the_same_parameter_name() {
    let src = "class Repo {\n  fn a<T>(x: T): string { return \"a\"; }\n  fn b<T>(x: T): string { return \"b\"; }\n}\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}

/// And a top-level generic `fn` beside a generic class is not nested in it, so a shared name is
/// nobody's shadow.
#[test]
fn a_top_level_generic_fn_does_not_shadow_a_generic_class() {
    let src = "class Repo<T> {\n  pub v: int\n}\nfn label<T>(x: T): string { return \"x\"; }\necho label(1);\n";
    assert!(codes(src).is_empty(), "{:?}", messages(src));
}
