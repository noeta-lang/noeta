//! **Point-to-point paths** through the call graph: how one declaration reaches another, as the
//! k shortest routes rather than one.
//!
//! [`trace`](crate::trace) walks forward from a role and [`impact`](crate::impact) walks backward
//! from an edit. This is the third question — "how does `handle` get to `save`?" — answered with
//! Yen's k-shortest-simple-paths over the same graph, so every route is a real chain of call and
//! reference edges with its sites.
//!
//! External and dynamic callees are nodes here, and they are sinks. A path may end on one, which
//! is the honest answer to "how does this reach `fs.write`", and no path runs through one, because
//! the graph holds no body to run through.
//!
//! When the graph offers more routes than the request asked for, the survivors are chosen by
//! PathRAG's flow: a unit of resource leaves the source, decays by [`DECAY_ALPHA`] at every hop and
//! splits across the node's out-edges, and a path scores the mean resource over its nodes. A route
//! through a wide dispatcher therefore scores below a direct one of the same length, and a route
//! scoring below [`PRUNE_THRESHOLD`] is dropped. Ranked results are emitted **ascending**, so the
//! strongest route is the last thing a reader sees.

use std::collections::{HashMap, HashSet, VecDeque};

use noeta_span::Span;

use crate::callgraph::{CallGraph, Callee, NameLookup};
use crate::rank::EdgeKind;

/// Routes returned when the request names no `k`.
pub const DEFAULT_K: usize = 3;
/// The most routes a request can ask for.
pub const MAX_K: usize = 10;
/// The longest route searched when the request names no depth.
pub const DEFAULT_MAX_DEPTH: usize = 12;
/// The longest route searched, whatever the request asks for.
pub const MAX_DEPTH_CAP: usize = 24;
/// Candidate routes enumerated before ranking stops.
pub const CANDIDATE_CAP: usize = 64;

/// The share of a node's resource that survives one hop. At 0.8 a route keeps most of its strength
/// per hop, so length costs a route less than fan-out does: crossing a node with eight callees
/// leaves a tenth of the flow, crossing a node with one leaves four fifths.
pub const DECAY_ALPHA: f64 = 0.8;
/// The mean resource below which a route is dropped. A route under 0.05 has passed through enough
/// fan-out that its nodes carry almost none of the source's flow, which is the shape of a route
/// that only technically exists. The strongest route is never pruned, so a graph of nothing but
/// weak routes still answers.
pub const PRUNE_THRESHOLD: f64 = 0.05;

/// Which routes survive when the graph offers more than `k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathRanker {
    /// PathRAG's resource flow, decayed and pruned.
    #[default]
    Flow,
    /// Hop count alone.
    Shortest,
}

impl PathRanker {
    pub fn as_str(self) -> &'static str {
        match self {
            PathRanker::Flow => "flow",
            PathRanker::Shortest => "shortest",
        }
    }
}

impl std::fmt::Display for PathRanker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PathRanker {
    type Err = String;

    fn from_str(s: &str) -> Result<PathRanker, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "flow" => Ok(PathRanker::Flow),
            "shortest" => Ok(PathRanker::Shortest),
            other => Err(format!(
                "`{other}` is no path ranker — use flow or shortest"
            )),
        }
    }
}

/// What a path node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSort {
    Function,
    Method,
    /// A callee outside the program, named by its own identity — a sink.
    External,
    /// A statically unresolvable callee — a sink.
    Dynamic,
}

impl NodeSort {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeSort::Function => "function",
            NodeSort::Method => "method",
            NodeSort::External => "external",
            NodeSort::Dynamic => "dynamic",
        }
    }

    /// Whether a route can pass through this node rather than end on it.
    pub fn traversable(self) -> bool {
        matches!(self, NodeSort::Function | NodeSort::Method)
    }
}

/// One node of the path graph.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub sort: NodeSort,
    /// The call-graph index, for a declaration.
    pub function: Option<usize>,
    /// The declared name's span, for a declaration.
    pub decl_span: Option<Span>,
}

/// One hop.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub to: usize,
    pub kind: EdgeKind,
    /// Where the call or reference is written, in the caller.
    pub site: Span,
}

/// The call graph as a node-indexed adjacency, with the labeled leaves as sinks.
#[derive(Debug, Clone, Default)]
pub struct PathGraph {
    pub nodes: Vec<Node>,
    pub out: Vec<Vec<Step>>,
}

