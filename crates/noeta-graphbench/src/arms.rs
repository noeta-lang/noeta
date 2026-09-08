//! The arms: one fixed composition of tool calls per question category.
//!
//! An arm is a strategy, not an agent. It is given exactly what a developer types — a leaf name and
//! the file it sits in, a module path, or a sentence — and it composes tool calls the way a
//! competent agent plausibly would. It never sees the question's gold, and it never sees the
//! qualified name the graph knows a declaration by, because obtaining that name is part of what is
//! being measured.
//!
//! | Arm | What it has | What it isolates |
//! |---|---|---|
//! | A0 | today's MCP surface: `symbols`, `definition`, `references`, `trace`, `module_graph`, `reflect`, file reads | the floor a real agent works from |
//! | A1 | A0 plus `code_search` | seed selection |
//! | A2 | A1 plus `context_map` | budgeted ranking |
//! | A3 | A2 plus `path`, `impact`, `architecture`, `callers` | multi-hop and role structure |
//! | A4 | a fixed repo map at a token budget, no navigation | whether precomputation alone suffices |
//! | A5 | a lexical scan of the files, no Noeta tools | whether any of this beats grep |
//!
//! A1 through A4 name tools the service does not advertise yet. Each reports **SKIP** with the tool
//! it is missing, which the report prints as its own status and never as a pass.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::adapter::{self, LineIndexes, NodeRef, Symbol};
use crate::metrics::Prediction;
use crate::question::{Category, Question, Subject};
use crate::service::{Service, Spend, Tool, args};

/// The six arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    A0Today,
    A1CodeSearch,
    A2ContextMap,
    A3GraphTools,
    A4RepoMap,
    A5Lexical,
}

impl Arm {
    pub const ALL: [Arm; 6] = [
        Arm::A0Today,
        Arm::A1CodeSearch,
        Arm::A2ContextMap,
        Arm::A3GraphTools,
        Arm::A4RepoMap,
        Arm::A5Lexical,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Arm::A0Today => "A0",
            Arm::A1CodeSearch => "A1",
            Arm::A2ContextMap => "A2",
            Arm::A3GraphTools => "A3",
            Arm::A4RepoMap => "A4",
            Arm::A5Lexical => "A5",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Arm::A0Today => "today's MCP surface",
            Arm::A1CodeSearch => "+ code_search",
            Arm::A2ContextMap => "+ context_map",
            Arm::A3GraphTools => "+ path/impact/architecture/callers",
            Arm::A4RepoMap => "fixed repo map, no navigation",
            Arm::A5Lexical => "lexical scan, no Noeta tools",
        }
    }

    /// The tools this arm cannot run without.
    pub fn requires(self) -> &'static [Tool] {
        match self {
            Arm::A0Today | Arm::A5Lexical => &[],
            Arm::A1CodeSearch => &[Tool::CodeSearch],
            Arm::A2ContextMap => &[Tool::CodeSearch, Tool::ContextMap],
            Arm::A3GraphTools => &[Tool::Path, Tool::Impact, Tool::Architecture, Tool::Callers],
            Arm::A4RepoMap => &[Tool::ContextMap],
        }
    }
}

impl std::fmt::Display for Arm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Arm {
    type Err = String;

    fn from_str(text: &str) -> Result<Arm, String> {
        Arm::ALL
            .into_iter()
            .find(|a| a.as_str().eq_ignore_ascii_case(text))
            .ok_or_else(|| format!("`{text}` names no arm"))
    }
}

/// The token budget the fixed repo-map arm is defined at.
pub const REPO_MAP_BUDGET: usize = 4_000;

/// What an arm produced for one question.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Answer {
    pub predictions: Vec<Prediction>,
    /// The tool the answer's evidence came from.
    pub evidence: Option<Tool>,
    pub spend: Spend,
}

/// Everything an arm may read about a project. Deliberately no gold, and no qualified names.
#[derive(Debug)]
pub struct ProjectContext {
    pub name: String,
    pub root: PathBuf,
    pub entry: PathBuf,
    /// Every `.noe` file of the project, in a stable order.
    pub files: Vec<PathBuf>,
    pub indexes: LineIndexes,
    /// The package's import prefix, read from `noeta.toml` the way an agent would read it.
    pub prefix: String,
}

