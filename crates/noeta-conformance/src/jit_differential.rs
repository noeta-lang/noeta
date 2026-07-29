//! The JIT differential oracle (milestone P-JIT): run every corpus program through the interpreter
//! (tier 0) *and* through the forced tier-1 JIT, and assert they produce byte-identical
//! [`RunResult`]s — plus that the forced-JIT run leaves zero heap residency (refcount parity under
//! native code).
//!
//! The eval↔interpreter differential ([`crate::differential`]) never exercises the JIT — the JIT is
//! a real-host accelerator, invisible to the deterministic sandbox path — so it gets this **own**
//! gate, mirroring the existing discipline. Both runs here use the same deterministic
//! [`noeta_stdlib::SandboxHost`] (baked into `run_module` / `run_module_jit`), so the *only* variable
//! is tier 0 vs tier 1; any divergence is a JIT bug. A program the VM cannot compile is skipped
//! (never run on either tier). This whole module is `jit`-feature-gated — without it, Cranelift is
//! not even a dependency.

use std::path::Path;

use noeta_backend::RunResult;
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

use crate::collect_cases;
use crate::differential::Mismatch;
use crate::leaks::Leak;

/// The outcome of a JIT differential run over a corpus.
#[derive(Debug, Default)]
pub struct JitDiffReport {
    /// Programs both tiers actually RAN, and on which they agreed. Strictly execution coverage:
    /// a program the checker rejected never ran and is counted in [`not_run`], not here.
    pub matched: usize,
    /// Every case excluded before the comparison, by reason (see [`NotRun`]).
    pub not_run: crate::NotRun,
    /// Checker-rejected cases declaring no `// expect: error` — see the eval differential's field
    /// of the same name. A fixture that stops compiling leaves tier coverage silently otherwise.
    pub unexpected_rejections: Vec<String>,
    /// Total prototypes compiled to *real native code* across the corpus — the JIT-coverage number.
    pub native_protos: usize,
    /// Total prototypes compiled at all (native + bail stubs).
    pub compiled_protos: usize,
    /// Programs where tier 1 diverged from tier 0 — result failures.
    pub mismatches: Vec<Mismatch>,
    /// Programs that left nonzero heap residency under the forced-JIT run — refcount failures.
    pub leaks: Vec<Leak>,
}

impl JitDiffReport {
    /// Programs the VM compiled (matched + diverged).
    pub fn supported(&self) -> usize {
        self.matched + self.mismatches.len()
    }

    /// Whether tier 1 agreed with tier 0 and leaked nothing on every compiled program.
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty() && self.leaks.is_empty() && self.unexpected_rejections.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "jit-differential: {} ran and agreed, {} not run ({}); {}/{} prototypes native (rest bail stubs)",
            self.matched,
            self.not_run.total(),
            self.not_run.to_human(),
            self.native_protos,
            self.compiled_protos,
        );
        if !self.unexpected_rejections.is_empty() {
            let _ = writeln!(
                out,
                "{} case(s) REJECTED BY THE CHECKER WITHOUT DECLARING AN ERROR — they no longer \
                 run, so they cover neither tier. Either fix the fixture or declare the diagnostic \
                 with `// expect: error <CODE> at <line>:<col>`:",
                self.unexpected_rejections.len()
            );
            for name in &self.unexpected_rejections {
                let _ = writeln!(out, "  {name}");
            }
        }
        if self.ok() {
            out.push_str(
                "tier 1 agrees with tier 0 and leaks nothing on every compiled program ✓\n",
            );
        } else {
            if !self.mismatches.is_empty() {
                let _ = writeln!(out, "{} RESULT MISMATCH(es):", self.mismatches.len());
                for m in &self.mismatches {
                    let _ = writeln!(out, "  {} — {}", m.name, m.detail);
                }
            }
            if !self.leaks.is_empty() {
                let _ = writeln!(out, "{} LEAK(s) under forced JIT:", self.leaks.len());
                for l in &self.leaks {
                    let _ = writeln!(out, "  {} — residual {}", l.name, l.residual);
                }
            }
        }
        out
    }
}

