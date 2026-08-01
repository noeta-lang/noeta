//! **Every position a `@<tier> { … }` block can be written in must hover as that tier.**
//!
//! `DocumentStore::hover_tier` finds the tier name under the cursor by walking the parse
//! (`tier_name_at`). That walk used to end in `_ => None` on both its `Stmt` and its `Expr` match,
//! and the wildcard was swallowing **25 of the 50 `Expr` variants and 6 of the 21 `Stmt` ones** —
//! so a tier block written inside `construct(…)`, `invoke(…)`, `params_of(…)`,
//! `field_specs_of(…)`, a `"${ … }"` hole, a `channel(…)`, a turbofish call, an assignment's
//! right-hand side, or the body of a standalone `impl` hovered as **nothing at all**. Nothing
//! announced the miss: a wildcard cannot tell "deliberately nothing here" from "nobody has
//! written this arm yet", and hover returning `None` is indistinguishable from the cursor simply
//! not being on a tier.
//!
//! Two halves close it, and they close different failure modes:
//!
//! 1. **Structural** — `tier_name_at`'s two matches are now exhaustive with no wildcard, so a
//!    *newly added* `Expr` or `Stmt` variant is a compile error in that function rather than a
//!    quiet hole. That half is proven by deleting an arm and watching the build fail; it needs no
//!    test, and a test could not express it.
//! 2. **Behavioural** — this file. Exhaustiveness proves each variant is *named*; it cannot prove
//!    the arm actually *recurses* (`Expr::Invoke { .. } => None` compiles). Below, every position
//!    is written out and hovered, which is what proves the arms reach.
//!
//! Exactly the pairing `noeta_loader::ast_walk_coverage` describes for the qualifier's walk, for
//! exactly the bug class it was written for — banning the wildcard proves the variant is
//! mentioned, and a probe proves the field is visited.
//!
//! `@json` is std's native expression tier, so the fixtures need no `@tier` declaration. The
//! snippets are written for *placement*, not for type-correctness: `hover_tier` reads the parse,
//! never the check, and a tier block's hover must work while the surrounding expression is still
//! half-typed — that is when an editor is asked.

use noeta_ide::{DocumentStore, Encoding, Position};

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[]));
}

const URI: &str = "file:///tier_reach.noe";
const TIER: &str = "@json { {} }";

/// What hover must answer at a `@json` tier name, whatever expression encloses it.
const DESCRIPTOR: &str = "expression tier `@json` — `json` body, evaluates to `string`";

/// One position per row, named by the AST node whose arm reaches it. A row may place the tier more
/// than once (`invoke`'s three operands); **every** occurrence in the snippet is hovered, so
/// widening a row costs one `@json` rather than one test.
///
/// The controls at the top are positions that already worked before the walk was made exhaustive —
/// they are here so a regression that breaks *everything* reads as itself rather than as this
/// file's own subject.
const POSITIONS: &[(&str, &str)] = &[
    // -- controls: reached before the wildcard was removed ------------------------------------
    ("Stmt::Binding value", "x = @json { {} }\n"),
    ("Stmt::Echo value", "echo @json { {} }\n"),
    (
        "Expr::Call argument",
        "fn f(s: string): int { return 1 }\nx = f(@json { {} })\n",
    ),
    (
        "Expr::Coalesce fallback",
        "y = none\nx = y ?? @json { {} }\n",
    ),
    ("Expr::TypeOf value", "x = type_of(@json { {} })\n"),
    ("Expr::FieldsOf value", "x = fields_of(@json { {} })\n"),
    ("Expr::TraitsOf value", "x = traits_of(@json { {} })\n"),
    // -- the reflection surface: the by-name (runtime string) operands ------------------------
    ("Expr::ParamsOf target", "x = params_of(@json { {} })\n"),
    ("Expr::ReturnsOf target", "x = returns_of(@json { {} })\n"),
    (
        "Expr::Invoke recv/name/args",
        "x = invoke(@json { {} }, @json { {} }, [@json { {} }])\n",
    ),
    (
        "Expr::FieldSpecsOf dynamic name",
        "x = field_specs_of(@json { {} })\n",
    ),
    (
        "Expr::VariantsOf dynamic name",
        "x = variants_of(@json { {} })\n",
    ),
    (
        "Expr::Construct dynamic name and fields",
        "x = construct(@json { {} }, [@json { {} }])\n",
    ),
    (
        "Expr::FromBytes blob",
        "@packed\nstruct V2 { x: f32 y: f32 }\nx = from_bytes::<V2>(@json { {} })\n",
    ),
    // -- the other positions the same wildcard was hiding --------------------------------------
    ("Expr::Interp hole", "x = \"${@json { {} }}\"\n"),
    (
        "Expr::Channel capacity",
        "c = channel::<int>(@json { {} })\n",
    ),
    (
        "Expr::TypedCall argument",
        "fn g<T>(a: T): T { return a }\nx = g::<string>(@json { {} })\n",
    ),
    (
        "Expr::TypedMethodCall receiver argument",
        "class C { fn m<T>(a: T): T { return a } }\nc = C {}\nx = c.m::<string>(@json { {} })\n",
    ),
    (
        "Expr::FieldSet value",
        "class P { mut s: string }\np = P { s: \"a\" }\np.s = @json { {} }\n",
    ),
    (
        "Stmt::Impl method body",
        "struct S {}\ntrait Tr { fn m(): string }\n\
         impl Tr for S { fn m(): string { return @json { {} } } }\n",
    ),
    (
        "Stmt::Trait default method body",
        "trait Tr { fn m(): string { return @json { {} } } }\n",
    ),
];

