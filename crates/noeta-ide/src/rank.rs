//! The **context map**: rank a program's declarations from a set of seeds and emit as many of them
//! as a token budget holds, connected to a seed.
//!
//! The graph ranked here is the [`callgraph`](crate::callgraph) plus the `use` import relation.
//! Nodes are the program's functions and its modules; an edge carries one of three kinds, weighted
//! so a call pulls harder than a passed reference and a reference harder than an import. Personalized
//! PageRank over that graph answers "given that I care about these declarations, what else is close
//! enough to matter", which is the question a budgeted map has to answer before it can choose what
//! to emit.
//!
//! The map is a **connected subgraph**, not a top-k list. Selection starts at the seeds and only
//! ever admits a node adjacent to something already in the map, so every declaration a reader gets
//! has a stated route back to what they asked about, carried on the node as `via`. A declaration
//! nothing connects to a seed is left out however well it scores.
//!
//! [`Ranker`] is the ablation knob: `ppr` is the ranking above, `degree` scores by weighted degree
//! alone (importance with no regard for the seeds), and `random` scores from a seeded PRNG. The
//! selection, the budget and the connectivity rule are identical under all three, so a benchmark
//! comparing them measures the ranking and nothing else.

use std::collections::{HashMap, HashSet};

use noeta_ast::{Program, Stmt};
use noeta_span::Span;

use crate::callgraph::{CallGraph, Callee, NameLookup};

/// The weight of a syntactic call (`f(...)`) — the strongest structural claim one declaration makes
/// on another.
pub const WEIGHT_CALL: f64 = 4.0;
/// The weight of a passed reference (a callback, a handler registration): real flow, one
/// indirection away from a call.
pub const WEIGHT_REFERENCE: f64 = 2.0;
/// The weight of the import relation — a `use` edge between two modules, and the membership edge
/// between a module and each declaration it holds.
pub const WEIGHT_IMPORT: f64 = 1.0;

/// PageRank's damping: the share of a node's mass that flows along its edges, with the rest
/// teleporting back to the seeds.
pub const DAMPING: f64 = 0.85;
/// Power iterations before the ranker stops, whether or not it converged.
pub const MAX_ITERATIONS: usize = 64;
/// The L1 change between two iterations below which the ranking is settled.
pub const CONVERGENCE: f64 = 1e-9;

/// How many characters one token is counted as.
pub const CHARS_PER_TOKEN: usize = 4;
/// The budget a request that names none gets.
pub const DEFAULT_BUDGET_TOKENS: usize = 4096;
/// The largest budget a request can ask for.
pub const MAX_BUDGET_TOKENS: usize = 65536;

/// What kind of relation an edge carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// A syntactic call.
    Call,
    /// The callee passed as a value.
    Reference,
    /// A `use` between two modules, or a module and one of its declarations.
    Import,
}

impl EdgeKind {
    /// Every kind, in weight order.
    pub const ALL: [EdgeKind; 3] = [EdgeKind::Call, EdgeKind::Reference, EdgeKind::Import];

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Call => "call",
            EdgeKind::Reference => "reference",
            EdgeKind::Import => "import",
        }
    }

    pub fn weight(self) -> f64 {
        match self {
            EdgeKind::Call => WEIGHT_CALL,
            EdgeKind::Reference => WEIGHT_REFERENCE,
            EdgeKind::Import => WEIGHT_IMPORT,
        }
    }
}

impl std::fmt::Display for EdgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EdgeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<EdgeKind, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "call" => Ok(EdgeKind::Call),
            "reference" => Ok(EdgeKind::Reference),
            "import" => Ok(EdgeKind::Import),
            other => Err(format!(
                "`{other}` is no edge kind — use call, reference or import"
            )),
        }
    }
}

/// Which ranking scores the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Ranker {
    /// Personalized PageRank seeded on the request's seeds.
    #[default]
    Ppr,
    /// Weighted degree — importance in the graph as a whole, blind to the seeds.
    Degree,
    /// A seeded PRNG.
    Random,
}

impl Ranker {
    pub fn as_str(self) -> &'static str {
        match self {
            Ranker::Ppr => "ppr",
            Ranker::Degree => "degree",
            Ranker::Random => "random",
        }
    }
}

impl std::fmt::Display for Ranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Ranker {
    type Err = String;

    fn from_str(s: &str) -> Result<Ranker, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ppr" => Ok(Ranker::Ppr),
            "degree" => Ok(Ranker::Degree),
            "random" => Ok(Ranker::Random),
            other => Err(format!(
                "`{other}` is no ranker — use ppr, degree or random"
            )),
        }
    }
}

