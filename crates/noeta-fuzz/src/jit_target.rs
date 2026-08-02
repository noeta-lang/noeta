//! The tier-1 JIT against the interpreter, over generated programs.
//!
//! # Why this arm exists separately
//!
//! [`crate::run_target`] already cross-checks two *interpreters* — the bytecode VM and the Core-IR
//! reference — and they are independent enough to catch a lowering or dispatch mistake. What they
//! cannot catch is anything that only exists in **native code**: a wrong register allocation, a
//! guard that does not bail, a retain the compiler emitted and a release it did not. Those live in
//! Cranelift output, which miri cannot execute and no interpreter differential can see.
//!
//! The conformance harness runs exactly this comparison over the corpus (`--jit-differential`), and
//! it is documented as *the* gate for native-code refcount contracts. This runs it over programs
//! nobody wrote — which matters more here than elsewhere, because tier-1 compiles a *subset* of the
//! language and the interesting inputs are the ones that straddle the boundary, bailing mid-frame
//! and resuming interpreted.
//!
//! # The three things compared
//!
//! 1. **Observable result.** Forced tier-1 must produce the interpreter's `RunResult` exactly.
//! 2. **Residency.** The heap delta around the native run must be zero, like the interpreted one.
//! 3. **Refcount anomalies.** A skipped retain or release that teardown's backup sweep would
//!    absorb — invisible to residency, which is why it is counted separately.
//!
//! Feature-gated so the default build of this crate does not pull Cranelift, matching how
//! `noeta-conformance` gates the same oracle.

use noeta_span::{Source, SourceId};
use noeta_vm::{RunOptions, Tiering, VmBackend};

use crate::run_target::Reach;

/// The base seed this target's sweeps walk.
pub const BASE_SEED: u64 = 0x71E1;

/// A disagreement between the interpreter and forced tier-1.
#[derive(Debug, Clone)]
pub enum JitViolation {
    /// The two tiers produced different observable results.
    ResultsDiffer { detail: String },
    /// Native code left objects live that the interpreter did not.
    Residency { residual: i64 },
    /// A retain or release went missing under native code.
    RefcountAnomalies(u64),
}

impl std::fmt::Display for JitViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JitViolation::ResultsDiffer { detail } => {
                write!(f, "tier-1 disagreed with the interpreter — {detail}")
            }
            JitViolation::Residency { residual } => write!(
                f,
                "the forced-JIT run left {residual} live object(s) the interpreter did not"
            ),
            JitViolation::RefcountAnomalies(n) => write!(
                f,
                "{n} refcount anomal(ies) under native code — a retain or release went missing"
            ),
        }
    }
}

/// The generated program `(seed, nonce)` denotes.
pub fn source(seed: u64, nonce: u32) -> String {
    crate::run_target::source(seed, nonce)
}

/// Run `src` interpreted and under forced tier-1, and hold the two to the same result, residency
/// and refcount contract.
pub fn jit_check(src: &str) -> Result<Reach, JitViolation> {
    noeta_conformance::ensure_std_registry();

    let source = Source::new(SourceId::FIRST, "jit.noe", src);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    if noeta_diagnostics::has_errors(lexed.diagnostics.iter().chain(parsed.diagnostics.iter())) {
        return Ok(Reach::Unparsed);
    }
    let checked = noeta_check::check_all(&parsed.program);
    if noeta_diagnostics::has_errors(checked.diagnostics.iter()) {
        return Ok(Reach::Rejected);
    }
    let Ok(module) =
        noeta_compiler::compile_with_sites(&parsed.program, checked.sites, false, false)
    else {
        return Ok(Reach::Rejected);
    };

    let interp = VmBackend::new().run_module(&module);

    let before = noeta_value::live_count() as i64;
    noeta_value::reset_refcount_anomalies();
    let out = VmBackend::new().run_module_with(
        &module,
        RunOptions {
            tiering: Tiering::Forced,
            ..RunOptions::default()
        },
    );
    let residual = noeta_value::live_count() as i64 - before;
    let anomalies = noeta_value::refcount_anomalies() as u64;

    if interp != out.result {
        return Err(JitViolation::ResultsDiffer {
            detail: describe(&interp, &out.result),
        });
    }
    if residual != 0 {
        return Err(JitViolation::Residency { residual });
    }
    if anomalies != 0 {
        return Err(JitViolation::RefcountAnomalies(anomalies));
    }
    Ok(Reach::Ran)
}

/// The first field on which the two tiers differ.
fn describe(interp: &noeta_backend::RunResult, jit: &noeta_backend::RunResult) -> String {
    if interp.stdout != jit.stdout {
        return format!(
            "stdout: interpreter {:?}, tier-1 {:?}",
            head(&interp.stdout),
            head(&jit.stdout)
        );
    }
    if interp.stderr != jit.stderr {
        return format!(
            "stderr: interpreter {:?}, tier-1 {:?}",
            head(&interp.stderr),
            head(&jit.stderr)
        );
    }
    if interp.exit_code != jit.exit_code {
        return format!(
            "exit: interpreter {}, tier-1 {}",
            interp.exit_code, jit.exit_code
        );
    }
    let codes = |r: &noeta_backend::RunResult| {
        r.diagnostics
            .iter()
            .map(|d| (d.code.code(), d.span))
            .collect::<Vec<_>>()
    };
    format!(
        "diagnostics: interpreter {:?}, tier-1 {:?}",
        codes(interp),
        codes(jit)
    )
}

/// Keep a mismatch readable when a bounded loop printed a few hundred lines.
fn head(text: &str) -> String {
    const MAX: usize = 200;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}… ({} bytes)", &text[..at], text.len()),
        None => text.to_string(),
    }
}
