//! Pointing the generator at the *execution* pipeline: what the checker promises, and what
//! running actually does.
//!
//! # The invariants are the project's own words
//!
//! None of the three properties below is a rule invented for the fuzzer. Each is already written
//! down somewhere in the tree as a thing that is supposed to hold, which is what makes a violation
//! a defect rather than a difference of opinion:
//!
//! 1. **A checked program compiles.** `noeta_runner::compile_real` says so outright — *"every
//!    program that parses and type-checks compiles to bytecode … so an `Err` here is an internal
//!    invariant break"*. So `check` clean + `compile` refusing is a bug by the compiler's own
//!    documentation.
//! 2. **A checked program does not fail statically at run time.** The check-vs-run divergence
//!    class, which the project has been closing one surface at a time (`closed_to_new_methods`
//!    and `user_type_is_closed` in `noeta-check` are two of those surfaces): the checker was
//!    lenient about a method it did not recognise, the program ran, and the *runtime* produced
//!    E0005. Nothing underlines while you type and the failure only appears on Run, which is the
//!    worst shape the bug can take.
//! 3. **The two backends agree.** The conformance harness asserts this over a fixed corpus; this
//!    asserts it over generated programs, so the VM meets shapes nobody wrote a fixture for.
//!
//! # What makes invariant 2 sharp
//!
//! The runtime constructs `TypeMismatch` at 159 sites and `UnknownName` at 40 — but most of those
//! are legitimately dynamic, because a `dyn` value's member access *is* typed at run time and
//! reflection resolves names there by design. Calling either code a divergence unconditionally
//! would be wrong.
//!
//! What makes the classification sharp is the *input language*: the generator emits no `dyn`, no
//! reflection, and no `type_of`, so none of those dynamic paths is reachable. Under that
//! restriction a runtime `TypeMismatch` means the checker let an ill-typed program through. The
//! restriction is not assumed — `crate::run_target::uses_dynamic_typing` states it and the suite
//! asserts it over the generated corpus, because an oracle whose precondition quietly stops holding
//! is an oracle that quietly stops finding anything.
//!
//! # Why the programs are safe to run
//!
//! [`GenOptions::terminating`] bounds every loop and makes the call graph acyclic, so a generated
//! program halts by construction. That matters more than it sounds: neither backend caps call
//! depth, so runaway recursion is a stack overflow, and a stack overflow aborts the process rather
//! than failing a test. Termination here is structural, not a watchdog.
//!
//! [`GenOptions::terminating`]: crate::generate::GenOptions::terminating

use noeta_backend::RunResult;
use noeta_diagnostics::DiagnosticCode;
use noeta_span::{Source, SourceId};

/// The base seed this target's sweeps walk. A failure reports the nonce, and
/// `crate::run_target::source(BASE_SEED, nonce)` reproduces the exact program.
pub const BASE_SEED: u64 = 0x2117A11;

/// Codes that a **checked** program must never produce at run time.
///
/// Each is a statement about the program's *types or names* — the checker's whole job. The list is
/// derived from what the two backends actually construct (they build no others of this kind), not
/// from the full catalogue, so it stays a claim about reachable behavior.
///
/// Deliberately absent, because each is genuinely dynamic and a correct program can hit it:
/// `Panic`, `IndexOutOfBounds`, `KeyNotFound`, `DivisionByZero`, `IoError`, `AwaitCancelled`,
/// `ReactiveCycle`.
pub const STATIC_AT_RUNTIME: &[DiagnosticCode] = &[
    // The two that carry the class: the checker accepted a name or a type the runtime rejects.
    DiagnosticCode::UnknownName,
    DiagnosticCode::TypeMismatch,
    // Field and mutability facts are settled entirely at declaration sites.
    DiagnosticCode::MissingField,
    DiagnosticCode::ImmutableField,
    DiagnosticCode::ImmutableAssignment,
    // Arity and kind of type arguments; `@packed` legality; `Send`-ness of a shipped value. All
    // three are decided from declarations the checker has in hand.
    DiagnosticCode::InvalidTypeArguments,
    DiagnosticCode::InvalidPackedType,
    DiagnosticCode::NotSend,
];

