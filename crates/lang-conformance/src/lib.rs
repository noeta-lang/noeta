//! The conformance harness: the executable specification.
//!
//! A conformance case is a `.lang` file with an expectation header:
//!
//! ```text
//! // expect: stdout "Order #1 awaiting payment"
//! // expect: exit 0
//! ```
//!
//! and negative cases:
//!
//! ```text
//! // expect: error E0003 at 12:5
//! // expect: exit 1
//! ```
//!
//! This corpus *is* the language spec in executable form — every feature lands with
//! cases here. The same runner powers the language's own suite and (later) user-facing
//! `lang test`. Output is available as human text or machine-readable JSON, and runs
//! can be narrowed by file or by pipeline stage so an agent's loop stays fast.

use lang_eval::{Backend, TreeWalkBackend};
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId};

mod differential;
mod expectation;
mod report;

pub use differential::{DiffReport, Mismatch, run_differential};
pub use expectation::{ErrorExpectation, Expectations};
pub use report::{CaseResult, CaseStatus, Report};

/// Which pipeline stages to run a case through. Narrowing the stage makes an agent's
/// inner loop fast (`--stage parser` reruns only lexing+parsing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stage {
    Lexer,
    Parser,
    #[default]
    Eval,
}

impl Stage {
    pub fn parse(name: &str) -> Option<Stage> {
        match name {
            "lexer" => Some(Stage::Lexer),
            "parser" => Some(Stage::Parser),
            "eval" => Some(Stage::Eval),
            _ => None,
        }
    }
}

/// What actually happened when a case ran: its captured stdout, exit code, and the
/// `(code, line, col)` of every diagnostic, in order.
struct Outcome {
    stdout: String,
    exit_code: i32,
    errors: Vec<ErrorExpectation>,
}

/// Run a source string through the pipeline up to `stage` and capture its outcome.
fn run_source(name: &str, text: &str, stage: Stage) -> Outcome {
    let source = Source::new(SourceId::FIRST, name, text);

    let lexed = lex(&source);
    let mut diagnostics = lexed.diagnostics.clone();

    let mut stdout = String::new();
    let mut exit_code;

    if stage == Stage::Lexer {
        exit_code = if diagnostics.is_empty() { 0 } else { 1 };
    } else {
        let parsed = parse(&source, &lexed.tokens);
        diagnostics.extend(parsed.diagnostics);

        // The type checker (M1.7) is the front-end gate for the eval stage: a program with type
        // errors is rejected before it runs, exactly as the bytecode pipeline gates it. Running
        // it here (not only on the VM path) is what lets a negative type-error case assert via
        // `// expect: error E00xx`, and keeps the tree-walker and VM observably identical.
        if stage == Stage::Eval && diagnostics.is_empty() {
            diagnostics.extend(lang_check::check(&parsed.program));
        }

        // Only evaluate a program that checked cleanly and only when asked to.
        if stage == Stage::Eval && diagnostics.is_empty() {
            let result = TreeWalkBackend::new().run(&parsed.program);
            stdout = result.stdout;
            diagnostics.extend(result.diagnostics);
            exit_code = result.exit_code;
        } else {
            exit_code = if diagnostics.is_empty() { 0 } else { 1 };
        }
    }

    // A compile error always means a failing exit, even if a stage stopped early.
    if !diagnostics.is_empty() && exit_code == 0 {
        exit_code = 1;
    }

    let errors = diagnostics
        .iter()
        .map(|d| {
            let at = source.line_col(d.span.start);
            ErrorExpectation {
                code: d.code.to_string(),
                line: at.line,
                col: at.col,
            }
        })
        .collect();

    Outcome {
        stdout,
        exit_code,
        errors,
    }
}

/// Run a single named case (already-loaded source text) and compare it to its header.
pub fn run_case(name: &str, text: &str, stage: Stage) -> CaseResult {
    let expectations = match Expectations::parse(text) {
        Ok(expectations) => expectations,
        Err(message) => return CaseResult::malformed(name, message),
    };
    let outcome = run_source(name, text, stage);
    compare(name, &expectations, &outcome, stage)
}

fn compare(name: &str, expected: &Expectations, actual: &Outcome, stage: Stage) -> CaseResult {
    let mut failures = Vec::new();

    // stdout and exit code only become meaningful once the program is evaluated, so
    // partial-stage runs (`--stage lexer`/`parser`) check error expectations only.
    if stage == Stage::Eval {
        if let Some(expected_exit) = expected.exit
            && expected_exit != actual.exit_code
        {
            failures.push(format!(
                "exit: expected {expected_exit}, got {}",
                actual.exit_code
            ));
        }

        if let Some(expected_lines) = &expected.stdout_lines {
            let actual_lines: Vec<&str> = actual.stdout.lines().collect();
            if actual_lines
                != expected_lines
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            {
                failures.push(format!(
                    "stdout: expected {:?}, got {:?}",
                    expected_lines, actual_lines
                ));
            }
        }
    }

    for expected_error in &expected.errors {
        if !actual.errors.contains(expected_error) {
            failures.push(format!(
                "error: expected {} at {}:{}, but it was not produced (got {:?})",
                expected_error.code, expected_error.line, expected_error.col, actual.errors
            ));
        }
    }

    if failures.is_empty() {
        CaseResult::pass(name)
    } else {
        CaseResult::fail(name, failures)
    }
}

/// Discover and run every `.lang` file under `root`, optionally narrowed to a single
/// file. Returns a [`Report`] that can be rendered as text or JSON.
pub fn run_corpus(root: &std::path::Path, only: Option<&std::path::Path>, stage: Stage) -> Report {
    let mut files = Vec::new();
    collect_lang_files(root, &mut files);
    files.sort();

    let mut report = Report::default();
    for path in files {
        if let Some(only) = only
            && path != only
            && !path.ends_with(only)
        {
            continue;
        }
        let display = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        match std::fs::read_to_string(&path) {
            Ok(text) => report.push(run_case(&display, &text, stage)),
            Err(err) => report.push(CaseResult::malformed(
                &display,
                format!("could not read: {err}"),
            )),
        }
    }
    report
}

pub(crate) fn collect_lang_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lang_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "lang") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passing_case_passes() {
        let case = "// expect: stdout \"hello\"\n// expect: exit 0\necho \"hello\";\n";
        assert_eq!(
            run_case("hello", case, Stage::Eval).status,
            CaseStatus::Pass
        );
    }

    #[test]
    fn wrong_stdout_fails() {
        let case = "// expect: stdout \"goodbye\"\necho \"hello\";\n";
        assert_eq!(run_case("x", case, Stage::Eval).status, CaseStatus::Fail);
    }

    #[test]
    fn negative_case_matches_error_code_and_position() {
        // Positions are absolute in the file; the two header lines push the `echo`
        // to line 3, where the unterminated string opens at column 6.
        let case = "// expect: error E0002 at 3:6\n// expect: exit 1\necho \"oops;\n";
        let result = run_case("bad", case, Stage::Eval);
        assert_eq!(result.status, CaseStatus::Pass, "{:?}", result.failures);
    }

    #[test]
    fn malformed_header_is_reported() {
        let case = "// expect: nonsense\necho \"x\";\n";
        assert_eq!(
            run_case("m", case, Stage::Eval).status,
            CaseStatus::Malformed
        );
    }
}
