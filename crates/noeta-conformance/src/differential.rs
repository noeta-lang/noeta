//! The differential oracle: run every corpus program through both backends and assert
//! they produce identical [`RunResult`]s.
//!
//! This is the spine of M1's runtime-first sequence. The reference is the Core-IR interpreter
//! (`reference_run` — the tree-walk backend executing the drop-annotated Core IR); the M1
//! bytecode VM (`VmBackend`) must reproduce it exactly. Both consume the *same* lowered,
//! drop-annotated IR, so they are two independent executors (Rc tree-walk vs manual-RC register
//! bytecode) of one program — and agree on last-use destruction by construction. Because the VM
//! only compiles a growing subset of the language, programs it cannot lower yet are **skipped**
//! (counted toward a climbing coverage percentage), never failed. Only observable output is
//! compared (stdout / exit code / diagnostic code+span), never internal value representation,
//! which is exactly why the two backends can use completely different value models.
//!
//! (Through Phase 3 the reference was the AST tree-walker, with the IR interpreter validated
//! against it by the now-retired faithfulness oracle; Phase 4 promoted the IR interpreter to the
//! reference, since last-use destruction is expressible only on the IR.)

use std::path::Path;

use noeta_backend::RunResult;
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

use crate::collect_cases;
use crate::reference::reference_run;

/// A backend disagreement on one program: the field that differed.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub name: String,
    pub detail: String,
}

/// The outcome of a differential run over a corpus.
#[derive(Debug, Default)]
pub struct DiffReport {
    /// Programs both backends actually RAN, and on which they agreed. Strictly execution
    /// coverage: a program the checker rejected never ran and is counted in [`not_run`], not here.
    pub matched: usize,
    /// Every case excluded before the comparison, by reason (see [`NotRun`]).
    pub not_run: crate::NotRun,
    /// Cases the checker rejected that declare **no** `// expect: error` — a rejection nobody asked
    /// for. This is the teeth on the exclusion accounting: a corpus fixture invalidated by a
    /// language change stops running and silently leaves backend coverage, and every count that
    /// would reveal it (`matched`, then `checker_rejected`) moves by exactly one either way. Naming
    /// the cases turns that into a failure that says which file.
    pub unexpected_rejections: Vec<String>,
    /// Programs the VM compiled but whose result diverged — these are failures.
    pub mismatches: Vec<Mismatch>,
}

impl DiffReport {
    /// Programs the VM compiled (matched + diverged).
    pub fn supported(&self) -> usize {
        self.matched + self.mismatches.len()
    }

    /// Programs eligible for comparison: those that reached the backend gate at all — the ones
    /// that ran, plus the ones the VM could not compile. Cases excluded earlier (unparseable,
    /// unlinkable, checker-rejected) were never candidates and must not dilute the metric.
    pub fn comparable(&self) -> usize {
        self.supported() + self.not_run.unsupported
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

    /// Whether the two backends agreed on every program the VM ran, and no case dropped out of
    /// coverage without declaring it.
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty() && self.unexpected_rejections.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "differential: {} ran and agreed, {} not run ({}); VM covers {:.1}% of comparable cases",
            self.matched,
            self.not_run.total(),
            self.not_run.to_human(),
            self.coverage_pct(),
        );
        if !self.unexpected_rejections.is_empty() {
            let _ = writeln!(
                out,
                "{} case(s) REJECTED BY THE CHECKER WITHOUT DECLARING AN ERROR — they no longer \
                 run, so they cover neither backend. Either fix the fixture or declare the \
                 diagnostic with `// expect: error <CODE> at <line>:<col>`:",
                self.unexpected_rejections.len()
            );
            for name in &self.unexpected_rejections {
                let _ = writeln!(out, "  {name}");
            }
        }
        if !self.mismatches.is_empty() {
            let _ = writeln!(out, "{} MISMATCH(es):", self.mismatches.len());
            for m in &self.mismatches {
                let _ = writeln!(out, "  {} — {}", m.name, m.detail);
            }
        }
        if self.ok() {
            out.push_str("backends agree on every compiled program ✓\n");
        }
        out
    }
}

