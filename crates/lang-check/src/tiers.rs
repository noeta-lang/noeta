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

use lang_ast::{AttrArg, Attribute, Program, Stmt};
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

/// A code-tier `fn` surfaced by activation — a root the matching runner invokes by name (a `@test`
/// fn for `lang test`, a `@bench` fn for `lang bench`).
#[derive(Debug, Clone, PartialEq)]
pub struct TierFn {
    /// The fn's name, used to invoke it.
    pub name: String,
    /// Where it is declared (for the runner's report).
    pub span: Span,
    /// The directive arguments on the block that introduced it — e.g. `@bench(iterations: 1000)`.
    /// The runner reads the ones it understands (the bench runner reads `iterations`); a bare block
    /// carries none.
    pub args: Vec<AttrArg>,
    /// The `#[...]` data attributes on the fn itself — test metadata the runner reads (`#[Skip]`,
    /// `#[Name("…")]`, `#[Group("…")]`, `#[Data([…])]`). Empty for an unannotated fn.
    pub attrs: Vec<Attribute>,
}

/// A `@doc { … }` text-tier block's verbatim body, surfaced for extraction by `lang doc` (slice
/// 6f). The text is the raw source between the braces, captured un-parsed by the lexer.
#[derive(Debug, Clone, PartialEq)]
pub struct DocBlock {
    /// The verbatim body text.
    pub text: String,
    /// The whole `@doc { … }` span, for the extractor's source-location header.
    pub span: Span,
}

/// Collect every top-level `@doc { … }` block's verbatim body, in source order. `@doc` is a
/// *declaration-position* text tier, so a top-level walk is the whole story; the bodies never reach
/// the checker or lowering (a normal run strips them like any inactive tier). `lang doc` extracts
/// these.
pub fn collect_docs(program: &Program) -> Vec<DocBlock> {
    program
        .stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::TierBlock {
                tier,
                doc_text: Some(text),
                span,
                ..
            } if tier == "doc" => Some(DocBlock {
                text: text.clone(),
                span: *span,
            }),
            _ => None,
        })
        .collect()
}

/// The result of resolving a program's tier blocks against an active set.
#[derive(Debug, Clone, PartialEq)]
pub struct Activated {
    /// The program with active tier blocks inlined and inactive ones removed — ready to check and
    /// lower as if the tier blocks had never been a distinct form.
    pub program: Program,
    /// The `@test` fns activated by this resolution, in source order (roots for `lang test`).
    pub tests: Vec<TierFn>,
    /// The `@bench` fns activated by this resolution, in source order (roots for `lang bench`).
    pub benches: Vec<TierFn>,
    /// `E0036` for any block naming an unknown tier (active or not).
    pub diagnostics: Vec<Diagnostic>,
}

/// Resolve `program`'s `@<tier> { … }` blocks against `active` (the set of live tier names),
/// **everywhere they appear** — top-level (a `@test` block of declarations) and nested in statement
/// position (a `@debug { … }` block inside a fn/method body or a control-flow branch). Active blocks
/// are inlined in place; inactive blocks are dropped; every block's name is validated. The `@test`
/// fns among the activated *top-level* blocks are collected as roots the runner invokes.
pub fn activate_tiers(program: &Program, active: &[&str]) -> Activated {
    let mut roots = Roots::default();
    let mut diagnostics = Vec::new();
    // The top-level statement list collects roots (a `@test`/`@bench` block's fns are runnable roots
    // only here — a tier block nested in a fn body holds inline code, not roots).
    let stmts = resolve_block(&program.stmts, active, &mut diagnostics, &mut roots, true);
    Activated {
        program: Program {
            stmts,
            span: program.span,
        },
        tests: roots.tests,
        benches: roots.benches,
        diagnostics,
    }
}

/// The runnable roots a top-level activation collects, partitioned by tier — `@test` fns for
/// `lang test`, `@bench` fns for `lang bench`.
#[derive(Default)]
struct Roots {
    tests: Vec<TierFn>,
    benches: Vec<TierFn>,
}

/// Resolve the tier blocks in one statement list. A `@<tier> { … }` is validated, then inlined (its
/// items spliced in place, each recursively resolved) when its tier is active or dropped when not;
/// every other statement is left in place with its *own* nested statement lists resolved
/// ([`resolve_children`]). `collect_tests` is true only for the program's top-level list, so only a
/// top-level `@test` block's fns become runnable roots.
fn resolve_block(
    stmts: &[Stmt],
    active: &[&str],
    diagnostics: &mut Vec<Diagnostic>,
    roots: &mut Roots,
    collect_roots: bool,
) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        let Stmt::TierBlock {
            tier,
            tier_span,
            args,
            items,
            ..
        } = stmt
        else {
            out.push(resolve_children(stmt, active, diagnostics));
            continue;
        };

        if !BUILTIN_TIERS.contains(&tier.as_str()) {
            diagnostics.push(unknown_tier_diagnostic(tier, *tier_span));
        }
        if !active.contains(&tier.as_str()) {
            // Inactive (including an unknown tier): stripped, never reaches the checker or the IR.
            continue;
        }

        // Active tier: resolve the items (so a tier block nested among them, and each item's own
        // body, are handled), then splice them in place. The items are spliced at *this* level, so
        // `collect_roots` carries through unchanged. Each lifted `fn` is marked `is_dev_tier` so the
        // checker grants it white-box access to the module's private fields (slice 6d); a top-level
        // `@test`/`@bench` block's fns are also recorded as roots (carrying the block's directive
        // args, e.g. `@bench(iterations: …)`, so the runner can read them).
        let resolved = resolve_block(items, active, diagnostics, roots, collect_roots);
        for mut item in resolved {
            if let Stmt::Fn(decl) = &mut item {
                decl.is_dev_tier = true;
                if collect_roots {
                    let sink = match tier.as_str() {
                        "test" => Some(&mut roots.tests),
                        "bench" => Some(&mut roots.benches),
                        _ => None,
                    };
                    if let Some(sink) = sink {
                        sink.push(TierFn {
                            name: decl.name.clone(),
                            span: decl.name_span,
                            args: args.clone(),
                            attrs: decl.attrs.clone(),
                        });
                    }
                }
            }
            out.push(item);
        }
    }
    out
}