impl ProjectContext {
    pub fn open(name: &str, root: &Path, entry: &Path) -> Result<ProjectContext, String> {
        let mut files = Vec::new();
        collect_noe(root, &mut files);
        files.sort();
        let mut indexes = LineIndexes::new();
        for file in &files {
            let text = std::fs::read_to_string(file)
                .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            indexes.add(&file.display().to_string(), &text);
        }
        Ok(ProjectContext {
            name: name.to_string(),
            root: root.to_path_buf(),
            entry: entry.to_path_buf(),
            files,
            indexes,
            prefix: read_prefix(root),
        })
    }

    fn entry_arg(&self) -> String {
        self.entry.display().to_string()
    }

    /// The module path a file derives, the way the loader derives it: the package prefix plus the
    /// file's location under the root, with `src/` dropped and the `.noe` stem as the last segment.
    fn module_of(&self, file: &Path) -> String {
        let relative = file.strip_prefix(&self.root).unwrap_or(file);
        let mut segments = vec![self.prefix.clone()];
        for part in relative.components() {
            let text = part.as_os_str().to_string_lossy().to_string();
            if text == "src" {
                continue;
            }
            segments.push(text.trim_end_matches(".noe").to_string());
        }
        segments.join(".")
    }
}

fn collect_noe(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.join("noeta.toml").exists() {
                continue;
            }
            collect_noe(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

/// The package's import prefix — `[package] root`, else the package half of `name`.
fn read_prefix(root: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(root.join("noeta.toml")) else {
        return String::new();
    };
    let value = |key: &str| -> Option<String> {
        text.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split('"').nth(1))
            .map(str::to_string)
    };
    if let Some(root_key) = value("root =") {
        return root_key;
    }
    value("name =")
        .and_then(|name| name.rsplit('/').next().map(str::to_string))
        .unwrap_or_default()
}

/// The handle a question hands an arm: what a developer types.
#[derive(Debug, Clone)]
pub struct Handle {
    pub leaf: String,
    pub file: Option<String>,
}

impl Handle {
    fn of(subject: &Subject) -> Option<Handle> {
        match subject {
            Subject::Symbol { leaf, file, .. } => Some(Handle {
                leaf: leaf.clone(),
                file: Some(file.clone()),
            }),
            _ => None,
        }
    }
}

/// Answer one question with one arm.
pub async fn answer(
    arm: Arm,
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> Answer {
    let _ = service.take_spend();
    let predictions_and_evidence = match arm {
        Arm::A0Today => today(service, context, question).await,
        Arm::A5Lexical => lexical(service, context, question),
        Arm::A1CodeSearch | Arm::A2ContextMap | Arm::A3GraphTools | Arm::A4RepoMap => {
            (Vec::new(), None)
        }
    };
    Answer {
        predictions: predictions_and_evidence.0,
        evidence: predictions_and_evidence.1,
        spend: service.take_spend(),
    }
}

// ----------------------------------------------------------------------------------------- A0

async fn today(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    match question.category {
        Category::Definition => a0_definition(service, context, question).await,
        Category::Callers => a0_callers(service, context, question).await,
        Category::Callees => a0_callees(service, context, question).await,
        Category::Importers => a0_importers(service, context, question).await,
        Category::RoleReach => a0_role_reach(service, context, question).await,
        Category::Path => a0_path(service, context, question).await,
        Category::Impact => a0_impact(service, context, question).await,
        Category::SeedMapping => a0_seed_mapping(service, context, question).await,
    }
}

/// `symbols` for one file, flattened.
async fn outline(service: &mut Service, file: &Path) -> Vec<Symbol> {
    let name = file.display().to_string();
    let value = service
        .call(Tool::Symbols, args([("file", json!(name))]))
        .await;
    adapter::symbols(&value, &name)
}

/// The innermost declaration whose range holds `offset`.
fn enclosing(symbols: &[Symbol], offset: u32) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| s.range.0 <= offset && offset <= s.range.1)
        .filter(|s| {
            matches!(
                s.node.kind.as_deref(),
                Some("function") | Some("method") | None
            )
        })
        .min_by_key(|s| s.range.1 - s.range.0)
}

