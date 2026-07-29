//! The bundle oracle (P-AOT L1.0 + L1.3): compile every corpus program to a
//! [`noeta_bytecode::Module`] and prove it survives serialization on two levels.
//!
//! - **L1.0 — structural round-trip.** The module encodes→decodes→re-encodes byte-for-byte, so the
//!   whole bytecode graph (ops, constants, shapes, method/derive tables, the reflection artifact)
//!   serializes losslessly. `Module` has no `PartialEq` (its ops carry none), so byte stability is
//!   the structural equality check.
//! - **L1.3 — execution differential.** The *decoded* module, run on the deterministic
//!   [`noeta_stdlib::SandboxHost`], produces a byte-identical [`RunResult`] to the source-compiled
//!   module. This is the real ship-safety gate: a `.noeb` runs exactly like its source. It mirrors
//!   the backend differential's discipline (same sandbox, so the only variable is
//!   "compiled-from-source vs decoded-from-bytes"), keeping `0 skipped`.

use std::path::Path;

use noeta_backend::RunResult;
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_vm::VmBackend;

use crate::collect_cases;

/// The outcome of a bundle round-trip run over a corpus.
#[derive(Debug, Default)]
pub struct BundleReport {
    /// Programs the VM compiled and whose module round-tripped byte-for-byte.
    pub matched: usize,
    /// Programs outside the VM's current subset (no module to serialize).
    pub not_run: crate::NotRun,
    /// Programs whose module failed to decode, or re-encoded to different bytes — a serde bug.
    pub failures: Vec<BundleFailure>,
}

/// One program whose module did not survive the round-trip.
#[derive(Debug)]
pub struct BundleFailure {
    pub name: String,
    pub detail: String,
}

impl BundleReport {
    /// Whether every compiled module round-tripped losslessly.
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// A human-readable summary.
    pub fn to_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "bundle round-trip: {} ran and round-tripped, {} not run ({})",
            self.matched,
            self.not_run.total(),
            self.not_run.to_human(),
        );
        if self.failures.is_empty() {
            out.push_str("every compiled module round-trips losslessly and runs identically ✓\n");
        } else {
            let _ = writeln!(out, "{} BUNDLE FAILURE(s):", self.failures.len());
            for f in &self.failures {
                let _ = writeln!(out, "  {} — {}", f.name, f.detail);
            }
        }
        out
    }
}

/// Round-trip every `.noe` file under `root` (optionally narrowed to one file).
pub fn run_bundle_roundtrip(root: &Path, only: Option<&Path>) -> BundleReport {
    crate::ensure_std_registry();
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = BundleReport::default();
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
            match crate::read_case_workspace(&case.entry) {
                Ok(raw) => roundtrip_workspace(&name, &raw, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => roundtrip_single(&name, &text, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        }
    }
    report
}

/// Check `module` on both levels — byte round-trip (L1.0) and execution differential (L1.3) —
/// folding the first failure into `report`. Shared by the single-file and workspace paths.
fn check(name: &str, module: &noeta_bytecode::Module, report: &mut BundleReport) {
    let blob = module.encode();
    let decoded = match noeta_bytecode::Module::decode(&blob) {
        Ok(d) => d,
        Err(e) => {
            report.failures.push(BundleFailure {
                name: name.to_string(),
                detail: format!("decode failed: {e}"),
            });
            return;
        }
    };
    // L1.0: byte stability proves a lossless structural round-trip.
    if decoded.encode() != blob {
        report.failures.push(BundleFailure {
            name: name.to_string(),
            detail: "re-encode produced different bytes (lossy round-trip)".to_string(),
        });
        return;
    }
    // L1.3: the decoded module must execute identically to the source-compiled one. Both run on the
    // deterministic sandbox, so any divergence is a serialization fidelity bug.
    let from_source = VmBackend::new().run_module(module);
    let from_bundle = VmBackend::new().run_module(&decoded);
    if let Some(detail) = describe_run_difference(&from_source, &from_bundle) {
        report.failures.push(BundleFailure {
            name: name.to_string(),
            detail,
        });
        return;
    }
    report.matched += 1;
}

/// The first field on which a source-run and a bundle-run diverge, or `None` if identical.
fn describe_run_difference(source: &RunResult, bundle: &RunResult) -> Option<String> {
    if source.stdout != bundle.stdout {
        return Some(format!(
            "stdout: source {:?}, bundle {:?}",
            source.stdout, bundle.stdout
        ));
    }
    if source.exit_code != bundle.exit_code {
        return Some(format!(
            "exit: source {}, bundle {}",
            source.exit_code, bundle.exit_code
        ));
    }
    let codes = |r: &RunResult| {
        r.diagnostics
            .iter()
            .map(|d| (d.code, d.span))
            .collect::<Vec<_>>()
    };
    if codes(source) != codes(bundle) {
        return Some(format!(
            "diagnostics: source {:?}, bundle {:?}",
            codes(source),
            codes(bundle)
        ));
    }
    None
}

/// Compile one single-file program to a module (gating exactly like the differential:
/// parse-clean, checker-accepted, VM-compilable) and round-trip it.
fn roundtrip_single(name: &str, text: &str, report: &mut BundleReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    if !noeta_db::tokens(&db, src).0.diagnostics.is_empty()
        || !noeta_db::ast(&db, src).0.diagnostics.is_empty()
    {
        report.not_run.parse_failed += 1;
        return;
    }
    if !noeta_db::checked(&db, src).diagnostics.is_empty() {
        // A checker-rejected program produces no module — nothing to serialize, so it exercises no
        // round trip. Counting it matched (as this used to) inflated the headline with programs
        // that never produced bytes at all.
        report.not_run.checker_rejected += 1;
        return;
    }
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => report.not_run.unsupported += 1,
        Ok(module) => check(name, module, report),
    }
}

/// The workspace analogue of [`roundtrip_single`] for a multi-file fixture.
fn roundtrip_workspace(name: &str, raw: &noeta_loader::RawWorkspace, report: &mut BundleReport) {
    let db = LangDatabase::default();
    let ws = noeta_db::workspace(
        &db,
        &raw.entry,
        &raw.modules,
        noeta_lexer::Edition::DEFAULT,
        &raw.paths,
    );

    if noeta_db::linked(&db, ws).program.is_err() {
        report.not_run.link_failed += 1;
        return;
    }
    if !noeta_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.not_run.checker_rejected += 1;
        return;
    }
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.not_run.unsupported += 1,
        Ok(module) => check(name, module, report),
    }
}