/// Run the JIT differential oracle over every `.noe` file under `root` (optionally narrowed to one
/// file).
pub fn run_jit_differential(root: &Path, only: Option<&Path>) -> JitDiffReport {
    crate::ensure_std_registry();
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = JitDiffReport::default();
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
            match noeta_loader::read_workspace(&case.entry, None) {
                Ok(raw) => compare_tiers_workspace(&name, &raw, &case.entry, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => compare_tiers(&name, &text, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        }
    }
    report
}

/// Compare tier 0 vs tier 1 on one single-file program. Gates identically to the eval differential
/// (parse-clean, checker-accepted, VM-compilable), then runs the interpreter and the forced JIT and
/// compares — measuring the forced-JIT run's heap residency in the same pass.
/// Record a checker rejection the case's own header does not account for — see the eval
/// differential's `note_rejection`.
fn note_rejection(name: &str, text: &str, report: &mut JitDiffReport) {
    let declares_error = crate::Expectations::parse(text)
        .map(|e| !e.errors.is_empty())
        .unwrap_or(false);
    if !declares_error {
        report.unexpected_rejections.push(name.to_string());
    }
}

fn compare_tiers(name: &str, text: &str, report: &mut JitDiffReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    let tokens = noeta_db::tokens(&db, src);
    let parsed = noeta_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.not_run.parse_failed += 1;
        return;
    }
    // A checker-rejected program never runs a backend — its diagnostics are its whole result, the
    // same on either tier, so it cannot disagree. That makes it a trivial agreement, NOT tier
    // coverage: counting it as matched (as this and the eval differential both used to) inflated
    // the headline and let a fixture that stopped compiling slip from one side of it to the other
    // without moving the number.
    if !noeta_db::checked(&db, src).diagnostics.is_empty() {
        report.not_run.checker_rejected += 1;
        note_rejection(name, text, report);
        return;
    }
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => report.not_run.unsupported += 1,
        Ok(module) => run_and_compare(name, module, report),
    }
}

/// The workspace analogue of [`compare_tiers`] for a multi-file fixture.
fn compare_tiers_workspace(
    name: &str,
    raw: &noeta_loader::RawWorkspace,
    entry: &std::path::Path,
    report: &mut JitDiffReport,
) {
    let db = LangDatabase::default();
    // A case with package subdirectories is a *dependency graph*, so it becomes a workspace WITH
    // deps — the same construction the eval differential makes, and for the same reason: without
    // them its `use <pkg>.…` resolves to nothing and the case is rejected by the checker, covering
    // neither tier while looking like an ordinary "not run". Package-less cases take the deps-free
    // workspace they always did.
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
        noeta_db::workspace_with_deps(
            &db,
            &raw.entry,
            &raw.modules,
            &deps,
            noeta_lexer::Edition::DEFAULT,
            &raw.paths,
        )
    };

    if noeta_db::linked(&db, ws).program.is_err() {
        report.not_run.link_failed += 1;
        return;
    }
    if !noeta_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.not_run.checker_rejected += 1;
        note_rejection(name, raw.entry.text(), report);
        return;
    }
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.not_run.unsupported += 1,
        Ok(module) => run_and_compare(name, module, report),
    }
}

/// Run `module` on both tiers and fold the result comparison + the forced-JIT leak measurement into
/// `report`. Shared by the single-file and workspace paths.
fn run_and_compare(name: &str, module: &noeta_bytecode::Module, report: &mut JitDiffReport) {
    let interp = VmBackend::new().run_module(module);

    // Forced-JIT run, measuring heap residency around it (refcount parity under native code): the
    // integer fast path leaves every register an immediate, so residency must match the interpreter.
    let before = noeta_value::live_count() as i64;
    noeta_value::reset_refcount_anomalies();
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(module);
    let residual = noeta_value::live_count() as i64 - before;
    // A refcount anomaly (skipped release/retain) is invisible to end-of-run residency — the
    // teardown's final backup sweep reclaims orphans and cycles alike — so the teardown measures
    // it separately (see `noeta_gc::count_refcount_anomalies`) and the oracle asserts it here.
    let anomalies = noeta_value::refcount_anomalies() as i64;

    report.native_protos += stats.native;
    report.compiled_protos += stats.compiled;

    if interp == jit {
        report.matched += 1;
    } else {
        report.mismatches.push(Mismatch {
            name: name.to_string(),
            detail: describe_difference(&interp, &jit),
        });
    }
    if residual != 0 {
        report.leaks.push(Leak {
            name: name.to_string(),
            backend: "jit",
            residual,
        });
    }
    if anomalies != 0 {
        report.leaks.push(Leak {
            name: name.to_string(),
            backend: "jit (refcount)",
            residual: anomalies,
        });
    }
}

/// Describe the first field on which the two tiers' results differ.
fn describe_difference(interp: &RunResult, jit: &RunResult) -> String {
    if interp.stdout != jit.stdout {
        return format!("stdout: interp {:?}, jit {:?}", interp.stdout, jit.stdout);
    }
    if interp.stderr != jit.stderr {
        return format!("stderr: interp {:?}, jit {:?}", interp.stderr, jit.stderr);
    }
    if interp.exit_code != jit.exit_code {
        return format!("exit: interp {}, jit {}", interp.exit_code, jit.exit_code);
    }
    let codes = |r: &RunResult| {
        r.diagnostics
            .iter()
            .map(|d| (d.code, d.span))
            .collect::<Vec<_>>()
    };
    format!(
        "diagnostics: interp {:?}, jit {:?}",
        codes(interp),
        codes(jit)
    )
}
