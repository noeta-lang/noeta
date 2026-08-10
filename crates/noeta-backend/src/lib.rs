//! The execution-backend seam: the contract every runtime implements.
//!
//! Extracted into its own crate in M1 so the two backends — the Core-IR interpreter
//! (`noeta-eval`) and the M1 bytecode VM (`noeta-vm`) — are *siblings*: neither depends
//! on the other, and both depend only on this tiny vocabulary. The conformance harness
//! runs a program through both and asserts their [`RunResult`]s are identical (the
//! differential oracle). Comparing `RunResult` — observable output, not internal value
//! representation — is exactly what lets the two backends use completely different value
//! models (the interpreter's `Rc`-based enum vs. the VM's NaN-boxed words).
//!
//! It also owns the other end of that seam: [`RunTail`], the one rendering of a finished run into
//! output. This crate is where it can live — it is the common ancestor of every execution surface
//! (the CLI, the lean runner, the AOT runtime staticlib, the wasip1 runner, the `wasi:http`
//! component), each of which reaches it without acquiring a host, a compiler, or tokio.

use noeta_ast::Program;
use noeta_diagnostics::Diagnostic;

mod tail;

pub use tail::{Component, Part, RunTail, Stream};

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
    render_trace_colored(trace, sources, false)
}

/// [`render_trace`] with ANSI colour when `color`.
///
/// What gets painted is everything *except* the function names: the header, the `at`, each
/// `(file:line)`, and the elision note are dimmed, so the names — the one column you actually read
/// a traceback for — stand out by being left alone. A traceback is printed directly under a
/// diagnostic, so the grey is [`noeta_diagnostics::DIM`], the same one `ariadne` uses for a
/// report's gutter.
pub fn render_trace_colored(
    trace: &[TraceFrame],
    sources: &noeta_span::SourceMap,
    color: bool,
) -> String {
    use std::fmt::Write as _;
    /// The most frames a rendered traceback shows — enough for any legitimate stack, while a
    /// stack-overflow abort with thousands of identical frames stays readable.
    const MAX_FRAMES: usize = 64;
    let (dim, reset) = if color {
        (noeta_diagnostics::DIM, noeta_diagnostics::RESET)
    } else {
        ("", "")
    };
    let mut out = format!("{dim}stack trace (most recent call first):{reset}\n");
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
                let _ = writeln!(out, "{dim}  at {reset}{name}{dim} ({file}:{line}){reset}");
            }
            None => {
                let _ = writeln!(out, "{dim}  at {reset}{name}");
            }
        }
    }
    if trace.len() > MAX_FRAMES {
        let _ = writeln!(
            out,
            "{dim}  … and {} more frames{reset}",
            trace.len() - MAX_FRAMES
        );
    }
    out
}

#[cfg(test)]
mod trace_tests {
    use super::{TraceFrame, render_trace, render_trace_colored};
    use noeta_span::{Source, SourceId, SourceMap, Span};

    /// Drop every SGR sequence, so the coloured rendering can be compared against the plain one.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    fn two_frames() -> (SourceMap, Vec<TraceFrame>) {
        let text = "fn inner() {\n    boom()\n}\nfn outer() {\n    inner()\n}\n";
        let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, "app.noe", text)]);
        let at = |needle: &str| {
            let start = text.find(needle).expect("the fixture contains it") as u32;
            Some(Span::new(start, start + needle.len() as u32))
        };
        (
            sources,
            vec![
                TraceFrame {
                    name: Some("inner".into()),
                    span: at("boom()"),
                },
                TraceFrame {
                    name: Some("outer".into()),
                    span: at("inner()"),
                },
            ],
        )
    }

    #[test]
    fn the_plain_rendering_has_no_escape_sequences() {
        // The DAP, the MCP server and the playground all put this text in a JSON string or a
        // browser, and none of them ask for colour.
        let (sources, trace) = two_frames();
        let rendered = render_trace(&trace, &sources);
        assert!(
            !rendered.contains('\u{1b}'),
            "plain traceback must stay plain:\n{rendered:?}"
        );
    }

    #[test]
    fn colour_adds_colour_and_nothing_else() {
        let (sources, trace) = two_frames();
        let plain = render_trace(&trace, &sources);
        let colored = render_trace_colored(&trace, &sources, true);
        assert!(colored.contains('\u{1b}'), "the coloured form carries it");
        assert_eq!(
            strip_ansi(&colored),
            plain,
            "stripping the colour gives back the plain traceback exactly"
        );
    }

    #[test]
    fn the_function_names_are_the_part_left_undimmed() {
        // The design: everything around a name is dimmed so the names are what your eye lands on.
        // If a name were dimmed too, the traceback would be uniformly grey and the colour would be
        // costing escape sequences for nothing.
        let (sources, trace) = two_frames();
        let colored = render_trace_colored(&trace, &sources, true);
        let line = colored
            .lines()
            .find(|l| l.contains("inner"))
            .expect("the innermost frame is rendered");
        assert!(
            line.contains(&format!("{}inner", noeta_diagnostics::RESET)),
            "the name follows a reset rather than sitting inside the dim run:\n{line:?}"
        );
        assert!(
            line.contains(&format!("{}  at ", noeta_diagnostics::DIM)),
            "the `at` and the location around it are dimmed:\n{line:?}"
        );
    }

    #[test]
    fn a_frame_with_no_location_still_renders() {
        let sources = SourceMap::new(vec![Source::new(SourceId::FIRST, "app.noe", "")]);
        let trace = vec![TraceFrame {
            name: None,
            span: None,
        }];
        let colored = render_trace_colored(&trace, &sources, true);
        assert!(
            colored.contains("<anonymous>"),
            "names the frame: {colored}"
        );
        assert_eq!(strip_ansi(&colored), render_trace(&trace, &sources));
    }
}