/// Runtime failures that carry a [`STATIC_AT_RUNTIME`] code but are **value-dependent**: the same
/// well-typed expression succeeds or fails depending on what the values are at the moment it runs.
/// The checker is not wrong to let these through, so the oracle must not call them divergences.
///
/// One entry so far. `[1, 2] - [3]` aborts with `element-wise `-` expects two lists of equal
/// length`, filed under `TypeMismatch` — but both operands are `List<int>`, the types agree, and it
/// is the *lengths* that do not. Length is not part of the type, and `xs - ys` over two computed
/// lists could not be settled statically by any checker.
///
/// # This matches on the message, and that is a real limitation
///
/// There is no phase or kind on a `Diagnostic` that separates "ill-typed" from "wrong shape at run
/// time" — both are E0007 — so the message is the only signal available. The failure mode is benign
/// in the direction that matters: if the wording changes, the exception stops matching and the
/// sweep reports it again as a violation. It cannot go quietly silent, only quietly loud. Widening
/// this list is the thing to be suspicious of, and each entry should have to argue, as this one
/// does, that no checker could have known.
const VALUE_DEPENDENT: &[&str] = &["expects two lists of equal length"];

/// Whether `message` names one of the [`VALUE_DEPENDENT`] runtime failures.
fn is_value_dependent(message: &str) -> bool {
    VALUE_DEPENDENT.iter().any(|m| message.contains(m))
}

/// How far into the pipeline a program got. The sweep reports the distribution and asserts a floor
/// on [`Reach::Ran`]: every invariant here is conditioned on a program that *executes*, so a
/// generator drifting toward programs the checker rejects would leave the suite green and empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reach {
    /// Did not lex or parse. Nothing here applies.
    Unparsed,
    /// The checker rejected it — the ordinary outcome for a generator that does not track types,
    /// and not a finding: the checker is *entitled* to reject.
    Rejected,
    /// Checked clean and ran.
    Ran,
}

/// An invariant that did not hold, with enough detail to name the defect.
#[derive(Debug, Clone)]
pub enum Violation {
    /// The checker accepted the program and the compiler then refused it.
    CompileRefusedCheckedProgram { detail: String },
    /// A run produced a diagnostic from [`STATIC_AT_RUNTIME`] — the checker missed it.
    StaticErrorAtRuntime {
        backend: &'static str,
        code: &'static str,
        message: String,
    },
    /// The VM and the Core-IR reference produced different observable results.
    BackendsDisagree { detail: String },
    /// Some stage panicked. The pipeline reports an unsupported construct as a value
    /// (`Err(Unsupported)`), so a panic is a different thing entirely: a shape nobody enumerated.
    Panicked { where_: String },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::CompileRefusedCheckedProgram { detail } => write!(
                f,
                "the checker accepted this program and the compiler refused it: {detail}"
            ),
            Violation::StaticErrorAtRuntime {
                backend,
                code,
                message,
            } => write!(
                f,
                "{backend} reported {code} at run time on a program the checker accepted: {message}"
            ),
            Violation::BackendsDisagree { detail } => {
                write!(f, "the backends disagreed — {detail}")
            }
            Violation::Panicked { where_ } => {
                write!(
                    f,
                    "a stage panicked on a program the checker accepted: {where_}"
                )
            }
        }
    }
}

