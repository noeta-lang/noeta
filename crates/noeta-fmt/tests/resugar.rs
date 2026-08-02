//! Surface-sugar preservation: the parser desugars several constructs to plain AST with no marker
//! node (`#{…}` set literals, compound assignment `+= -= *= /= %= ~= ??=`, index-assignment
//! `x[k] = v`, and their field twins), so the printer must *resugar* them from span evidence or
//! `noeta fmt` silently rewrites the author's code. The corpus harness cannot catch this class —
//! its safety gate asserts the output re-parses to the same AST, and the desugared spelling
//! re-parses identically by construction — so preservation is pinned here explicitly, alongside
//! the counter-cases proving hand-written desugared spellings are NOT sugared up.

use noeta_fmt::{FmtConfig, format_source};

/// Formatting `src` yields exactly `src` (the input is already canonically formatted).
fn preserved(src: &str) {
    let out = format_source("resugar.noe", src, &FmtConfig::default()).expect("formats");
    assert_eq!(out, src, "fmt must round-trip the surface form");
}

// --- set literals ---

#[test]
fn set_literal_round_trips() {
    preserved("s = #{3, 1, 2}\n");
}

#[test]
fn empty_set_literal_round_trips() {
    preserved("s = #{}\n");
}

#[test]
fn nested_set_literal_round_trips() {
    preserved("echo #{1, 2}.len()\n");
}

#[test]
fn authored_to_set_is_not_sugared() {
    // A hand-written `[..].to_set()` begins at `[`, not `#{` — it must stay as written.
    preserved("s = [3, 1, 2].to_set()\n");
}

// --- compound assignment on a bare name ---

#[test]
fn compound_assign_round_trips() {
    preserved("mut n = 1\nn += 2\nn -= 3\nn *= 4\nn /= 5\nn %= 6\n");
}

#[test]
fn concat_assign_round_trips() {
    preserved("mut s = \"a\"\ns ~= \"b\"\n");
}

#[test]
fn coalesce_assign_round_trips() {
    preserved("mut n = none\nn ??= some(2)\n");
}

#[test]
fn compound_assign_with_binary_rhs_round_trips() {
    // The rhs is itself a binary: `n += a + b` desugars to `n = n + (a + b)`; the resugared
    // form re-parses to the same right-nested tree.
    preserved("mut n = 1\nn += n + 2\n");
}

#[test]
fn authored_self_reference_is_not_sugared() {
    // A hand-written `n = n + 2` re-reads `n` at a *different* offset than the binding target,
    // so span identity keeps it as written.
    preserved("mut n = 1\nn = n + 2\nn = n ?? 3\n");
}

// --- index-assignment ---

#[test]
fn index_assign_round_trips() {
    preserved("mut m = {\"a\": 1}\nm[\"b\"] = 2\n");
}

#[test]
fn authored_set_call_is_not_sugared() {
    preserved("mut m = {\"a\": 1}\nm = m.set(\"b\", 2)\n");
}

// --- field twins ---

#[test]
fn field_compound_assign_round_trips() {
    preserved("struct P {\n    mut x: int\n}\n\nmut p = P { x: 1 }\np.x += 2\np.x ~= 3\n");
}

#[test]
fn field_coalesce_assign_round_trips() {
    preserved("struct P {\n    mut x: ?int\n}\n\nmut p = P { x: none }\np.x ??= some(1)\n");
}

#[test]
fn field_index_assign_round_trips() {
    preserved(
        "struct B {\n    mut cells: List<int>\n}\n\nmut b = B { cells: [0] }\nb.cells[0] = 5\n",
    );
}

#[test]
fn authored_field_self_reference_is_not_sugared() {
    preserved("struct P {\n    mut x: int\n}\n\nmut p = P { x: 1 }\np.x = p.x + 2\n");
}

