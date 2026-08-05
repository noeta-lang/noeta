//! The session-checker oracle (session-checker C4): per-entry [`SessionChecker::check_entry`]
//! diagnostics must agree with a **whole-program re-check of the accumulated clean source**,
//! restricted to the new entry's own `SourceId`.
//!
//! The restriction is semantic, not a shortcut: a whole-program check sees a *later* entry's
//! declarations while checking an *earlier* body, which a session, by definition, cannot —
//! cross-entry forward references are runtime-deferred at a prompt (pinned in the unit tests).
//! Restricting the comparison to the entry-under-check's spans compares exactly the judgement both
//! sides make about the same code with the same knowledge.
//!
//! Erroring entries are excluded from the accumulation, mirroring `check_entry`'s
//! transactionality: a skipped entry never ran, so the accumulated source must not contain it.

use noeta_check::{SessionChecker, check_all};
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId, Span};

/// Run `entries` through a session AND the accumulated-source oracle, asserting the per-entry
/// `(code, span)` diagnostics agree at every step.
fn assert_session_matches_oracle(entries: &[&str]) {
    // This oracle is its own assembling driver (audit-6 F2): seed the std units first.
    noeta_stdlib::registry::default_seeded();
    let mut session = SessionChecker::new();
    let mut accumulated: Vec<noeta_ast::Stmt> = Vec::new();
    let mut accumulated_span = Span::empty_at(0);

    for (i, text) in entries.iter().enumerate() {
        let id = SourceId(i as u32);
        let source = Source::new(id, format!("<entry:{i}>"), *text);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "entry {i} must parse cleanly: {:?}",
            parsed.diagnostics
        );

        let session_diags: Vec<(String, Span)> = session
            .check_entry(&parsed.program)
            .iter()
            .map(|d| (d.code.to_string(), d.span))
            .collect();

        // Oracle: one whole-program check of (clean prefix + this entry), restricted to this
        // entry's source.
        let mut oracle_stmts = accumulated.clone();
        oracle_stmts.extend(parsed.program.stmts.iter().cloned());
        let oracle_program = noeta_ast::Program {
            stmts: oracle_stmts,
            span: parsed.program.span,
        };
        let oracle_diags: Vec<(String, Span)> = check_all(&oracle_program)
            .diagnostics
            .iter()
            .filter(|d| d.span.source == id)
            .map(|d| (d.code.to_string(), d.span))
            .collect();

        assert_eq!(
            session_diags, oracle_diags,
            "entry {i} diverged from the accumulated-source oracle: {text:?}"
        );

        // Only a clean entry joins the accumulation — check_entry rolled an erroring one back.
        if session_diags.is_empty() {
            accumulated.extend(parsed.program.stmts);
            accumulated_span = parsed.program.span;
        }
    }
    let _ = accumulated_span;
}

#[test]
fn clean_sessions_agree_with_the_oracle() {
    assert_session_matches_oracle(&[
        "fn twice(n: int): int { return n * 2 }\nstruct P { x: int }\n",
        "mut total = twice(4)\n",
        "mut p = P { x: total }\ntotal = p.x + 1\n",
        "fn use_both(q: P): int { return twice(q.x) }\necho use_both(p)\n",
    ]);
}

#[test]
fn erroring_entries_agree_and_stay_out_of_the_accumulation() {
    assert_session_matches_oracle(&[
        "mut n = 1\nfixed = 2\n",
        // Retype: E0007 on both sides, then rolled back / excluded.
        "n = \"s\"\n",
        // Immutable reassign: E0006 on both sides.
        "fixed = 3\n",
        // The session recovered; a clean entry still agrees.
        "n = 7\necho n\n",
        // Re-`mut` retypes legally on both sides.
        "mut n = \"now a string\"\nn = \"still one\"\n",
    ]);
}

#[test]
fn methods_and_redeclared_types_agree() {
    assert_session_matches_oracle(&[
        "struct Point {\n    x: int\n    pub fn mag2(): int { return self.x * self.x }\n}\n",
        "mut p = Point { x: 3 }\necho p.mag2()\n",
        // Redeclaring the type (legal at a prompt) and using the NEW shape.
        "struct Point {\n    x: int\n    y: int\n    pub fn mag2(): int { return self.x * self.x + self.y * self.y }\n}\n",
        "mut q = Point { x: 3, y: 4 }\necho q.mag2()\n",
        // Wrong arity against a session-known method: same code, same span, both sides.
        "echo q.mag2(1)\n",
    ]);
}

#[test]
fn destructor_reachability_accumulates_identically() {
    assert_session_matches_oracle(&[
        "class Holder { r: Res }\nstruct Res { x: int }\n",
        "class Res { x: int\n    destruct { echo \"drop\" }\n}\n",
        "fn make(): Holder { return Holder { r: Res { x: 1 } } }\n",
    ]);
}

#[test]
fn required_signatures_and_reserved_names_agree() {
    assert_session_matches_oracle(&[
        // E0022 missing parameter signature.
        "fn f(n) { return n }\n",
        // E0046 reserved prelude value name.
        "fn assert(): int { return 1 }\n",
        // Clean aftermath.
        "fn g(n: int): int { return n }\necho g(2)\n",
    ]);
}