/// One source's identity, as the caller names it: the module path the program addresses it by, and
/// the file a reader opens.
#[derive(Debug, Clone, Default)]
pub struct ModuleId {
    /// The dotted module path (`app.store`), empty when the source has none.
    pub name: String,
    /// The file name, as the caller reports files.
    pub file: String,
}

/// What a rank-graph node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSort {
    Function,
    Method,
    Module,
}

impl NodeSort {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeSort::Function => "function",
            NodeSort::Method => "method",
            NodeSort::Module => "module",
        }
    }
}

/// One node of the ranked graph.
#[derive(Debug, Clone)]
pub struct Node {
    /// The declaration's post-link name, or the module's path (its file when it has no path).
    pub name: String,
    pub sort: NodeSort,
    /// The call-graph index, for a declaration node.
    pub function: Option<usize>,
    /// The source this node lives in, by [`noeta_span::SourceId`] index.
    pub source: usize,
    /// The declared name's span, for a declaration node.
    pub decl_span: Option<Span>,
    /// The whole declaration's span, for a declaration node.
    pub body_span: Option<Span>,
    /// The file this node lives in, as the caller names files.
    pub file: String,
}

/// One directed edge of the ranked graph.
#[derive(Debug, Clone, Copy)]
pub struct Arc {
    pub to: usize,
    pub kind: EdgeKind,
}

/// The call graph and the import relation as one ranked graph.
#[derive(Debug, Clone, Default)]
pub struct RankGraph {
    pub nodes: Vec<Node>,
    /// Out-adjacency, one list per node.
    pub out: Vec<Vec<Arc>>,
    /// In-adjacency, one list per node — the reverse of `out`, carried so connectivity and degree
    /// read both directions without a second pass.
    pub incoming: Vec<Vec<Arc>>,
}

impl RankGraph {
    /// The node named `name`: a declaration by post-link name or unique leaf, else a module by its
    /// path or its file.
    pub fn node_named(&self, graph: &CallGraph, name: &str) -> NodeLookup {
        match graph.lookup_named(name) {
            NameLookup::Found(f) => {
                if let Some(i) = self.nodes.iter().position(|n| n.function == Some(f)) {
                    return NodeLookup::Found(i);
                }
            }
            NameLookup::Ambiguous(candidates) => return NodeLookup::Ambiguous(candidates),
            NameLookup::Missing => {}
        }
        let want = name.trim();
        let module = self.nodes.iter().position(|n| {
            n.sort == NodeSort::Module
                && (n.name == want
                    || n.file_matches(want)
                    || (!n.name.is_empty() && n.name.ends_with(&format!(".{want}"))))
        });
        match module {
            Some(i) => NodeLookup::Found(i),
            None => NodeLookup::Missing,
        }
    }

    /// The neighbors of `node` in either direction, restricted to `kinds`.
    fn neighbors(&self, node: usize, kinds: &[EdgeKind]) -> Vec<(usize, EdgeKind)> {
        let keep = |a: &&Arc| kinds.contains(&a.kind);
        self.out[node]
            .iter()
            .filter(keep)
            .chain(self.incoming[node].iter().filter(keep))
            .map(|a| (a.to, a.kind))
            .collect()
    }
}

impl Node {
    /// Whether `want` names this node's file — the whole name, or a trailing path segment run of
    /// it, so `store.noe` finds `src/store.noe`.
    fn file_matches(&self, want: &str) -> bool {
        let file = self.file.as_str();
        if file.is_empty() || want.is_empty() {
            return false;
        }
        // Either spelling may be the longer one: a caller naming `store.noe` for `src/store.noe`,
        // or naming an absolute path for the project-relative name the map reports.
        file == want || file.ends_with(&format!("/{want}")) || want.ends_with(&format!("/{file}"))
    }
}

/// What resolving a seed name found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeLookup {
    Found(usize),
    Ambiguous(Vec<String>),
    Missing,
}