/// Build the path graph over `graph`. Top-level statements are not a node: they call, but nothing
/// calls them, so no route runs through them.
pub fn build(graph: &CallGraph) -> PathGraph {
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
            decl_span: Some(f.name_span),
        })
        .collect();
    let mut leaves: HashMap<(bool, String), usize> = HashMap::new();
    let mut out: Vec<Vec<Step>> = vec![Vec::new(); nodes.len()];
    for edge in &graph.edges {
        let Some(caller) = edge.caller else { continue };
        let kind = if edge.call {
            EdgeKind::Call
        } else {
            EdgeKind::Reference
        };
        let to = match &edge.callee {
            Callee::Function(i) => *i,
            Callee::External(name) | Callee::Dynamic(name) => {
                let external = matches!(edge.callee, Callee::External(_));
                let key = (external, name.clone());
                match leaves.get(&key) {
                    Some(&i) => i,
                    None => {
                        let i = nodes.len();
                        nodes.push(Node {
                            name: name.clone(),
                            sort: if external {
                                NodeSort::External
                            } else {
                                NodeSort::Dynamic
                            },
                            function: None,
                            decl_span: None,
                        });
                        out.push(Vec::new());
                        leaves.insert(key, i);
                        i
                    }
                }
            }
        };
        if to == caller {
            continue; // a self-call is no route to anywhere
        }
        out[caller].push(Step {
            to,
            kind,
            site: edge.site,
        });
    }
    // A deterministic adjacency order is what makes the search's tie-breaks repeatable.
    for steps in &mut out {
        steps.sort_by_key(|s| (s.to, s.kind, s.site.source.0, s.site.start));
    }
    PathGraph { nodes, out }
}

/// What resolving an endpoint name found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Found(usize),
    /// A bare leaf several declarations carry.
    Ambiguous(Vec<String>),
    Missing,
}

impl PathGraph {
    /// The node `name` addresses: a declaration by post-link name or unique leaf, the way `trace`
    /// and `callers` are addressed, else an external or dynamic callee by its label.
    pub fn endpoint(&self, graph: &CallGraph, name: &str) -> Endpoint {
        match graph.lookup_named(name) {
            NameLookup::Found(f) => {
                if let Some(i) = self.nodes.iter().position(|n| n.function == Some(f)) {
                    return Endpoint::Found(i);
                }
            }
            NameLookup::Ambiguous(candidates) => return Endpoint::Ambiguous(candidates),
            NameLookup::Missing => {}
        }
        let want = name.trim();
        let leaves: Vec<usize> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| !n.sort.traversable() && n.name == want)
            .map(|(i, _)| i)
            .collect();
        match leaves.as_slice() {
            [only] => Endpoint::Found(*only),
            [] => Endpoint::Missing,
            many => Endpoint::Ambiguous(many.iter().map(|i| self.nodes[*i].name.clone()).collect()),
        }
    }

    /// The declared names closest to `name` — what a not-found report offers instead.
    pub fn near_matches(&self, name: &str, limit: usize) -> Vec<String> {
        let want = name.trim().to_ascii_lowercase();
        if want.is_empty() {
            return Vec::new();
        }
        let mut near: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| n.name.to_ascii_lowercase().contains(&want))
            .map(|n| n.name.clone())
            .collect();
        near.sort();
        near.dedup();
        near.truncate(limit);
        near
    }
}

/// One route.
#[derive(Debug, Clone)]
pub struct Route {
    /// The nodes, source first.
    pub nodes: Vec<usize>,
    /// One step per hop, so `steps[i]` is how `nodes[i]` reaches `nodes[i + 1]`.
    pub steps: Vec<Step>,
    /// The mean resource over the route's nodes.
    pub score: f64,
}

/// A finished search.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    pub found: bool,
    pub from: Option<usize>,
    pub to: Option<usize>,
    /// The routes, weakest first when the flow ranker chose among them, shortest first otherwise.
    pub routes: Vec<Route>,
    /// True when there were more candidate routes than `k` and the ranker picked.
    pub ranked: bool,
    /// Candidate routes dropped below [`PRUNE_THRESHOLD`].
    pub pruned: usize,
    pub ranker: PathRanker,
}

/// What a caller asks for.
#[derive(Debug, Clone)]
pub struct Request {
    pub k: usize,
    pub max_depth: usize,
    pub edge_kinds: Vec<EdgeKind>,
    pub ranker: PathRanker,
}

impl Default for Request {
    fn default() -> Request {
        Request {
            k: DEFAULT_K,
            max_depth: DEFAULT_MAX_DEPTH,
            edge_kinds: Vec::new(),
            ranker: PathRanker::default(),
        }
    }
}

