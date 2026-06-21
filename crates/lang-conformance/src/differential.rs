//! The differential oracle: run every corpus program through both backends and assert
//! they produce identical [`RunResult`]s.
//!
//! This is the spine of M1's runtime-first sequence. The M0 tree-walker (`TreeWalkBackend`)
//! is frozen as the reference; the M1 bytecode VM (`VmBackend`) must reproduce it exactly.
//! Because the VM only compiles a growing subset of the language, programs it cannot lower
//! yet are **skipped** (counted toward a climbing coverage percentage), never failed — so
//! the VM can land one slice at a time while every program it *does* compile is proven
//! identical to the oracle. Only observable output is compared (stdout / exit code /
//! diagnostic code+span), never internal value representation, which is exactly why the two
//! backends can use completely different value models.

use std::path::Path;

use lang_backend::{Backend, RunResult};
use lang_db::LangDatabase;
use lang_eval::TreeWalkBackend;
use lang_span::{Source, SourceId};
use lang_vm::VmBackend;

use crate::collect_lang_files;

/// A backend disagreement on one program: the field that differed.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub name: String,
    pub detail: String,
}

/// The outcome of a differential run over a corpus.
#[derive(Debug, Default)]
pub struct DiffReport {
    /// Programs the VM compiled and that matched the tree-walker.
    pub matched: usize,
    /// Programs outside the VM's current subset (skipped, not failed).
    pub skipped: usize,
    /// Programs that did not parse cleanly, so there is no eval-level result to compare.
    pub parse_failed: usize,
    /// Programs the VM compiled but whose result diverged — these are failures.
    pub mismatches: Vec<Mismatch>,
}

impl DiffReport {
    /// Programs the VM compiled (matched + diverged).
    pub fn supported(&self) -> usize {
        self.matched + self.mismatches.len()
    }

    /// Programs eligible for comparison (parsed cleanly): supported + skipped.
    pub fn comparable(&self) -> usize {
        self.supported() + self.skipped
    }

    /// Percentage of comparable programs the VM compiles — the climbing coverage metric.
    pub fn coverage_pct(&self) -> f64 {
        let comparable = self.comparable();
        if comparable == 0 {
            0.0
        } else {
            self.supported() as f64 / comparable as f64 * 100.0
        }
    }

    /// Whether the two backends agreed on every program the VM compiled.
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "differential: {} matched, {} skipped (unsupported), {} parse-failed; VM covers {:.1}% of comparable cases",
            self.matched,
            self.skipped,
            self.parse_failed,
            self.coverage_pct(),
        );
        if self.mismatches.is_empty() {
            out.push_str("backends agree on every compiled program ✓\n");
        } else {
            let _ = writeln!(out, "{} MISMATCH(es):", self.mismatches.len());
            for m in &self.mismatches {
                let _ = writeln!(out, "  {} — {}", m.name, m.detail);
            }
        }
        out
    }
}

/// Run the differential oracle over every `.lang` file under `root` (optionally narrowed to
/// one file).
pub fn run_differential(root: &Path, only: Option<&Path>) -> DiffReport {
    let mut files = Vec::new();
    collect_lang_files(root, &mut files);
    files.sort();

    let mut report = DiffReport::default();
    for path in files {
        if let Some(only) = only
            && path != only
            && !path.ends_with(only)
        {
            continue;
        }
        let name = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        match std::fs::read_to_string(&path) {
            Ok(text) => compare_backends(&name, &text, &mut report),
            Err(_) => report.parse_failed += 1,
        }
    }
    report
}

fn compare_backends(name: &str, text: &str, report: &mut DiffReport) {
    // Drive the whole pipeline through the salsa graph (M1.1): both backends consume artifacts
    // derived from the same `tokens`/`ast`/`bytecode` queries. The tree-walker runs the parsed
    // AST; the VM runs the `Module` the `bytecode` query produced — proving the query layer is
    // behavior-preserving, since any divergence would surface here.
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = lang_db::source_program(&db, &source);

    // A program that does not parse cleanly has no eval-level behavior to compare — that is
    // the normal conformance harness's job (the lexer/parser stages). Exclude it here.
    let tokens = lang_db::tokens(&db, src);
    let parsed = lang_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }

    // The type checker (M1.7) is a shared front-end: a program it rejects never reaches either
    // backend, and its diagnostics are the program's whole observable result — identical no
    // matter which backend would have run. So a type error is a guaranteed agreement, counted as
    // matched. (The corpus harness separately asserts the diagnostic's code+span.)
    if !lang_db::checked(&db, src).0.is_empty() {
        report.matched += 1;
        return;
    }

    let tree = TreeWalkBackend::new().run(&parsed.0.program);
    match &lang_db::bytecode(&db, src).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => {
            let vm = VmBackend::new().run_module(module);
            if vm == tree {
                report.matched += 1;
            } else {
                report.mismatches.push(Mismatch {
                    name: name.to_string(),
                    detail: describe_difference(&tree, &vm),
                });
            }
        }
    }
}

/// Describe the first field on which the two results differ.
fn describe_difference(tree: &RunResult, vm: &RunResult) -> String {
    if tree.stdout != vm.stdout {
        return format!("stdout: tree-walker {:?}, vm {:?}", tree.stdout, vm.stdout);
    }
    if tree.exit_code != vm.exit_code {
        return format!("exit: tree-walker {}, vm {}", tree.exit_code, vm.exit_code);
    }
    let codes = |r: &RunResult| {
        r.diagnostics
            .iter()
            .map(|d| (d.code, d.span))
            .collect::<Vec<_>>()
    };
    format!(
        "diagnostics: tree-walker {:?}, vm {:?}",
        codes(tree),
        codes(vm)
    )
}
