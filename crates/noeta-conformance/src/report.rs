//! The result of running cases: per-case outcomes and an aggregate report, renderable
//! as human text or machine-readable JSON (agents parse the JSON; humans read the text).

use serde::Serialize;

/// Why corpus cases never reached the stage an oracle measures — the shared tally every oracle
/// (differential, jit-differential, bundle, ir-lowering, wasm, leaks) folds its exclusions into.
///
/// These are kept **apart** because they are different facts, and collapsing them hides the one
/// that matters. Each oracle used to count "the checker rejected it" as a *match* — defensible
/// about agreement (a program that never runs trivially agrees on both backends, and the corpus
/// harness asserts its diagnostics separately) but badly misleading as *coverage*: it made a
/// headline number of 793 "matched" out of only 576 programs that actually ran, and a fixture that
/// silently stopped compiling moved from one side of that number to the other without changing it.
/// Reporting the exclusions by reason makes rot visible as a rising count instead of invisible.
///
/// Likewise `parse_failed` used to absorb link failures and I/O errors, so "16 parse-failed" could
/// mean four unparseable programs and twelve that failed to link.
#[derive(Debug, Default, Clone, Serialize)]
pub struct NotRun {
    /// The case file could not be read at all.
    pub read_failed: usize,
    /// Lexer/parser diagnostics — no program to run. Corpus negatives (`// expect: error E0003…`)
    /// land here legitimately.
    pub parse_failed: usize,
    /// A multi-file fixture whose modules failed to load/link.
    pub link_failed: usize,
    /// The checker rejected it, so it never reached a backend. Its diagnostics are its whole
    /// result — which the corpus harness asserts — but it contributes **no** backend coverage.
    pub checker_rejected: usize,
    /// Outside the subset the oracle measures (e.g. the VM cannot compile it yet).
    pub unsupported: usize,
}

impl NotRun {
    /// Every excluded case, whatever the reason.
    pub fn total(&self) -> usize {
        self.read_failed
            + self.parse_failed
            + self.link_failed
            + self.checker_rejected
            + self.unsupported
    }

    /// The exclusions as `"217 checker-rejected, 14 parse-failed, 2 link-failed"` — reasons with a
    /// nonzero count only, so the common case stays short and a new nonzero reason stands out.
    pub fn to_human(&self) -> String {
        let parts = [
            (self.checker_rejected, "checker-rejected"),
            (self.parse_failed, "parse-failed"),
            (self.link_failed, "link-failed"),
            (self.unsupported, "unsupported"),
            (self.read_failed, "unreadable"),
        ];
        let listed: Vec<String> = parts
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, label)| format!("{n} {label}"))
            .collect();
        if listed.is_empty() {
            "none".to_string()
        } else {
            listed.join(", ")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseStatus {
    Pass,
    Fail,
    /// The case's `// expect:` header could not be parsed.
    Malformed,
}

/// The outcome of one conformance case.
#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub name: String,
    pub status: CaseStatus,
    /// Human-readable failure descriptions (empty when the case passed). A failure produced by one
    /// engine leads with that engine — `[vm] stdout: expected …` — so a divergence between the two
    /// backends is legible in the failure text itself.
    pub failures: Vec<String>,
    /// The engines that actually executed this case's program. **Empty is a fact, not a gap**: a
    /// case the front end rejected (a negative fixture, a load failure) has no program to run, and
    /// a front-end-only stage runs none at all. A case that *does* run and lists one engine under a
    /// two-engine selection is a case the other engine could not execute — which the failures say.
    pub executed_on: Vec<crate::Engine>,
}

impl CaseResult {
    pub fn pass(name: &str) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status: CaseStatus::Pass,
            failures: Vec::new(),
            executed_on: Vec::new(),
        }
    }

    pub fn fail(name: &str, failures: Vec<String>) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status: CaseStatus::Fail,
            failures,
            executed_on: Vec::new(),
        }
    }

    pub fn malformed(name: &str, message: impl Into<String>) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status: CaseStatus::Malformed,
            failures: vec![message.into()],
            executed_on: Vec::new(),
        }
    }
}