/// The class a violation belongs to, for deduplicating a sweep's findings down to distinct defects.
/// Messages carry names and spans that differ per program; the class is what does not.
pub fn class(v: &Violation) -> String {
    match v {
        Violation::CompileRefusedCheckedProgram { .. } => "compile-refused".to_string(),
        Violation::StaticErrorAtRuntime { code, .. } => format!("runtime-{code}"),
        Violation::BackendsDisagree { detail } => {
            // The leading word names the field that differed (`stdout:`, `exit:`, `diagnostics:`).
            let field = detail.split(':').next().unwrap_or("?");
            format!("backends-{field}")
        }
        // The panic's own location, which is what distinguishes two panics; the message around it
        // varies with the program.
        Violation::Panicked { where_ } => {
            format!("panic-{}", where_.split_whitespace().next().unwrap_or("?"))
        }
    }
}

/// The generated program `(seed, nonce)` denotes — terminating, comment-free, ready to run.
pub fn source(seed: u64, nonce: u32) -> String {
    crate::generate::program_with(
        &crate::seed_bytes(seed, nonce),
        &crate::generate::GenOptions::terminating(),
    )
}

/// Whether `src` reaches a construct whose typing is *supposed* to happen at run time — the
/// precondition [`STATIC_AT_RUNTIME`] rests on.
///
/// Textual on purpose. This is a guard on the generator, not an analysis of the language: it has to
/// stay obviously right, and it has to fire if some future generator arm starts emitting `dyn`
/// without anyone noticing that it just blunted the oracle.
pub fn uses_dynamic_typing(src: &str) -> bool {
    ["dyn ", "dyn<", "type_of", "construct", "field_specs_of"]
        .iter()
        .any(|needle| src.contains(needle))
}

/// The checker's own verdict on `src`, rendered one line per diagnostic — the *other* half of a
/// divergence report. Knowing that the checker rejected a program says nothing useful until you
/// know what it said, and a rejection for an unrelated reason is how a triage session convinces
/// itself of a defect that is not there.
pub fn check_diagnostics(src: &str) -> Vec<String> {
    noeta_conformance::ensure_std_registry();
    let source = Source::new(SourceId::FIRST, "fuzz.noe", src);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    noeta_check::check_all(&parsed.program)
        .diagnostics
        .iter()
        .map(|d| format!("{} {}", d.code.code(), d.message))
        .collect()
}

/// Run the whole oracle over one program.
///
/// `Ok(reach)` says how far it got with every applicable invariant holding; `Err` names the first
/// that did not.
pub fn evaluate(src: &str) -> Result<Reach, Violation> {
    // The front-end resolves `std.*` through the process-wide registry and panics on its first
    // lookup if none is installed. Idempotent, so paying it per program costs nothing.
    noeta_conformance::ensure_std_registry();

    let source = Source::new(SourceId::FIRST, "fuzz.noe", src);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    if noeta_diagnostics::has_errors(lexed.diagnostics.iter().chain(parsed.diagnostics.iter())) {
        return Ok(Reach::Unparsed);
    }

    let checked = noeta_check::check_all(&parsed.program);
    if noeta_diagnostics::has_errors(checked.diagnostics.iter()) {
        return Ok(Reach::Rejected);
    }

    // Invariant 1. The same compile the salsa `bytecode` query performs — no isolates, no debug
    // info — so a refusal here is the one the ordinary pipeline would hit.
    let module = match noeta_compiler::compile_with_sites(
        &parsed.program,
        checked.sites.clone(),
        false,
        false,
    ) {
        Ok(module) => module,
        Err(unsupported) => {
            return Err(Violation::CompileRefusedCheckedProgram {
                detail: unsupported.to_string(),
            });
        }
    };

    let vm = noeta_vm::VmBackend::new().run_module(&module);
    let reference = noeta_conformance::reference::reference_run(&parsed.program, checked.sites);

    // Invariant 2, on both backends: either reaching a static code is a divergence, and reporting
    // which one reached it is what says whether the defect is in the shared front end or in one
    // executor.
    for (backend, result) in [("the VM", &vm), ("the reference interpreter", &reference)] {
        if let Some(d) = result
            .diagnostics
            .iter()
            .find(|d| STATIC_AT_RUNTIME.contains(&d.code) && !is_value_dependent(&d.message))
        {
            return Err(Violation::StaticErrorAtRuntime {
                backend,
                code: d.code.code(),
                message: d.message.clone(),
            });
        }
    }

    // Invariant 3.
    if vm != reference {
        return Err(Violation::BackendsDisagree {
            detail: describe_difference(&reference, &vm),
        });
    }
    Ok(Reach::Ran)
}

