//! An extension directive's **declared argument contract**, as the author sees it fail.
//!
//! `max_args` and `named_keys` are enforced by `arg_faults` (in `noeta_parser::directives`, shared
//! with the loader so an expansion hook never sees a malformed invocation) and worded here in the
//! checker. Nothing tested the wording: no shipped extension declares either field, so the
//! conformance corpus — which is otherwise the authority on diagnostics — cannot reach this path at
//! all. It went unnoticed until the rule moved crates and "the corpus is green" turned out to mean
//! nothing about it.
//!
//! A fixture extension is what makes it reachable, so this file exists to pin the messages. Its own
//! test binary because the extension registry installs once per process.

use noeta_ext_abi::registry::{ExtDirective, ExtModule, Extension, TierSite};

struct Fixture;

impl Extension for Fixture {
    fn name(&self) -> &'static str {
        "argfixture"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn directives(&self) -> &'static [ExtDirective] {
        &[
            ExtDirective {
                name: "fx_one",
                sites: &[TierSite::Type],
                max_args: Some(1),
                named_keys: &["version", "prefix"],
                detail: "one positional argument",
                doc: "test fixture",
                params: &["spec"],
                expand: None,
            },
            // Takes positional arguments only — the branch whose help text differs.
            ExtDirective {
                name: "fx_positional_only",
                sites: &[TierSite::Type],
                max_args: Some(2),
                named_keys: &[],
                detail: "positional only",
                doc: "test fixture",
                params: &["a", "b"],
                expand: None,
            },
        ]
    }
}

static FIXTURE: Fixture = Fixture;

/// Check a program and return its diagnostics as `(code, message, help)` triples.
fn check(source: &str) -> Vec<(String, String, Option<String>)> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FIXTURE]));

    let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "t.noe", source);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    assert!(
        parsed.diagnostics.is_empty(),
        "the fixture program must parse: {:?}",
        parsed.diagnostics
    );
    noeta_check::check(&parsed.program)
        .into_iter()
        .map(|d| (d.code.code().to_string(), d.message, d.help))
        .collect()
}

/// Only the diagnostics this file is about. Selected by the directive they blame rather than by
/// code: the argument contract reports under `E0037` and `E0005`, and `E0005` (unknown name) is
/// common enough that filtering on it would quietly admit unrelated faults and let a broken
/// assertion look like a passing one.
fn argument_faults(source: &str) -> Vec<(String, String, Option<String>)> {
    check(source)
        .into_iter()
        .filter(|(_, message, _)| message.contains("`@fx_"))
        .collect()
}

#[test]
fn too_many_positional_arguments_names_the_limit_and_the_count() {
    let faults = argument_faults(
        r#"
        @fx_one("a", "b", "c")
        struct T { x: int }
        "#,
    );
    assert_eq!(faults.len(), 1, "expected one fault, got {faults:?}");
    assert_eq!(
        faults[0].1,
        "`@fx_one` takes at most 1 argument, but 3 were given"
    );
}

#[test]
fn the_limit_is_singular_or_plural_to_match_it() {
    let faults = argument_faults(
        r#"
        @fx_positional_only("a", "b", "c")
        struct T { x: int }
        "#,
    );
    assert_eq!(faults.len(), 1, "expected one fault, got {faults:?}");
    // "2 arguments", not "2 argument" — the plural follows the maximum, not the count given.
    assert_eq!(
        faults[0].1,
        "`@fx_positional_only` takes at most 2 arguments, but 3 were given"
    );
}

#[test]
fn an_unknown_key_lists_the_keys_that_are_understood() {
    let faults = argument_faults(
        r#"
        @fx_one(nope: "x")
        struct T { x: int }
        "#,
    );
    assert_eq!(faults.len(), 1, "expected one fault, got {faults:?}");
    assert_eq!(faults[0].1, "`@fx_one` has no argument `nope`");
    assert_eq!(
        faults[0].2.as_deref(),
        Some("it understands `version:`, `prefix:`")
    );
}

#[test]
fn a_directive_taking_no_keys_says_so_rather_than_listing_none() {
    let faults = argument_faults(
        r#"
        @fx_positional_only(nope: "x")
        struct T { x: int }
        "#,
    );
    assert_eq!(faults.len(), 1, "expected one fault, got {faults:?}");
    assert_eq!(
        faults[0].2.as_deref(),
        Some("`@fx_positional_only` takes positional arguments only")
    );
}

#[test]
fn every_unknown_key_is_reported_not_just_the_first() {
    // The reason `arg_faults` returns a list: an author who wrote two bad keys should learn about
    // both in one compile rather than one per attempt.
    let faults = argument_faults(
        r#"
        @fx_one(nope: "x", also_nope: "y")
        struct T { x: int }
        "#,
    );
    assert_eq!(faults.len(), 2, "expected two faults, got {faults:?}");
    assert!(faults[0].1.contains("`nope`"));
    assert!(faults[1].1.contains("`also_nope`"));
}

#[test]
fn a_conforming_invocation_draws_no_argument_fault() {
    let faults = argument_faults(
        r#"
        @fx_one("spec.yaml", version: "3", prefix: "v2")
        struct T { x: int }
        "#,
    );
    assert!(faults.is_empty(), "unexpected faults: {faults:?}");
}

#[test]
fn a_named_argument_does_not_count_against_the_positional_maximum() {
    // `max_args` bounds the positional arguments alone. Counting a `version:` as a second
    // positional would reject a call the directive plainly declared support for.
    let faults = argument_faults(
        r#"
        @fx_one("spec.yaml", version: "3")
        struct T { x: int }
        "#,
    );
    assert!(faults.is_empty(), "unexpected faults: {faults:?}");
}