async fn a0_definition(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Some(handle) = Handle::of(&question.subject) else {
        return (Vec::new(), None);
    };
    let mut out = Vec::new();
    let mut evidence = None;
    let direct = service
        .call(
            Tool::Definition,
            args([
                ("file", json!(context.entry_arg())),
                ("symbol", json!(handle.leaf)),
            ]),
        )
        .await;
    if let Some(found) = adapter::definition(&direct, &context.entry_arg(), &context.indexes) {
        evidence = Some(Tool::Definition);
        out.push(Prediction::node(NodeRef {
            qualified: None,
            leaf: handle.leaf.clone(),
            file: found.file.clone(),
            span: Some((found.offset, found.end)),
            kind: None,
        }));
    }
    // A declaration in a sibling module is not addressable from the entry file, so the sweep is not
    // a fallback here, it is the only way to see the second one.
    for file in &context.files {
        for symbol in outline(service, file).await {
            if symbol.node.leaf == handle.leaf {
                evidence.get_or_insert(Tool::Symbols);
                out.push(Prediction::node(symbol.node.clone()));
            }
        }
    }
    (out, evidence)
}

async fn a0_callers(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Some(handle) = Handle::of(&question.subject) else {
        return (Vec::new(), None);
    };
    let addressed = handle.file.clone().unwrap_or_else(|| context.entry_arg());
    let value = service
        .call(
            Tool::References,
            args([
                ("file", json!(addressed)),
                ("symbol", json!(handle.leaf)),
                ("include_declaration", json!(false)),
            ]),
        )
        .await;
    let hits = adapter::references(&value, &addressed, &context.indexes);
    let mut out = Vec::new();
    let mut outlines: HashMap<String, Vec<Symbol>> = HashMap::new();
    for hit in hits {
        let file = hit.file.clone().unwrap_or_else(|| addressed.clone());
        if !outlines.contains_key(&file) {
            let symbols = outline(service, Path::new(&file)).await;
            outlines.insert(file.clone(), symbols);
        }
        let symbols = &outlines[&file];
        match enclosing(symbols, hit.offset) {
            Some(owner) => out.push(Prediction::node(owner.node.clone())),
            None => out.push(Prediction::node(NodeRef {
                qualified: None,
                leaf: crate::gold::TOP_LEVEL.to_string(),
                file: Some(context.entry_arg()),
                span: Some((0, 0)),
                kind: None,
            })),
        }
    }
    (out, Some(Tool::References))
}

/// The names `trace` might answer to, in the order an agent would try them.
///
/// `trace` matches the post-link qualified name exactly, and every other tool speaks bare names, so
/// getting into the graph at all is a search. This ladder is that search: the bare name, then the
/// module path the file derives plus the name, then the name qualified by the type the outline
/// nests it under.
async fn trace_addresses(
    service: &mut Service,
    context: &ProjectContext,
    handle: &Handle,
) -> Vec<String> {
    let mut names = vec![handle.leaf.clone()];
    if let Some(file) = &handle.file {
        names.push(format!(
            "{}.{}",
            context.module_of(Path::new(file)),
            handle.leaf
        ));
        for symbol in outline(service, Path::new(file)).await {
            if symbol.node.leaf == handle.leaf
                && let Some(owner) = &symbol.owner
            {
                names.push(format!("{owner}.{}", handle.leaf));
            }
        }
    }
    names.dedup();
    names
}

/// Walk the addressing ladder until `trace` finds a root.
async fn traced(
    service: &mut Service,
    context: &ProjectContext,
    handle: &Handle,
    depth: usize,
) -> Option<Value> {
    for name in trace_addresses(service, context, handle).await {
        let value = service
            .call(
                Tool::Trace,
                args([
                    ("file", json!(context.entry_arg())),
                    ("from", json!(name)),
                    ("max_depth", json!(depth)),
                ]),
            )
            .await;
        let (found, hits) = adapter::trace(&value);
        if found && !hits.is_empty() {
            return Some(value);
        }
    }
    None
}

async fn a0_callees(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Some(handle) = Handle::of(&question.subject) else {
        return (Vec::new(), None);
    };
    let Some(value) = traced(service, context, &handle, question.depth).await else {
        return (Vec::new(), Some(Tool::Trace));
    };
    let (_, hits) = adapter::trace(&value);
    let out = hits
        .into_iter()
        .filter(|hit| hit.depth > 0 && !hit.external && !hit.dynamic)
        .map(|hit| Prediction::node(hit.node))
        .collect();
    (out, Some(Tool::Trace))
}