/// Run the differential oracle over every `.noe` file under `root` (optionally narrowed to
/// one file).
pub fn run_differential(root: &Path, only: Option<&Path>) -> DiffReport {
    crate::ensure_std_registry();
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = DiffReport::default();
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
            // A multi-file fixture flows through the salsa module graph (M1.9.3): its sources
            // become a `Workspace`, the `linked` query merges them, and both backends consume the
            // workspace queries — the multi-file analogue of the single-file path below.
            match crate::read_case_workspace(&case.entry) {
                Ok(raw) => compare_backends_workspace(&name, &raw, &case.entry, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => compare_backends(&name, &text, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        }
    }
    report
}

/// Record a checker rejection that the case's own header does not account for. A case carrying
/// `// expect: error …` is a deliberate negative fixture and belongs in the exclusion tally
/// silently; one without is a fixture that stopped compiling, which is what this catches.
fn note_rejection(name: &str, text: &str, report: &mut DiffReport) {
    let declares_error = crate::Expectations::parse(text)
        .map(|e| !e.errors.is_empty())
        .unwrap_or(false);
    if !declares_error {
        report.unexpected_rejections.push(name.to_string());
    }
}

fn compare_backends(name: &str, text: &str, report: &mut DiffReport) {
    // Drive the whole pipeline through the salsa graph (M1.1): both backends consume artifacts
    // derived from the same `tokens`/`ast`/`bytecode` queries. The tree-walker runs the parsed
    // AST; the VM runs the `Module` the `bytecode` query produced — proving the query layer is
    // behavior-preserving, since any divergence would surface here.
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    // A program that does not parse cleanly has no eval-level behavior to compare — that is
    // the normal conformance harness's job (the lexer/parser stages). Exclude it here.
    let tokens = noeta_db::tokens(&db, src);
    let parsed = noeta_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.not_run.parse_failed += 1;
        return;
    }

    // The type checker (M1.7) is a shared front-end: a program it rejects never reaches either
    // backend, and its diagnostics are the program's whole observable result — identical no
    // matter which backend would have run. So a type error is a guaranteed agreement, counted as
    // matched. (The corpus harness separately asserts the diagnostic's code+span.)
    if crate::has_error(&noeta_db::checked(&db, src).diagnostics) {
        report.not_run.checker_rejected += 1;
        note_rejection(name, text, report);
        return;
    }

    // Thread the checker's site bundle (already memoized by the gate above) into the reference,
    // which runs the same drop-annotated Core IR the VM compiles — so the differential
    // cross-checks two independent executors of one IR.
    let checked = noeta_db::checked(&db, src);
    let tree = reference_run(&parsed.0.program, checked.sites.clone());
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => report.not_run.unsupported += 1,
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

/// Compare both backends on a multi-file fixture, driven through the salsa module graph (M1.9.3):
/// the workspace's sources become `SourceProgram` inputs, the `linked` query resolves+merges them,
/// and the backends consume the workspace queries. A load failure (entry parse error, E0019,
/// E0020) has no eval-level behavior to compare and is excluded, like a single-file parse failure.
/// The checker is the shared front-end (a rejected program is a guaranteed agreement, counted
/// matched); the tree-walker runs the merged AST, the VM the compiled `Module`.
fn compare_backends_workspace(
    name: &str,
    raw: &noeta_loader::RawWorkspace,
    entry: &std::path::Path,
    report: &mut DiffReport,
) {
    let db = LangDatabase::default();
    // A case with package subdirectories is a *dependency graph*, so it becomes a workspace WITH
    // deps — otherwise its `use <pkg>.…` resolves to nothing, the link fails, and the case would
    // sit silently in the "not run" tally rather than being compared. Package-less cases (every
    // pre-existing one) take the same deps-free workspace they always did.
    let deps = crate::dep_sources(entry, (raw.modules.len() + 1) as u32);
    let ws = if deps.is_empty() {
        noeta_db::workspace(
            &db,
            &raw.entry,
            &raw.modules,
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    } else {
        // No `@name` tables: the corpus's dependency graph is synthesized from the case's
        // subdirectories (`crate::dep_sources`), not from a `noeta.toml`, so no package binds a
        // `[tiers]`/`[directives]` local name — an empty `PackageUses` is behavior-identical.
        noeta_db::workspace_with_deps(
            &db,
            &raw.entry,
            &raw.modules,
            &deps,
            &noeta_span::PackageUses::new(),
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    };

    let program = match &noeta_db::linked(&db, ws).program {
        Ok(program) => program,
        Err(_) => {
            report.not_run.link_failed += 1;
            return;
        }
    };
    if crate::has_error(&noeta_db::linked_checked(&db, ws).diagnostics) {
        report.not_run.checker_rejected += 1;
        note_rejection(name, raw.entry.text(), report);
        return;
    }
    let checked = noeta_db::linked_checked(&db, ws);
    let tree = reference_run(program, checked.sites.clone());
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.not_run.unsupported += 1,
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
    if tree.stderr != vm.stderr {
        return format!("stderr: tree-walker {:?}, vm {:?}", tree.stderr, vm.stderr);
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
