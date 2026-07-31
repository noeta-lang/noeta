//! The **compile-determinism oracle**: the same program must compile to the same bytes, in every
//! process, forever.
//!
//! This is a distinct guarantee from the bundle oracle next door. [`crate::run_bundle_roundtrip`]
//! proves a module survives `encode → decode → encode` *within one process*; nothing there notices
//! a module whose bytes are stable within a run and different on the next one. That is exactly the
//! failure mode a `HashSet`/`HashMap` iterated into a serialized table produces: Rust seeds the
//! default hasher randomly, so the collected order — and therefore the encoded bytes — differ per
//! process while every in-process invariant still holds.
//!
//! So the oracle's unit is a **digest per program**, cheap enough to ship between processes, and
//! the gate ([`crate::tests`] side: `tests/determinism.rs`) compares two digest sets produced by
//! two *separate* processes. Comparing two compiles inside one process would not catch the bug
//! class at all: the hasher's per-thread seed is fixed for the life of the thread, so both arms of
//! an in-process comparison agree on the same wrong order.
//!
//! Why the whole corpus rather than a handful of samples: the divergence is per-program and
//! probabilistic (a two-element hash set lands in the same order half the time), so breadth *is*
//! the sensitivity. Over a thousand programs, any field whose order is hash-derived diverges in
//! practically all of them.

use std::path::Path;

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};

use crate::collect_cases;

/// Every corpus program that compiled, paired with the digest of its module's bytes.
#[derive(Debug, Default, Clone)]
pub struct DeterminismReport {
    /// `(case name, digest)` in case order — the order is itself part of the comparison, so a
    /// program that compiles in one process and not in another shows up as a mismatch too.
    pub digests: Vec<(String, String)>,
    /// Programs that produced no module to digest (parse/link/check failures, unsupported).
    pub not_run: crate::NotRun,
}

impl DeterminismReport {
    /// The cases whose digests differ between two runs, as `(name, mine, theirs)`, plus any case
    /// present in one report and absent from the other.
    pub fn diff(&self, other: &Self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < self.digests.len() && j < other.digests.len() {
            let (ln, ld) = &self.digests[i];
            let (rn, rd) = &other.digests[j];
            match ln.cmp(rn) {
                std::cmp::Ordering::Equal => {
                    if ld != rd {
                        out.push((ln.clone(), ld.clone(), rd.clone()));
                    }
                    i += 1;
                    j += 1;
                }
                std::cmp::Ordering::Less => {
                    out.push((ln.clone(), ld.clone(), "<absent>".to_string()));
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push((rn.clone(), "<absent>".to_string(), rd.clone()));
                    j += 1;
                }
            }
        }
        for (n, d) in &self.digests[i..] {
            out.push((n.clone(), d.clone(), "<absent>".to_string()));
        }
        for (n, d) in &other.digests[j..] {
            out.push((n.clone(), "<absent>".to_string(), d.clone()));
        }
        out
    }

    /// The wire form the child process prints and the parent parses: one `name\tdigest` line per
    /// program. Deliberately not serde — the gate must not depend on the very serialization it is
    /// there to police.
    pub fn to_wire(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (name, digest) in &self.digests {
            let _ = writeln!(out, "{name}\t{digest}");
        }
        out
    }

    /// Parse [`Self::to_wire`]. Lines without a tab are ignored, so a child's incidental output
    /// (libtest's own banner, a warning) does not corrupt the comparison.
    pub fn from_wire(text: &str) -> Self {
        let digests = text
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, digest)| (name.to_string(), digest.to_string()))
            .collect();
        Self {
            digests,
            not_run: crate::NotRun::default(),
        }
    }
}

/// FNV-1a over the encoded module, rendered hex. A digest rather than the bytes themselves because
/// the two arms live in different processes and a thousand modules of bytecode is not something to
/// pipe through stdout. Hand-rolled rather than a hash crate: the oracle must not inherit a
/// dependency's own ordering behaviour, and a 64-bit collision across two encodings of the *same
/// program* is not a risk worth a dependency.
fn digest(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    // Length is folded in explicitly so a truncation cannot alias.
    format!("{h:016x}-{:x}", bytes.len())
}

/// Compile every `.noe` file under `root` and digest each resulting module's bytes.
///
/// Gated exactly like the bundle oracle (parse-clean, checker-accepted, compilable), so the two
/// oracles cover the same set of programs and a divergence here is never "one arm skipped it".
pub fn digest_corpus(root: &Path) -> DeterminismReport {
    crate::ensure_std_registry();
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = DeterminismReport::default();
    for case in cases {
        let name = case
            .entry
            .strip_prefix(root)
            .unwrap_or(&case.entry)
            .to_string_lossy()
            .into_owned();
        if case.multi {
            match crate::read_case_workspace(&case.entry) {
                Ok(raw) => digest_workspace(&name, &raw, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => digest_single(&name, &text, &mut report),
                Err(_) => report.not_run.read_failed += 1,
            }
        }
    }
    report
}

/// Digest one single-file program's module.
fn digest_single(name: &str, text: &str, report: &mut DeterminismReport) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, name, text);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);

    if noeta_diagnostics::has_errors(
        noeta_db::tokens(&db, src)
            .0
            .diagnostics
            .iter()
            .chain(noeta_db::ast(&db, src).0.diagnostics.iter()),
    ) {
        report.not_run.parse_failed += 1;
        return;
    }
    if crate::has_error(&noeta_db::checked(&db, src).diagnostics) {
        report.not_run.checker_rejected += 1;
        return;
    }
    match &noeta_db::bytecode(&db, src).0 {
        Err(_) => report.not_run.unsupported += 1,
        Ok(module) => report
            .digests
            .push((name.to_string(), digest(&module.encode()))),
    }
}

/// The multi-file analogue of [`digest_single`].
fn digest_workspace(name: &str, raw: &noeta_loader::RawWorkspace, report: &mut DeterminismReport) {
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
    if crate::has_error(&noeta_db::linked_checked(&db, ws).diagnostics) {
        report.not_run.checker_rejected += 1;
        return;
    }
    match &noeta_db::linked_bytecode(&db, ws).0 {
        Err(_) => report.not_run.unsupported += 1,
        Ok(module) => report
            .digests
            .push((name.to_string(), digest(&module.encode()))),
    }
}