async fn a0_importers(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Subject::Module { path } = &question.subject else {
        return (Vec::new(), None);
    };
    let value = service
        .call(
            Tool::ModuleGraph,
            args([("file", json!(context.entry_arg()))]),
        )
        .await;
    let out = adapter::module_graph(&value)
        .into_iter()
        .filter(|node| node.imports.iter().any(|module| module == path))
        .map(|node| Prediction::file(node.file))
        .collect();
    (out, Some(Tool::ModuleGraph))
}

async fn a0_role_reach(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Some(handle) = Handle::of(&question.subject) else {
        return (Vec::new(), None);
    };
    let Some(value) = traced(service, context, &handle, question.depth).await else {
        return (Vec::new(), Some(Tool::Trace));
    };
    let located = service
        .call(Tool::Reflect, args([("file", json!(context.entry_arg()))]))
        .await;
    let by_target: HashMap<String, NodeRef> = adapter::reflect_roles(&located)
        .into_iter()
        .map(|(node, _)| (node.qualified.clone().unwrap_or(node.leaf.clone()), node))
        .collect();
    let out = adapter::boundaries(&value)
        .into_iter()
        .map(|(node, _)| {
            let key = node.qualified.clone().unwrap_or(node.leaf.clone());
            Prediction::node(by_target.get(&key).cloned().unwrap_or(node))
        })
        .collect();
    (out, Some(Tool::Trace))
}

async fn a0_path(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Subject::Pair { from, to } = &question.subject else {
        return (Vec::new(), None);
    };
    let (Some(start), Some(goal)) = (Handle::of(from), Handle::of(to)) else {
        return (Vec::new(), None);
    };
    let Some(value) = traced(service, context, &start, question.depth).await else {
        return (Vec::new(), Some(Tool::Trace));
    };
    let (_, hits) = adapter::trace(&value);
    let Some(reached) = hits
        .iter()
        .filter(|hit| hit.node.leaf == goal.leaf && !hit.external && !hit.dynamic)
        .min_by_key(|hit| hit.depth)
    else {
        return (Vec::new(), Some(Tool::Trace));
    };
    let by_name: HashMap<&str, &NodeRef> = hits
        .iter()
        .map(|hit| (hit.node.leaf.as_str(), &hit.node))
        .collect();
    let out = reached
        .path
        .iter()
        .map(|name| {
            let leaf = name.rsplit('.').next().unwrap_or(name);
            Prediction::node(
                by_name
                    .get(leaf)
                    .map(|node| (*node).clone())
                    .unwrap_or(NodeRef::named(name)),
            )
        })
        .collect();
    (out, Some(Tool::Trace))
}

async fn a0_impact(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let Some(handle) = Handle::of(&question.subject) else {
        return (Vec::new(), None);
    };
    // No tool walks an edge backwards, so the composition is a bounded reverse sweep over
    // `references`, and the test functions have to come out of the files themselves because the
    // outline does not descend into a `@test` block.
    let mut frontier = VecDeque::from([handle.clone()]);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut reached: Vec<NodeRef> = Vec::new();
    let mut outlines: HashMap<String, Vec<Symbol>> = HashMap::new();
    for _ in 0..question.depth {
        let mut next = VecDeque::new();
        while let Some(at) = frontier.pop_front() {
            if !seen.insert(format!("{}|{:?}", at.leaf, at.file)) {
                continue;
            }
            let addressed = at.file.clone().unwrap_or_else(|| context.entry_arg());
            let value = service
                .call(
                    Tool::References,
                    args([
                        ("file", json!(addressed)),
                        ("symbol", json!(at.leaf)),
                        ("include_declaration", json!(false)),
                    ]),
                )
                .await;
            for hit in adapter::references(&value, &addressed, &context.indexes) {
                let file = hit.file.clone().unwrap_or_else(|| addressed.clone());
                if !outlines.contains_key(&file) {
                    let symbols = outline(service, Path::new(&file)).await;
                    outlines.insert(file.clone(), symbols);
                }
                if let Some(owner) = enclosing(&outlines[&file], hit.offset) {
                    reached.push(owner.node.clone());
                    next.push_back(Handle {
                        leaf: owner.node.leaf.clone(),
                        file: Some(file.clone()),
                    });
                }
            }
        }
        frontier = next;
    }
    let tests = test_functions(service, context);
    let out = reached
        .into_iter()
        .filter(|node| {
            tests
                .iter()
                .any(|(name, file)| name == &node.leaf && Some(file) == node.file.as_ref())
        })
        .map(Prediction::node)
        .collect();
    (out, Some(Tool::References))
}