/// The aggregate result of a corpus run.
///
/// It carries what the run *was* as well as what it found: the stage it ran and the engines it
/// validated against. A bare "1265 passed" reads the same whether the bytecode VM was exercised or
/// never built a module, and the whole point of the corpus is that the language is implemented
/// twice — so the answer travels with the numbers, in the text and in the JSON alike.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub cases: Vec<CaseResult>,
    /// The pipeline stage the cases ran through.
    pub stage: crate::Stage,
    /// The engines whose output was checked against every `// expect:` header. Empty for a stage
    /// that executes no program.
    pub engines: Vec<crate::Engine>,
}

impl Report {
    pub fn push(&mut self, case: CaseResult) {
        self.cases.push(case);
    }

    /// How many cases executed their program on `engine`. The run's *reach*: a corpus run whose VM
    /// count is zero validated one implementation of the language while reading as a verdict on
    /// the language.
    pub fn executed_on(&self, engine: crate::Engine) -> usize {
        self.cases
            .iter()
            .filter(|c| c.executed_on.contains(&engine))
            .count()
    }

    /// How many cases executed their program on at least one engine.
    pub fn executed(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| !c.executed_on.is_empty())
            .count()
    }

    pub fn passed(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| c.status == CaseStatus::Pass)
            .count()
    }

    pub fn failed(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| c.status != CaseStatus::Pass)
            .count()
    }

    pub fn all_passed(&self) -> bool {
        self.failed() == 0
    }

    /// Pretty, human-facing summary. Failing cases list their mismatches.
    pub fn to_human(&self) -> String {
        let mut out = String::new();
        for case in &self.cases {
            match case.status {
                CaseStatus::Pass => out.push_str(&format!("  ok    {}\n", case.name)),
                CaseStatus::Fail => {
                    out.push_str(&format!("  FAIL  {}\n", case.name));
                    for failure in &case.failures {
                        out.push_str(&format!("          {failure}\n"));
                    }
                }
                CaseStatus::Malformed => {
                    out.push_str(&format!("  ERROR {} (malformed case)\n", case.name));
                    for failure in &case.failures {
                        out.push_str(&format!("          {failure}\n"));
                    }
                }
            }
        }
        out.push_str(&format!(
            "\n{} passed, {} failed, {} total\n",
            self.passed(),
            self.failed(),
            self.cases.len()
        ));
        out.push_str(&self.coverage_line());
        out
    }

    /// What the numbers above are a verdict *on*: the engines the headers were checked against, and
    /// how many cases reached one. Printed on every run, including the narrowed ones — a `--file`
    /// run is what gets cited as the evidence that a fix works, so it has to say what it drove.
    pub fn coverage_line(&self) -> String {
        if self.engines.is_empty() {
            return format!(
                "stage {}: `// expect: error` lines only — no program ran, so stdout, stderr and \
                 exit code went unchecked\n",
                self.stage
            );
        }
        let named: Vec<&str> = self.engines.iter().map(|e| e.description()).collect();
        let counts: Vec<String> = self
            .engines
            .iter()
            .map(|e| format!("{} on {e}", self.executed_on(*e)))
            .collect();
        format!(
            "expectations checked against {}: {}; {} ran no program (the front end rejected \
             them)\n",
            join_and(&named),
            counts.join(", "),
            self.cases.len() - self.executed(),
        )
    }

    /// Machine-readable JSON for agentic consumption. Carries `stage`, `engines` and each case's
    /// `executed_on` for the same reason the text does: a consumer counting `"status": "pass"` has
    /// no other way to tell which implementation passed.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Report serializes cleanly")
    }
}

