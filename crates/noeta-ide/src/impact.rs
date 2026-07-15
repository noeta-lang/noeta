//! The **impact engine** (server-hmr W3) — one runner-agnostic seam answering: *given this edit,
//! which declarations may behave differently?* Consumers filter the answer to their own tier and
//! rerun only that: `noeta test --watch` reruns the impacted `@test` fns (via the runner's
//! `--name` filter), `noeta bench --watch` the impacted `@bench` fns, and a third-party tier
//! runner gets the same query through this module.
//!
//! The pipeline composes two shipped pieces: the hot-swap differ
//! ([`noeta_compiler::hotswap::diff_programs`]) attributes the edit to definition names, and the
//! **reverse transitive closure** over the static call graph ([`crate::callgraph`]) walks from
//! those names to everything that calls (or references) them — a changed leaf reruns exactly its
//! caller-tests.
//!
//! # Soundness valves (part of the contract, not the consumer's job)
//!
//! An edit the differ cannot attribute — a layout/signature/namespace change, a changed
//! *top-level statement* (fixtures and globals live there), red code — degrades to
//! [`Impact::All`] **with the reason**, as does a top-level *use* of an impacted declaration
//! (setup may differ for every run). The closure is static: a call through a closure that was
//! stored in a data structure and invoked elsewhere is attributed to where the function was
//! *referenced*, not where the value ends up called — reference edges (`f` passed as a value)
//! are followed exactly like calls, which covers the common callback shapes, but consumers
//! should still surface an occasional full pass. False positives rerun harmlessly; the valves
//! exist so false *negatives* require genuinely dynamic reachability.

use std::collections::BTreeSet;

use noeta_compiler::hotswap::SwapDiff;

use crate::callgraph::{self, Callee};

/// The engine's answer for one edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Impact {
    /// The declarations (top-level fns, `Type.method`s, tier fns) whose behavior may have
    /// changed — the edited definitions plus the reverse closure of their callers. Empty means
    /// the edit was behaviorally inert (formatting, comments).
    Decls(Vec<String>),
    /// The edit cannot be attributed to declarations; rerun everything, and report why.
    All { reason: String },
}

/// Compute the impact of editing `old_src` into `new_src` (one file — the entry the runner was
/// pointed at; an edit to any *other* file is the caller's impact-all case).
pub fn impact_of_edit(old_src: &str, new_src: &str, edition: noeta_lexer::Edition) -> Impact {
    let Some(old_program) = parse_clean(old_src, edition) else {
        // The BASELINE not parsing means we cannot attribute anything against it.
        return Impact::All {
            reason: "the previous version does not parse".into(),
        };
    };
    let Some(new_program) = parse_clean(new_src, edition) else {
        return Impact::All {
            reason: "the edit does not parse".into(),
        };
    };
    match noeta_compiler::hotswap::diff_programs(&old_program, old_src, &new_program, new_src) {
        SwapDiff::Unchanged => Impact::Decls(Vec::new()),
        SwapDiff::NeedsRestart(blockers) => Impact::All {
            reason: blockers
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        },
        SwapDiff::Swap(plan) => {
            if plan.rerun_top_level {
                // Top-level statements are every run's setup (globals, fixtures): a change
                // there can shift what any consumer observes.
                return Impact::All {
                    reason: "top-level statements changed".into(),
                };
            }
            // Activate EVERY tier the file declares before checking and graphing: tier fns are
            // ordinary fns to a runner, and their method-call edges only resolve with checker
            // types — which the residual (stripped) form never gets.
            let tiers: BTreeSet<String> = new_program
                .stmts
                .iter()
                .filter_map(|s| match s {
                    noeta_ast::Stmt::TierBlock { tier, .. } => Some(tier.clone()),
                    _ => None,
                })
                .collect();
            let tier_refs: Vec<&str> = tiers.iter().map(String::as_str).collect();
            let activated = noeta_check::activate_tiers(&new_program, &tier_refs);
            let program = &activated.program;
            let mut editions = noeta_lexer::EditionMap::new();
            editions.set(noeta_span::SourceId::FIRST, edition);
            let checked = noeta_check::check_all_with_editions(program, editions);
            if !activated.diagnostics.is_empty() || !checked.diagnostics.is_empty() {
                // Red code: the consumer's own run will surface the diagnostics.
                return Impact::All {
                    reason: "the edit does not check".into(),
                };
            }
            let graph = callgraph::build(program, &checked.expr_types, &[new_src]);
            let mut impacted: BTreeSet<String> = plan
                .changed
                .iter()
                .chain(&plan.added)
                .chain(&plan.removed)
                .cloned()
                .collect();
            // Reverse transitive closure, to a fixpoint: any caller (or referencer) of an
            // impacted declaration is impacted. A TOP-LEVEL use fires the setup valve. Two edge
            // flavors count: a resolved `Function` edge, and — the method fallback — a `Dynamic`
            // member edge whose member NAME matches an impacted `Type.method` (an untyped
            // receiver's `c.bump()` cannot be resolved to `Counter.bump` statically, so it
            // over-approximates by name; a false positive reruns harmlessly, and the fallback is
            // what keeps missed static method calls out of the false-negative budget).
            loop {
                let mut grew = false;
                let impacted_methods: BTreeSet<String> = impacted
                    .iter()
                    .filter_map(|n| n.split_once('.').map(|(_, m)| m.to_string()))
                    .collect();
                for edge in &graph.edges {
                    let hit: Option<String> = match &edge.callee {
                        Callee::Function(i) => impacted
                            .contains(&graph.functions[*i].name)
                            .then(|| graph.functions[*i].name.clone()),
                        Callee::Dynamic(target) => {
                            let member = target.rsplit('.').next().unwrap_or(target);
                            impacted_methods.contains(member).then(|| target.clone())
                        }
                        Callee::External(_) => None,
                    };
                    let Some(used) = hit else { continue };
                    match edge.caller {
                        Some(j) => {
                            if impacted.insert(graph.functions[j].name.clone()) {
                                grew = true;
                            }
                        }
                        None => {
                            return Impact::All {
                                reason: format!("the top level uses changed `{used}`"),
                            };
                        }
                    }
                }
                if !grew {
                    break;
                }
            }
            Impact::Decls(impacted.into_iter().collect())
        }
    }
}

