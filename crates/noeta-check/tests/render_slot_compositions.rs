//! **What a render-slot composition costs a program that renders nothing.**
//!
//! A call inside a generic body that instantiates its callee at a type the body BUILT out of its own
//! type parameters — `wrap([v])` inside `fn built<T>(v: T)`, at `List<T>` — is answered by composing
//! the slot the body does hold. The composition's cases are enumerated over the type-argument table
//! while it is being built, and interning one composed entry per combination is the only way this
//! mechanism can make a program pay for itself.
//!
//! So the claim under test is a *size* claim, not a behavioral one (the corpus covers behavior):
//! the table grows by exactly the composed instantiations that carry a hint, and by nothing else. A
//! `List<T>` composition in a program that only ever instantiates it at `int` composes to no hint at
//! every combination, appends no entry, and leaves the table byte-for-byte the size it is without
//! the composition at all.
//!
//! The second claim is the one that keeps a degradation from becoming an abort: a composed entry
//! carries **no decode recipe**. It is reachable only from a render slot, which is interned with no
//! recipe demand and may legitimately name nothing; a recipe on one would let a recipe-consuming
//! door resolve off it and turn a check-time diagnostic into a runtime abort.

use noeta_check::{Sites, check_all};
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

/// The checker's site bundle for `src`, which must check clean.
fn sites_of(src: &str) -> Sites {
    noeta_stdlib::registry::default_seeded();
    let source = Source::new(SourceId(0), "<test>".to_string(), src);
    let lexed = lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "the fixture must parse cleanly: {:?}",
        parsed.diagnostics
    );
    let checked = check_all(&parsed.program);
    assert!(
        checked.diagnostics.iter().all(|d| !d.is_error()),
        "the fixture must check cleanly: {:?}",
        checked.diagnostics
    );
    checked.sites
}

/// A program forwarding `v` to a generic `wrap`, with `built`'s body either BUILDING a `List<T>` or
/// handing the bare parameter on, and instantiated at `ty`/`value`.
fn program(build: bool, ty: &str, value: &str) -> String {
    let forwarded = if build { "wrap([v])" } else { "wrap(v)" };
    format!(
        "use std.json\n\
         fn wrap<T>(v: T): string {{ return json.stringify(v); }}\n\
         fn built<T>(v: T): string {{ return {forwarded}; }}\n\
         x: {ty} = {value};\n\
         echo built(x);\n"
    )
}

#[test]
fn a_composition_that_renders_nothing_adds_no_table_entry() {
    let composed = sites_of(&program(true, "int", "7"));
    let plain = sites_of(&program(false, "int", "7"));

    assert_eq!(
        composed.type_arg_compositions.len(),
        1,
        "the body builds `List<T>`, so it registers exactly one composition"
    );
    assert!(
        composed.type_arg_compositions[0].cases.is_empty(),
        "`List<int>` composes to no hint at every combination, so no case is recorded: {:?}",
        composed.type_arg_compositions[0]
    );
    assert_eq!(
        composed.type_arg_table.len(),
        plain.type_arg_table.len(),
        "the composition interned an entry into a program that renders nothing"
    );
    assert!(
        composed.type_arg_hints.iter().all(|h| h.is_empty()),
        "a program with no `u64` under a generic carries no hints at all"
    );
}

#[test]
fn a_composition_that_renders_interns_exactly_its_own_answers() {
    let composed = sites_of(&program(true, "u64", "18446744073709551615u64"));
    let plain = sites_of(&program(false, "u64", "18446744073709551615u64"));

    let cases = &composed.type_arg_compositions[0].cases;
    assert_eq!(
        cases.len(),
        1,
        "one hint-carrying instantiation reaches the leaf, so one combination composes to \
         something: {cases:?}"
    );
    assert_eq!(
        composed.type_arg_table.len(),
        plain.type_arg_table.len() + 1,
        "exactly the composed `List<u64>` is appended"
    );

    let at = usize::try_from(cases[0].composed).expect("a composed case names a real table entry");
    assert!(
        !composed.type_arg_hints[at].is_empty(),
        "the composed entry is the one that carries `Elements(Unsigned)`"
    );
    assert!(
        composed.type_arg_table[at].recipe.is_none(),
        "a composed entry must carry no decode recipe: it is reachable only from a render slot, \
         which may legitimately name nothing, so a recipe on one would move an unbuildable \
         instantiation from a check-time diagnostic to a runtime abort"
    );
}
