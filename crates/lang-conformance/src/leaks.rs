//! The leak oracle: run every corpus program through both backends and assert each reclaims
//! all of its heap before returning (architecture §0/§5).
//!
//! The measuring stick is a per-isolate live-object counter in each backend
//! ([`lang_value::live_count`] for the VM heap, [`lang_eval::live_count`] for the tree-walker's
//! `Rc` aggregates). Because the harness runs many programs on one thread, residency is measured
//! as a **per-program delta**: the count before a run subtracted from the count after. A clean
//! program returns to its starting count (delta 0); a cycle leak or a missed release leaves a
//! positive residual, which is a test failure — turning "did we leak?" into a CI gate regardless
//! of which collector is wired.
//!
//! A program the VM cannot compile yet is measured on the tree-walker only (the VM's residual is
//! simply not sampled), exactly as the differential skips it.

use std::path::Path;

use lang_db::LangDatabase;
use lang_span::{Source, SourceId};
use lang_vm::VmBackend;

use crate::collect_cases;
use crate::reference::reference_run;

/// One program's nonzero residency on one backend.
#[derive(Debug, Clone)]
pub struct Leak {
    pub name: String,
    pub backend: &'static str,
    /// Objects still live after the program returned and tore down (should be zero).
    pub residual: i64,
}

/// The outcome of a leak-oracle run over a corpus.
#[derive(Debug, Default)]
pub struct LeakReport {
    /// Programs measured on the tree-walker (checked cleanly).
    pub eval_measured: usize,
    /// Programs measured on the VM (compiled to bytecode).
    pub vm_measured: usize,
    /// Programs whose result was a parse/load failure, so no backend ran.
    pub parse_failed: usize,
    /// Every backend×program pair that ended with live residency.
    pub leaks: Vec<Leak>,
}

impl LeakReport {
    /// Whether every measured program reclaimed fully on both backends.
    pub fn ok(&self) -> bool {
        self.leaks.is_empty()
    }

    /// Residual leaks on a single backend.
    pub fn leaks_on(&self, backend: &str) -> Vec<&Leak> {
        self.leaks.iter().filter(|l| l.backend == backend).collect()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "leak oracle: {} programs on the tree-walker, {} on the VM ({} parse/load-failed, not run)",
            self.eval_measured, self.vm_measured, self.parse_failed,
        );
        if self.leaks.is_empty() {
            out.push_str("residency 0 at clean exit on every program, both backends ✓\n");
        } else {
            let _ = writeln!(
                out,
                "{} LEAK(s) — live residency at clean exit:",
                self.leaks.len()
            );
            for l in &self.leaks {
                let _ = writeln!(
                    out,
                    "  [{}] {} — {} object(s) still live",
                    l.backend, l.name, l.residual
                );
            }
        }
        out
    }
}

/// Run the leak oracle over every `.lang` file under `root` (optionally narrowed to one file).
pub fn run_leak_check(root: &Path, only: Option<&Path>) -> LeakReport {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = LeakReport::default();
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
                Ok(raw) => measure_workspace(&name, &raw, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => measure_single(&name, &text, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        }
    }
    report
}

/// Measure one single-file program on both backends, driving the same salsa graph the differential
/// uses so the inputs are identical.
fn measure_single(name: &str, text: &str, report: &mut LeakReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = lang_db::source_program(&db, &source);

    let tokens = lang_db::tokens(&db, src);
    let parsed = lang_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }
    // A program the checker rejects never runs a backend — its diagnostics are its whole result.
    if !lang_db::checked(&db, src).diagnostics.is_empty() {
        return;
    }
    let checked = lang_db::checked(&db, src);
    let sites = checked.type_of_sites.clone();
    let packed = checked.packed_list_sites.clone();
    let index_fields = checked.index_field_sites.clone();

    // Reference (Core-IR interpreter): measure the live `Rc`-aggregate delta across a full run.
    let before = lang_eval::live_count();
    let _ = reference_run(
        &parsed.0.program,
        sites,
        packed,
        index_fields,
        checked.ext_call_sites.clone(),
        checked.for_stream_sites.clone(),
        checked.width_sites.clone(),
        checked.construction_sites.clone(),
        &checked.destructor_relevance,
    );
    record(report, name, "eval", lang_eval::live_count() - before);
    report.eval_measured += 1;

    // VM: measure the live heap-object delta, but only when the program compiles.
    if let Ok(module) = &lang_db::bytecode(&db, src).0 {
        let before = lang_value::live_count() as i64;
        let _ = VmBackend::new().run_module(module);
        record(report, name, "vm", lang_value::live_count() as i64 - before);
        report.vm_measured += 1;
    }
}

/// Measure one multi-file fixture on both backends (the workspace analogue of [`measure_single`]).
fn measure_workspace(name: &str, raw: &lang_loader::RawWorkspace, report: &mut LeakReport) {
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
        return;
    }
    let checked = lang_db::linked_checked(&db, ws);
    let sites = checked.type_of_sites.clone();
    let packed = checked.packed_list_sites.clone();
    let index_fields = checked.index_field_sites.clone();

    let before = lang_eval::live_count();
    let _ = reference_run(
        program,
        sites,
        packed,
        index_fields,
        checked.ext_call_sites.clone(),
        checked.for_stream_sites.clone(),
        checked.width_sites.clone(),
        checked.construction_sites.clone(),
        &checked.destructor_relevance,
    );
    record(report, name, "eval", lang_eval::live_count() - before);
    report.eval_measured += 1;

    if let Ok(module) = &lang_db::linked_bytecode(&db, ws).0 {
        let before = lang_value::live_count() as i64;
        let _ = VmBackend::new().run_module(module);
        record(report, name, "vm", lang_value::live_count() as i64 - before);
        report.vm_measured += 1;
    }
}

/// Record a nonzero residual as a leak.
fn record(report: &mut LeakReport, name: &str, backend: &'static str, residual: i64) {
    if residual != 0 {
        report.leaks.push(Leak {
            name: name.to_string(),
            backend,
            residual,
        });
    }
}