/// The `@test` functions of a project, read out of the files, because no tool reports them.
fn test_functions(service: &mut Service, context: &ProjectContext) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for file in &context.files {
        let text = service.read_file(file);
        let name = file.display().to_string();
        let mut depth = 0i32;
        let mut inside = 0i32;
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("@test") && inside == 0 {
                inside = depth + 1;
            }
            if inside > 0
                && let Some(rest) = trimmed.strip_prefix("fn ").or_else(|| {
                    trimmed
                        .strip_prefix("pub fn ")
                        .filter(|_| trimmed.starts_with("pub fn "))
                })
                && let Some(named) = rest.split('(').next()
            {
                out.push((named.trim().to_string(), name.clone()));
            }
            depth += line.matches('{').count() as i32;
            depth -= line.matches('}').count() as i32;
            if inside > 0 && depth < inside {
                inside = 0;
            }
        }
    }
    out
}

async fn a0_seed_mapping(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    // With no workspace search, the first move an agent has is the role index plus an outline sweep,
    // ranked by how much of the question's vocabulary a declaration's own name and detail carry.
    let roles = service
        .call(Tool::Reflect, args([("file", json!(context.entry_arg()))]))
        .await;
    let role_targets: HashMap<String, String> = adapter::reflect_roles(&roles)
        .into_iter()
        .map(|(node, role)| (node.leaf.clone(), role))
        .collect();
    let wanted = tokens(&question.prompt);
    let mut scored: Vec<(i64, NodeRef)> = Vec::new();
    for file in &context.files {
        for symbol in outline(service, file).await {
            let mut haystack = symbol.node.leaf.clone();
            if let Some(detail) = &symbol.detail {
                haystack.push(' ');
                haystack.push_str(detail);
            }
            if let Some(role) = role_targets.get(&symbol.node.leaf) {
                haystack.push(' ');
                haystack.push_str(role);
            }
            for role in &symbol.roles {
                haystack.push(' ');
                haystack.push_str(role);
            }
            haystack.push(' ');
            haystack.push_str(&file.display().to_string());
            let score = overlap(&wanted, &tokens(&haystack));
            if score > 0 {
                scored.push((score, symbol.node.clone()));
            }
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.leaf.cmp(&b.1.leaf)));
    scored.truncate(10);
    (
        scored
            .into_iter()
            .map(|(_, node)| Prediction::node(node))
            .collect(),
        Some(Tool::Symbols),
    )
}

/// Identifier words, lowercased and split on the usual boundaries.
fn tokens(text: &str) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    let mut word = String::new();
    let push = |word: &mut String, out: &mut BTreeMap<String, usize>| {
        if word.len() >= 3 {
            *out.entry(std::mem::take(word)).or_default() += 1;
        } else {
            word.clear();
        }
    };
    let mut previous_lower = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if ch.is_uppercase() && previous_lower {
                push(&mut word, &mut out);
            }
            word.push(ch.to_ascii_lowercase());
            previous_lower = ch.is_lowercase();
        } else {
            push(&mut word, &mut out);
            previous_lower = false;
        }
    }
    push(&mut word, &mut out);
    out
}

/// How much of the question's vocabulary a candidate carries, with a longer shared word worth more.
fn overlap(wanted: &BTreeMap<String, usize>, have: &BTreeMap<String, usize>) -> i64 {
    let mut score = 0i64;
    for (word, _) in wanted {
        if have.contains_key(word) {
            score += 2 + word.len() as i64;
        } else if have
            .keys()
            .any(|k| k.contains(word.as_str()) || word.contains(k.as_str()))
        {
            score += 1;
        }
    }
    score
}

