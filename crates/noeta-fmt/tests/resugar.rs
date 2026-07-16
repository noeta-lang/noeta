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