/// Build the ranked graph. `modules[i]` identifies source `i`; a source the slice does not cover
/// gets an empty identity and still holds its declarations.
pub fn build(graph: &CallGraph, program: &Program, modules: &[ModuleId]) -> RankGraph {
    let sources = source_count(graph, modules);
    let mut nodes: Vec<Node> = graph
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| Node {
            name: f.name.clone(),
            sort: if f.method {
                NodeSort::Method
            } else {
                NodeSort::Function
            },
            function: Some(i),
            source: f.name_span.source.0 as usize,
            decl_span: Some(f.name_span),
            body_span: Some(f.decl_span),
            file: modules
                .get(f.name_span.source.0 as usize)
                .map(|m| m.file.clone())
                .unwrap_or_default(),
        })
        .collect();
    // The module nodes follow the declarations, so a function's own index is its call-graph index.
    let first_module = nodes.len();
    for source in 0..sources {
        let id = modules.get(source).cloned().unwrap_or_default();
        let name = if id.name.is_empty() {
            id.file.clone()
        } else {
            id.name.clone()
        };
        nodes.push(Node {
            name,
            sort: NodeSort::Module,
            function: None,
            source,
            decl_span: None,
            body_span: None,
            file: id.file,
        });
    }

    let mut arcs: Vec<(usize, usize, EdgeKind)> = Vec::new();
    // Call and reference edges: only a callee the graph holds is a node, so an external or dynamic
    // leaf contributes nothing to the ranking. It is still an honest answer elsewhere; it is just
    // not a declaration a map can emit.
    for edge in &graph.edges {
        let (Some(caller), Callee::Function(callee)) = (edge.caller, &edge.callee) else {
            continue;
        };
        if caller == *callee {
            continue; // a self-call adds no proximity
        }
        arcs.push((
            caller,
            *callee,
            if edge.call {
                EdgeKind::Call
            } else {
                EdgeKind::Reference
            },
        ));
    }
    // Membership: a module and each declaration it holds, both ways, so a file's declarations are
    // mutually reachable and a file seed reaches what it declares.
    for (i, f) in graph.functions.iter().enumerate() {
        let module = first_module + f.name_span.source.0 as usize;
        if module < nodes.len() {
            arcs.push((module, i, EdgeKind::Import));
            arcs.push((i, module, EdgeKind::Import));
        }
    }
    // `use` edges between modules.
    let by_name: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.sort == NodeSort::Module && !n.name.is_empty())
        .map(|(i, n)| (n.name.as_str(), i))
        .collect();
    for stmt in &program.stmts {
        let Stmt::Use { path, names, span } = stmt else {
            continue;
        };
        let from = first_module + span.source.0 as usize;
        if from >= nodes.len() {
            continue;
        }
        let base = path.join(".");
        // `use pkg.alpha` spells the package as the path and the module as the imported name, so a
        // target is `{path}` or `{path}.{name}` — the resolution `module_graph` reports.
        let mut targets: Vec<usize> = Vec::new();
        if let Some(&to) = by_name.get(base.as_str()) {
            targets.push(to);
        }
        for n in &names[..] {
            // A one-segment `use lib` parses with an empty path and the module as the imported
            // name, so the candidate is the name itself.
            let candidate = if base.is_empty() {
                n.name.clone()
            } else {
                format!("{base}.{}", n.name)
            };
            if let Some(&to) = by_name.get(candidate.as_str()) {
                targets.push(to);
            }
        }
        for to in targets {
            if to != from {
                arcs.push((from, to, EdgeKind::Import));
            }
        }
    }

    let mut out: Vec<Vec<Arc>> = vec![Vec::new(); nodes.len()];
    let mut incoming: Vec<Vec<Arc>> = vec![Vec::new(); nodes.len()];
    let mut seen: HashSet<(usize, usize, EdgeKind)> = HashSet::new();
    for (from, to, kind) in arcs {
        if !seen.insert((from, to, kind)) {
            continue;
        }
        out[from].push(Arc { to, kind });
        incoming[to].push(Arc { to: from, kind });
    }
    RankGraph {
        nodes,
        out,
        incoming,
    }
}

/// How many sources the graph spans: enough to cover every declaration and every named module.
fn source_count(graph: &CallGraph, modules: &[ModuleId]) -> usize {
    let widest = graph
        .functions
        .iter()
        .map(|f| f.name_span.source.0 as usize + 1)
        .max()
        .unwrap_or(0);
    widest.max(modules.len())
}

/// Score every node. `seeds` personalizes [`Ranker::Ppr`] and is ignored by the other two.
pub fn score(
    graph: &RankGraph,
    seeds: &[usize],
    kinds: &[EdgeKind],
    ranker: Ranker,
    seed: u64,
) -> Vec<f64> {
    match ranker {
        Ranker::Ppr => personalized_pagerank(graph, seeds, kinds),
        Ranker::Degree => degree(graph, kinds),
        Ranker::Random => random(graph.nodes.len(), seed),
    }
}

