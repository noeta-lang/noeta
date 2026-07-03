//! The JIT differential oracle (milestone P-JIT): run every corpus program through the interpreter
//! (tier 0) *and* through the forced tier-1 JIT, and assert they produce byte-identical
//! [`RunResult`]s — plus that the forced-JIT run leaves zero heap residency (refcount parity under
//! native code).
//!
//! The eval↔interpreter differential ([`crate::differential`]) never exercises the JIT — the JIT is
//! a real-host accelerator, invisible to the deterministic sandbox path — so it gets this **own**
//! gate, mirroring the existing discipline. Both runs here use the same deterministic
//! [`lang_stdlib::SandboxHost`] (baked into `run_module` / `run_module_jit`), so the *only* variable
//! is tier 0 vs tier 1; any divergence is a JIT bug. A program the VM cannot compile is skipped
//! (never run on either tier). This whole module is `jit`-feature-gated — without it, Cranelift is
//! not even a dependency.

use std::path::Path;

use lang_backend::RunResult;
use lang_db::LangDatabase;
use lang_span::{Source, SourceId};
use lang_vm::VmBackend;

use crate::collect_cases;
use crate::differential::Mismatch;
use crate::leaks::Leak;

/// The outcome of a JIT differential run over a corpus.
#[derive(Debug, Default)]
pub struct JitDiffReport {
    /// Programs the VM compiled and on which tier 0 and tier 1 agreed.
    pub matched: usize,
    /// Programs outside the VM's current subset (skipped — neither tier runs them).
    pub skipped: usize,
    /// Programs that did not parse cleanly, so there is no result to compare.
    pub parse_failed: usize,
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
        self.mismatches.is_empty() && self.leaks.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "jit-differential: {} matched, {} skipped (unsupported), {} parse-failed; {}/{} prototypes native (rest bail stubs)",
            self.matched, self.skipped, self.parse_failed, self.native_protos, self.compiled_protos,
        );
        if self.mismatches.is_empty() && self.leaks.is_empty() {
            out.push_str("tier 1 agrees with tier 0 and leaks nothing on every compiled program ✓\n");
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

/// Run the JIT differential oracle over every `.lang` file under `root` (optionally narrowed to one
/// file).
pub fn run_jit_differential(root: &Path, only: Option<&Path>) -> JitDiffReport {
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
            match lang_loader::read_workspace(&case.entry) {
                Ok(raw) => compare_tiers_workspace(&name, &raw, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => compare_tiers(&name, &text, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        }
    }
    report
}

/// Compare tier 0 vs tier 1 on one single-file program. Gates identically to the eval differential
/// (parse-clean, checker-accepted, VM-compilable), then runs the interpreter and the forced JIT and
/// compares — measuring the forced-JIT run's heap residency in the same pass.
fn compare_tiers(name: &str, text: &str, report: &mut JitDiffReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = lang_db::source_program(&db, &source);

    let tokens = lang_db::tokens(&db, src);
    let parsed = lang_db::ast(&db, src);
    if !tokens.0.diagnostics.is_empty() || !parsed.0.diagnostics.is_empty() {
        report.parse_failed += 1;
        return;
    }
    // A checker-rejected program never runs a backend — its diagnostics are its whole result, the
    // same on either tier. Count it matched (like the eval differential does).
    if !lang_db::checked(&db, src).diagnostics.is_empty() {
        report.matched += 1;
        return;
    }
    match &lang_db::bytecode(&db, src).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => run_and_compare(name, module, report),
    }
}

/// The workspace analogue of [`compare_tiers`] for a multi-file fixture.
fn compare_tiers_workspace(name: &str, raw: &lang_loader::RawWorkspace, report: &mut JitDiffReport) {
    let db = LangDatabase::default();
    let ws = lang_db::workspace(&db, &raw.entry, &raw.modules);

    if lang_db::linked(&db, ws).0.is_err() {
        report.parse_failed += 1;
        return;
    }
    if !lang_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.matched += 1;
        return;
    }
    match &lang_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => run_and_compare(name, module, report),
    }
}

/// Run `module` on both tiers and fold the result comparison + the forced-JIT leak measurement into
/// `report`. Shared by the single-file and workspace paths.
fn run_and_compare(name: &str, module: &lang_bytecode::Module, report: &mut JitDiffReport) {
    let interp = VmBackend::new().run_module(module);

    // Forced-JIT run, measuring heap residency around it (refcount parity under native code): the
    // integer fast path leaves every register an immediate, so residency must match the interpreter.
    let before = lang_value::live_count() as i64;
    let (jit, stats) = VmBackend::new().run_module_jit_with_stats(module);
    let residual = lang_value::live_count() as i64 - before;

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
}

/// Describe the first field on which the two tiers' results differ.
fn describe_difference(interp: &RunResult, jit: &RunResult) -> String {
    if interp.stdout != jit.stdout {
        return format!("stdout: interp {:?}, jit {:?}", interp.stdout, jit.stdout);
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