/// `["a"] → "a"`, `["a", "b"] → "a and b"`, `["a", "b", "c"] → "a, b and c"`.
fn join_and(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod coverage_line_tests {
    use super::{CaseResult, Report};
    use crate::{Engine, Stage};

    fn case(name: &str, executed_on: Vec<Engine>) -> CaseResult {
        let mut result = CaseResult::pass(name);
        result.executed_on = executed_on;
        result
    }

    /// The summary has to name the engines and count what each one ran, because "2 passed" reads
    /// identically whether the bytecode VM executed both programs or never compiled one.
    #[test]
    fn it_names_every_engine_and_what_it_ran() {
        let report = Report {
            cases: vec![
                case("a", vec![Engine::Reference, Engine::Vm]),
                case("b", vec![Engine::Reference, Engine::Vm]),
                case("negative", Vec::new()),
            ],
            stage: Stage::Eval,
            engines: vec![Engine::Reference, Engine::Vm],
        };
        assert_eq!(
            report.coverage_line(),
            "expectations checked against the reference interpreter and the bytecode VM: 2 on \
             reference, 2 on vm; 1 ran no program (the front end rejected them)\n"
        );
        assert_eq!(report.executed_on(Engine::Vm), 2);
        assert_eq!(report.executed(), 2);
    }

    /// A front-end stage evaluates nothing, so it claims no engine — and says which assertions it
    /// therefore skipped, rather than letting a pass imply the program's output was checked.
    #[test]
    fn a_front_end_stage_says_what_it_did_not_check() {
        let report = Report {
            cases: vec![case("a", Vec::new())],
            stage: Stage::Parser,
            engines: Vec::new(),
        };
        assert_eq!(
            report.coverage_line(),
            "stage parser: `// expect: error` lines only — no program ran, so stdout, stderr and \
             exit code went unchecked\n"
        );
        assert_eq!(report.executed(), 0);
    }

    #[test]
    fn one_engine_reads_as_one_engine() {
        let report = Report {
            cases: vec![case("a", vec![Engine::Vm])],
            stage: Stage::Eval,
            engines: vec![Engine::Vm],
        };
        assert_eq!(
            report.coverage_line(),
            "expectations checked against the bytecode VM: 1 on vm; 0 ran no program (the front \
             end rejected them)\n"
        );
    }
}

#[cfg(test)]
mod not_run_tests {
    use super::NotRun;

    /// The whole point of the type: a reader can tell *why* cases were excluded. Reasons with a
    /// zero count stay out so the line is short, and the ones present read in severity order.
    #[test]
    fn to_human_lists_only_nonzero_reasons() {
        let tally = NotRun {
            checker_rejected: 216,
            parse_failed: 11,
            link_failed: 5,
            ..NotRun::default()
        };
        assert_eq!(
            tally.to_human(),
            "216 checker-rejected, 11 parse-failed, 5 link-failed"
        );
        assert_eq!(tally.total(), 232);
    }

    /// No exclusions must not render as an empty string — a bare "0 not run ()" reads like a bug.
    #[test]
    fn an_empty_tally_says_none() {
        assert_eq!(NotRun::default().to_human(), "none");
        assert_eq!(NotRun::default().total(), 0);
    }

    /// Every reason is counted in `total`, including the ones that used to be silently dropped
    /// (the leak oracle counted checker-rejected cases as nothing at all) or folded into
    /// `parse_failed` (link failures and unreadable files).
    #[test]
    fn total_counts_every_reason() {
        let tally = NotRun {
            read_failed: 1,
            parse_failed: 2,
            link_failed: 3,
            checker_rejected: 4,
            unsupported: 5,
        };
        assert_eq!(tally.total(), 15);
        let human = tally.to_human();
        for reason in [
            "1 unreadable",
            "2 parse-failed",
            "3 link-failed",
            "4 checker-rejected",
            "5 unsupported",
        ] {
            assert!(human.contains(reason), "{human} must mention {reason}");
        }
    }
}
