//! The execution-backend seam: the contract every runtime implements.
//!
//! Extracted into its own crate in M1 so the two backends — the Core-IR interpreter
//! (`noeta-eval`) and the M1 bytecode VM (`noeta-vm`) — are *siblings*: neither depends
//! on the other, and both depend only on this tiny vocabulary. The conformance harness
//! runs a program through both and asserts their [`RunResult`]s are identical (the
//! differential oracle). Comparing `RunResult` — observable output, not internal value
//! representation — is exactly what lets the two backends use completely different value
//! models (the interpreter's `Rc`-based enum vs. the VM's NaN-boxed words).

use noeta_ast::Program;
use noeta_diagnostics::Diagnostic;

/// The observable outcome of running a program: everything it wrote to stdout and stderr, its
/// process exit code, and any runtime diagnostics it produced. This is the unit the
/// conformance harness compares and the unit two backends are checked to agree on.
///
/// `stderr` is *observable output* (`std.io`'s `err`/`errln`), routed through the backends' own
/// buffers exactly like `stdout` — not a host effect — so the differential oracle compares it too
/// and the two backends are held byte-identical on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub diagnostics: Vec<Diagnostic>,
}

impl RunResult {
    /// Whether the run produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// An execution backend. The two implementations — the Core-IR interpreter (`noeta-eval`,
/// the retired M0 AST tree-walker's successor) and the M1 bytecode VM (`noeta-vm`) — are
/// cross-checked against this contract.
pub trait Backend {
    fn run(&self, program: &Program) -> RunResult;
}

/// One frame of an abort traceback: the function's name (`None` for an anonymous closure/thunk) and
/// the source location it was at — the failing instruction for the innermost frame, the call site
/// for each caller. `span` is `None` where no location is known (e.g. a caller that re-entered the
/// VM through a native call). Produced by both backends, innermost frame first; rides **beside**
/// [`RunResult`] (not inside it) so the differential's compared unit is unchanged while the two
/// tracebacks converge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceFrame {
    pub name: Option<String>,
    pub span: Option<noeta_span::Span>,
}

/// Render an abort traceback for human consumption, resolving each frame's span against `sources` —
/// the rendering the CLI prints after a panic diagnostic and the debug adapter forwards as error
/// output. Innermost frame first. Deep stacks (runaway recursion) are capped, with the elided count
/// noted.
pub fn render_trace(trace: &[TraceFrame], sources: &noeta_span::SourceMap) -> String {
    use std::fmt::Write as _;
    /// The most frames a rendered traceback shows — enough for any legitimate stack, while a
    /// stack-overflow abort with thousands of identical frames stays readable.
    const MAX_FRAMES: usize = 64;
    let mut out = String::from("stack trace (most recent call first):\n");
    for frame in trace.iter().take(MAX_FRAMES) {
        let name = frame.name.as_deref().unwrap_or("<anonymous>");
        // A span that does not fit its resolved source renders name-only rather than panicking the
        // renderer — the REPL reuses one `SourceId` across entries, so a frame from a function
        // defined in an *earlier* entry carries a span into text the current entry no longer has.
        let located = frame.span.and_then(|span| {
            let source = sources.source(span.source);
            (span.start as usize <= source.text().len())
                .then(|| (source.name(), source.line_col(span.start).line))
        });
        match located {
            Some((file, line)) => {
                let _ = writeln!(out, "  at {name} ({file}:{line})");
            }
            None => {
                let _ = writeln!(out, "  at {name}");
            }
        }
    }
    if trace.len() > MAX_FRAMES {
        let _ = writeln!(out, "  … and {} more frames", trace.len() - MAX_FRAMES);
    }
    out
}
