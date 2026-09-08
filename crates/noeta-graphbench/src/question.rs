//! What the benchmark asks, and what the right answer is.
//!
//! Structural questions are **sampled from the gold facts**, so their answers are exact and their
//! phrasing is a template. Seed-mapping questions are hand-labeled in
//! `tests/graphbench/questions/<project>.json`, because "where does an order get persisted?" names
//! no identifier and no index can derive it.
//!
//! Sampling has rules, and a candidate that meets none of them is dropped. A question must span two
//! files, or take two hops, or turn on a leaf name that names more than one declaration, or end at a
//! labeled `external`/`dynamic` leaf. Anything a single `grep` answers measures nothing. Each
//! category also carries **negatives** — a function nothing calls, a module nothing imports, an
//! entry point that reaches no boundary, a pair with no path between them — where the right answer
//! is the empty set and a confident wrong one is the failure being measured.

use std::collections::BTreeSet;
use std::path::Path;

use crate::gold::{Facts, NodeKind};

/// The question kinds, from the retrieval literature's own taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Definition,
    Callers,
    Callees,
    Importers,
    RoleReach,
    Path,
    Impact,
    SeedMapping,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Definition => "definition",
            Category::Callers => "callers",
            Category::Callees => "callees",
            Category::Importers => "importers",
            Category::RoleReach => "role_reach",
            Category::Path => "path",
            Category::Impact => "impact",
            Category::SeedMapping => "seed_mapping",
        }
    }

    pub const ALL: [Category; 8] = [
        Category::Definition,
        Category::Callers,
        Category::Callees,
        Category::Importers,
        Category::RoleReach,
        Category::Path,
        Category::Impact,
        Category::SeedMapping,
    ];

    /// Whether the answer is an ordered list, so `Acc@k` and MRR mean something.
    pub fn ranked(self) -> bool {
        matches!(self, Category::Definition | Category::SeedMapping)
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Category {
    type Err = String;

    fn from_str(text: &str) -> Result<Category, String> {
        Category::ALL
            .into_iter()
            .find(|c| c.as_str() == text)
            .ok_or_else(|| format!("`{text}` names no question category"))
    }
}

/// Why a sampled question is worth asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// The answer lives in a file other than the subject's.
    TwoFiles,
    /// The answer is more than one hop away.
    TwoHops,
    /// The subject's leaf name names more than one declaration.
    CollidingName,
    /// The walk terminates at an `external` or `dynamic` label.
    LabeledLeaf,
    /// The right answer is the empty set.
    Negative,
    /// Hand-labeled from a natural-language description.
    HandLabeled,
}

/// What an arm is given to work with.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    /// One declaration, addressed the way a developer would: by its leaf name, with the file it
    /// sits in and the qualified name the graph knows it by.
    Symbol {
        leaf: String,
        qualified: String,
        file: String,
    },
    /// Two declarations, for a point-to-point question.
    Pair {
        from: Box<Subject>,
        to: Box<Subject>,
    },
    /// A module path.
    Module { path: String },
    /// Nothing but the prompt.
    Free,
}

impl Subject {
    pub fn leaf(&self) -> Option<&str> {
        match self {
            Subject::Symbol { leaf, .. } => Some(leaf),
            _ => None,
        }
    }

    pub fn file(&self) -> Option<&str> {
        match self {
            Subject::Symbol { file, .. } => Some(file),
            _ => None,
        }
    }

    pub fn qualified(&self) -> Option<&str> {
        match self {
            Subject::Symbol { qualified, .. } => Some(qualified),
            _ => None,
        }
    }

    fn of(facts: &Facts, id: usize) -> Subject {
        let node = facts.node(id);
        Subject::Symbol {
            leaf: node.leaf.clone(),
            qualified: node.qualified.clone(),
            file: node.file.clone(),
        }
    }
}

/// One question, with the answer already known.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Question {
    pub id: String,
    pub project: String,
    pub category: Category,
    pub prompt: String,
    pub subject: Subject,
    /// The right answer, as canonical keys (see [`crate::metrics::key`]).
    pub gold: Vec<String>,
    pub rules: Vec<Rule>,
    pub negative: bool,
    /// The hop budget a walk question is asked at.
    pub depth: usize,
}