fn parse_clean(src: &str, edition: noeta_lexer::Edition) -> Option<noeta_ast::Program> {
    let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "<impact>", src);
    let lexed = noeta_lexer::lex_in(&source, edition, &noeta_lexer::TextTiers::default());
    let parsed = noeta_parser::parse_in(
        &source,
        &lexed.tokens,
        edition,
        &noeta_lexer::TextTiers::default(),
    );
    (lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty()).then_some(parsed.program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls(impact: Impact) -> Vec<String> {
        match impact {
            Impact::Decls(d) => d,
            Impact::All { reason } => panic!("expected attributed impact, got All: {reason}"),
        }
    }

    const V1: &str = "fn leaf(): int { return 1; }\n\
                      fn mid(): int { return leaf(); }\n\
                      fn other(): int { return 2; }\n\
                      @test fn t_mid(): void { assert(mid() == 1); }\n\
                      @test fn t_other(): void { assert(other() == 2); }\n";

    #[test]
    fn a_leaf_edit_impacts_exactly_its_reverse_closure() {
        // leaf changed → mid (calls leaf) → t_mid (calls mid). t_other untouched.
        let v2 = V1.replace("return 1;", "return 0 + 1;");
        assert_eq!(
            decls(impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["leaf".to_string(), "mid".to_string(), "t_mid".to_string()]
        );
    }

    #[test]
    fn a_test_body_edit_impacts_only_that_test() {
        let v2 = V1.replace("assert(other() == 2);", "assert(other() == 1 + 1);");
        assert_eq!(
            decls(impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["t_other".to_string()]
        );
    }

    #[test]
    fn a_formatting_edit_impacts_nothing() {
        let v2 = V1.replace(
            "fn other(): int { return 2; }",
            "fn other(): int  { return 2; }",
        );
        assert_eq!(
            decls(impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn unattributable_edits_degrade_to_all_with_a_reason() {
        // A signature change.
        let v2 = V1.replace("fn leaf(): int", "fn leaf(n: int): int");
        let Impact::All { reason } = impact_of_edit(V1, &v2, noeta_lexer::Edition::DEFAULT) else {
            panic!("a signature change is unattributable");
        };
        assert!(reason.contains("leaf"), "{reason}");
        // A top-level statement change.
        let v3 = format!("{V1}echo mid()\n");
        assert!(matches!(
            impact_of_edit(V1, &v3, noeta_lexer::Edition::DEFAULT),
            Impact::All { .. }
        ));
        // Red code.
        let v4 = V1.replace("return leaf();", "return leaf() * \"boom\";");
        let Impact::All { reason } = impact_of_edit(V1, &v4, noeta_lexer::Edition::DEFAULT) else {
            panic!("red code is unattributable");
        };
        assert!(reason.contains("check"), "{reason}");
    }

    #[test]
    fn a_reference_edge_counts_like_a_call() {
        // `apply` takes leaf as a VALUE; the closure walks the reference edge.
        let v1 = "fn leaf(): int { return 1; }\n\
                  fn apply(f: () -> int): int { return f(); }\n\
                  @test fn t(): void { assert(apply(leaf) == 1); }\n";
        let v2 = v1.replace("return 1;", "return 2 - 1;");
        assert_eq!(
            decls(impact_of_edit(v1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["leaf".to_string(), "t".to_string()]
        );
    }

    #[test]
    fn a_method_edit_impacts_its_calling_tests() {
        let v1 = "struct Counter {\n    n: int\n\n    fn bump(): int { return self.n + 1; }\n}\n\
                  @test fn t_bump(): void {\n    c = Counter { n: 1 }\n    assert(c.bump() == 2);\n}\n\
                  @test fn t_none(): void { assert(true); }\n";
        let v2 = v1.replace("return self.n + 1;", "return 1 + self.n;");
        assert_eq!(
            decls(impact_of_edit(v1, &v2, noeta_lexer::Edition::DEFAULT)),
            vec!["Counter.bump".to_string(), "t_bump".to_string()]
        );
    }
}