/// The `Position` of the tier name for the `@json` occurrence starting at byte `at`, pointing one
/// character into the name (an unambiguous interior offset — the span's own edges are shared with
/// the neighbouring tokens).
fn tier_name_position(src: &str, at: usize) -> Position {
    let idx = at + 2;
    let line = src[..idx].matches('\n').count() as u32;
    let col = (idx - src[..idx].rfind('\n').map(|i| i + 1).unwrap_or(0)) as u32;
    Position::new(line, col)
}

/// Every byte offset in `src` at which the fixture wrote the tier.
fn occurrences(src: &str) -> Vec<usize> {
    src.match_indices(TIER).map(|(i, _)| i).collect()
}

#[test]
fn every_expression_position_a_tier_block_can_occupy_hovers_as_that_tier() {
    install();
    let mut missed: Vec<String> = Vec::new();
    for (what, src) in POSITIONS {
        let mut store = DocumentStore::default();
        store.open(URI, (*src).to_string());
        let at = occurrences(src);
        assert!(
            !at.is_empty(),
            "`{what}`'s fixture writes no `{TIER}` — the row asserts nothing",
        );
        for (nth, start) in at.into_iter().enumerate() {
            let position = tier_name_position(src, start);
            match store.hover_tier(URI, position, Encoding::Utf16) {
                Some((descriptor, _)) if descriptor == DESCRIPTOR => {}
                Some((descriptor, _)) => missed.push(format!(
                    "{what} (occurrence {nth}): hovered as {descriptor:?}, expected {DESCRIPTOR:?}"
                )),
                None => missed.push(format!(
                    "{what} (occurrence {nth}): no tier hover — `tier_name_at` does not reach this \
                     position"
                )),
            }
        }
    }
    assert!(
        missed.is_empty(),
        "a tier block hovers as nothing in {} position(s):\n  {}",
        missed.len(),
        missed.join("\n  "),
    );
}

/// The complement, so "reaches everything" cannot be satisfied by answering everywhere: off a tier
/// name — on the block's *body*, and on a file with no tier at all — hover stays silent.
#[test]
fn hover_off_a_tier_name_stays_silent() {
    install();
    let src = "x = field_specs_of(@json { {} })\n";
    let mut store = DocumentStore::default();
    store.open(URI, src.to_string());
    // Inside the body, past the `{`.
    let body = src.find("{ {}").expect("the fixture has a body") + 2;
    let line = 0;
    let col = body as u32;
    assert_eq!(
        store.hover_tier(URI, Position::new(line, col), Encoding::Utf16),
        None,
        "the tier's body is not its name",
    );

    let mut plain = DocumentStore::default();
    plain.open(URI, "x = field_specs_of(\"Cfg\")\n".to_string());
    assert_eq!(
        plain.hover_tier(URI, Position::new(0, 6), Encoding::Utf16),
        None,
        "a reflection call with no tier in it hovers as no tier",
    );
}
