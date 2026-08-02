//! Memory reclamation over generated programs: every object a program allocates is freed before it
//! returns.
//!
//! # Why this is worth its own oracle
//!
//! Every other property in this crate compares an *answer*. The formatter's output is compared to
//! its input, the two backends' `RunResult`s are compared to each other, the checker's verdict is
//! compared to the runtime's. A leak produces no wrong answer at all: the program prints exactly
//! what it should, exits zero, and the differential is perfectly happy. No comparison-based oracle
//! can see it, which is precisely why it needs one of its own.
//!
//! # What is measured
//!
//! Both backends keep a per-thread live-object counter — [`noeta_value::live_count`] for the VM's
//! heap, [`noeta_eval::live_count`] for the reference interpreter's `Rc` aggregates — and the
//! measurement is a **delta** around one program: the count before subtracted from the count after.
//! A clean program returns to where it started. Anything positive is a cycle the collector missed
//! or a release that never happened.
//!
//! The VM additionally reports **refcount anomalies**: during cycle collection, an unreachable
//! object's refcount must equal its in-edges from the garbage set, since unreachable garbage can
//! only be referenced by other garbage. A mismatch means a retain or release went missing even when
//! teardown's backup sweep would have absorbed the orphan — a bug the residency delta alone cannot
//! see, because the sweep hides it.
//!
//! # What this adds over the corpus leak oracle
//!
//! `noeta-conformance` already runs this over ~1,200 corpus programs, and that is the gate. What it
//! cannot do is *vary* the shapes: every corpus program was written to demonstrate something, so
//! the object graphs it builds are the ones somebody thought of. The generator builds graphs nobody
//! thought of — nested closures capturing loop variables, structs holding lists of structs, values
//! that outlive the block that made them — which is where a missed release tends to live.
//!
//! The counters are thread-local, so this measures only its own allocations and is safe to run
//! beside anything else.

use crate::run_target::Reach;
use noeta_span::{Source, SourceId};

/// The base seed this target's sweeps walk.
pub const BASE_SEED: u64 = 0x1EA_C0DE;

/// A program that did not give its memory back.
#[derive(Debug, Clone)]
pub enum LeakViolation {
    /// Objects still live after the program returned and tore down.
    Residency {
        backend: &'static str,
        residual: i64,
    },
    /// The backup collector saw refcounts that do not match the garbage set's own edges.
    RefcountAnomalies(u64),
}

impl std::fmt::Display for LeakViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeakViolation::Residency { backend, residual } => write!(
                f,
                "{backend} still held {residual} live object(s) after the program returned"
            ),
            LeakViolation::RefcountAnomalies(n) => write!(
                f,
                "{n} refcount anomal(ies) during collection — a retain or release went missing, \
                 even though teardown's backup sweep absorbed the result"
            ),
        }
    }
}

/// The generated program `(seed, nonce)` denotes — the same terminating shape the execution oracle
/// uses, so a leak found here reproduces there and vice versa.
pub fn source(seed: u64, nonce: u32) -> String {
    crate::run_target::source(seed, nonce)
}

/// Run `src` on both backends and assert each reclaimed everything it allocated.
///
/// A program the checker rejects never runs, and one the compiler cannot build is measured on the
/// reference only — the same exclusions the corpus leak oracle makes.
pub fn leak_check(src: &str) -> Result<Reach, LeakViolation> {
    noeta_conformance::ensure_std_registry();

    let source = Source::new(SourceId::FIRST, "leak.noe", src);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    if noeta_diagnostics::has_errors(lexed.diagnostics.iter().chain(parsed.diagnostics.iter())) {
        return Ok(Reach::Unparsed);
    }
    let checked = noeta_check::check_all(&parsed.program);
    if noeta_diagnostics::has_errors(checked.diagnostics.iter()) {
        return Ok(Reach::Rejected);
    }

    // The reference interpreter. Measured first and on this very thread — the counter is
    // thread-local, so the run and both samples have to stay together.
    let before = noeta_eval::live_count();
    let _ = noeta_conformance::reference::reference_run(&parsed.program, checked.sites.clone());
    let residual = noeta_eval::live_count() - before;
    if residual != 0 {
        return Err(LeakViolation::Residency {
            backend: "the reference interpreter",
            residual,
        });
    }

    // The VM, when the program compiles. Residency and refcount anomalies are separate questions:
    // the sweep can absorb an orphan and leave residency at zero while the anomaly count records
    // that a refcount was wrong.
    if let Ok(module) =
        noeta_compiler::compile_with_sites(&parsed.program, checked.sites, false, false)
    {
        let before = noeta_value::live_count() as i64;
        noeta_value::reset_refcount_anomalies();
        let _ = noeta_vm::VmBackend::new().run_module(&module);
        let residual = noeta_value::live_count() as i64 - before;
        if residual != 0 {
            return Err(LeakViolation::Residency {
                backend: "the VM",
                residual,
            });
        }
        let anomalies = noeta_value::refcount_anomalies() as u64;
        if anomalies != 0 {
            return Err(LeakViolation::RefcountAnomalies(anomalies));
        }
    }
    Ok(Reach::Ran)
}

/// Whether `src` still leaks, for the minimizer. Any violation counts: a reduction that turns a
/// residency leak into an anomaly has still preserved "this program does not give its memory back".
pub fn still_leaks(src: &str) -> bool {
    matches!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| leak_check(src))),
        Ok(Err(_))
    )
}

/// Line-granular delta debugging that preserves the leak.
pub fn minimize(src: &str) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
    let mut chunk = lines.len().max(1);
    loop {
        let mut i = 0;
        while i < lines.len() {
            let end = (i + chunk).min(lines.len());
            let mut candidate = lines.clone();
            candidate.drain(i..end);
            let text = candidate.join("\n");
            if !text.trim().is_empty() && still_leaks(&text) {
                lines = candidate;
            } else {
                i += 1;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk /= 2;
    }
    lines.join("\n")
}
