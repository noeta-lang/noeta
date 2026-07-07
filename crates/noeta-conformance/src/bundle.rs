//! The bundle serialization oracle (P-AOT L1.0): compile every corpus program to a
//! [`noeta_bytecode::Module`] and assert it survives a serialize→deserialize→serialize round-trip
//! byte-for-byte. This proves the whole bytecode graph — ops, constants, shapes, method/derive
//! tables, and the reflection artifact — serializes losslessly, the precondition for shipping a
//! `.noeb` bundle instead of source.
//!
//! `Module` does not derive `PartialEq` (its ops carry no equality), so the round-trip is checked
//! structurally via **byte stability**: `encode(decode(encode(m)))` must equal `encode(m)`. A
//! successful decode that re-encodes identically is a lossless round-trip. The *execution*
//! differential (a decoded module runs identically to source) is the separate L1.3 oracle.

use std::path::Path;

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};

use crate::collect_cases;

/// The outcome of a bundle round-trip run over a corpus.
#[derive(Debug, Default)]
pub struct BundleReport {
    /// Programs the VM compiled and whose module round-tripped byte-for-byte.
    pub matched: usize,
    /// Programs outside the VM's current subset (no module to serialize).
    pub skipped: usize,
    /// Programs that did not parse/check cleanly (no module produced).
    pub parse_failed: usize,
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
            "bundle round-trip: {} matched, {} skipped (unsupported), {} parse-failed",
            self.matched, self.skipped, self.parse_failed,
        );
        if self.failures.is_empty() {
            out.push_str("every compiled module serializes losslessly ✓\n");
        } else {
            let _ = writeln!(out, "{} ROUND-TRIP FAILURE(s):", self.failures.len());
            for f in &self.failures {
                let _ = writeln!(out, "  {} — {}", f.name, f.detail);
            }
        }
        out
    }
}

/// Round-trip every `.noe` file under `root` (optionally narrowed to one file).
pub fn run_bundle_roundtrip(root: &Path, only: Option<&Path>) -> BundleReport {
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
            match noeta_loader::read_workspace(&case.entry) {
                Ok(raw) => roundtrip_workspace(&name, &raw, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => roundtrip_single(&name, &text, &mut report),
                Err(_) => report.parse_failed += 1,
            }
        }
    }
    report
}

/// Assert `module` survives an encode→decode→encode round-trip byte-for-byte, folding the outcome
/// into `report`. Shared by the single-file and workspace paths.
fn check(name: &str, module: &noeta_bytecode::Module, report: &mut BundleReport) {
    let blob = module.encode();
    match noeta_bytecode::Module::decode(&blob) {
        Err(e) => report.failures.push(BundleFailure {
            name: name.to_string(),
            detail: format!("decode failed: {e}"),
        }),
        Ok(decoded) => {
            if decoded.encode() == blob {
                report.matched += 1;
            } else {
                report.failures.push(BundleFailure {
                    name: name.to_string(),
                    detail: "re-encode produced different bytes (lossy round-trip)".to_string(),
                });
            }
        }
    }
}

/// Compile one single-file program to a module (gating exactly like the differential:
/// parse-clean, checker-accepted, VM-compilable) and round-trip it.
fn roundtrip_single(name: &str, text: &str, report: &mut BundleReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source);

    if !noeta_db::tokens(&db, src).0.diagnostics.is_empty()
        || !noeta_db::ast(&db, src).0.diagnostics.is_empty()
    {
        report.parse_failed += 1;
        return;
    }
    if !noeta_db::checked(&db, src).diagnostics.is_empty() {
        // A checker-rejected program produces no module — nothing to serialize; count it matched
        // (its diagnostics are its whole result), mirroring the differential's accounting.
        report.matched += 1;
        return;
    }
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => check(name, module, report),
    }
}

/// The workspace analogue of [`roundtrip_single`] for a multi-file fixture.
fn roundtrip_workspace(name: &str, raw: &noeta_loader::RawWorkspace, report: &mut BundleReport) {
    let db = LangDatabase::default();
    let ws = noeta_db::workspace(&db, &raw.entry, &raw.modules);

    if noeta_db::linked(&db, ws).0.is_err() {
        report.parse_failed += 1;
        return;
    }
    if !noeta_db::linked_checked(&db, ws).diagnostics.is_empty() {
        report.matched += 1;
        return;
    }
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.skipped += 1,
        Ok(module) => check(name, module, report),
    }
}