/// The canonical key of a universe node: the file it is declared in and its name span's start.
pub fn node_key(facts: &Facts, id: usize) -> String {
    let node = facts.node(id);
    format!("{}#{}", normalize_file(&node.file), node.name_span.0)
}

/// The human-readable label of a universe node — what an agent can be asked to produce, and what a
/// hand-written seed question names.
pub fn node_label(facts: &Facts, id: usize) -> String {
    let node = facts.node(id);
    format!("{}#{}", normalize_file(&node.file), node.leaf)
}

/// The label of every gold answer of a question, for the agent-layer arm.
pub fn gold_labels(facts: &Facts, question: &Question) -> Vec<String> {
    question
        .gold
        .iter()
        .map(|key| {
            facts
                .nodes
                .iter()
                .find(|node| &node_key(facts, node.id) == key)
                .map(|node| node_label(facts, node.id))
                .unwrap_or_else(|| key.clone())
        })
        .collect()
}

/// The canonical key of a file answer.
pub fn file_key(file: &str) -> String {
    format!("file:{}", normalize_file(file))
}

/// File names travel through four layers, so they are compared by their last two path segments —
/// enough to tell `handlers/orders.noe` from `parse/orders.noe`, and blind to whether a caller
/// spelled the path absolutely or relative to the project root.
pub fn normalize_file(file: &str) -> String {
    let cleaned = file.replace('\\', "/");
    let parts: Vec<&str> = cleaned.split('/').filter(|p| !p.is_empty()).collect();
    let tail = parts.len().saturating_sub(2);
    parts[tail..].join("/")
}

/// How many questions one category may contribute per project.
const PER_CATEGORY: usize = 8;

/// Build every structural question for one project, plus the hand-labeled seed-mapping set.
pub fn build(project: &str, facts: &Facts, questions_dir: &Path) -> Result<Vec<Question>, String> {
    let mut out = Vec::new();
    out.extend(definitions(project, facts));
    out.extend(callers(project, facts));
    out.extend(callees(project, facts));
    out.extend(importers(project, facts));
    out.extend(role_reach(project, facts));
    out.extend(paths(project, facts));
    out.extend(impact(project, facts));
    out.extend(seed_mapping(project, facts, questions_dir)?);
    Ok(out)
}

/// Take an evenly spaced sample, so a cap does not mean "everything in the first file".
fn spread<T>(mut candidates: Vec<T>, limit: usize) -> Vec<T> {
    if candidates.len() <= limit {
        return candidates;
    }
    let step = candidates.len() as f64 / limit as f64;
    let mut picked = Vec::with_capacity(limit);
    let mut taken = vec![false; candidates.len()];
    for slot in 0..limit {
        let at = ((slot as f64 * step) as usize).min(candidates.len() - 1);
        if !taken[at] {
            taken[at] = true;
            picked.push(at);
        }
    }
    let mut out = Vec::with_capacity(picked.len());
    for at in picked.into_iter().rev() {
        out.push(candidates.remove(at));
    }
    out.reverse();
    out
}

fn definitions(project: &str, facts: &Facts) -> Vec<Question> {
    let mut candidates: Vec<usize> = facts
        .nodes
        .iter()
        .filter(|n| n.kind != NodeKind::TopLevel && !n.is_test)
        .filter(|n| facts.collides(&n.leaf) || n.file != entry_file(facts))
        .map(|n| n.id)
        .collect();
    candidates.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    candidates.dedup_by_key(|id| facts.node(*id).leaf.clone());
    spread(candidates, PER_CATEGORY)
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let node = facts.node(id);
            let colliding = facts.collides(&node.leaf);
            let mut gold: Vec<usize> = facts.by_leaf(&node.leaf).to_vec();
            gold.sort_by_key(|g| (facts.node(*g).file.clone(), facts.node(*g).name_span.0));
            let mut rules = vec![Rule::TwoFiles];
            if colliding {
                rules.push(Rule::CollidingName);
            }
            Question {
                id: format!("{project}/definition/{n}"),
                project: project.to_string(),
                category: Category::Definition,
                prompt: format!("Where is `{}` declared?", node.leaf),
                subject: Subject::of(facts, id),
                gold: gold.into_iter().map(|g| node_key(facts, g)).collect(),
                rules,
                negative: false,
                depth: 0,
            }
        })
        .collect()
}

