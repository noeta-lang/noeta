//! Dev-tier **activation** (object-model slice 6): resolve a program's `@<tier> { … }` blocks
//! against an *active-tier set* before the checker and the backends see it.
//!
//! A tier block is co-located developer-tooling content (`test`/`bench`/`doc`/`debug`). Whether a
//! block is compiled in is the *profile*'s call (the package manifest, later); this module is the
//! front-end mechanism a profile drives. Given the resolved active set, [`activate_tiers`]:
//!
//! - **inlines** an active code-tier block's items into the top-level statement stream, so they are
//!   checked and lowered as ordinary declarations (the block is pure grouping sugar);
//! - **drops** an inactive block (it never reaches the checker or the IR — the strip is by
//!   construction, no DCE pass);
//! - **validates** every block's tier name against [`BUILTIN_TIERS`] (an unknown tier is an
//!   `E0036`, active or not — a typo must surface, not silently vanish); and
//! - **discovers** the `@test` fns it activated, so the runner finds them without a second walk.
//!
//! The *default* program path (`lang run`, the conformance differential) runs with an **empty**
//! active set and does **not** call this — those paths keep stripping inactive blocks at lowering
//! (`lang_ir::lower`), and the checker keeps validating tier names in place. Only the test runner
//! activates a tier, so the differential is untouched by construction. The two E0036 sources (this
//! module and the checker's in-place arm) share [`unknown_tier_diagnostic`], so they never drift.

use lang_ast::{Program, Stmt};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::Span;

/// The dev-tiers the language ships built in. A `@<tier> { … }` block against any other name is an
/// `E0036` (a typo must not silently vanish). Hardcoded for now; once `@tier` declarations + the
/// package manifest land, the active set becomes a build profile's resolved provider-map and this
/// constant gives way to that set.
pub const BUILTIN_TIERS: &[&str] = &["test", "bench", "doc", "debug"];

/// The `E0036 UnknownTier` diagnostic for a `@<tier>` whose name is not a built-in tier. Shared by
/// [`activate_tiers`] and the checker's in-place `TierBlock` arm so the two never diverge.
pub fn unknown_tier_diagnostic(tier: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownTier,
        span,
        format!("unknown dev-tier `@{tier}`"),
    )
    .with_help(format!(
        "the built-in tiers are {}",
        BUILTIN_TIERS
            .iter()
            .map(|t| format!("`@{t}`"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// A `@test` fn surfaced by activation — a root the runner invokes by name.
#[derive(Debug, Clone, PartialEq)]
pub struct TestFn {
    /// The fn's name, used to invoke it.
    pub name: String,
    /// Where it is declared (for the runner's report).
    pub span: Span,
}

/// The result of resolving a program's tier blocks against an active set.
#[derive(Debug, Clone, PartialEq)]
pub struct Activated {
    /// The program with active tier blocks inlined and inactive ones removed — ready to check and
    /// lower as if the tier blocks had never been a distinct form.
    pub program: Program,
    /// The `@test` fns activated by this resolution, in source order.
    pub tests: Vec<TestFn>,
    /// `E0036` for any block naming an unknown tier (active or not).
    pub diagnostics: Vec<Diagnostic>,
}

/// Resolve `program`'s top-level `@<tier> { … }` blocks against `active` (the set of live tier
/// names). Active blocks are inlined into the statement stream; inactive blocks are dropped; every
/// block's name is validated. The `@test` fns among the activated blocks are collected as roots.
pub fn activate_tiers(program: &Program, active: &[&str]) -> Activated {
    let mut stmts = Vec::with_capacity(program.stmts.len());
    let mut tests = Vec::new();
    let mut diagnostics = Vec::new();

    for stmt in &program.stmts {
        let Stmt::TierBlock {
            tier,
            tier_span,
            items,
            ..
        } = stmt
        else {
            stmts.push(stmt.clone());
            continue;
        };

        if !BUILTIN_TIERS.contains(&tier.as_str()) {
            diagnostics.push(unknown_tier_diagnostic(tier, *tier_span));
        }
        if !active.contains(&tier.as_str()) {
            // Inactive (including an unknown tier): stripped, never reaches the checker or the IR.
            continue;
        }

        // Active code tier: inline its items as ordinary top-level declarations, recording the
        // `@test` fns so the runner can invoke them.
        for item in items {
            if tier == "test"
                && let Stmt::Fn(decl) = item
            {
                tests.push(TestFn {
                    name: decl.name.clone(),
                    span: decl.name_span,
                });
            }
            stmts.push(item.clone());
        }
    }

    Activated {
        program: Program {
            stmts,
            span: program.span,
        },
        tests,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_lexer::lex;
    use lang_parser::parse;
    use lang_span::{Source, SourceId};

    fn parse_program(text: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "test.lang", text.to_string());
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "fixture must parse cleanly"
        );
        parsed.program
    }

    /// An active `@test` block inlines its fns as top-level decls and surfaces them as tests; the
    /// program's own declarations are preserved and the `@test` *block* form is gone.
    #[test]
    fn active_test_block_inlines_and_discovers() {
        let program = parse_program(
            "fn add(a: int, b: int): int { return a + b; }\n\
             @test {\n\
                 fn adds() { assert(add(1, 2) == 3); }\n\
                 fn more() { assert(add(2, 2) == 4); }\n\
             }\n",
        );
        let out = activate_tiers(&program, &["test"]);
        assert!(out.diagnostics.is_empty());
        assert_eq!(
            out.tests
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["adds", "more"]
        );
        // `add` + the two inlined test fns — and no `TierBlock` survives.
        assert_eq!(out.program.stmts.len(), 3);
        assert!(
            !out.program
                .stmts
                .iter()
                .any(|s| matches!(s, Stmt::TierBlock { .. }))
        );
    }

    /// With the tier inactive (a normal run / `lang test` over a non-`test` tier) the block is
    /// dropped entirely: no inlining, no tests, and nothing left in the stream.
    #[test]
    fn inactive_test_block_is_stripped() {
        let program = parse_program(
            "fn add(a: int, b: int): int { return a + b; }\n\
             @test { fn adds() { assert(add(1, 2) == 3); } }\n",
        );
        let out = activate_tiers(&program, &[]);
        assert!(out.diagnostics.is_empty());
        assert!(out.tests.is_empty());
        assert_eq!(out.program.stmts.len(), 1);
    }

    /// An unknown tier is an `E0036` whether or not it would be active, and its block is dropped.
    #[test]
    fn unknown_tier_reports_e0036() {
        let program = parse_program("@tset { fn x() { echo \"hi\"; } }\n");
        let out = activate_tiers(&program, &["test"]);
        assert_eq!(out.diagnostics.len(), 1);
        assert_eq!(out.diagnostics[0].code, DiagnosticCode::UnknownTier);
        assert!(out.tests.is_empty());
        assert!(out.program.stmts.is_empty());
    }
}