/// Describe the first field on which the two results differ. Mirrors the conformance
/// differential's rendering so a finding here reads like one from there.
fn describe_difference(reference: &RunResult, vm: &RunResult) -> String {
    if reference.stdout != vm.stdout {
        return format!(
            "stdout: reference {:?}, vm {:?}",
            truncate(&reference.stdout),
            truncate(&vm.stdout)
        );
    }
    if reference.stderr != vm.stderr {
        return format!(
            "stderr: reference {:?}, vm {:?}",
            truncate(&reference.stderr),
            truncate(&vm.stderr)
        );
    }
    if reference.exit_code != vm.exit_code {
        return format!(
            "exit: reference {}, vm {}",
            reference.exit_code, vm.exit_code
        );
    }
    let codes = |r: &RunResult| {
        r.diagnostics
            .iter()
            .map(|d| (d.code.code(), d.span))
            .collect::<Vec<_>>()
    };
    format!(
        "diagnostics: reference {:?}, vm {:?}",
        codes(reference),
        codes(vm)
    )
}

/// Keep a mismatch report readable when a bounded loop still printed a few hundred lines.
fn truncate(text: &str) -> String {
    const MAX: usize = 200;
    match text.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}… ({} bytes)", &text[..at], text.len()),
        None => text.to_string(),
    }
}

/// [`evaluate`], made total: a panic anywhere in the pipeline becomes a [`Violation::Panicked`]
/// rather than taking the process with it.
///
/// A fuzz driver has to be the thing that *reports* a panic, not the thing that dies of one — a
/// sweep that aborts at its first hit tells you about one program and nothing about the 5,000
/// behind it. The panic hook is silenced for the duration, because the default one writes to stderr
/// and a scan with a few hundred hits is then unreadable; the location is recovered from the
/// payload instead, which is what `class` deduplicates on.
///
/// This is the entry point every sweep should call. [`evaluate`] stays panic-transparent for the
/// caller that wants a backtrace.
pub fn evaluate_total(src: &str) -> Result<Reach, Violation> {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};

    let site: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&site);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".to_string());
        *sink.lock().expect("panic-site sink") = format!("{at} — {}", panic_message(info));
    }));
    let out = catch_unwind(AssertUnwindSafe(|| evaluate(src)));
    std::panic::set_hook(prev);
    out.unwrap_or_else(|_| {
        let where_ = site.lock().expect("panic-site sink").clone();
        Err(Violation::Panicked { where_ })
    })
}

/// The panic's own message, for the report. `PanicHookInfo::payload` is the only place it lives.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    let p = info.payload();
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

/// Whether `src` still fails in the same *class*, for the minimizer.
///
/// Class rather than message, for the same reason the fmt minimizer does it: a reduction that
/// changes which name is missing has not preserved the defect, and one that merely renumbers a span
/// has.
pub fn still_fails(src: &str, target: &str) -> bool {
    matches!(evaluate_total(src), Err(v) if class(&v) == target)
}

/// Line-granular delta debugging that preserves the violation class.
///
/// A reduction that stops parsing, or that the checker starts rejecting, no longer violates
/// anything — so it is rejected, and the reduction stays a valid program without the minimizer
/// knowing any grammar. That is the same self-correcting trick the fmt minimizer uses.
pub fn minimize(src: &str, target: &str) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
    let mut chunk = lines.len().max(1);
    loop {
        let mut i = 0;
        while i < lines.len() {
            let end = (i + chunk).min(lines.len());
            let mut candidate = lines.clone();
            candidate.drain(i..end);
            let text = candidate.join("\n");
            if !text.trim().is_empty() && still_fails(&text, target) {
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
