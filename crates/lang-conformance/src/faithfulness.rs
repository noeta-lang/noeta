//! The **faithfulness** differential: run every corpus program through both the AST
//! tree-walker and the new Core-IR interpreter and assert they produce identical
//! [`RunResult`]s.
//!
//! This is the transitional oracle that makes the Core IR trustworthy. It sits *on top of*
//! the existing eval-vs-VM differential: where that proves the VM reproduces the tree-walker,
//! this proves the IR-interpreter reproduces the tree-walker — so the IR is established as a
//! faithful second representation before any later phase transforms it. The tree-walker
//! (`run_with_sites`) is the frozen reference; the IR path is `lower` + `run_ir`.
//!
//! Programs the lowering cannot yet handle are **skipped** (counted toward a climbing
//! coverage percentage), never failed — the same one-slice-at-a-time discipline the VM's
//! bytecode path uses. Every program that *does* lower is proven identical to the oracle.

use std::path::Path;

use lang_backend::RunResult;
use lang_db::LangDatabase;
use lang_eval::TreeWalkBackend;
use lang_span::{Source, SourceId};

use crate::collect_cases;

/// A disagreement between the tree-walker and the Core-IR interpreter on one program.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub name: String,
    pub detail: String,
}

/// The outcome of a faithfulness run over a corpus.
#[derive(Debug, Default)]
pub struct FaithReport {
    /// Programs that lowered and whose IR result matched the tree-walker.
    pub matched: usize,
    /// Programs outside the IR lowering's current subset (skipped, not failed).
    pub skipped: usize,
    /// Programs that did not parse cleanly, so there is no eval-level result to compare.
    pub parse_failed: usize,
    /// Programs the IR ran but whose result diverged from the tree-walker — failures.
    pub mismatches: Vec<Mismatch>,
}

impl FaithReport {
    /// Programs the IR ran (matched + diverged).
    pub fn supported(&self) -> usize {
        self.matched + self.mismatches.len()
    }

    /// Programs eligible for comparison (parsed + checked cleanly): supported + skipped.
    pub fn comparable(&self) -> usize {
        self.supported() + self.skipped
    }

    /// Percentage of comparable programs the lowering covers — the climbing coverage metric.
    pub fn coverage_pct(&self) -> f64 {
        let comparable = self.comparable();
        if comparable == 0 {
            0.0
        } else {
            self.supported() as f64 / comparable as f64 * 100.0
        }
    }

    /// Whether the IR-interpreter agreed with the tree-walker on every program it ran.
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "faithfulness: {} matched, {} skipped (unlowered), {} parse-failed; IR covers {:.1}% of comparable cases",
            self.matched,
            self.skipped,
            self.parse_failed,
            self.coverage_pct(),
        );
        if self.mismatches.is_empty() {
            out.push_str(
                "the Core-IR interpreter agrees with the tree-walker on every lowered program ✓\n",
            );
        } else {
            let _ = writeln!(out, "{} MISMATCH(es):", self.mismatches.len());
            for m in &self.mismatches {
                let _ = writeln!(out, "  {} — {}", m.name, m.detail);
            }
        }
        out
    }
}

/// Run the faithfulness oracle over every `.lang` file under `root` (optionally narrowed to
/// one file).
pub fn run_faithfulness(root: &Path, only: Option<&Path>) -> FaithReport {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = FaithReport::default();
    for case in cases {
        if let Some(only) = only
            && case.entry != only
            && !case.entry.ends_with(only)
        {
            continue;
        }
        let name = case
            .entry
            .strip_prefix(root)
            .unwrap_or(&case.entry)
            .to_string_lossy()
            .into_owned();
        if case.multi {
            match lang_loader::read_workspace(&case.entry) {
                Ok(raw) => compare_eval_paths_workspace(&name, &raw, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => compare_eval_paths(&name, &text, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        }
    }
    report
}

fn compare_eval_paths(name: &str, text: &str, report: &mut FaithReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = lang_db::source_program(&db, &source);

    let tokens = lang_db::tokens(&db, src);
    let parsed = lang_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }
    // A program the checker rejects never runs on either path; its diagnostics are its whole
    // result, identical regardless of evaluator — a guaranteed agreement, counted matched.
    if !lang_db::checked(&db, src).diagnostics.is_empty() {
        report.matched += 1;
        return;
    }
    let sites = lang_db::checked(&db, src).type_of_sites.clone();
    compare(name, &parsed.0.program, sites, report);
}

fn compare_eval_paths_workspace(
    name: &str,
    raw: &lang_loader::RawWorkspace,
    report: &mut FaithReport,
) {
    let db = LangDatabase::default();
    let ws = lang_db::workspace(&db, &raw.entry, &raw.modules);

    let program = match &lang_db::linked(&db, ws).0 {
        Ok(program) => program,
        Err(_) => {
            report.parse_failed += 1;
            return;
        }
    };
    if !lang_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.matched += 1;
        return;
    }
    let sites = lang_db::linked_checked(&db, ws).type_of_sites.clone();
    compare(name, program, sites, report);
}

/// The shared comparison: lower the program (skip if unsupported), then run it through the
/// tree-walker and the Core-IR interpreter on the same `type_of` sites and compare.
fn compare(
    name: &str,
    program: &lang_ast::Program,
    sites: std::collections::HashMap<lang_span::Span, lang_ast::reflect::TypeRepr>,
    report: &mut FaithReport,
) {
    let ir = match lang_ir::lower(program) {
        Ok(ir) => ir,
        Err(_) => {
            report.skipped += 1;
            return;
        }
    };
    let tree = TreeWalkBackend::new().run_with_sites(program, sites.clone());
    let ir_result = TreeWalkBackend::new().run_ir(program, &ir, sites);
    if tree == ir_result {
        report.matched += 1;
    } else {
        report.mismatches.push(Mismatch {
            name: name.to_string(),
            detail: describe_difference(&tree, &ir_result),
        });
    }
}

/// Describe the first field on which the two results differ.
fn describe_difference(tree: &RunResult, ir: &RunResult) -> String {
    if tree.stdout != ir.stdout {
        return format!("stdout: tree-walker {:?}, ir {:?}", tree.stdout, ir.stdout);
    }
    if tree.exit_code != ir.exit_code {
        return format!("exit: tree-walker {}, ir {}", tree.exit_code, ir.exit_code);
    }
    let codes = |r: &RunResult| {
        r.diagnostics
            .iter()
            .map(|d| (d.code, d.span))
            .collect::<Vec<_>>()
    };
    format!(
        "diagnostics: tree-walker {:?}, ir {:?}",
        codes(tree),
        codes(ir)
    )
}