fn entry_file(facts: &Facts) -> String {
    facts.node(facts.top_level()).file.clone()
}

fn callers(project: &str, facts: &Facts) -> Vec<Question> {
    let mut positive: Vec<usize> = Vec::new();
    let mut negative: Vec<usize> = Vec::new();
    for node in &facts.nodes {
        if !node.kind.callable() || node.is_test {
            continue;
        }
        let callers = facts.callers(node.id);
        if callers.is_empty() {
            negative.push(node.id);
            continue;
        }
        let files: BTreeSet<&str> = callers
            .iter()
            .map(|c| facts.node(*c).file.as_str())
            .collect();
        if files.len() > 1 || files.iter().any(|f| *f != node.file) || facts.collides(&node.leaf) {
            positive.push(node.id);
        }
    }
    positive.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    negative.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    let mut chosen = spread(positive, PER_CATEGORY.saturating_sub(2));
    chosen.extend(spread(negative, 2));
    chosen
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let node = facts.node(id);
            let gold = facts.callers(id);
            let mut rules = vec![Rule::TwoFiles];
            if gold.is_empty() {
                rules = vec![Rule::Negative];
            }
            if facts.collides(&node.leaf) {
                rules.push(Rule::CollidingName);
            }
            Question {
                id: format!("{project}/callers/{n}"),
                project: project.to_string(),
                category: Category::Callers,
                prompt: format!(
                    "Which declarations call or reference `{}` (declared in {})?",
                    node.leaf, node.file
                ),
                subject: Subject::of(facts, id),
                gold: gold.iter().map(|g| node_key(facts, *g)).collect(),
                rules,
                negative: gold.is_empty(),
                depth: 1,
            }
        })
        .collect()
}

/// The hop budget every reach question is asked at.
pub const REACH_DEPTH: usize = 3;

fn callees(project: &str, facts: &Facts) -> Vec<Question> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for node in &facts.nodes {
        if !node.kind.callable() {
            continue;
        }
        let reached = facts.callees(node.id, REACH_DEPTH);
        if reached.is_empty() {
            if facts.labeled_leaves(node.id, REACH_DEPTH).is_empty() {
                negative.push(node.id);
            }
            continue;
        }
        let files: BTreeSet<&str> = reached
            .iter()
            .map(|c| facts.node(*c).file.as_str())
            .collect();
        let two_files = files.iter().any(|f| *f != node.file);
        let two_hops = reached.len() > facts.callees(node.id, 1).len();
        if two_files || two_hops {
            positive.push(node.id);
        }
    }
    positive.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    negative.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    let mut chosen = spread(positive, PER_CATEGORY.saturating_sub(1));
    chosen.extend(spread(negative, 1));
    chosen
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let node = facts.node(id);
            let gold = facts.callees(id, REACH_DEPTH);
            let mut rules = Vec::new();
            if gold.is_empty() {
                rules.push(Rule::Negative);
            } else {
                rules.push(Rule::TwoHops);
            }
            if !facts.labeled_leaves(id, REACH_DEPTH).is_empty() {
                rules.push(Rule::LabeledLeaf);
            }
            Question {
                id: format!("{project}/callees/{n}"),
                project: project.to_string(),
                category: Category::Callees,
                prompt: format!(
                    "What does `{}` reach, directly or transitively, within {REACH_DEPTH} hops?",
                    node.leaf
                ),
                subject: Subject::of(facts, id),
                gold: gold.iter().map(|g| node_key(facts, *g)).collect(),
                rules,
                negative: gold.is_empty(),
                depth: REACH_DEPTH,
            }
        })
        .collect()
}

fn importers(project: &str, facts: &Facts) -> Vec<Question> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for (module, _) in &facts.modules {
        let hits = facts.importers(module);
        if hits.is_empty() {
            negative.push(module.clone());
        } else {
            positive.push(module.clone());
        }
    }
    let mut chosen = spread(positive, PER_CATEGORY.saturating_sub(1));
    chosen.extend(spread(negative, 1));
    chosen
        .into_iter()
        .enumerate()
        .map(|(n, module)| {
            let gold = facts.importers(&module);
            Question {
                id: format!("{project}/importers/{n}"),
                project: project.to_string(),
                category: Category::Importers,
                prompt: format!("Which files import the module `{module}`?"),
                subject: Subject::Module {
                    path: module.clone(),
                },
                gold: gold.iter().map(|f| file_key(f)).collect(),
                rules: if gold.is_empty() {
                    vec![Rule::Negative]
                } else {
                    vec![Rule::TwoFiles]
                },
                negative: gold.is_empty(),
                depth: 1,
            }
        })
        .collect()
}

