//! The baseline: the floor each scored row holds, and the ceiling on what it costs to get there.
//!
//! `tests/graphbench/baseline.txt` carries one row per (arm, category) with an F1 and a token size.
//! A default run measures, compares, and exits non-zero when an F1 falls below its floor or a token
//! size rises above its ceiling. `--record` rewrites the file from the current run.
//!
//! The tolerances are tight because nothing here varies. There is no model, no sampling, no clock in
//! any answer and no dependency on machine speed, so two runs of the same tree produce byte-equal
//! numbers. The F1 tolerance is half of the last printed digit, which is rounding and nothing else,
//! and the token ceiling is exact. A row that moves has moved because the code moved.

use std::collections::BTreeMap;
use std::path::Path;

use crate::arms::Arm;
use crate::question::Category;
use crate::report::Report;

/// How far an F1 may fall below its recorded floor: half of the last printed digit.
pub const F1_TOLERANCE: f64 = 0.0005;

/// How far a row's token size may rise above its recorded ceiling.
pub const TOKEN_TOLERANCE: usize = 0;

/// One recorded row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recorded {
    pub f1: f64,
    pub tokens: usize,
    pub questions: usize,
}

/// The whole baseline, keyed by arm and category.
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    pub rows: BTreeMap<(String, String), Recorded>,
}

impl Baseline {
    pub fn read(path: &Path) -> Result<Baseline, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("no baseline at {}: {e}", path.display()))?;
        let mut rows = BTreeMap::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_whitespace();
            if fields.next() != Some("row") {
                return Err(format!(
                    "{}:{}: a line that is neither blank, a comment, nor a `row`",
                    path.display(),
                    number + 1
                ));
            }
            let arm = fields.next().ok_or("a row needs an arm")?.to_string();
            let category = fields.next().ok_or("a row needs a category")?.to_string();
            let mut recorded = Recorded {
                f1: 0.0,
                tokens: 0,
                questions: 0,
            };
            for field in fields {
                let (key, value) = field
                    .split_once('=')
                    .ok_or_else(|| format!("`{field}` is not `key=value`"))?;
                match key {
                    "f1" => {
                        recorded.f1 = value
                            .parse()
                            .map_err(|_| format!("`{value}` is no number"))?
                    }
                    "tokens" => {
                        recorded.tokens = value
                            .parse()
                            .map_err(|_| format!("`{value}` is no number"))?
                    }
                    "n" => {
                        recorded.questions = value
                            .parse()
                            .map_err(|_| format!("`{value}` is no number"))?
                    }
                    _ => return Err(format!("`{key}` is no baseline field")),
                }
            }
            rows.insert((arm, category), recorded);
        }
        Ok(Baseline { rows })
    }

    pub fn get(&self, arm: Arm, category: Category) -> Option<Recorded> {
        self.rows
            .get(&(arm.as_str().to_string(), category.as_str().to_string()))
            .copied()
    }
}

/// Render the baseline file for a run.
pub fn record(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(
        "# Baseline for `cargo run -p noeta-graphbench` — one row per (arm, category).\n\
         #\n\
         # `f1` is the floor and `tokens` is the ceiling. A default run compares against them and\n\
         # exits non-zero when an F1 falls below its floor or a row's tokens-per-question rises\n\
         # above its ceiling.\n\
         #\n\
         # TOLERANCE. F1 may fall by 0.0005, which is half of the last printed digit and therefore\n\
         # rounding; tokens may not rise at all. The bands are that tight because nothing in this\n\
         # measurement varies: the retrieval layer has no model in it, no sampling, no clock in any\n\
         # answer, and no dependency on machine speed, so two runs of one tree produce byte-equal\n\
         # numbers. That is the difference from the instructions-retired perf ratchet, whose bands\n\
         # exist because a hardware counter has spread. Here a row that moved, moved because the\n\
         # code moved.\n\
         #\n\
         # Wall time is recorded in `--json` and gated nowhere: this box carries several concurrent\n\
         # agent builds, so a millisecond column measures the afternoon.\n\
         #\n\
         # An arm that reports SKIP has no row. A SKIP is never a pass, and a baseline that recorded\n\
         # one as a zero would turn \"the tool does not exist yet\" into a measurement.\n\
         #\n\
         # Re-record with:  cargo run -p noeta-graphbench -- --record\n\
         # Read the diff's sign: F1 up is an improvement worth pinning, F1 down needs a reason, and\n\
         # tokens up needs the sentence saying what the extra context bought.\n\n",
    );
    for row in &report.rows {
        if !row.status.scored() {
            continue;
        }
        out.push_str(&format!(
            "row {:<3} {:<14} f1={:.4} tokens={} n={}\n",
            row.arm.as_str(),
            row.category.as_str(),
            row.score.f1,
            row.tokens_per_question(),
            row.score.questions,
        ));
    }
    out
}

/// One way a run fell short of its baseline.
#[derive(Debug, Clone)]
pub struct Regression {
    pub arm: Arm,
    pub category: Category,
    pub what: String,
}

/// Compare a run against a baseline. An empty answer is a pass.
pub fn compare(report: &Report, baseline: &Baseline) -> Vec<Regression> {
    let mut out = Vec::new();
    for row in &report.rows {
        if !row.status.scored() {
            continue;
        }
        let Some(recorded) = baseline.get(row.arm, row.category) else {
            out.push(Regression {
                arm: row.arm,
                category: row.category,
                what: "no baseline row — record one with `--record`".to_string(),
            });
            continue;
        };
        if row.score.questions != recorded.questions {
            out.push(Regression {
                arm: row.arm,
                category: row.category,
                what: format!(
                    "the corpus asks {} questions, the baseline recorded {} — re-record after a \
                     corpus change",
                    row.score.questions, recorded.questions
                ),
            });
            continue;
        }
        if row.score.f1 + F1_TOLERANCE < recorded.f1 {
            out.push(Regression {
                arm: row.arm,
                category: row.category,
                what: format!(
                    "F1 {:.4} is below the floor {:.4}",
                    row.score.f1, recorded.f1
                ),
            });
        }
        let tokens = row.tokens_per_question();
        if tokens > recorded.tokens + TOKEN_TOLERANCE {
            out.push(Regression {
                arm: row.arm,
                category: row.category,
                what: format!(
                    "{tokens} tokens per question is above the ceiling {}",
                    recorded.tokens
                ),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_baseline_round_trips_through_its_own_text() {
        let text = "# a comment\n\nrow A0  callers        f1=0.8125 tokens=940 n=8\n";
        let path = std::env::temp_dir().join("graphbench-baseline-round-trip.txt");
        std::fs::write(&path, text).expect("write");
        let baseline = Baseline::read(&path).expect("read");
        let recorded = baseline
            .get(Arm::A0Today, Category::Callers)
            .expect("the row");
        assert_eq!(recorded.tokens, 940);
        assert_eq!(recorded.questions, 8);
        assert!((recorded.f1 - 0.8125).abs() < 1e-9);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let path = std::env::temp_dir().join("graphbench-baseline-unknown-field.txt");
        std::fs::write(&path, "row A0 callers f1=0.5 speed=9\n").expect("write");
        assert!(Baseline::read(&path).is_err());
        std::fs::remove_file(&path).ok();
    }
}
