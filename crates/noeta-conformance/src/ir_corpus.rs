//! The **IR-corpus sweep**: run every parse+check-clean corpus program through the Core-IR
//! interpreter (the reference) and report coverage.
//!
//! ## History — the retired faithfulness oracle
//!
//! Through Phase 3 of the memory-management migration this module was the *faithfulness*
//! oracle: it ran every program through both the AST tree-walker (the then-frozen reference)
//! and the new Core-IR interpreter and asserted byte-identical [`RunResult`]s — the transitional
//! gate that established the IR interpreter as a trustworthy second representation before any
//! later phase transformed the IR.
//!
//! Phase 4 makes destruction observable at last use, which the AST tree-walker cannot reproduce
//! (it has no liveness; it fires destructors only at global teardown). So the IR interpreter was
//! **promoted to the reference**, and the two backends now intentionally diverge on destructor
//! timing — an equality assertion against the AST walker would be wrong. The faithfulness
//! *comparison* is therefore retired; its job (validating the IR interpreter through Phase 3) is
//! done, and the live cross-check is now the differential (IR interpreter vs the VM, two
//! independent executors of one IR).
//!
//! What remains useful is the **sweep itself**: running every lowered program through the IR
//! interpreter. It (a) gates that the lowering stays **total** over the corpus (`skipped == 0`),
//! and (b) is the execution harness the drop-audit wraps to check the static-≤-dynamic last-use
//! property. This module keeps exactly that, with the comparison removed.

use std::path::Path;

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};

use crate::collect_cases;
use crate::reference::reference_run;

/// The outcome of an IR-corpus sweep.
#[derive(Debug, Default)]
pub struct IrCorpusReport {
    /// Programs that lowered and ran through the IR interpreter.
    pub ran: usize,
    /// Programs outside the IR lowering's current subset (skipped, not failed).
    pub skipped: usize,
    /// Programs that did not parse/check cleanly, so there is no eval-level run.
    pub parse_failed: usize,
}

impl IrCorpusReport {
    /// Programs eligible for the IR path (parsed + checked cleanly): ran + skipped.
    pub fn comparable(&self) -> usize {
        self.ran + self.skipped
    }

    /// Percentage of comparable programs the lowering covers — the climbing coverage metric.
    pub fn coverage_pct(&self) -> f64 {
        let comparable = self.comparable();
        if comparable == 0 {
            0.0
        } else {
            self.ran as f64 / comparable as f64 * 100.0
        }
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        format!(
            "ir-corpus: {} ran, {} skipped (unlowered), {} parse-failed; IR covers {:.1}% of comparable cases\n",
            self.ran,
            self.skipped,
            self.parse_failed,
            self.coverage_pct(),
        )
    }
}

/// Run every `.noe` file under `root` (optionally narrowed to one file) through the Core-IR
/// interpreter, reporting coverage. Side effects (e.g. an active drop-audit) are observed by the
/// caller; this returns only the coverage tally.
pub fn run_ir_corpus(root: &Path, only: Option<&Path>) -> IrCorpusReport {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = IrCorpusReport::default();
    for case in cases {
        if let Some(only) = only
            && case.entry != only
            && !case.entry.ends_with(only)
        {
            continue;
        }
        if case.multi {
            match noeta_loader::read_workspace(&case.entry) {
                Ok(raw) => run_workspace(&raw, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => run_single(&text, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        }
    }
    report
}

fn run_single(text: &str, report: &mut IrCorpusReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "ir-corpus", text);
    let src = noeta_db::source_program(&db, &source);

    let tokens = noeta_db::tokens(&db, src);
    let parsed = noeta_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }
    // A program the checker rejects never runs; its diagnostics are its whole result.
    if !noeta_db::checked(&db, src).diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }
    let checked = noeta_db::checked(&db, src);
    run(&parsed.0.program, checked, report);
}

fn run_workspace(raw: &noeta_loader::RawWorkspace, report: &mut IrCorpusReport) {
    let db = LangDatabase::default();
    let ws = noeta_db::workspace(&db, &raw.entry, &raw.modules);

    let program = match &noeta_db::linked(&db, ws).0 {
        Ok(program) => program,
        Err(_) => {
            report.parse_failed += 1;
            return;
        }
    };
    if !noeta_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }
    let checked = noeta_db::linked_checked(&db, ws);
    run(program, checked, report);
}

/// Run one program through the reference (the Core-IR interpreter), counting it as `ran` when the
/// lowering supports it and `skipped` otherwise. The result is discarded — only the side effects
/// (drop-audit) and the coverage tally matter here.
fn run(program: &noeta_ast::Program, checked: &noeta_db::Checked, report: &mut IrCorpusReport) {
    // Mirror `reference_run`'s lowering decision so the skip count reflects real lowering coverage
    // (a fallback to the AST walker would hide an unlowered program).
    match noeta_ir::lower(program) {
        Ok(_) => {
            let _ = reference_run(
                program,
                checked.type_of_sites.clone(),
                checked.packed_list_sites.clone(),
                checked.index_field_sites.clone(),
                checked.ext_call_sites.clone(),
                checked.for_stream_sites.clone(),
                checked.width_sites.clone(),
                checked.f32_literal_sites.clone(),
                checked.construction_sites.clone(),
                checked.handle_sites.clone(),
        checked.bound_handle_sites.clone(),
                &checked.destructor_relevance,
            );
            report.ran += 1;
        }
        Err(_) => report.skipped += 1,
    }
}