#[test]
fn authored_field_set_call_is_not_sugared() {
    preserved(
        "struct B {\n    mut cells: List<int>\n}\n\nmut b = B { cells: [0] }\nb.cells = b.cells.set(0, 5)\n",
    );
}

// --- idempotence over the resugared forms (fmt(fmt(x)) == fmt(x)) ---

#[test]
fn resugared_output_is_idempotent() {
    let src = "mut n = 1\nn += 2\ns = #{1, 2}\nmut m = {\"a\": 1}\nm[\"b\"] = 2\n";
    let once = format_source("resugar.noe", src, &FmtConfig::default()).expect("formats");
    let twice = format_source("resugar.noe", &once, &FmtConfig::default()).expect("formats");
    assert_eq!(once, twice);
}

// --- the two reflection surfaces stay distinct ---

#[test]
fn reflection_turbofish_round_trips() {
    preserved(
        "struct T {\n    a: int\n}\n\nspecs = field_specs_of::<T>()\nv = construct::<T>([1])\nc = variants_of::<T>()\n",
    );
}

/// `type_name::<T>()` is turbofish-only — there is no call form for the printer to drift into.
#[test]
fn type_name_turbofish_round_trips() {
    preserved("struct T {\n    a: int\n}\n\nname = type_name::<T>()\n");
}

#[test]
fn authored_reflection_string_is_not_sugared() {
    // The dynamic surface takes a runtime `string`, and a literal that happens to spell a local
    // type name is still that string — printing it back as `field_specs_of::<T>()` would change
    // what the program asks for, because the turbofish resolves to the type's *qualified* identity
    // while the string is taken verbatim. The two surfaces were previously one `Expr::Str` operand,
    // so the printer could not tell them apart and rewrote this into the turbofish form.
    preserved(
        "struct T {\n    a: int\n}\n\nspecs = field_specs_of(\"T\")\nv = construct(\"T\", [1])\nc = variants_of(\"T\")\n",
    );
}

// --- spread list literals ---
//
// `[...a, b]` desugars to `[] ~ ...a ~ [b]`, and the printer walks that `~` chain back to the
// literal. The chain a *literal* produces is indistinguishable, by shape alone, from that literal
// concatenated with something else — both bottom out in the same synthetic empty list — so the walk
// also has to reject chunks the desugar could never have emitted. Found by `noeta-fuzz`.

#[test]
fn spread_list_round_trips() {
    preserved("a = [1]\nv = [...a, 2]\necho v\n");
    preserved("a = [1]\nv = [2, ...a]\necho v\n");
    preserved("a = [1]\nb = [2]\nv = [...a, ...b]\necho v\n");
}

/// A spread list concatenated with an **empty** list keeps its `~ []`. The desugar groups plain
/// elements and never emits an empty group, so an empty `List` chunk can only be an author's `~ []`
/// — reading it as part of the literal printed `[...a]`, dropping a `Concat` node, and the safety
/// gate refused to format the file.
#[test]
fn a_spread_list_concatenated_with_an_empty_list_is_not_sugared() {
    preserved("a = [1]\nv = [...a] ~ []\necho v\n");
}

/// And concatenated with a non-list operand: an `Ident` chunk is likewise something the desugar
/// could not have produced, so the `~ c` must survive.
#[test]
fn a_spread_list_concatenated_with_a_value_is_not_sugared() {
    preserved("a = [1]\nc = [2]\nv = [...a] ~ c\necho v\n");
}

/// The genuinely ambiguous case, pinned deliberately: `[...a] ~ [b]` and `[...a, b]` desugar to the
/// *same* tree, so printing the literal spelling is a faithful rendering of it, not a rewrite.
#[test]
fn a_spread_list_concatenated_with_a_list_literal_is_sugared() {
    let out = format_source(
        "resugar.noe",
        "a = [1]\nv = [...a] ~ [2]\necho v\n",
        &FmtConfig::default(),
    )
    .expect("formats");
    assert_eq!(out, "a = [1]\nv = [...a, 2]\necho v\n");
}
