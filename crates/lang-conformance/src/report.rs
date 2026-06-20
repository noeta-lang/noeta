//! The result of running cases: per-case outcomes and an aggregate report, renderable
//! as human text or machine-readable JSON (agents parse the JSON; humans read the text).

use serde::Serialize;

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
