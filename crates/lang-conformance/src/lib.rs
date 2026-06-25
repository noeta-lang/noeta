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

use std::path::{Path, PathBuf};

use lang_diagnostics::Diagnostic;
use lang_eval::TreeWalkBackend;
use lang_lexer::lex;
use lang_parser::parse;
use lang_span::{Source, SourceId, SourceMap};

mod differential;
mod expectation;
mod leaks;
mod report;

pub use differential::{DiffReport, Mismatch, run_differential};
pub use expectation::{ErrorExpectation, Expectations};
pub use leaks::{Leak, LeakReport, run_leak_check};
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
        // One `check_all` yields both the gate diagnostics and the `type_of` site map the eval
        // backend needs, so the checker runs once per case instead of again inside the backend.
        let mut type_of_sites = std::collections::HashMap::new();
        if stage == Stage::Eval && diagnostics.is_empty() {
            let checked = lang_check::check_all(&parsed.program);
            diagnostics.extend(checked.diagnostics);
            type_of_sites = checked.type_of_sites;
        }

        // Only evaluate a program that checked cleanly and only when asked to.
        if stage == Stage::Eval && diagnostics.is_empty() {
            let result = TreeWalkBackend::new().run_with_sites(&parsed.program, type_of_sites);
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

    Outcome {
        stdout,
        exit_code,
        errors: errors_of(&source, &diagnostics),
    }
}

/// Map each diagnostic to its `(code, line, col)` expectation, resolved against `source`.
fn errors_of(source: &Source, diagnostics: &[Diagnostic]) -> Vec<ErrorExpectation> {
    diagnostics
        .iter()
        .map(|d| expectation(d, source.line_col(d.span.start)))
        .collect()
}

/// Map each diagnostic to its `(code, line, col)` expectation, resolving each span against the
/// source it belongs to (its `SourceId`) via the [`SourceMap`]. Used for the linked path, where a
/// diagnostic on a merged-in sibling declaration must render against that sibling, not the entry.
fn errors_of_mapped(sources: &SourceMap, diagnostics: &[Diagnostic]) -> Vec<ErrorExpectation> {
    diagnostics
        .iter()
        .map(|d| expectation(d, sources.line_col(d.span)))
        .collect()
}

fn expectation(d: &Diagnostic, at: lang_span::LineCol) -> ErrorExpectation {
    ErrorExpectation {
        code: d.code.to_string(),
        line: at.line,
        col: at.col,
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

/// Run a multi-file case rooted at `entry` (the `main.lang` of a module fixture): sibling
/// modules are loaded and linked (M1.9) and the merged program is checked and run like any
/// other case. The expectation header lives in the entry file.
pub fn run_case_path(entry: &Path, display: &str, stage: Stage) -> CaseResult {
    let text = match std::fs::read_to_string(entry) {
        Ok(text) => text,
        Err(err) => return CaseResult::malformed(display, format!("could not read: {err}")),
    };
    let expectations = match Expectations::parse(&text) {
        Ok(expectations) => expectations,
        Err(message) => return CaseResult::malformed(display, message),
    };
    let outcome = run_linked(entry, stage);
    compare(display, &expectations, &outcome, stage)
}

/// Load + link `entry` and run the merged program to an [`Outcome`]. Lex/parse errors render
/// against the source they came from; check/runtime diagnostics against the entry source.
fn run_linked(entry: &Path, stage: Stage) -> Outcome {
    let linked = match lang_loader::load(entry) {
        Ok(Ok(linked)) => linked,
        Ok(Err(load_diagnostics)) => {
            let errors = load_diagnostics
                .iter()
                .flat_map(|ld| errors_of(&ld.source, std::slice::from_ref(&ld.diagnostic)))
                .collect();
            return Outcome {
                stdout: String::new(),
                exit_code: 1,
                errors,
            };
        }
        Err(err) => {
            return Outcome {
                stdout: format!("could not read: {err}"),
                exit_code: 1,
                errors: Vec::new(),
            };
        }
    };

    // The loader already lexed + parsed cleanly; the lexer/parser stages have nothing more to do.
    if stage != Stage::Eval {
        return Outcome {
            stdout: String::new(),
            exit_code: 0,
            errors: Vec::new(),
        };
    }

    // Check/runtime diagnostics may land on a declaration merged in from a sibling module, so they
    // resolve through the source map (by each span's `SourceId`) rather than always against the
    // entry — that is what gives a cross-module error its real file/line/column.
    let checked = lang_check::check_all(&linked.program);
    if !checked.diagnostics.is_empty() {
        return Outcome {
            stdout: String::new(),
            exit_code: 1,
            errors: errors_of_mapped(&linked.sources, &checked.diagnostics),
        };
    }
    let result = TreeWalkBackend::new().run_with_sites(&linked.program, checked.type_of_sites);
    Outcome {
        errors: errors_of_mapped(&linked.sources, &result.diagnostics),
        stdout: result.stdout,
        exit_code: result.exit_code,
    }
}

/// Discover and run every case under `root`, optionally narrowed to a single entry file.
/// Returns a [`Report`] that can be rendered as text or JSON.
pub fn run_corpus(root: &Path, only: Option<&Path>, stage: Stage) -> Report {
    let mut cases = Vec::new();
    collect_cases(root, &mut cases);
    cases.sort_by(|a, b| a.entry.cmp(&b.entry));

    let mut report = Report::default();
    for case in cases {
        if let Some(only) = only
            && case.entry != only
            && !case.entry.ends_with(only)
        {
            continue;
        }
        let display = case
            .entry
            .strip_prefix(root)
            .unwrap_or(&case.entry)
            .to_string_lossy()
            .into_owned();
        if case.multi {
            report.push(run_case_path(&case.entry, &display, stage));
        } else {
            match std::fs::read_to_string(&case.entry) {
                Ok(text) => report.push(run_case(&display, &text, stage)),
                Err(err) => report.push(CaseResult::malformed(
                    &display,
                    format!("could not read: {err}"),
                )),
            }
        }
    }
    report
}

/// One discovered case: its entry `.lang` file and whether it is a multi-file module fixture.
pub(crate) struct Case {
    pub entry: PathBuf,
    pub multi: bool,
}

/// Discover cases under `dir`. A directory that directly contains a `main.lang` is a single
/// **multi-file** case — its other `.lang` files are that program's modules, not standalone
/// cases — so discovery does not descend into it. Every other `.lang` file is its own
/// single-file case.
pub(crate) fn collect_cases(dir: &Path, out: &mut Vec<Case>) {
    let main = dir.join("main.lang");
    if main.is_file() {
        out.push(Case {
            entry: main,
            multi: true,
        });
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cases(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "lang") {
            out.push(Case {
                entry: path,
                multi: false,
            });
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