// ----------------------------------------------------------------------------------------- A5

/// One declaration a lexical scan found.
#[derive(Debug, Clone)]
struct LexDecl {
    name: String,
    file: String,
    start: u32,
    end: u32,
    owner: Option<String>,
    in_test: bool,
    attributes: Vec<String>,
    doc: String,
}

/// Scan every file for declarations and their extents, the way an agent with grep would.
fn scan(service: &mut Service, context: &ProjectContext) -> Vec<LexDecl> {
    let mut out = Vec::new();
    for file in &context.files {
        let text = service.read_file(file);
        out.extend(scan_text(&text, &file.display().to_string()));
    }
    out
}

fn scan_text(text: &str, file: &str) -> Vec<LexDecl> {
    let bytes = text.as_bytes();
    let mut out: Vec<LexDecl> = Vec::new();
    let mut offset = 0usize;
    let mut depth = 0i32;
    let mut owners: Vec<(String, i32)> = Vec::new();
    let mut test_depth: Option<i32> = None;
    let mut pending: Vec<String> = Vec::new();
    let mut doc = String::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            doc.push(' ');
            doc.push_str(trimmed.trim_start_matches('/').trim());
        } else if trimmed.starts_with('#') && trimmed.contains('[') {
            if let Some(name) = trimmed
                .trim_start_matches('#')
                .trim_start_matches('[')
                .split(['(', ']'])
                .next()
            {
                pending.push(name.trim().to_string());
            }
        } else if trimmed.starts_with("@test") {
            test_depth = Some(depth + 1);
        } else if let Some((keyword, rest)) = declaration(trimmed) {
            let name: String = rest
                .split(['(', '{', ':', '<', ' '])
                .next()
                .unwrap_or_default()
                .trim()
                .to_string();
            let name_at = offset + line.find(&name).unwrap_or(0);
            let end = matching_brace(bytes, offset + line.len()).unwrap_or(text.len());
            if !name.is_empty() {
                out.push(LexDecl {
                    name: name.clone(),
                    file: file.to_string(),
                    start: name_at as u32,
                    end: end as u32,
                    owner: owners.last().map(|(o, _)| o.clone()),
                    in_test: test_depth.is_some(),
                    attributes: std::mem::take(&mut pending),
                    doc: std::mem::take(&mut doc),
                });
            }
            if matches!(keyword, "struct" | "class" | "enum" | "trait" | "impl") {
                owners.push((name, depth + 1));
            }
        } else if !trimmed.is_empty() {
            doc.clear();
            pending.clear();
        }
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        owners.retain(|(_, at)| *at <= depth);
        if test_depth.is_some_and(|at| depth < at) {
            test_depth = None;
        }
        offset += line.len();
    }
    out
}

/// The declaration keyword a line opens with, if any.
fn declaration(line: &str) -> Option<(&'static str, &str)> {
    let body = line.strip_prefix("pub ").unwrap_or(line);
    for keyword in ["fn ", "struct ", "class ", "enum ", "trait ", "impl "] {
        if let Some(rest) = body.strip_prefix(keyword) {
            return Some((keyword.trim(), rest));
        }
    }
    None
}

