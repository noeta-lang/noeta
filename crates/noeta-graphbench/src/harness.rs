//! Preparing a project and running the arms over it — the loop both a measurement run and an
//! ablation run drive.

use std::collections::BTreeMap;
use std::path::Path;

use crate::arms::{self, Arm, ProjectContext};
use crate::corpus::{self, GateResult};
use crate::gold;
use crate::metrics::{self, Outcome};
use crate::question::{self, Category, Question};
use crate::report::{Report, Row, Status};
use crate::service::{Service, Spend, Tool};

/// One project, prepared on both sides: the gold facts, the arms' view of it, and its questions.
#[derive(Debug)]
pub struct Prepared {
    pub facts: gold::Facts,
    pub context: ProjectContext,
    pub questions: Vec<Question>,
    pub gate: GateResult,
}

/// Prepare every corpus project whose name matches, gating each one on a clean link.
pub fn prepare(data: &Path, only: Option<&str>) -> Result<Vec<Prepared>, String> {
    let mut out = Vec::new();
    for spec in corpus::roster(data)? {
        if let Some(only) = only
            && !spec.name.contains(only)
            && Category::ALL.iter().all(|c| !c.as_str().contains(only))
        {
            continue;
        }
        let root = data.join("corpus").join(&spec.name);
        let entry = root.join(&spec.entry);
        let analysis = corpus::Analysis::open(&spec.name, &root, &entry)?;
        let gate = corpus::gate(&analysis);
        if !gate.ok() {
            return Err(format!(
                "the corpus project `{}` does not link cleanly ({} errors, linked={}){}. The \
                 benchmark refuses to score a project whose graph the compiler never built.",
                gate.name,
                gate.errors,
                gate.linked,
                gate.first_problem
                    .as_ref()
                    .map(|p| format!(": {p}"))
                    .unwrap_or_default(),
            ));
        }
        let facts = gold::facts(&analysis)?;
        let questions = question::build(&spec.name, &facts, &data.join("questions"))?;
        let context = ProjectContext::open(&spec.name, &root, &entry)?;
        out.push(Prepared {
            facts,
            context,
            questions,
            gate,
        });
    }
    if out.is_empty() {
        return Err("no corpus project matched".to_string());
    }
    Ok(out)
}

/// Run every selected arm over every selected category and aggregate one report.
pub async fn measure(
    service: &mut Service,
    prepared: &[Prepared],
    wanted: &[Arm],
    categories: &[Category],
) -> Report {
    let mut rows = Vec::new();
    for arm in wanted {
        for category in categories {
            // Per category, because an arm's strategies do not all need the same tools and the two
            // that shipped should not wait behind the two that have not.
            let missing: Vec<Tool> = arm
                .requires(*category)
                .iter()
                .copied()
                .filter(|tool| !service.advertises(*tool))
                .collect();
            if !missing.is_empty() {
                rows.push(Row {
                    arm: *arm,
                    category: *category,
                    status: Status::Skipped {
                        reason: format!(
                            "tool not present: {}",
                            missing
                                .iter()
                                .map(|tool| tool.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    },
                    score: Default::default(),
                    spend: Spend::default(),
                    evidence: BTreeMap::new(),
                    outcomes: Vec::new(),
                });
                continue;
            }
            let mut outcomes: Vec<Outcome> = Vec::new();
            let mut spend = Spend::default();
            let mut evidence: BTreeMap<Tool, usize> = BTreeMap::new();
            for project in prepared {
                for question in project.questions.iter().filter(|q| q.category == *category) {
                    let answer = arms::answer(*arm, service, &project.context, question).await;
                    spend.add(&answer.spend);
                    if let Some(tool) = answer.evidence {
                        *evidence.entry(tool).or_default() += 1;
                    }
                    outcomes.push(metrics::score(
                        &project.facts,
                        question,
                        &answer.predictions,
                    ));
                }
            }
            rows.push(Row {
                arm: *arm,
                category: *category,
                status: Status::Scored,
                score: metrics::aggregate(&outcomes),
                spend,
                evidence,
                outcomes,
            });
        }
    }
    let mut questions: BTreeMap<String, usize> = BTreeMap::new();
    for project in prepared {
        for question in &project.questions {
            *questions
                .entry(format!("{}/{}", project.context.name, question.category))
                .or_default() += 1;
        }
    }
    Report {
        gate: prepared.iter().map(|p| p.gate.clone()).collect(),
        tools: service.tool_names(),
        questions,
        rows,
        ablated: service.ablated(),
    }
}