/// The role every reachability question starts from.
const ENTRY_ROLE: &str = "Semantic.EntryPoint";

fn role_reach(project: &str, facts: &Facts) -> Vec<Question> {
    let mut entries: Vec<usize> = facts
        .role_bearers()
        .into_iter()
        .filter(|(_, role)| role == ENTRY_ROLE)
        .map(|(id, _)| id)
        .collect();
    entries.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    entries.dedup();
    spread(entries, PER_CATEGORY)
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let node = facts.node(id);
            let gold = facts.boundaries(id, REACH_DEPTH * 2);
            let ids: Vec<usize> = gold
                .iter()
                .filter_map(|target| {
                    facts
                        .nodes
                        .iter()
                        .find(|candidate| &candidate.qualified == target)
                        .map(|candidate| candidate.id)
                })
                .collect();
            Question {
                id: format!("{project}/role_reach/{n}"),
                project: project.to_string(),
                category: Category::RoleReach,
                prompt: format!(
                    "Starting at the entry point `{}`, which role-bearing declarations does the \
                     flow reach?",
                    node.leaf
                ),
                subject: Subject::of(facts, id),
                gold: ids.iter().map(|g| node_key(facts, *g)).collect(),
                rules: if ids.is_empty() {
                    vec![Rule::Negative]
                } else {
                    vec![Rule::TwoHops, Rule::TwoFiles]
                },
                negative: ids.is_empty(),
                depth: REACH_DEPTH * 2,
            }
        })
        .collect()
}

fn paths(project: &str, facts: &Facts) -> Vec<Question> {
    let entries: Vec<usize> = facts
        .role_bearers()
        .into_iter()
        .filter(|(_, role)| role == ENTRY_ROLE)
        .map(|(id, _)| id)
        .collect();
    let sinks: Vec<usize> = facts
        .role_bearers()
        .into_iter()
        .filter(|(_, role)| role != ENTRY_ROLE)
        .map(|(id, _)| id)
        .collect();
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for from in &entries {
        for to in &sinks {
            match facts.shortest_path(*from, *to) {
                Some(path) if path.len() >= 3 => positive.push((*from, *to, path)),
                Some(_) => {}
                None => negative.push((*from, *to)),
            }
        }
    }
    positive.sort_by_key(|(from, to, _)| {
        (
            facts.node(*from).qualified.clone(),
            facts.node(*to).qualified.clone(),
        )
    });
    negative.sort_by_key(|(from, to)| {
        (
            facts.node(*from).qualified.clone(),
            facts.node(*to).qualified.clone(),
        )
    });
    let mut out: Vec<Question> = spread(positive, PER_CATEGORY.saturating_sub(2))
        .into_iter()
        .enumerate()
        .map(|(n, (from, to, path))| Question {
            id: format!("{project}/path/{n}"),
            project: project.to_string(),
            category: Category::Path,
            prompt: format!(
                "How does `{}` reach `{}`? Name every declaration on the way.",
                facts.node(from).leaf,
                facts.node(to).leaf
            ),
            subject: Subject::Pair {
                from: Box::new(Subject::of(facts, from)),
                to: Box::new(Subject::of(facts, to)),
            },
            gold: path.iter().map(|g| node_key(facts, *g)).collect(),
            rules: vec![Rule::TwoHops, Rule::TwoFiles],
            negative: false,
            depth: REACH_DEPTH * 2,
        })
        .collect();
    let start = out.len();
    out.extend(
        spread(negative, 2)
            .into_iter()
            .enumerate()
            .map(|(n, (from, to))| Question {
                id: format!("{project}/path/{}", start + n),
                project: project.to_string(),
                category: Category::Path,
                prompt: format!(
                    "How does `{}` reach `{}`? Name every declaration on the way.",
                    facts.node(from).leaf,
                    facts.node(to).leaf
                ),
                subject: Subject::Pair {
                    from: Box::new(Subject::of(facts, from)),
                    to: Box::new(Subject::of(facts, to)),
                },
                gold: Vec::new(),
                rules: vec![Rule::Negative],
                negative: true,
                depth: REACH_DEPTH * 2,
            }),
    );
    out
}