/// Personalized PageRank: mass teleports to the seeds rather than to the whole graph, so the
/// ranking reads "close to what was asked for" instead of "important overall".
///
/// Mass flows **both ways along an edge**, because proximity is symmetric: a declaration's callers
/// are as much a part of what a reader needs as its callees, and a map that ranked only forward
/// would score every caller at zero and then drop it under the same rule that keeps the map
/// connected. The edge's kind still weights it, so a call pulls harder than an import in either
/// direction.
fn personalized_pagerank(graph: &RankGraph, seeds: &[usize], kinds: &[EdgeKind]) -> Vec<f64> {
    let n = graph.nodes.len();
    if n == 0 || seeds.is_empty() {
        return vec![0.0; n];
    }
    let mut teleport = vec![0.0; n];
    let share = 1.0 / seeds.len() as f64;
    for &s in seeds {
        if s < n {
            teleport[s] += share;
        }
    }
    // Out-weights, restricted to the requested kinds. A node with none is a sink; its mass returns
    // to the teleport distribution rather than vanishing, so the vector stays a distribution.
    let out: Vec<Vec<(usize, f64)>> = (0..n)
        .map(|i| {
            graph.out[i]
                .iter()
                .chain(graph.incoming[i].iter())
                .filter(|a| kinds.contains(&a.kind))
                .map(|a| (a.to, a.kind.weight()))
                .collect()
        })
        .collect();
    let totals: Vec<f64> = out
        .iter()
        .map(|arcs| arcs.iter().map(|(_, w)| *w).sum())
        .collect();

    let mut rank = teleport.clone();
    for _ in 0..MAX_ITERATIONS {
        let mut next = vec![0.0; n];
        let mut dangling = 0.0;
        for i in 0..n {
            if totals[i] <= 0.0 {
                dangling += rank[i];
                continue;
            }
            for (to, w) in &out[i] {
                next[*to] += rank[i] * (w / totals[i]);
            }
        }
        let mut delta = 0.0;
        for i in 0..n {
            let value =
                DAMPING * (next[i] + dangling * teleport[i]) + (1.0 - DAMPING) * teleport[i];
            delta += (value - rank[i]).abs();
            next[i] = value;
        }
        rank = next;
        if delta < CONVERGENCE {
            break;
        }
    }
    rank
}

/// Weighted degree in both directions, normalized to the largest.
fn degree(graph: &RankGraph, kinds: &[EdgeKind]) -> Vec<f64> {
    let raw: Vec<f64> = (0..graph.nodes.len())
        .map(|i| {
            let sum = |arcs: &Vec<Arc>| -> f64 {
                arcs.iter()
                    .filter(|a| kinds.contains(&a.kind))
                    .map(|a| a.kind.weight())
                    .sum()
            };
            sum(&graph.out[i]) + sum(&graph.incoming[i])
        })
        .collect();
    let max = raw.iter().copied().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return raw;
    }
    raw.into_iter().map(|v| v / max).collect()
}

/// A deterministic pseudo-random score per node — the control a ranking claim is measured against.
fn random(n: usize, seed: u64) -> Vec<f64> {
    (0..n)
        .map(|i| {
            // splitmix64, so the same (seed, index) always scores the same and two adjacent indices
            // do not score adjacently.
            let mut z = seed
                .wrapping_add(i as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        })
        .collect()
}

/// How a node entered the map.
#[derive(Debug, Clone, Copy)]
pub struct Via {
    /// The node already in the map that pulled this one in.
    pub from: usize,
    pub kind: EdgeKind,
}

/// One emitted declaration.
#[derive(Debug, Clone)]
pub struct MapNode {
    /// The index into [`RankGraph::nodes`].
    pub node: usize,
    /// 1-based position in the emitted order.
    pub rank: usize,
    pub score: f64,
    /// The edge that pulled this node in. Absent on a seed.
    pub via: Option<Via>,
    /// The declaration's signature — its source from the declaration's start up to its body.
    pub signature: String,
    /// What the signature cost against the budget.
    pub tokens: usize,
}

/// A budgeted, connected map.
#[derive(Debug, Clone, Default)]
pub struct ContextMap {
    /// The resolved seed nodes, in request order.
    pub seeds: Vec<usize>,
    /// Seed specs that named nothing.
    pub missing: Vec<String>,
    /// Seed specs whose leaf several declarations carry, with the candidates.
    pub ambiguous: Vec<(String, Vec<String>)>,
    /// The emitted declarations, strongest first.
    pub nodes: Vec<MapNode>,
    pub budget_tokens: usize,
    pub used_tokens: usize,
    pub ranker: Ranker,
    /// True when a connected, positively-scored declaration did not fit.
    pub truncated: bool,
}

/// What a caller asks the map for.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub seeds: &'a [String],
    pub budget_tokens: usize,
    pub edge_kinds: Vec<EdgeKind>,
    pub ranker: Ranker,
    /// The PRNG seed [`Ranker::Random`] draws from.
    pub seed: u64,
}