/// The k shortest simple routes from `from` to `to`, ranked.
pub fn find(graph: &PathGraph, from: usize, to: usize, request: &Request) -> Paths {
    let k = request.k.clamp(1, MAX_K);
    let depth = request.max_depth.clamp(1, MAX_DEPTH_CAP);
    let kinds: Vec<EdgeKind> = if request.edge_kinds.is_empty() {
        vec![EdgeKind::Call, EdgeKind::Reference]
    } else {
        request.edge_kinds.clone()
    };
    let mut result = Paths {
        found: false,
        from: Some(from),
        to: Some(to),
        routes: Vec::new(),
        ranked: false,
        pruned: 0,
        ranker: request.ranker,
    };
    if from == to {
        return result;
    }
    let candidates = yen(graph, from, to, &kinds, depth, k);
    if candidates.is_empty() {
        return result;
    }
    result.found = true;
    let scored: Vec<Route> = candidates
        .iter()
        .map(|nodes| materialize(graph, nodes, &kinds))
        .collect();

    let more_than_asked = scored.len() > k;
    result.ranked = more_than_asked && request.ranker == PathRanker::Flow;
    if result.ranked {
        let mut kept: Vec<Route> = scored
            .iter()
            .filter(|r| r.score >= PRUNE_THRESHOLD)
            .cloned()
            .collect();
        if kept.is_empty() {
            // The strongest route is never pruned: a weak answer beats none.
            kept = vec![strongest(&scored)];
        }
        result.pruned = scored.len() - kept.len();
        // Ascending, so the strongest route is the last one read.
        kept.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.nodes.len().cmp(&a.nodes.len()))
                .then_with(|| b.nodes.cmp(&a.nodes))
        });
        let drop = kept.len().saturating_sub(k);
        result.routes = kept.split_off(drop);
    } else {
        result.routes = scored.into_iter().take(k).collect();
    }
    result
}

fn strongest(routes: &[Route]) -> Route {
    routes
        .iter()
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
        .unwrap_or_else(|| Route {
            nodes: Vec::new(),
            steps: Vec::new(),
            score: 0.0,
        })
}

/// Attach each hop's edge and score the route by PathRAG's resource flow.
fn materialize(graph: &PathGraph, nodes: &[usize], kinds: &[EdgeKind]) -> Route {
    let mut steps = Vec::new();
    let mut resource = 1.0_f64;
    let mut total = 1.0_f64;
    for pair in nodes.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let out: Vec<&Step> = graph.out[from]
            .iter()
            .filter(|s| kinds.contains(&s.kind))
            .collect();
        let fan = out.len().max(1) as f64;
        if let Some(step) = out.iter().find(|s| s.to == to) {
            steps.push(**step);
        }
        resource = DECAY_ALPHA * resource / fan;
        total += resource;
    }
    Route {
        nodes: nodes.to_vec(),
        steps,
        score: total / nodes.len().max(1) as f64,
    }
}

/// Yen's k-shortest simple paths, over hop count, with a deterministic tie-break.
fn yen(
    graph: &PathGraph,
    from: usize,
    to: usize,
    kinds: &[EdgeKind],
    depth: usize,
    k: usize,
) -> Vec<Vec<usize>> {
    let Some(first) = shortest(
        graph,
        from,
        to,
        kinds,
        depth,
        &HashSet::new(),
        &HashSet::new(),
    ) else {
        return Vec::new();
    };
    let mut accepted: Vec<Vec<usize>> = vec![first];
    let mut candidates: Vec<Vec<usize>> = Vec::new();
    // One more than asked for, so the ranker has something to choose among.
    let want = (k + 1).min(CANDIDATE_CAP);
    while accepted.len() < want {
        let previous = accepted[accepted.len() - 1].clone();
        for i in 0..previous.len().saturating_sub(1) {
            let root = &previous[..=i];
            let mut banned_edges: HashSet<(usize, usize)> = HashSet::new();
            for path in &accepted {
                if path.len() > i + 1 && path[..=i] == *root {
                    banned_edges.insert((path[i], path[i + 1]));
                }
            }
            let banned_nodes: HashSet<usize> = root[..i].iter().copied().collect();
            let Some(spur) = shortest(
                graph,
                previous[i],
                to,
                kinds,
                depth.saturating_sub(i),
                &banned_nodes,
                &banned_edges,
            ) else {
                continue;
            };
            let mut whole: Vec<usize> = root[..i].to_vec();
            whole.extend(spur);
            if whole.len() > depth + 1 {
                continue;
            }
            if !accepted.contains(&whole) && !candidates.contains(&whole) {
                candidates.push(whole);
            }
        }
        if candidates.is_empty() {
            break;
        }
        candidates.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
        accepted.push(candidates.remove(0));
    }
    accepted
}

