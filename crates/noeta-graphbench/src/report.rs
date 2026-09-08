//! The report: one row per (arm, category), on stdout and as JSON.
//!
//! A row is either **scored** or **SKIP**, and a SKIP names the tool the arm is missing. The two
//! statuses print in different columns on purpose: a benchmark that renders "the tool does not
//! exist" as a zero reads as a measurement, and one that renders it as a pass reads as a result.

use std::collections::BTreeMap;

use crate::arms::Arm;
use crate::corpus::GateResult;
use crate::metrics::{Outcome, Score};
use crate::question::Category;
use crate::service::{Spend, Tool};

/// Whether a row carries a measurement.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Scored,
    /// The arm needs a tool the service does not advertise.
    Skipped {
        reason: String,
    },
}

impl Status {
    pub fn scored(&self) -> bool {
        matches!(self, Status::Scored)
    }
}

/// One (arm, category) measurement.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Row {
    pub arm: Arm,
    pub category: Category,
    pub status: Status,
    pub score: Score,
    pub spend: Spend,
    /// How often each tool supplied the evidence for an answer.
    pub evidence: BTreeMap<Tool, usize>,
    /// Every question's own outcome, for the JSON report.
    pub outcomes: Vec<Outcome>,
}

impl Row {
    /// The tool that supplied the most evidence, which is the column the table prints.
    pub fn leading_evidence(&self) -> Option<Tool> {
        self.evidence
            .iter()
            .max_by_key(|(tool, count)| (**count, std::cmp::Reverse(**tool)))
            .map(|(tool, _)| *tool)
    }

    /// Average tokens of tool output per question.
    pub fn tokens_per_question(&self) -> usize {
        if self.score.questions == 0 {
            return 0;
        }
        self.spend.tokens() / self.score.questions
    }

    /// Average logical tool calls per question.
    pub fn calls_per_question(&self) -> f64 {
        if self.score.questions == 0 {
            return 0.0;
        }
        self.spend.calls as f64 / self.score.questions as f64
    }
}

/// A whole run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Report {
    pub gate: Vec<GateResult>,
    pub tools: Vec<String>,
    pub questions: BTreeMap<String, usize>,
    pub rows: Vec<Row>,
    /// The tool this run stubbed to its empty answer, when it is an ablation.
    pub ablated: Option<Tool>,
}

impl Report {
    /// Render the per-arm-per-category table.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("graphbench: corpus\n");
        for gate in &self.gate {
            out.push_str(&format!(
                "  {:<18} {:>3} files  linked={}  errors={}  warnings={}{}\n",
                gate.name,
                gate.files,
                gate.linked,
                gate.errors,
                gate.warnings,
                gate.first_problem
                    .as_ref()
                    .map(|p| format!("  <- {p}"))
                    .unwrap_or_default(),
            ));
        }
        out.push_str("\ngraphbench: questions\n");
        for (category, count) in &self.questions {
            out.push_str(&format!("  {category:<14} {count:>4}\n"));
        }
        if let Some(tool) = self.ablated {
            out.push_str(&format!(
                "\ngraphbench: ABLATED `{tool}` to its empty answer\n"
            ));
        }
        out.push_str(&format!(
            "\n{:<4} {:<14} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>6} {:>7}  {}\n",
            "arm",
            "category",
            "n",
            "P",
            "R",
            "F1",
            "Acc@1",
            "Acc@5",
            "MRR",
            "tokens",
            "calls",
            "ms",
            "evidence",
        ));
        for row in &self.rows {
            match &row.status {
                Status::Skipped { reason } => out.push_str(&format!(
                    "{:<4} {:<14} {:>4} {:>6} {:>6} {:>6} {:>6} {:>6} {:>6} {:>8} {:>6} {:>7}  SKIP: {}\n",
                    row.arm.as_str(),
                    row.category.as_str(),
                    row.score.questions,
                    "-",
                    "-",
                    "SKIP",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    reason,
                )),
                Status::Scored => out.push_str(&format!(
                    "{:<4} {:<14} {:>4} {:>6.3} {:>6.3} {:>6.3} {:>6.3} {:>6.3} {:>6.3} {:>8} {:>6.1} {:>7}  {}\n",
                    row.arm.as_str(),
                    row.category.as_str(),
                    row.score.questions,
                    row.score.precision,
                    row.score.recall,
                    row.score.f1,
                    row.score.acc_at_1,
                    row.score.acc_at_5,
                    row.score.mrr,
                    row.tokens_per_question(),
                    row.calls_per_question(),
                    row.spend.millis,
                    row.leading_evidence()
                        .map(|t| t.as_str())
                        .unwrap_or("none"),
                )),
            }
        }
        out
    }

    /// Write the machine-readable form.
    pub fn write_json(&self, path: &std::path::Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("cannot serialize the report: {e}"))?;
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}
