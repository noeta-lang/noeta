//! The safety-gate comparison: are two programs structurally equal, ignoring spans?
//!
//! Formatting shifts every byte offset, so the AST's derived `PartialEq` — which compares spans
//! along with everything else — cannot be used directly. [`noeta_ast::normalize`] closes that one
//! gap: it sets every span to a fixed value in an exhaustive walk, after which `a == b` **is** the
//! question the gate is asking, with no rendering in between.
//!
//! That last clause is the point. Comparing a *printed* form of both programs is only equivalent to
//! structural equality if the printer is injective, and nothing enforces that — two formatter
//! defects reached `main` through exactly that gap (see the `normalize` module docs). Field
//! completeness here is what `#[derive(PartialEq)]` is, and two different node kinds cannot collapse
//! onto one rendering because there is no rendering.

use noeta_ast::Program;
use noeta_ast::normalize::{Normalization, normalize};

/// Whether `a` and `b` are the same program up to span positions **and up to import ordering**. The
/// latter lets the import-sorting formatter reorder `use` statements (and the names inside a `use`)
/// without tripping the gate — reordering imports is semantics-neutral, so canonicalizing it away on
/// both sides is sound and keeps every other structural difference caught. The canonicalization
/// reaches every statement list, because so does the printer: a `use` run inside a `@test { … }`
/// block or a function body is sorted exactly like a module's top-level one.
pub fn ast_equal_modulo_spans(a: &Program, b: &Program) -> bool {
    equal_under(
        a,
        b,
        &Normalization {
            canonical_imports: true,
            ..Normalization::default()
        },
    )
}

/// As [`ast_equal_modulo_spans`], but also ignoring the **static text of tier bodies** — the relaxed
/// gate for extension-driven tier-body formatting. A body formatter reflows a tier's foreign text, so
/// its `statics` change; fmt cannot prove that reflow value-preserving in the foreign language (only
/// the formatter's author can), so both sides have them cleared. Everything else — the tier name, the
/// `${…}` holes between the statics, and every node outside tier bodies — is still compared exactly,
/// so a real formatting bug is still caught.
pub fn ast_equal_ignoring_tier_statics(a: &Program, b: &Program) -> bool {
    equal_under(
        a,
        b,
        &Normalization {
            clear_tier_statics: true,
            canonical_imports: true,
        },
    )
}