/// The offset just past the brace opened at or before `from`.
fn matching_brace(bytes: &[u8], from: usize) -> Option<usize> {
    let open = bytes[..from].iter().rposition(|b| *b == b'{')?;
    let mut depth = 0i32;
    for (at, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// The lexical call relation: which declaration's body names which other declaration.
fn lexical_edges(
    service: &mut Service,
    context: &ProjectContext,
    decls: &[LexDecl],
) -> Vec<(usize, usize)> {
    let mut texts: HashMap<String, String> = HashMap::new();
    for file in &context.files {
        texts.insert(file.display().to_string(), service.read_file(file));
    }
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (at, decl) in decls.iter().enumerate() {
        by_name.entry(decl.name.as_str()).or_default().push(at);
    }
    let mut out = Vec::new();
    for (at, decl) in decls.iter().enumerate() {
        let Some(text) = texts.get(&decl.file) else {
            continue;
        };
        let body =
            &text[(decl.start as usize).min(text.len())..(decl.end as usize).min(text.len())];
        for (name, targets) in &by_name {
            if body.match_indices(name).any(|(found, _)| {
                let before = body[..found].chars().next_back();
                let after = body[found + name.len()..].chars().next();
                !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                    && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
            }) {
                for target in targets {
                    if *target != at {
                        out.push((at, *target));
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn lexical(
    service: &mut Service,
    context: &ProjectContext,
    question: &Question,
) -> (Vec<Prediction>, Option<Tool>) {
    let decls = scan(service, context);
    let node_of = |decl: &LexDecl| {
        Prediction::node(NodeRef {
            qualified: decl.owner.as_ref().map(|o| format!("{o}.{}", decl.name)),
            leaf: decl.name.clone(),
            file: Some(decl.file.clone()),
            span: Some((decl.start, decl.start + decl.name.len() as u32)),
            kind: None,
        })
    };
    match question.category {
        Category::Definition => {
            let Some(handle) = Handle::of(&question.subject) else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let out = decls
                .iter()
                .filter(|d| d.name == handle.leaf)
                .map(node_of)
                .collect();
            (out, Some(Tool::FileRead))
        }
        Category::Callers => {
            let Some(handle) = Handle::of(&question.subject) else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let edges = lexical_edges(service, context, &decls);
            let targets: Vec<usize> = decls
                .iter()
                .enumerate()
                .filter(|(_, d)| d.name == handle.leaf)
                .map(|(at, _)| at)
                .collect();
            let out = edges
                .iter()
                .filter(|(_, to)| targets.contains(to))
                .map(|(from, _)| node_of(&decls[*from]))
                .collect();
            (out, Some(Tool::FileRead))
        }
        Category::Callees => {
            let Some(handle) = Handle::of(&question.subject) else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let edges = lexical_edges(service, context, &decls);
            let roots: Vec<usize> = decls
                .iter()
                .enumerate()
                .filter(|(_, d)| d.name == handle.leaf)
                .map(|(at, _)| at)
                .collect();
            let reached = closure(&edges, &roots, question.depth, false);
            let out = reached.into_iter().map(|at| node_of(&decls[at])).collect();
            (out, Some(Tool::FileRead))
        }
        Category::Importers => {
            let Subject::Module { path } = &question.subject else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let mut out = Vec::new();
            for file in &context.files {
                let text = service.read_file(file);
                if text.lines().any(|line| {
                    let trimmed = line.trim();
                    trimmed.starts_with("use ")
                        && (trimmed.contains(&format!("{path};"))
                            || trimmed.contains(&format!("{path}."))
                            || trimmed.trim_end() == format!("use {path}"))
                }) {
                    out.push(Prediction::file(file.display().to_string()));
                }
            }
            (out, Some(Tool::FileRead))
        }
        Category::RoleReach => {
            let Some(handle) = Handle::of(&question.subject) else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let edges = lexical_edges(service, context, &decls);
            let roots: Vec<usize> = decls
                .iter()
                .enumerate()
                .filter(|(_, d)| d.name == handle.leaf)
                .map(|(at, _)| at)
                .collect();
            let reached = closure(&edges, &roots, question.depth, false);
            let bearing = role_attributes(&decls);
            let out = reached
                .into_iter()
                .filter(|at| decls[*at].attributes.iter().any(|a| bearing.contains(a)))
                .map(|at| node_of(&decls[at]))
                .collect();
            (out, Some(Tool::FileRead))
        }
        Category::Path => {
            let Subject::Pair { from, to } = &question.subject else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let (Some(start), Some(goal)) = (Handle::of(from), Handle::of(to)) else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let edges = lexical_edges(service, context, &decls);
            let roots: Vec<usize> = decls
                .iter()
                .enumerate()
                .filter(|(_, d)| d.name == start.leaf)
                .map(|(at, _)| at)
                .collect();
            let goals: Vec<usize> = decls
                .iter()
                .enumerate()
                .filter(|(_, d)| d.name == goal.leaf)
                .map(|(at, _)| at)
                .collect();
            let out = shortest(&edges, &roots, &goals)
                .into_iter()
                .map(|at| node_of(&decls[at]))
                .collect();
            (out, Some(Tool::FileRead))
        }
        Category::Impact => {
            let Some(handle) = Handle::of(&question.subject) else {
                return (Vec::new(), Some(Tool::FileRead));
            };
            let edges = lexical_edges(service, context, &decls);
            let roots: Vec<usize> = decls
                .iter()
                .enumerate()
                .filter(|(_, d)| d.name == handle.leaf)
                .map(|(at, _)| at)
                .collect();
            let reached = closure(&edges, &roots, question.depth, true);
            let out = reached
                .into_iter()
                .filter(|at| decls[*at].in_test)
                .map(|at| node_of(&decls[at]))
                .collect();
            (out, Some(Tool::FileRead))
        }
        Category::SeedMapping => {
            let wanted = tokens(&question.prompt);
            let mut scored: Vec<(i64, &LexDecl)> = decls
                .iter()
                .map(|decl| {
                    let haystack = format!(
                        "{} {} {} {}",
                        decl.name,
                        decl.doc,
                        decl.file,
                        decl.attributes.join(" ")
                    );
                    (overlap(&wanted, &tokens(&haystack)), decl)
                })
                .filter(|(score, _)| *score > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
            scored.truncate(10);
            (
                scored.into_iter().map(|(_, decl)| node_of(decl)).collect(),
                Some(Tool::FileRead),
            )
        }
    }
}

/// The attribute names that confer a role, found by reading the project's own `@role` tags.
fn role_attributes(decls: &[LexDecl]) -> BTreeSet<String> {
    decls
        .iter()
        .filter(|d| d.doc.contains("@role") || d.attributes.iter().any(|a| a.contains("role")))
        .map(|d| d.name.clone())
        .chain(
            decls
                .iter()
                .flat_map(|d| d.attributes.iter().cloned())
                .filter(|a| a.chars().next().is_some_and(char::is_uppercase)),
        )
        .collect()
}

/// Forward (or reverse) reachability over a lexical edge list, up to `depth` hops.
fn closure(edges: &[(usize, usize)], roots: &[usize], depth: usize, reverse: bool) -> Vec<usize> {
    let mut seen: BTreeSet<usize> = roots.iter().copied().collect();
    let mut out: BTreeSet<usize> = BTreeSet::new();
    let mut frontier: VecDeque<(usize, usize)> = roots.iter().map(|r| (*r, 0)).collect();
    while let Some((at, hops)) = frontier.pop_front() {
        if hops == depth {
            continue;
        }
        for (from, to) in edges {
            let (source, target) = if reverse { (*to, *from) } else { (*from, *to) };
            if source == at && seen.insert(target) {
                out.insert(target);
                frontier.push_back((target, hops + 1));
            }
        }
    }
    out.into_iter().collect()
}

/// The shortest lexical path from any root to any goal.
fn shortest(edges: &[(usize, usize)], roots: &[usize], goals: &[usize]) -> Vec<usize> {
    let mut previous: HashMap<usize, usize> = HashMap::new();
    let mut seen: BTreeSet<usize> = roots.iter().copied().collect();
    let mut frontier: VecDeque<usize> = roots.iter().copied().collect();
    while let Some(at) = frontier.pop_front() {
        if goals.contains(&at) {
            let mut path = vec![at];
            let mut cursor = at;
            while let Some(before) = previous.get(&cursor) {
                path.push(*before);
                cursor = *before;
            }
            path.reverse();
            return path;
        }
        for (from, to) in edges {
            if *from == at && seen.insert(*to) {
                previous.insert(*to, at);
                frontier.push_back(*to);
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lexical_scan_finds_declarations_their_owners_and_their_tests() {
        let text = "\
pub struct Item {
    sku: string

    pub fn label(): string { return self.sku }
}

pub fn build(): Item { return Item { sku: \"x\" } }

@test {
    fn builds(): void { assert(build().label() == \"x\") }
}
";
        let decls = scan_text(text, "a.noe");
        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["Item", "label", "build", "builds"]);
        assert_eq!(decls[1].owner.as_deref(), Some("Item"));
        assert!(!decls[2].in_test);
        assert!(decls[3].in_test);
    }

    #[test]
    fn tokens_split_on_case_and_drop_short_words() {
        let words = tokens("where does an OrderStore persist");
        assert!(words.contains_key("order"));
        assert!(words.contains_key("store"));
        assert!(words.contains_key("persist"));
        assert!(!words.contains_key("an"));
    }
}