impl Request<'_> {
    /// The budget, clamped to what a map will emit.
    fn budget(&self) -> usize {
        self.budget_tokens.clamp(1, MAX_BUDGET_TOKENS)
    }
}

/// What resolving a request's seed specs produced.
#[derive(Debug, Clone, Default)]
pub struct Seeds {
    /// The resolved nodes, in request order, each named once.
    pub nodes: Vec<usize>,
    /// Specs that named nothing.
    pub missing: Vec<String>,
    /// Specs whose leaf several declarations carry, with the candidates.
    pub ambiguous: Vec<(String, Vec<String>)>,
}

/// Resolve `specs` to seed nodes: a role name seeds every function bearing it, else a declaration
/// name, else a module path or file.
pub fn resolve_seeds(
    rank: &RankGraph,
    graph: &CallGraph,
    roles_by_target: &HashMap<String, Vec<String>>,
    specs: &[String],
) -> Seeds {
    let mut seeds: Vec<usize> = Vec::new();
    let mut missing = Vec::new();
    let mut ambiguous = Vec::new();
    for spec in specs {
        let want = spec.trim().to_ascii_lowercase();
        // A role seeds every bearer. Matched on the variant alone (`EntryPoint`) or qualified
        // (`Semantic.EntryPoint`), the two spellings `trace` accepts.
        let bearers: Vec<usize> = rank
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                roles_by_target
                    .get(&n.name)
                    .is_some_and(|roles| roles.iter().any(|r| role_matches(r, &want)))
            })
            .map(|(i, _)| i)
            .collect();
        if !bearers.is_empty() {
            for b in bearers {
                if !seeds.contains(&b) {
                    seeds.push(b);
                }
            }
            continue;
        }
        match rank.node_named(graph, spec) {
            NodeLookup::Found(i) => {
                if !seeds.contains(&i) {
                    seeds.push(i);
                }
            }
            NodeLookup::Ambiguous(candidates) => ambiguous.push((spec.clone(), candidates)),
            NodeLookup::Missing => missing.push(spec.clone()),
        }
    }
    Seeds {
        nodes: seeds,
        missing,
        ambiguous,
    }
}

/// Whether a role binding (`Semantic.EntryPoint`) answers to `want` (already lowercased).
fn role_matches(role: &str, want: &str) -> bool {
    let lower = role.to_ascii_lowercase();
    lower == want || lower.rsplit_once('.').is_some_and(|(_, leaf)| leaf == want)
}

/// Build the map: score the graph, then grow a connected subgraph from the seeds, taking the
/// highest-scoring reachable declaration each step until the budget is full.
///
/// The first seed is always emitted, even when its own signature is larger than the budget, so a
/// request with a small budget gets an answer rather than an empty map.
pub fn context_map(
    rank: &RankGraph,
    graph: &CallGraph,
    roles_by_target: &HashMap<String, Vec<String>>,
    texts: &[&str],
    request: &Request<'_>,
) -> ContextMap {
    let Seeds {
        nodes: seeds,
        missing,
        ambiguous,
    } = resolve_seeds(rank, graph, roles_by_target, request.seeds);
    let kinds = if request.edge_kinds.is_empty() {
        EdgeKind::ALL.to_vec()
    } else {
        request.edge_kinds.clone()
    };
    let scores = score(rank, &seeds, &kinds, request.ranker, request.seed);
    let budget = request.budget();

    let mut map = ContextMap {
        seeds: seeds.clone(),
        missing,
        ambiguous,
        nodes: Vec::new(),
        budget_tokens: budget,
        used_tokens: 0,
        ranker: request.ranker,
        truncated: false,
    };
    if seeds.is_empty() {
        return map;
    }

    let mut included: HashSet<usize> = HashSet::new();
    let mut rejected: HashSet<usize> = HashSet::new();
    // Everything adjacent to the map that is not in it yet, with the best edge that reaches it.
    let mut frontier: HashMap<usize, Via> = HashMap::new();
    let admit =
        |node: usize, via: Option<Via>, map: &mut ContextMap, included: &mut HashSet<usize>| {
            included.insert(node);
            let Some(signature) = signature_of(&rank.nodes[node], texts) else {
                return; // a module node carries the grouping, not a signature
            };
            let tokens = signature.len().div_ceil(CHARS_PER_TOKEN);
            map.used_tokens += tokens;
            map.nodes.push(MapNode {
                node,
                rank: map.nodes.len() + 1,
                score: scores[node],
                via,
                signature,
                tokens,
            });
        };

    for (i, &seed) in seeds.iter().enumerate() {
        // The first seed is emitted whatever it costs; a map that answers nothing because the
        // budget was small is worse than one that overshoots by a single signature.
        if i > 0 && !room_for(rank, seed, texts, map.used_tokens, budget) {
            map.truncated = true;
            rejected.insert(seed);
            continue;
        }
        admit(seed, None, &mut map, &mut included);
    }
    // Seeded in request order, so a tie between two routes into the same node resolves the same
    // way on every run.
    for &node in seeds.iter().filter(|s| included.contains(s)) {
        push_neighbors(rank, node, &kinds, &included, &rejected, &mut frontier);
    }

    while let Some((node, via)) = best(&frontier, &scores, rank) {
        frontier.remove(&node);
        if scores[node] <= 0.0 {
            rejected.insert(node);
            continue;
        }
        if !room_for(rank, node, texts, map.used_tokens, budget) {
            // A declaration that does not fit is passed over, not the end of the walk: a smaller
            // one further down the ranking may still fit, and the map should carry it.
            map.truncated = true;
            rejected.insert(node);
            continue;
        }
        admit(node, Some(via), &mut map, &mut included);
        push_neighbors(rank, node, &kinds, &included, &rejected, &mut frontier);
    }
    map
}