/// Canonicalize both programs under `how` and compare them structurally.
fn equal_under(a: &Program, b: &Program, how: &Normalization) -> bool {
    let (mut a, mut b) = (a.clone(), b.clone());
    normalize(&mut a, how);
    normalize(&mut b, how);
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    /// Parse `src`, with `sql`/`html` known as text tiers so a tier body lexes as one. Panics on a
    /// parse error: a test that silently compared two *empty* programs would pass for the wrong
    /// reason, which is exactly how the first draft of the tier test below passed.
    fn program(src: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let tiers = noeta_lexer::TextTiers::with(["sql".to_string(), "html".to_string()]);
        let lexed = noeta_lexer::lex_in(&source, noeta_lexer::Edition::DEFAULT, &tiers);
        let parsed = noeta_parser::parse_in(
            &source,
            &lexed.tokens,
            noeta_lexer::Edition::DEFAULT,
            &tiers,
        );
        assert!(
            parsed.diagnostics.is_empty(),
            "fixture must parse: {:?}",
            parsed.diagnostics
        );
        parsed.program
    }

    #[test]
    fn spans_alone_never_make_two_programs_differ() {
        // The whole reason the gate needs a normalization step: the same program written with
        // different whitespace is the same program, and every byte offset in it differs.
        assert!(ast_equal_modulo_spans(
            &program("echo 1\necho 2\n"),
            &program("\n\n   echo 1\n\n\n      echo 2\n"),
        ));
    }

    #[test]
    fn a_payload_less_variant_pattern_is_not_a_catch_all_binding() {
        // **The regression test for a defect that shipped.** `Ok()` is a constructor pattern that
        // matches one case; bare `Ok` is a binding that matches *everything*, so every later arm
        // goes dead. The printer was dropping exactly those parens, and the gate — which compared a
        // rendering in which both printed as `Ok` — approved the rewrite.
        assert!(!ast_equal_modulo_spans(
            &program("x = match r { Ok() => 1, _ => 2 }\n"),
            &program("x = match r { Ok => 1, _ => 2 }\n"),
        ));
    }

    #[test]
    fn an_attached_tier_block_is_not_a_braced_one() {
        // **The other defect that shipped.** `@test fn t() {…}` decorates the declaration;
        // `@test { fn t() {…} }` is a block that contains it. The checker branches on the
        // difference (a declared-site check runs only when attached), so collapsing one into the
        // other can invent an error the author's source does not have.
        assert!(!ast_equal_modulo_spans(
            &program("@test fn t(): void {}\n"),
            &program("@test { fn t(): void {} }\n"),
        ));
    }

    #[test]
    fn imports_are_compared_up_to_order() {
        // The one canonicalization the gate grants, because the formatter is allowed to sort
        // imports and reordering them is semantics-neutral.
        assert!(ast_equal_modulo_spans(
            &program("use std.io\nuse std.fs\n"),
            &program("use std.fs\nuse std.io\n"),
        ));
        // …and it is granted to imports only: reordering anything else is a different program.
        assert!(!ast_equal_modulo_spans(
            &program("echo 1\necho 2\n"),
            &program("echo 2\necho 1\n"),
        ));
    }

    #[test]
    fn imports_are_compared_up_to_order_at_every_depth() {
        // **The regression test for a defect that shipped.** The printer sorts imports in every
        // statement list it prints, and the gate canonicalized only the module's top level — so a
        // `use` run inside a `@test { … }` block or a function body was sorted by the printer,
        // seen as a reordering by the gate, and the file was refused as a printer bug.
        assert!(ast_equal_modulo_spans(
            &program("@test {\n    use A.b\n    use A.a\n}\n"),
            &program("@test {\n    use A.a\n    use A.b\n}\n"),
        ));
        assert!(ast_equal_modulo_spans(
            &program("fn f(): void {\n    use A.{b, a}\n}\n"),
            &program("fn f(): void {\n    use A.{a, b}\n}\n"),
        ));
        // The relaxation stays confined to imports at depth, too.
        assert!(!ast_equal_modulo_spans(
            &program("@test {\n    echo 1\n    echo 2\n}\n"),
            &program("@test {\n    echo 2\n    echo 1\n}\n"),
        ));
    }

    #[test]
    fn an_alias_is_part_of_the_import() {
        // Canonicalizing order must not canonicalize away *which* name each import binds: two
        // `use`s of the same path differing only in alias bind different locals.
        assert!(!ast_equal_modulo_spans(
            &program("use A.b as x\n"),
            &program("use A.b as y\n"),
        ));
        assert!(!ast_equal_modulo_spans(
            &program("use A.{b as x, c}\n"),
            &program("use A.{b as y, c}\n"),
        ));
    }

    #[test]
    fn a_type_annotation_is_part_of_the_program() {
        // Representative of the whole class the survey had to close by hand, one field at a time.
        // Under a derived comparison it needs no arm at all — the field is compared because it
        // exists.
        assert!(!ast_equal_modulo_spans(
            &program("acc: List<int> = []\n"),
            &program("acc = []\n"),
        ));
    }

    #[test]
    fn the_relaxed_gate_ignores_tier_body_text_and_nothing_else() {
        let a = program("x = @sql { select 1 }\n");
        let b = program("x = @sql { SELECT   1 }\n");
        // A body formatter reflowed the foreign text: the strict gate sees it…
        assert!(!ast_equal_modulo_spans(&a, &b));
        // …and the relaxed one, which the caller opts into only when a body formatter ran, does not.
        assert!(ast_equal_ignoring_tier_statics(&a, &b));
        // The relaxation is *only* the static text. A different tier is a different program…
        assert!(!ast_equal_ignoring_tier_statics(
            &a,
            &program("x = @html { select 1 }\n"),
        ));
        // …and so is a different hole between the statics, which is user code and never reflowed.
        assert!(!ast_equal_ignoring_tier_statics(
            &program("x = @sql { select ${a} }\n"),
            &program("x = @sql { select ${b} }\n"),
        ));
    }
}
