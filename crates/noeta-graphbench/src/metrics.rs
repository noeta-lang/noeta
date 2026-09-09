//! Scoring: turning what an arm named into precision, recall, F1, `Acc@k` and MRR.
//!
//! An arm answers with whatever identity the tool it used could give it, so scoring first
//! **resolves** each answer against the project's declaration universe. Resolution is strict and
//! deterministic: a byte span pins one declaration, a file plus a leaf name usually does, and a bare
//! leaf name that answers to several resolves to the first of them by file and offset — which is
//! sometimes the wrong one, and that is the cost of a colliding name showing up in the number.
//!
//! Set categories score by macro-averaged precision, recall and F1 over their questions, counting a
//! negative question (gold is empty) as perfect when the arm answered nothing and as zero precision
//! when it answered anything. Ranked categories add `Acc@1`, `Acc@5` and MRR over the same answers
//! in the order the arm produced them.

use std::collections::BTreeSet;

use crate::adapter::NodeRef;
use crate::gold::Facts;
use crate::question::{Question, node_key, normalize_file};

/// One thing an arm named.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Prediction {
    Node(NodeRef),
    File(String),
}

impl Prediction {
    pub fn node(node: NodeRef) -> Prediction {
        Prediction::Node(node)
    }

    pub fn file(file: impl Into<String>) -> Prediction {
        Prediction::File(file.into())
    }
}

/// Resolve one prediction to a canonical key, or to a key nothing in gold can carry.
pub fn resolve(facts: &Facts, prediction: &Prediction) -> String {
    match prediction {
        Prediction::File(file) => crate::question::file_key(file),
        Prediction::Node(node) => {
            let mut candidates: Vec<usize> = facts.by_leaf(&node.leaf).to_vec();
            if let Some(span) = node.span {
                let pinned: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|id| facts.node(*id).name_span.0 == span.0)
                    .collect();
                if !pinned.is_empty() {
                    candidates = pinned;
                }
            }
            if let Some(file) = &node.file {
                let want = normalize_file(file);
                let pinned: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|id| normalize_file(&facts.node(*id).file) == want)
                    .collect();
                if !pinned.is_empty() {
                    candidates = pinned;
                }
            }
            if let Some(qualified) = &node.qualified {
                let pinned: Vec<usize> = candidates
                    .iter()
                    .copied()
                    .filter(|id| &facts.node(*id).qualified == qualified)
                    .collect();
                if !pinned.is_empty() {
                    candidates = pinned;
                }
            }
            candidates
                .sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
            match candidates.first() {
                Some(id) => node_key(facts, *id),
                None => format!("unresolved:{}", node.leaf),
            }
        }
    }
}

/// What one arm scored on one question.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Outcome {
    pub question: String,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    /// The 1-based rank of the first correct answer, when the arm produced one.
    pub first_hit: Option<usize>,
    pub predicted: usize,
    pub gold: usize,
}

/// Score one answer against one question.
pub fn score(facts: &Facts, question: &Question, predictions: &[Prediction]) -> Outcome {
    let gold: BTreeSet<&str> = question.gold.iter().map(String::as_str).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut ordered: Vec<String> = Vec::new();
    for prediction in predictions {
        let key = resolve(facts, prediction);
        if seen.insert(key.clone()) {
            ordered.push(key);
        }
    }
    let hits = ordered.iter().filter(|k| gold.contains(k.as_str())).count();
    let precision = if ordered.is_empty() {
        // Answering nothing to a question whose answer is nothing is right, not vacuous.
        if gold.is_empty() { 1.0 } else { 0.0 }
    } else {
        hits as f64 / ordered.len() as f64
    };
    let recall = if gold.is_empty() {
        if ordered.is_empty() { 1.0 } else { 0.0 }
    } else {
        hits as f64 / gold.len() as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    let first_hit = ordered
        .iter()
        .position(|k| gold.contains(k.as_str()))
        .map(|at| at + 1);
    Outcome {
        question: question.id.clone(),
        precision,
        recall,
        f1,
        first_hit,
        predicted: ordered.len(),
        gold: gold.len(),
    }
}

/// The macro average over a category's questions.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Score {
    pub questions: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
    pub acc_at_1: f64,
    pub acc_at_5: f64,
    pub mrr: f64,
}

pub fn aggregate(outcomes: &[Outcome]) -> Score {
    if outcomes.is_empty() {
        return Score::default();
    }
    let n = outcomes.len() as f64;
    let mean = |pick: fn(&Outcome) -> f64| outcomes.iter().map(pick).sum::<f64>() / n;
    Score {
        questions: outcomes.len(),
        precision: mean(|o| o.precision),
        recall: mean(|o| o.recall),
        f1: mean(|o| o.f1),
        acc_at_1: mean(|o| f64::from(o.first_hit == Some(1))),
        acc_at_5: mean(|o| f64::from(o.first_hit.is_some_and(|rank| rank <= 5))),
        mrr: mean(|o| o.first_hit.map_or(0.0, |rank| 1.0 / rank as f64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(precision: f64, recall: f64, first_hit: Option<usize>) -> Outcome {
        let f1 = if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        };
        Outcome {
            question: "q".to_string(),
            precision,
            recall,
            f1,
            first_hit,
            predicted: 1,
            gold: 1,
        }
    }

    #[test]
    fn a_perfect_answer_and_an_empty_one_average_to_a_half() {
        let score = aggregate(&[outcome(1.0, 1.0, Some(1)), outcome(0.0, 0.0, None)]);
        assert_eq!(score.questions, 2);
        assert!((score.f1 - 0.5).abs() < 1e-9);
        assert!((score.acc_at_1 - 0.5).abs() < 1e-9);
        assert!((score.mrr - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_hit_at_rank_four_counts_for_acc_at_5_and_not_acc_at_1() {
        let score = aggregate(&[outcome(0.25, 1.0, Some(4))]);
        assert_eq!(score.acc_at_1, 0.0);
        assert_eq!(score.acc_at_5, 1.0);
        assert!((score.mrr - 0.25).abs() < 1e-9);
    }
}