fn impact(project: &str, facts: &Facts) -> Vec<Question> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for node in &facts.nodes {
        if !node.kind.callable() || node.is_test {
            continue;
        }
        if facts.impacted_tests(node.id).is_empty() {
            negative.push(node.id);
        } else {
            positive.push(node.id);
        }
    }
    positive.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    negative.sort_by_key(|id| (facts.node(*id).file.clone(), facts.node(*id).name_span.0));
    let mut chosen = spread(positive, PER_CATEGORY.saturating_sub(2));
    chosen.extend(spread(negative, 2));
    chosen
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let node = facts.node(id);
            let gold = facts.impacted_tests(id);
            Question {
                id: format!("{project}/impact/{n}"),
                project: project.to_string(),
                category: Category::Impact,
                prompt: format!(
                    "If `{}` changes, which `@test` functions must run again?",
                    node.leaf
                ),
                subject: Subject::of(facts, id),
                gold: gold.iter().map(|g| node_key(facts, *g)).collect(),
                rules: if gold.is_empty() {
                    vec![Rule::Negative]
                } else {
                    vec![Rule::TwoHops, Rule::TwoFiles]
                },
                negative: gold.is_empty(),
                depth: REACH_DEPTH * 2,
            }
        })
        .collect()
}

/// One hand-labeled natural-language query.
#[derive(Debug, Clone, serde::Deserialize)]
struct Seed {
    query: String,
    /// The declarations that answer it, best first, each `file#leaf` or a bare leaf name.
    answers: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct SeedFile {
    #[serde(default)]
    comment: String,
    seeds: Vec<Seed>,
}

fn seed_mapping(project: &str, facts: &Facts, dir: &Path) -> Result<Vec<Question>, String> {
    let path = dir.join(format!("{project}.json"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let file: SeedFile = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    let _ = file.comment;
    let mut out = Vec::new();
    for (n, seed) in file.seeds.iter().enumerate() {
        let mut gold = Vec::new();
        for answer in &seed.answers {
            let id = resolve_label(facts, answer).ok_or_else(|| {
                format!(
                    "{}: seed {n} names `{answer}`, which is no declaration of {project}",
                    path.display()
                )
            })?;
            gold.push(node_key(facts, id));
        }
        out.push(Question {
            id: format!("{project}/seed_mapping/{n}"),
            project: project.to_string(),
            category: Category::SeedMapping,
            prompt: seed.query.clone(),
            subject: Subject::Free,
            gold,
            rules: vec![Rule::HandLabeled],
            negative: seed.answers.is_empty(),
            depth: 0,
        });
    }
    Ok(out)
}

/// Resolve a hand-written label (`store.noe#save_order`, or a unique bare `save_order`).
fn resolve_label(facts: &Facts, label: &str) -> Option<usize> {
    let (file, leaf) = match label.split_once('#') {
        Some((file, leaf)) => (Some(normalize_file(file)), leaf),
        None => (None, label),
    };
    let candidates: Vec<usize> = facts
        .by_leaf(leaf)
        .iter()
        .copied()
        .filter(|id| match &file {
            Some(want) => normalize_file(&facts.node(*id).file).ends_with(want.as_str()),
            None => true,
        })
        .collect();
    match candidates.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_key_is_blind_to_how_the_path_was_spelled() {
        assert_eq!(
            file_key("/home/x/corpus/orders_service/handlers/orders.noe"),
            file_key("handlers/orders.noe")
        );
        assert_ne!(
            file_key("handlers/orders.noe"),
            file_key("parse/orders.noe")
        );
    }

    #[test]
    fn a_spread_sample_is_evenly_placed_and_deterministic() {
        let picked = spread((0..10).collect::<Vec<u32>>(), 4);
        assert_eq!(picked, vec![0, 2, 5, 7]);
        assert_eq!(spread((0..3).collect::<Vec<u32>>(), 8), vec![0, 1, 2]);
    }
}