/// Whether `node`'s signature fits in what is left of the budget. A module node costs nothing: it
/// is the grouping the map emits declarations under, not a declaration.
fn room_for(rank: &RankGraph, node: usize, texts: &[&str], used: usize, budget: usize) -> bool {
    match signature_of(&rank.nodes[node], texts) {
        Some(sig) => used + sig.len().div_ceil(CHARS_PER_TOKEN) <= budget,
        None => true,
    }
}

/// The highest-scoring frontier node, ties broken by name so two runs agree.
fn best(frontier: &HashMap<usize, Via>, scores: &[f64], rank: &RankGraph) -> Option<(usize, Via)> {
    frontier
        .iter()
        .max_by(|(a, _), (b, _)| {
            scores[**a]
                .partial_cmp(&scores[**b])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| rank.nodes[**b].name.cmp(&rank.nodes[**a].name))
                .then_with(|| b.cmp(a))
        })
        .map(|(node, via)| (*node, *via))
}

/// Offer every neighbor of `node` to the frontier, keeping the strongest edge that reaches each.
fn push_neighbors(
    rank: &RankGraph,
    node: usize,
    kinds: &[EdgeKind],
    included: &HashSet<usize>,
    rejected: &HashSet<usize>,
    frontier: &mut HashMap<usize, Via>,
) {
    for (neighbor, kind) in rank.neighbors(node, kinds) {
        if included.contains(&neighbor) || rejected.contains(&neighbor) {
            continue;
        }
        let via = Via { from: node, kind };
        frontier
            .entry(neighbor)
            .and_modify(|held| {
                if kind < held.kind {
                    *held = via;
                }
            })
            .or_insert(via);
    }
}