/// Rewrite a non-tier statement's own nested statement lists (control-flow branches, loop and
/// fn/method bodies, a class destructor), resolving any tier blocks within. Nested lists never
/// collect tests (`collect_tests = false`). Statements with no nested statements are returned
/// unchanged. Tier blocks live only in statement position, so there is no need to descend into
/// expressions (closures and `match`/`if` *expressions* are expression-bodied).
fn resolve_children(stmt: &Stmt, active: &[&str], diagnostics: &mut Vec<Diagnostic>) -> Stmt {
    let mut stmt = stmt.clone();
    let block = |stmts: &[Stmt], diags: &mut Vec<Diagnostic>| -> Vec<Stmt> {
        // Nested statement lists never produce runnable roots (`collect_roots = false`); the sink is
        // a throwaway.
        let mut sink = Roots::default();
        resolve_block(stmts, active, diags, &mut sink, false)
    };
    match &mut stmt {
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            *then_body = block(then_body, diagnostics);
            if let Some(eb) = else_body {
                *eb = block(eb, diagnostics);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => {
            *body = block(body, diagnostics);
        }
        Stmt::Fn(decl) => decl.body = block(&decl.body, diagnostics),
        Stmt::Class(c) => {
            for m in &mut c.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut c.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
            if let Some(d) = &mut c.destructor {
                *d = block(d, diagnostics);
            }
        }
        Stmt::Struct(s) => {
            for m in &mut s.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut s.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
        }
        Stmt::Enum(en) => {
            for m in &mut en.methods {
                m.body = block(&m.body, diagnostics);
            }
            for im in &mut en.impls {
                for m in &mut im.methods {
                    m.body = block(&m.body, diagnostics);
                }
            }
        }
        Stmt::Impl(im) => {
            for m in &mut im.methods {
                m.body = block(&m.body, diagnostics);
            }
        }
        _ => {}
    }
    stmt
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

    /// White-box (slice 6d): a private `class` field is visible inside an active dev-tier (`@test`)
    /// fn body — read, write, and construct — so a white-box test type-checks with no E0035. With
    /// the tier inactive the block is stripped, so the bare program checks clean too.
    #[test]
    fn dev_tier_fn_gets_white_box_field_access() {
        let program = parse_program(
            "class Account { mut balance: int  fn new(b: int): Account { return Account { balance: b }; } }\n\
             @test fn touches(): void { mut a = Account { balance: 0 }; a.balance = 5; assert(a.balance == 5); }\n",
        );
        let active = crate::check_all(&activate_tiers(&program, &["test"]).program);
        assert!(
            active.diagnostics.is_empty(),
            "white-box dev-tier fn must not raise E0035: {:?}",
            active.diagnostics
        );
        let inactive = crate::check_all(&activate_tiers(&program, &[]).program);
        assert!(inactive.diagnostics.is_empty());
    }

    /// The white-box relaxation is **scoped**: ordinary same-module code (not a dev-tier fn) still
    /// cannot read a private field — it is an E0035, exactly as before slice 6d.
    #[test]
    fn ordinary_fn_cannot_read_private_field() {
        let program = parse_program(
            "class Account { balance: int  fn new(b: int): Account { return Account { balance: b }; } }\n\
             fn reads(): int { a = Account.new(1); return a.balance; }\n",
        );
        let diags = crate::check_all(&program).diagnostics;
        assert!(
            diags.iter().any(|d| d.code == DiagnosticCode::PrivateField),
            "ordinary code reading a private field must still be E0035: {diags:?}"
        );
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

    /// The number of `echo` statements anywhere inside a fn body (recursively) — a proxy for how
    /// much of a `@debug` block survived activation.
    fn echoes_in_fn(stmt: &Stmt) -> usize {
        fn count(stmts: &[Stmt]) -> usize {
            stmts
                .iter()
                .map(|s| match s {
                    Stmt::Echo { .. } => 1,
                    Stmt::For { body, .. } | Stmt::While { body, .. } => count(body),
                    _ => 0,
                })
                .sum()
        }
        match stmt {
            Stmt::Fn(decl) => count(&decl.body),
            _ => 0,
        }
    }

    /// A `@debug { … }` block *nested in a fn body* (statement position) is resolved recursively:
    /// inlined in place when `debug` is active, stripped when not. (The top-level `@test` resolution
    /// is not the only one — activation reaches inside bodies.)
    #[test]
    fn nested_debug_block_is_resolved_in_place() {
        let program = parse_program(
            "fn f(x: int): void {\n\
                 @debug { echo \"dbg ${x}\"; }\n\
                 echo \"always\";\n\
             }\n",
        );
        // Inactive: only the unconditional `echo` survives in the body.
        let stripped = activate_tiers(&program, &[]);
        assert_eq!(echoes_in_fn(&stripped.program.stmts[0]), 1);
        // Active: the `@debug` echo is inlined too — two echoes in the body.
        let active = activate_tiers(&program, &["debug"]);
        assert_eq!(echoes_in_fn(&active.program.stmts[0]), 2);
        // An active nested `@debug` block does not produce test roots.
        assert!(active.tests.is_empty());
    }
}