/// The shortest route by hop count, avoiding `banned_nodes` and `banned_edges`. Adjacency is
/// pre-sorted, so a breadth-first search returns the same route on every run.
fn shortest(
    graph: &PathGraph,
    from: usize,
    to: usize,
    kinds: &[EdgeKind],
    depth: usize,
    banned_nodes: &HashSet<usize>,
    banned_edges: &HashSet<(usize, usize)>,
) -> Option<Vec<usize>> {
    if from == to {
        return Some(vec![from]);
    }
    if banned_nodes.contains(&from) || from >= graph.nodes.len() {
        return None;
    }
    let mut previous: HashMap<usize, usize> = HashMap::new();
    let mut seen: HashSet<usize> = [from].into_iter().collect();
    let mut queue: VecDeque<(usize, usize)> = [(from, 0)].into_iter().collect();
    while let Some((node, hops)) = queue.pop_front() {
        if hops >= depth {
            continue;
        }
        // Only a declaration has a body to run through; a labeled leaf is where a route ends.
        if node != from && !graph.nodes[node].sort.traversable() {
            continue;
        }
        for step in &graph.out[node] {
            if !kinds.contains(&step.kind)
                || banned_nodes.contains(&step.to)
                || banned_edges.contains(&(node, step.to))
                || !seen.insert(step.to)
            {
                continue;
            }
            previous.insert(step.to, node);
            if step.to == to {
                let mut route = vec![to];
                let mut at = to;
                while let Some(&back) = previous.get(&at) {
                    route.push(back);
                    at = back;
                }
                route.reverse();
                return Some(route);
            }
            queue.push_back((step.to, hops + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    fn setup(src: &str) -> (CallGraph, PathGraph) {
        let source = Source::new(SourceId::FIRST, "test.noe", src);
        let lexed = noeta_lexer::lex(&source);
        let parsed = noeta_parser::parse(&source, &lexed.tokens);
        assert!(
            lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
            "fixture parses"
        );
        let checked = noeta_check::check_all_with_types(&parsed.program);
        let graph =
            crate::callgraph::build(&parsed.program, &checked.expr_types, &checked.sites, &[src]);
        let paths = build(&graph);
        (graph, paths)
    }

    fn route_names(graph: &PathGraph, paths: &Paths) -> Vec<Vec<String>> {
        paths
            .routes
            .iter()
            .map(|r| {
                r.nodes
                    .iter()
                    .map(|&i| graph.nodes[i].name.clone())
                    .collect()
            })
            .collect()
    }

    fn find_named(
        graph: &CallGraph,
        paths: &PathGraph,
        from: &str,
        to: &str,
        req: &Request,
    ) -> Paths {
        let Endpoint::Found(a) = paths.endpoint(graph, from) else {
            panic!("`{from}` resolves");
        };
        let Endpoint::Found(b) = paths.endpoint(graph, to) else {
            panic!("`{to}` resolves");
        };
        find(paths, a, b, req)
    }

    /// `entry` reaches `sink` two ways: straight through `quick`, and the long way through
    /// `slow_one` then `slow_two`. Both come back, the shorter one first.
    const DIAMOND: &str = "\
fn sink(): int { return 1 }
fn quick(): int { return sink() }
fn slow_two(): int { return sink() }
fn slow_one(): int { return slow_two() }
fn entry(): int { return quick() + slow_one() }
echo entry()
";

    #[test]
    fn a_diamond_yields_both_routes_with_the_shorter_first() {
        let (graph, paths) = setup(DIAMOND);
        let found = find_named(&graph, &paths, "entry", "sink", &Request::default());
        assert!(found.found);
        assert_eq!(
            route_names(&paths, &found),
            vec![
                vec!["entry", "quick", "sink"],
                vec!["entry", "slow_one", "slow_two", "sink"],
            ]
        );
        assert!(!found.ranked, "two routes under a k of 3 need no ranking");
        // Every hop carries its edge and its site.
        let first = &found.routes[0];
        assert_eq!(first.steps.len(), 2);
        assert!(first.steps.iter().all(|s| s.kind == EdgeKind::Call));
        assert!(first.steps[0].site.end > first.steps[0].site.start);
    }

    #[test]
    fn an_unreachable_pair_says_so() {
        let (graph, paths) = setup(
            "fn a(): int { return 1 }\nfn b(): int { return 1 }\nfn top(): int { return a() + b() }\n",
        );
        let found = find_named(&graph, &paths, "a", "b", &Request::default());
        assert!(!found.found, "nothing calls b from a");
        assert!(found.routes.is_empty());
    }

    /// A route can end on an external or dynamic callee, and never runs through one.
    #[test]
    fn a_labeled_leaf_terminates_a_route() {
        let (graph, paths) = setup(
            "use std.math\n\
             fn deep(): float { return math.sqrt(4.0) }\n\
             fn entry(): float { return deep() }\n",
        );
        let found = find_named(&graph, &paths, "entry", "math.sqrt", &Request::default());
        assert!(found.found, "the external leaf is addressable");
        assert_eq!(
            route_names(&paths, &found),
            vec![vec!["entry", "deep", "math.sqrt"]]
        );
        let last = *found.routes[0].nodes.last().unwrap();
        assert_eq!(paths.nodes[last].sort, NodeSort::External);
        assert!(paths.nodes[last].decl_span.is_none(), "no body to open");
        assert!(paths.out[last].is_empty(), "a leaf leads nowhere");
    }

    /// Asking for calls only drops a route that exists solely through a passed reference.
    #[test]
    fn edge_kinds_exclude_reference_routes_when_asked() {
        let (graph, paths) = setup(
            "fn sink(n: int): int { return n }\n\
             fn run(f: (int) -> int): int { return f(1) }\n\
             fn entry(): int { return run(sink) }\n",
        );
        // `entry` reaches `sink` by passing it as a value.
        let both = find_named(&graph, &paths, "entry", "sink", &Request::default());
        assert!(both.found, "the reference edge is a route");
        assert!(
            both.routes[0]
                .steps
                .iter()
                .any(|s| s.kind == EdgeKind::Reference)
        );
        let calls_only = find_named(
            &graph,
            &paths,
            "entry",
            "sink",
            &Request {
                edge_kinds: vec![EdgeKind::Call],
                ..Request::default()
            },
        );
        assert!(!calls_only.found, "no route is written as a call");
    }

    /// More routes than asked for: the flow ranker prunes and orders, strongest last, and the
    /// `shortest` ablation takes the same routes in hop order instead.
    #[test]
    fn the_flow_ranker_orders_ascending_and_shortest_ablates_it() {
        // Four routes from `entry` to `sink`, of three different lengths.
        let src = "\
fn sink(): int { return 1 }
fn a1(): int { return sink() }
fn b1(): int { return sink() }
fn b2(): int { return b1() }
fn c1(): int { return sink() }
fn c2(): int { return c1() }
fn c3(): int { return c2() }
fn d1(): int { return sink() }
fn entry(): int { return a1() + b2() + c3() + d1() }
";
        let (graph, paths) = setup(src);
        let ranked = find_named(&graph, &paths, "entry", "sink", &Request::default());
        assert!(ranked.ranked, "four routes against a k of 3 is a choice");
        assert_eq!(ranked.routes.len(), 3);
        let scores: Vec<f64> = ranked.routes.iter().map(|r| r.score).collect();
        assert!(
            scores[0] <= scores[1] && scores[1] <= scores[2],
            "ascending, so the strongest is last: {scores:?}"
        );
        // The strongest route is the shortest: nothing decayed it. So the ranked order runs
        // longest to shortest.
        let lengths: Vec<usize> = ranked.routes.iter().map(|r| r.nodes.len()).collect();
        assert_eq!(lengths, vec![4, 3, 3], "{:?}", route_names(&paths, &ranked));

        let plain = find_named(
            &graph,
            &paths,
            "entry",
            "sink",
            &Request {
                ranker: PathRanker::Shortest,
                ..Request::default()
            },
        );
        assert!(!plain.ranked, "the ablation does not rank");
        let lengths: Vec<usize> = plain.routes.iter().map(|r| r.nodes.len()).collect();
        assert_eq!(
            lengths,
            vec![3, 3, 4],
            "hop order instead: {:?}",
            route_names(&paths, &plain)
        );
    }

    #[test]
    fn an_ambiguous_endpoint_reports_its_candidates() {
        let (graph, paths) = setup(
            "struct A { fn go(): int { return 1 } }\nstruct B { fn go(): int { return 2 } }\n",
        );
        assert!(matches!(
            paths.endpoint(&graph, "go"),
            Endpoint::Ambiguous(candidates) if candidates.len() == 2
        ));
        assert_eq!(paths.endpoint(&graph, "ghost"), Endpoint::Missing);
        assert!(matches!(paths.endpoint(&graph, "A.go"), Endpoint::Found(_)));
    }

    #[test]
    fn the_path_ranker_round_trips_its_spelling() {
        for ranker in [PathRanker::Flow, PathRanker::Shortest] {
            assert_eq!(ranker.to_string().parse::<PathRanker>(), Ok(ranker));
        }
        assert!("nonsense".parse::<PathRanker>().is_err());
    }
}
