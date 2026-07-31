//! The **run tail**: the one place a finished run turns into output.
//!
//! A run produces a [`RunResult`](crate::RunResult), an abort traceback, and (on the JIT paths) a
//! `--jit-stats` report. Turning that triple into what a person or a platform actually sees —
//! which text goes to stdout, which to stderr, in what order, and what exit status the process
//! reports — is the *tail*, and it was hand-copied into seven places
//! (`plans/parallel-path-audit.md` row 1). Two of the seven wrote the program's own `stderr`
//! stream; four dropped it. One truncated the exit code with `as u8`, so a program exiting 256
//! exited 0. Two rendered no diagnostics and no traceback at all.
//!
//! [`RunTail`] is that epilogue as a value. Every surface builds one and either
//! [`emit`](RunTail::emit)s it to the process streams or reads its [`parts`](RunTail::parts) —
//! nobody re-derives "stdout then stderr then diagnostics then traceback then report" by hand.
//!
//! # Why a value and not a function
//!
//! Three surfaces do not want process streams. The `wasi:http` edge composes the run's output into
//! a **response body**; a `--parallel` serve worker wants to know whether the run aborted before it
//! writes; a test wants to assert on one component. A `fn print_the_run(...)` serves only the first
//! kind of caller and pushes the rest back into hand-writing the epilogue — which is exactly how
//! the seven copies happened. So the rendering and the writing are separate: [`RunTail::render`]
//! produces the components, and writing is one of several things you can then do with them.
//!
//! # Why `parts` and not public fields
//!
//! [`RunTail::parts`] destructures `Self` exhaustively. A **new output component** — a future third
//! stream, a resource summary, a deprecation report — added as a field makes `parts` fail to
//! compile until it is classified onto a [`Stream`] and given a position in the canonical order.
//! Every structured consumer iterates `parts()`, so the new component reaches all of them by
//! construction rather than by seven people remembering. That property is the whole point of the
//! type; `crates/noeta-backend/src/tail.rs`'s own tests pin it from the other side.

use std::io::Write;
use std::process::ExitCode;

use noeta_span::SourceMap;

use crate::{RunResult, TraceFrame, render_trace};

/// Which process stream a rendered component belongs on.
///
/// The program's own two streams keep their identity (`echo` is stdout, `std.io`'s `err`/`errln` is
/// stderr); everything the *runtime* adds — diagnostics, the traceback, the `--jit-stats` report —
/// is stderr, because it is commentary about the run and not the run's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// The rendered components of a run, in the canonical write order.
///
/// The order is load-bearing and asserted by tests on several surfaces: a program that reports
/// progress on stderr and then aborts must not have its report appear *after* the failure that
/// followed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Component {
    /// The program's own stdout (`echo`, `io.out`).
    Stdout,
    /// The program's own stderr (`std.io`'s `err` / `errln`) — observable program output, buffered
    /// exactly as stdout is, not a host effect.
    ProgramStderr,
    /// Runtime diagnostics the run recorded, rendered against the source map.
    Diagnostics,
    /// The abort traceback, when the run aborted with a call chain worth showing.
    Traceback,
    /// A caller-supplied trailing report (`--jit-stats`). Empty on every surface that has none.
    Report,
}

impl Component {
    /// The stream this component is written to. Total by construction — a `match` with no
    /// catch-all, so a new component must answer the question.
    pub fn stream(self) -> Stream {
        match self {
            Component::Stdout => Stream::Stdout,
            Component::ProgramStderr
            | Component::Diagnostics
            | Component::Traceback
            | Component::Report => Stream::Stderr,
        }
    }
}

/// One rendered component of a finished run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part<'a> {
    pub component: Component,
    pub text: &'a str,
}

impl Part<'_> {
    /// The stream this part belongs on.
    pub fn stream(&self) -> Stream {
        self.component.stream()
    }
}

/// A finished run, rendered but not yet written — the shared epilogue of every execution surface.
///
/// Build one with [`RunTail::render`], optionally attach a trailing report with
/// [`RunTail::with_report`], then either [`emit`](RunTail::emit) it to the process streams or read
/// its [`parts`](RunTail::parts).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunTail {
    stdout: String,
    program_stderr: String,
    diagnostics: String,
    traceback: String,
    report: String,
    exit_code: i32,
}

impl RunTail {
    /// Render a finished run: the program's two streams verbatim, its recorded diagnostics through
    /// [`render_mapped`](noeta_diagnostics::render_mapped), and its abort traceback through
    /// [`render_trace`].
    ///
    /// A traceback renders only when the chain has **two or more** frames: a single-frame trace
    /// repeats what the diagnostic's own span already says, so printing it is noise.
    pub fn render(result: &RunResult, trace: &[TraceFrame], sources: &SourceMap) -> Self {
        RunTail {
            stdout: result.stdout.clone(),
            program_stderr: result.stderr.clone(),
            diagnostics: noeta_diagnostics::render_mapped(sources, result.diagnostics.iter()),
            traceback: if trace.len() >= 2 {
                render_trace(trace, sources)
            } else {
                String::new()
            },
            report: String::new(),
            exit_code: result.exit_code,
        }
    }

