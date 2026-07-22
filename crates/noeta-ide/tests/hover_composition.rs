//! Pins the **composed hover** — `DocumentStore::hover_markdown`, the single method both the LSP
//! server and the web playground consume so their hovers cannot drift.
//!
//! Before this method existed the precedence and Markdown assembly lived inside the LSP handler
//! and the playground reimplemented only the bare-type slice; there was no test of the composition
//! at all. These assertions are what prove the extraction preserved behavior: the callable-name
//! hover must be the full signature fence (not the return type alone), a type name its declaration,
//! a plain sub-expression the bare type — and where two primitives could fire at one position, the
//! documented order must hold.
//!
//! Its own test binary because the extension registry installs once per process.

use noeta_ide::{DocumentStore, Encoding, Position};

/// The registry installs once per process; funnel every test through this idempotent install.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[]));
}

/// A store with one open document. The directory does not exist on disk, so the buffer is the
/// workspace's only member — the single-file editing case, exactly what the playground runs.
fn store_with(uri: &str, text: &str) -> DocumentStore {
    install();
    let mut store = DocumentStore::default();
    store.open(uri, text.to_string());
    store
}

fn pos(line: u32, character: u32) -> Position {
    Position { line, character }
}

const URI: &str = "hover.noe";

/// `fn add`, hovered three ways: at its declaration name, at a plain sub-expression, at a type.
const PROGRAM: &str = "\
@doc { Adds two integers. }
fn add(a: int, b: int): int {
  return a + b;
}

struct Point { x: int y: int }

echo add(1, 2);
";

/// Hovering a callable's name yields its **full signature** — `fn add(a: int, b: int): int` in a
/// `noeta` fence — not the bare return type a plain type hover would report. This is both the
/// point of the whole change AND proof that `signature` outranks `type` at a position both match.
#[test]
fn a_callable_name_hovers_as_its_signature_over_the_bare_type() {
    let store = store_with(URI, PROGRAM);
    // The call site `add(1, 2)` on the last line — `add` starts at column 5 of `echo add(…)`.
    let (value, range) = store
        .hover_markdown(URI, pos(7, 5), Encoding::Utf16)
        .expect("a callable name hovers");
    assert!(value.contains("```noeta"), "value: {value}");
    assert!(value.contains("fn add("), "value: {value}");
    assert!(value.contains("int"), "value: {value}");
    // Not the degenerate bare-return-type hover (which would be just `int` in a fence).
    assert!(
        value.trim() != "```noeta\nint\n```",
        "signature must beat the bare type, got: {value}"
    );
    // A signature hover carries the name's range.
    assert!(range.is_some(), "signature hover underlines the name");
}

/// The doc prose attached with `@doc { … }` is appended to the signature after a `\n\n---\n\n`
/// rule — the exact composition the LSP handler used to build inline.
#[test]
fn attached_doc_prose_follows_the_signature_after_a_rule() {
    let store = store_with(URI, PROGRAM);
    // The declaration name `add` on line 1 (0-based), column 3 of `fn add(…)`.
    let (value, _) = store
        .hover_markdown(URI, pos(1, 3), Encoding::Utf16)
        .expect("the declaration name hovers");
    assert!(value.contains("fn add("), "value: {value}");
    assert!(value.contains("\n\n---\n\n"), "doc separator: {value}");
    assert!(value.contains("Adds two integers."), "doc prose: {value}");
}

/// A type name hovers as its **declaration** (fields/methods), not the bare nominal a type hover
/// would print — proving `type_definition` outranks `type`.
#[test]
fn a_type_name_hovers_as_its_declaration() {
    let store = store_with(URI, PROGRAM);
    // `Point` in `struct Point { … }` on line 5, column 7.
    let (value, range) = store
        .hover_markdown(URI, pos(5, 7), Encoding::Utf16)
        .expect("a type name hovers");
    assert!(value.contains("```noeta"), "value: {value}");
    assert!(value.contains("Point"), "value: {value}");
    // The declaration shows the fields — a bare nominal hover would not.
    assert!(value.contains("x") && value.contains("y"), "value: {value}");
    assert!(range.is_some(), "type-def hover underlines the name");
}

/// A plain typed sub-expression hovers as the **bare type** — the slice the playground used to
/// show, still reachable at a position no higher-precedence primitive claims.
#[test]
fn a_plain_sub_expression_hovers_as_the_bare_type() {
    let store = store_with(URI, PROGRAM);
    // `a` in `return a + b;` on line 2, column 9.
    let (value, range) = store
        .hover_markdown(URI, pos(2, 9), Encoding::Utf16)
        .expect("a sub-expression hovers");
    assert_eq!(value, "```noeta\nint\n```", "bare type hover: {value}");
    assert!(range.is_some(), "a type hover underlines the expression");
}

/// A built-in decorator directive (`@packed`) hovers in place from the metadata table — the
/// `directive` branch, ahead of everything below it. Reachable without a fixture extension.
#[test]
fn a_builtin_directive_hovers_in_place() {
    let store = store_with(URI, "@packed struct P { x: int, y: int }\necho 1;\n");
    // The directive name on line 0: `@packed` — hover the `p` of `packed` (column 3).
    let (value, range) = store
        .hover_markdown(URI, pos(0, 3), Encoding::Utf16)
        .expect("a built-in directive hovers");
    assert!(!value.is_empty(), "directive descriptor: {value}");
    assert!(range.is_some(), "directive hover underlines the name");
}

/// Nothing under the cursor → no hover.
#[test]
fn empty_space_hovers_nothing() {
    let store = store_with(URI, PROGRAM);
    // The blank line 3 (0-based), column 0.
    assert!(
        store
            .hover_markdown(URI, pos(3, 0), Encoding::Utf16)
            .is_none()
    );
}
