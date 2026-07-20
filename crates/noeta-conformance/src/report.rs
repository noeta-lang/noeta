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
    /// Human-readable failure descriptions (empty when the case passed).
    pub failures: Vec<String>,
}

impl CaseResult {
    pub fn pass(name: &str) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status: CaseStatus::Pass,
            failures: Vec::new(),
        }
    }

    pub fn fail(name: &str, failures: Vec<String>) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status: CaseStatus::Fail,
            failures,
        }
    }

    pub fn malformed(name: &str, message: impl Into<String>) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status: CaseStatus::Malformed,
            failures: vec![message.into()],
        }
    }
}

/// The aggregate result of a corpus run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub cases: Vec<CaseResult>,
}

impl Report {
    pub fn push(&mut self, case: CaseResult) {
        self.cases.push(case);
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
        out
    }

    /// Machine-readable JSON for agentic consumption.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Report serializes cleanly")
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