    /// Attach a trailing report — the `--jit-stats` rendering, which only the JIT-carrying surfaces
    /// produce. It is passed in rather than rendered here so this crate stays free of the JIT's
    /// report types (and so the AOT runtime, which links no JIT compiler, can use this tail).
    #[must_use]
    pub fn with_report(mut self, report: impl Into<String>) -> Self {
        self.report = report.into();
        self
    }

    /// Every rendered component, in the canonical write order — the seam for a surface that wants
    /// structured output rather than process streams (the `wasi:http` edge composes a body; a serve
    /// worker attributes lines to a worker).
    ///
    /// The exhaustive destructure below is the guarantee: a new field on [`RunTail`] fails to
    /// compile here until it is given a [`Component`] and a position in the order, and every
    /// consumer that iterates this picks it up with no edit of its own.
    pub fn parts(&self) -> Vec<Part<'_>> {
        let RunTail {
            stdout,
            program_stderr,
            diagnostics,
            traceback,
            report,
            // Not a rendered component: it is the run's *status*, exposed by `exit_code`/`status`.
            exit_code: _,
        } = self;
        vec![
            Part {
                component: Component::Stdout,
                text: stdout,
            },
            Part {
                component: Component::ProgramStderr,
                text: program_stderr,
            },
            Part {
                component: Component::Diagnostics,
                text: diagnostics,
            },
            Part {
                component: Component::Traceback,
                text: traceback,
            },
            Part {
                component: Component::Report,
                text: report,
            },
        ]
    }

    /// The components destined for `stream`, in order, with the empty ones dropped.
    pub fn parts_for(&self, stream: Stream) -> Vec<Part<'_>> {
        self.parts()
            .into_iter()
            .filter(|p| p.stream() == stream && !p.text.is_empty())
            .collect()
    }

    /// Everything destined for `stream`, concatenated in order.
    pub fn text_for(&self, stream: Stream) -> String {
        self.parts_for(stream).iter().map(|p| p.text).collect()
    }

    /// Whether the run aborted with a traceback worth showing. A surface that labels a failure (a
    /// serve worker naming which worker died) asks this rather than re-deriving `trace.len() >= 2`.
    pub fn aborted(&self) -> bool {
        !self.traceback.is_empty()
    }

    /// The program's own exit code, unclamped — the value the differential oracles compare.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// The **process** exit status for this run: a `u8`, because that is all a process can report.
    ///
    /// Out-of-range codes clamp to `1`, not to their low byte. `as u8` truncates — a program
    /// exiting 256 would exit **0**, turning a failure into a success. Two tails did that; this is
    /// the one conversion in the tree.
    pub fn status(&self) -> u8 {
        u8::try_from(self.exit_code).unwrap_or(1)
    }

    /// [`status`](RunTail::status) as a [`std::process::ExitCode`], for a `main` that returns one.
    pub fn process_exit_code(&self) -> ExitCode {
        ExitCode::from(self.status())
    }

    /// Write the tail to two sinks in canonical order, flushing stdout before the first stderr byte
    /// so the two streams interleave the way they were produced. The testable half of
    /// [`emit`](RunTail::emit).
    pub fn write_to(&self, out: &mut dyn Write, err: &mut dyn Write) -> std::io::Result<()> {
        for part in self.parts() {
            if part.text.is_empty() {
                continue;
            }
            match part.stream() {
                Stream::Stdout => out.write_all(part.text.as_bytes())?,
                Stream::Stderr => {
                    // Everything already on stdout must land before a stderr byte does; a terminal
                    // shows one interleaving and a captured log another otherwise.
                    out.flush()?;
                    err.write_all(part.text.as_bytes())?;
                }
            }
        }
        out.flush()?;
        err.flush()
    }

    /// Write the tail to the process streams and return the process exit status. **This is the
    /// call every run surface makes.** IO errors on the process streams are ignored: a closed pipe
    /// is not the program's failure and must not change its exit code.
    pub fn emit_status(&self) -> u8 {
        let _ = self.write_to(&mut std::io::stdout(), &mut std::io::stderr());
        self.status()
    }

    /// [`emit_status`](RunTail::emit_status) for a `main` that returns an [`ExitCode`].
    pub fn emit(&self) -> ExitCode {
        ExitCode::from(self.emit_status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_diagnostics::{Diagnostic, DiagnosticCode};
    use noeta_span::{Source, SourceId, Span};

    fn sources() -> SourceMap {
        SourceMap::new(vec![Source::new(
            SourceId::FIRST,
            "app.noe",
            "echo \"hi\"\n",
        )])
    }

    fn result(stdout: &str, stderr: &str, exit_code: i32) -> RunResult {
        RunResult {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn the_program_streams_survive_the_tail() {
        // The row-1 defect in one line: four tails wrote `stdout` and dropped `stderr`.
        let tail = RunTail::render(&result("out\n", "err\n", 0), &[], &sources());
        assert_eq!(tail.text_for(Stream::Stdout), "out\n");
        assert_eq!(tail.text_for(Stream::Stderr), "err\n");
    }

    #[test]
    fn the_program_stderr_precedes_the_diagnostics_and_the_traceback() {
        let mut run = result("", "step one\n", 1);
        run.diagnostics.push(Diagnostic::error(
            DiagnosticCode::Panic,
            Span::new_in(SourceId::FIRST, 0, 4),
            "kaboom",
        ));
        let trace = vec![
            TraceFrame {
                name: Some("boom".to_string()),
                span: None,
            },
            TraceFrame {
                name: Some("main".to_string()),
                span: None,
            },
        ];
        let tail = RunTail::render(&run, &trace, &sources());
        let stderr = tail.text_for(Stream::Stderr);
        let program = stderr.find("step one").expect("program stderr");
        let diagnostic = stderr.find("kaboom").expect("diagnostics");
        let traceback = stderr.find("stack trace").expect("traceback");
        assert!(program < diagnostic, "{stderr:?}");
        assert!(diagnostic < traceback, "{stderr:?}");
        assert!(tail.aborted());
    }

    #[test]
    fn a_single_frame_trace_renders_no_traceback() {
        let trace = vec![TraceFrame {
            name: Some("main".to_string()),
            span: None,
        }];
        let tail = RunTail::render(&result("", "", 1), &trace, &sources());
        assert!(!tail.aborted());
        assert_eq!(tail.text_for(Stream::Stderr), "");
    }

    #[test]
    fn an_out_of_range_exit_code_clamps_to_one_and_never_truncates() {
        // `result.exit_code as u8` makes 256 exit 0 — a failure reported as a success. Two tails
        // shipped that; this is the property that says they cannot again.
        assert_eq!(
            RunTail::render(&result("", "", 256), &[], &sources()).status(),
            1
        );
        assert_eq!(
            RunTail::render(&result("", "", 512), &[], &sources()).status(),
            1
        );
        assert_eq!(
            RunTail::render(&result("", "", -1), &[], &sources()).status(),
            1
        );
        assert_eq!(
            RunTail::render(&result("", "", 3), &[], &sources()).status(),
            3
        );
        assert_eq!(
            RunTail::render(&result("", "", 255), &[], &sources()).status(),
            255
        );
        // The unclamped code stays available for the oracles that compare it.
        assert_eq!(
            RunTail::render(&result("", "", 256), &[], &sources()).exit_code(),
            256
        );
    }

    #[test]
    fn every_component_is_classified_onto_a_stream_and_reachable_from_parts() {
        // The seam this type exists for: `parts()` enumerates ALL components, so a surface that
        // composes structured output (the wasi:http edge) cannot silently miss one. `parts()`
        // destructures `Self` exhaustively, so this count and the field count move together.
        let tail = RunTail::render(&result("o", "e", 0), &[], &sources()).with_report("r");
        let parts = tail.parts();
        assert_eq!(
            parts.len(),
            5,
            "a new component must join the canonical order"
        );
        for expected in [
            Component::Stdout,
            Component::ProgramStderr,
            Component::Diagnostics,
            Component::Traceback,
            Component::Report,
        ] {
            assert!(
                parts.iter().any(|p| p.component == expected),
                "{expected:?} is not reachable from parts()"
            );
        }
        assert_eq!(
            parts
                .iter()
                .filter(|p| p.stream() == Stream::Stdout)
                .count(),
            1,
            "only the program's own stdout is stdout"
        );
    }

    #[test]
    fn the_report_trails_everything_else() {
        let tail = RunTail::render(&result("out\n", "err\n", 0), &[], &sources())
            .with_report("── JIT report ──\n");
        let stderr = tail.text_for(Stream::Stderr);
        assert!(stderr.starts_with("err\n"), "{stderr:?}");
        assert!(stderr.ends_with("── JIT report ──\n"), "{stderr:?}");
    }

    #[test]
    fn write_to_orders_the_two_sinks_the_same_way() {
        let tail = RunTail::render(&result("out\n", "err\n", 0), &[], &sources());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        tail.write_to(&mut out, &mut err).expect("write");
        assert_eq!(String::from_utf8(out).unwrap(), "out\n");
        assert_eq!(String::from_utf8(err).unwrap(), "err\n");
    }
}