/// A declaration's signature: its source from the declaration's start up to the body's opening
/// brace, with interior whitespace collapsed. `None` for a module node.
pub fn signature_of(node: &Node, texts: &[&str]) -> Option<String> {
    let span = node.body_span?;
    let text = texts.get(span.source.0 as usize)?;
    let decl = text.get(span.start as usize..span.end as usize)?;
    let head = decl.split_once('{').map_or(decl, |(head, _)| head);
    let collapsed = head.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if collapsed.is_empty() {
        node.name.clone()
    } else {
        collapsed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    /// Parse `sources` (one per `SourceId`) and merge their statements into one program, the way a
    /// linked workspace hands the call graph a single program spanning several files.
    fn setup(sources: &[(&str, &str)]) -> (CallGraph, RankGraph, Vec<String>, Vec<ModuleId>) {
        let parsed: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(i, &(name, text))| {
                let source = Source::new(SourceId(i as u32), name, text);
                let lexed = noeta_lexer::lex(&source);
                let parsed = noeta_parser::parse(&source, &lexed.tokens);
                assert!(
                    lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
                    "fixture {name} parses"
                );
                parsed.program
            })
            .collect();
        let mut program = parsed[0].clone();
        for extra in &parsed[1..] {
            program.stmts.extend(extra.stmts.iter().cloned());
        }
        let texts: Vec<String> = sources.iter().map(|(_, t)| t.to_string()).collect();
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let checked = noeta_check::check_all_with_types(&program);
        let graph =
            crate::callgraph::build(&program, &checked.expr_types, &checked.sites, &borrowed);
        let modules: Vec<ModuleId> = sources
            .iter()
            .map(|(name, _)| ModuleId {
                name: name.trim_end_matches(".noe").to_string(),
                file: name.to_string(),
            })
            .collect();
        let rank = build(&graph, &program, &modules);
        (graph, rank, texts, modules)
    }

    fn request<'a>(seeds: &'a [String], budget: usize, ranker: Ranker) -> Request<'a> {
        Request {
            seeds,
            budget_tokens: budget,
            edge_kinds: Vec::new(),
            ranker,
            seed: 1,
        }
    }

    fn names(map: &ContextMap, rank: &RankGraph) -> Vec<String> {
        map.nodes
            .iter()
            .map(|n| rank.nodes[n.node].name.clone())
            .collect()
    }

    /// A hub every module calls, and a leaf with a private chain of its own.
    const HUB: &str = "\
fn deep_a(): int { return 1 }
fn helper_a(): int { return deep_a() }
fn leaf_a(): int { return helper_a() }
fn unrelated_1(): int { return 1 }
fn unrelated_2(): int { return 1 }
fn unrelated_3(): int { return 1 }
fn unrelated_4(): int { return 1 }
fn unrelated_5(): int { return 1 }
fn unrelated_6(): int { return 1 }
fn hub(): int { return unrelated_1() + unrelated_2() + unrelated_3() + unrelated_4() + unrelated_5() + unrelated_6() }
fn caller_1(): int { return hub() }
fn caller_2(): int { return hub() }
fn caller_3(): int { return hub() }
";

    /// PPR seeded on a leaf ranks that leaf's own chain above the hub's callees; degree centrality
    /// ranks the hub first because it is blind to the seed.
    #[test]
    fn ppr_follows_the_seed_where_degree_follows_the_hub() {
        let (graph, rank, texts, _) = setup(&[("app.noe", HUB)]);
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let seeds = vec!["leaf_a".to_string()];
        let roles = HashMap::new();

        let ppr = context_map(
            &rank,
            &graph,
            &roles,
            &borrowed,
            &request(&seeds, 4096, Ranker::Ppr),
        );
        let order = names(&ppr, &rank);
        let at = |name: &str| order.iter().position(|n| n == name).unwrap_or(usize::MAX);
        assert_eq!(order[0], "leaf_a", "the seed leads: {order:?}");
        assert!(
            at("helper_a") < at("unrelated_1") && at("deep_a") < at("unrelated_1"),
            "the seed's own chain outranks the hub's callees: {order:?}"
        );
        assert!(
            at("helper_a") < at("hub"),
            "the seed's neighbor outranks the hub: {order:?}"
        );

        let by_degree = context_map(
            &rank,
            &graph,
            &roles,
            &borrowed,
            &request(&seeds, 4096, Ranker::Degree),
        );
        let order = names(&by_degree, &rank);
        assert_eq!(order[0], "leaf_a", "the seed still leads: {order:?}");
        assert_eq!(
            order[1], "hub",
            "degree takes the most-connected node next: {order:?}"
        );
    }

    /// The map fills its budget and stops: nothing emitted overshoots it, and the next candidate
    /// would have.
    #[test]
    fn the_budget_is_filled_to_within_one_signature() {
        let (graph, rank, texts, _) = setup(&[("app.noe", HUB)]);
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let seeds = vec!["leaf_a".to_string()];
        let roles = HashMap::new();

        let full = context_map(
            &rank,
            &graph,
            &roles,
            &borrowed,
            &request(&seeds, 4096, Ranker::Ppr),
        );
        assert!(full.nodes.len() > 4, "the whole program fits at 4k");
        assert!(!full.truncated);

        let budget = full.used_tokens / 2;
        let small = context_map(
            &rank,
            &graph,
            &roles,
            &borrowed,
            &request(&seeds, budget, Ranker::Ppr),
        );
        assert!(
            small.used_tokens <= budget,
            "used {} over a budget of {budget}",
            small.used_tokens
        );
        assert!(small.truncated, "something was left out");
        // Everything the small map holds, the full one holds too, in the same relative order:
        // both admit by descending score, so the budget subsets the ranking rather than
        // reordering it.
        let (small_names, full_names) = (names(&small, &rank), names(&full, &rank));
        let mut walk = full_names.iter();
        for name in &small_names {
            assert!(
                walk.any(|f| f == name),
                "{name} is out of order against {full_names:?}"
            );
        }
        // And every declaration it left out is one that would not have fit — the map stopped on
        // the budget, not on the graph.
        for node in &full.nodes {
            let name = &rank.nodes[node.node].name;
            if small_names.contains(name) {
                continue;
            }
            assert!(
                small.used_tokens + node.tokens > budget,
                "{name} costs {} and {} of {budget} was spent — it should have been emitted",
                node.tokens,
                small.used_tokens
            );
        }
    }

    /// A module nothing imports and nothing calls is never emitted, however well it scores.
    #[test]
    fn a_disconnected_module_is_never_included() {
        let (graph, rank, texts, _) = setup(&[
            ("app.noe", "fn entry(): int { return 1 }\n"),
            (
                "island.noe",
                "fn marooned(): int { return 2 }\nfn also_marooned(): int { return marooned() }\n",
            ),
        ]);
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let seeds = vec!["entry".to_string()];
        let map = context_map(
            &rank,
            &graph,
            &HashMap::new(),
            &borrowed,
            &request(&seeds, 4096, Ranker::Ppr),
        );
        let order = names(&map, &rank);
        assert_eq!(
            order,
            vec!["entry"],
            "only the seed's own component: {order:?}"
        );
        assert!(!map.truncated, "nothing was cut for budget");
        // The random ranker cannot smuggle one in either: connectivity is the gate, not the score.
        let random = context_map(
            &rank,
            &graph,
            &HashMap::new(),
            &borrowed,
            &request(&seeds, 4096, Ranker::Random),
        );
        assert_eq!(names(&random, &rank), vec!["entry"]);
    }

    /// An imported module's declarations are reachable, which is what the import edge buys.
    #[test]
    fn an_import_edge_connects_two_modules() {
        let (graph, rank, texts, _) = setup(&[
            ("app.noe", "use lib\nfn entry(): int { return 1 }\n"),
            ("lib.noe", "fn helper(): int { return 2 }\n"),
        ]);
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let seeds = vec!["entry".to_string()];
        let map = context_map(
            &rank,
            &graph,
            &HashMap::new(),
            &borrowed,
            &request(&seeds, 4096, Ranker::Ppr),
        );
        let order = names(&map, &rank);
        assert!(order.contains(&"helper".to_string()), "{order:?}");
        let helper = map
            .nodes
            .iter()
            .find(|n| rank.nodes[n.node].name == "helper")
            .expect("helper is in the map");
        assert_eq!(
            helper.via.expect("a non-seed carries its route").kind,
            EdgeKind::Import
        );
    }

    /// A role seeds every function bearing it.
    #[test]
    fn a_role_seed_expands_to_its_bearers() {
        let (graph, rank, texts, _) = setup(&[(
            "app.noe",
            "fn first(): int { return 1 }\nfn second(): int { return 2 }\nfn third(): int { return 3 }\n",
        )]);
        let borrowed: Vec<&str> = texts.iter().map(String::as_str).collect();
        let mut roles: HashMap<String, Vec<String>> = HashMap::new();
        roles.insert("first".to_string(), vec!["Semantic.EntryPoint".to_string()]);
        roles.insert(
            "second".to_string(),
            vec!["Semantic.EntryPoint".to_string()],
        );
        let map = context_map(
            &rank,
            &graph,
            &roles,
            &borrowed,
            &request(&["EntryPoint".to_string()], 4096, Ranker::Ppr),
        );
        let seeded: Vec<&str> = map
            .seeds
            .iter()
            .map(|&i| rank.nodes[i].name.as_str())
            .collect();
        assert_eq!(seeded, vec!["first", "second"], "both bearers seed the map");
        assert!(map.nodes[0].via.is_none() && map.nodes[1].via.is_none());
        // The qualified spelling seeds the same pair.
        let qualified = context_map(
            &rank,
            &graph,
            &roles,
            &borrowed,
            &request(&["Semantic.EntryPoint".to_string()], 4096, Ranker::Ppr),
        );
        assert_eq!(qualified.seeds, map.seeds);
    }

    #[test]
    fn edge_kinds_and_rankers_round_trip_their_spelling() {
        for kind in EdgeKind::ALL {
            assert_eq!(kind.to_string().parse::<EdgeKind>(), Ok(kind));
        }
        for ranker in [Ranker::Ppr, Ranker::Degree, Ranker::Random] {
            assert_eq!(ranker.to_string().parse::<Ranker>(), Ok(ranker));
        }
        assert!("nonsense".parse::<Ranker>().is_err());
    }
}
