//! `path` — how one declaration reaches another, as the k shortest routes.
//!
//! `trace` walks forward from a role and `callers` walks backward from a declaration. This answers
//! the question between them: given two declarations, what chain of calls and passed references
//! joins them, and what are the alternatives. Each route carries its nodes with their ids, the edge
//! kind of every hop, and the call site the hop is written at.
//!
//! A route can end on an external or dynamic callee (`math.sqrt`, a closure-valued field), which is
//! the honest answer to "how does this reach the filesystem". No route runs through one, because
//! the graph holds no body to run through.
//!
//! The search lives in [`noeta_ide::paths`]. This module owns the wire shapes, the endpoint
//! addressing, and the notes.

use noeta_ast::Program;
use noeta_ide::paths::{self as engine, Endpoint, PathRanker};
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{self, LinkStatus, Loc, NodeId, NodeKind, Prepared};
use crate::context::{function_id, join, parse_kinds};
use crate::graph::DeclIndex;

/// How many near matches a not-found report offers.
const NEAR_MATCHES: usize = 6;

/// The `path` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct PathOutput {
    /// True when at least one route joins the two declarations.
    pub found: bool,
    pub from: Option<NodeId>,
    pub to: Option<NodeId>,
    /// Every declaration an ambiguous endpoint leaf matched.
    pub candidates: Vec<NodeId>,
    /// The routes. Ranked answers run weakest first, so the strongest route is last.
    pub paths: Vec<Route>,
    /// True when the graph offered more routes than `k` and the ranker chose among them.
    pub ranked: bool,
    /// Routes the flow ranker dropped as too weak to carry.
    pub pruned: usize,
    /// Which ranking chose the routes: `flow` or `shortest`.
    pub ranker: String,
    /// Whether the search ran over the merged workspace program.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// One route.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Route {
    /// Hops from the source to the target.
    pub length: usize,
    /// The route's flow score — the mean resource its nodes carry.
    pub score: f64,
    pub nodes: Vec<RouteNode>,
}

/// One node of a route.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RouteNode {
    /// The declaration's identity — the same `id` every other graph tool reports for it. An
    /// external or dynamic callee has a name and a kind and nothing to open.
    pub id: NodeId,
    pub name: String,
    /// `function`, `method`, `external` or `dynamic`.
    pub kind: String,
    /// How the previous node reaches this one: `call` or `reference`. Absent on the source.
    pub edge: Option<String>,
    /// Where that call or reference is written, in the previous node's file. Absent on the source.
    pub site: Option<Loc>,
    /// The file the site sits in, relative to the project root.
    pub site_file: Option<String>,
    /// This node comes from an unlinked program, so its classification is unproven.
    pub unverified: bool,
}

/// Scores are ratios; six places order them and keep two runs byte-identical.
const SCORE_PLACES: f64 = 1e6;

/// Answer `path`: the k shortest routes from `from` to `to`.
pub fn path(
    p: &Prepared,
    from: &str,
    to: &str,
    k: Option<usize>,
    edge_kinds: Option<&[String]>,
    max_depth: Option<usize>,
    ranker: Option<&str>,
) -> PathOutput {
    let status = LinkStatus::of(p);
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = match &linked.program {
        Ok(program) => program,
        Err(_) => &entry.0.program,
    };
    let checked = noeta_db::linked_checked_ide(&p.db, p.ws);
    let texts: Vec<&str> = p.sources.iter().map(|s| s.text()).collect();
    let graph = noeta_ide::callgraph::build(program, &checked.expr_types, &checked.sites, &texts);
    let decls = DeclIndex::build(program);
    let path_graph = engine::build(&graph);

    let mut notes: Vec<String> = status.note().into_iter().collect();
    let ranker = match ranker {
        None => PathRanker::default(),
        Some(name) => match name.parse::<PathRanker>() {
            Ok(ranker) => ranker,
            Err(message) => {
                notes.push(message);
                PathRanker::default()
            }
        },
    };
    let (kinds, kind_note) = parse_kinds(edge_kinds);
    notes.extend(kind_note);

    let source = match resolve(&graph, &path_graph, from) {
        Ok(node) => node,
        Err(problem) => return not_found(p, &decls, &status, notes, problem),
    };
    let target = match resolve(&graph, &path_graph, to) {
        Ok(node) => node,
        Err(problem) => return not_found(p, &decls, &status, notes, problem),
    };
    if source == target {
        notes.push(format!("`{from}` and `{to}` are the same declaration"));
        return not_found(p, &decls, &status, notes, Problem::Same);
    }

    let found = engine::find(
        &path_graph,
        source,
        target,
        &engine::Request {
            k: k.unwrap_or(engine::DEFAULT_K),
            max_depth: max_depth.unwrap_or(engine::DEFAULT_MAX_DEPTH),
            edge_kinds: kinds,
            ranker,
        },
    );
    if !found.found {
        notes.push(format!(
            "no route from `{from}` to `{to}` within the depth searched — `callers` walks back \
             from `{to}` and `trace` walks forward from `{from}`"
        ));
    }

    PathOutput {
        found: found.found,
        from: Some(id_of(p, &decls, &graph, &path_graph, source)),
        to: Some(id_of(p, &decls, &graph, &path_graph, target)),
        candidates: Vec::new(),
        paths: found
            .routes
            .iter()
            .map(|route| Route {
                length: route.nodes.len().saturating_sub(1),
                score: (route.score * SCORE_PLACES).round() / SCORE_PLACES,
                nodes: route
                    .nodes
                    .iter()
                    .enumerate()
                    .map(|(i, &node)| {
                        // The step *into* this node is the previous hop's, so the source has none.
                        let step = i.checked_sub(1).and_then(|prev| route.steps.get(prev));
                        let at = step.and_then(|s| analyze::locate_span(p, s.site));
                        RouteNode {
                            id: id_of(p, &decls, &graph, &path_graph, node),
                            name: path_graph.nodes[node].name.clone(),
                            kind: path_graph.nodes[node].sort.as_str().to_string(),
                            edge: step.map(|s| s.kind.to_string()),
                            site: at.map(|(_, loc)| loc.start),
                            site_file: step.and_then(|s| p.file_name(s.site.source.0 as usize)),
                            unverified: !status.linked,
                        }
                    })
                    .collect(),
            })
            .collect(),
        ranked: found.ranked,
        pruned: found.pruned,
        ranker: found.ranker.to_string(),
        linked: status.linked,
        link_diagnostics: status.link_diagnostics.clone(),
        note: join(notes),
    }
}

/// Why an endpoint did not resolve.
enum Problem {
    /// A bare leaf several declarations carry, with the spec and the candidates.
    Ambiguous(String, Vec<String>),
    /// A spec that named nothing, with the closest declared names.
    Missing(String, Vec<String>),
    /// Both endpoints are the same declaration.
    Same,
}

/// Resolve an endpoint the way `trace` and `callers` resolve theirs.
fn resolve(
    graph: &noeta_ide::callgraph::CallGraph,
    path_graph: &engine::PathGraph,
    name: &str,
) -> Result<usize, Problem> {
    match path_graph.endpoint(graph, name) {
        Endpoint::Found(node) => Ok(node),
        Endpoint::Ambiguous(candidates) => Err(Problem::Ambiguous(name.to_string(), candidates)),
        Endpoint::Missing => Err(Problem::Missing(
            name.to_string(),
            path_graph.near_matches(name, NEAR_MATCHES),
        )),
    }
}

fn not_found(
    p: &Prepared,
    decls: &DeclIndex,
    status: &LinkStatus,
    mut notes: Vec<String>,
    problem: Problem,
) -> PathOutput {
    let candidates = match &problem {
        Problem::Ambiguous(spec, names) => {
            notes.push(format!(
                "`{spec}` names {} declarations — pass one of: {}",
                names.len(),
                names.join(", ")
            ));
            names
                .iter()
                .filter_map(|name| match decls.lookup(name) {
                    crate::graph::Lookup::Found(decl) => Some(decl.id(p)),
                    _ => None,
                })
                .collect()
        }
        Problem::Missing(spec, near) => {
            let hint = if near.is_empty() {
                "try `symbols` for the declarations".to_string()
            } else {
                format!("did you mean {}?", near.join(", "))
            };
            notes.push(format!("`{spec}` matches no declaration here — {hint}"));
            Vec::new()
        }
        Problem::Same => Vec::new(),
    };
    PathOutput {
        found: false,
        from: None,
        to: None,
        candidates,
        paths: Vec::new(),
        ranked: false,
        pruned: 0,
        ranker: PathRanker::default().to_string(),
        linked: status.linked,
        link_diagnostics: status.link_diagnostics.clone(),
        note: join(notes),
    }
}

/// A path node's identity: the declaration inventory's for a declaration, a name and a kind for a
/// labeled leaf.
fn id_of(
    p: &Prepared,
    decls: &DeclIndex,
    graph: &noeta_ide::callgraph::CallGraph,
    path_graph: &engine::PathGraph,
    node: usize,
) -> NodeId {
    let info = &path_graph.nodes[node];
    match info.function {
        Some(function) => function_id(p, decls, graph, function),
        // An `external` target that names one of this project's own modules is a real intra-project
        // call the unlinked program could not resolve, and it degrades to `unresolved` the way a
        // trace node does rather than reading as "this leaves your code".
        None if info.sort == engine::NodeSort::External && p.names_a_project_module(&info.name) => {
            p.unlocated_id(&info.name, NodeKind::Unresolved)
        }
        None => p.unlocated_id(
            &info.name,
            match info.sort {
                engine::NodeSort::External => NodeKind::External,
                _ => NodeKind::Dynamic,
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    /// `handle` reaches `save` two ways, and reaches the filesystem through a labeled leaf.
    const SRC: &str = "\
use std.math

fn save(n: int): int { return n }
fn service(n: int): int { return save(n) }
fn audit(n: int): int { return service(n) }
fn shortcut(n: int): int { return save(n) }
fn handle(n: int): int { return audit(n) + shortcut(n) }
fn measured(): float { return math.sqrt(4.0) }
fn root(): float { return measured() }
echo handle(1)
";

    fn prep() -> Prepared {
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    fn route_names(out: &PathOutput) -> Vec<Vec<String>> {
        out.paths
            .iter()
            .map(|r| r.nodes.iter().map(|n| n.name.clone()).collect())
            .collect()
    }

    #[test]
    fn two_routes_come_back_shortest_first_with_their_sites() {
        let out = path(&prep(), "handle", "save", None, None, None, None);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(
            route_names(&out),
            vec![
                vec!["handle", "shortcut", "save"],
                vec!["handle", "audit", "service", "save"],
            ]
        );
        assert_eq!(out.paths[0].length, 2);
        assert!(!out.ranked, "two routes under a k of 3 need no ranking");
        // Every node carries the identity the other graph tools report, and every hop its site.
        let route = &out.paths[0];
        assert_eq!(route.nodes[0].id.name, "handle");
        assert!(route.nodes[0].edge.is_none() && route.nodes[0].site.is_none());
        assert_eq!(route.nodes[1].edge.as_deref(), Some("call"));
        assert!(route.nodes[1].site.is_some() && route.nodes[1].site_file.is_some());
        assert!(out.linked && !route.nodes[0].unverified);
    }

    #[test]
    fn an_unreachable_pair_reports_no_route_and_names_the_tools_that_would() {
        let out = path(&prep(), "save", "handle", None, None, None, None);
        assert!(!out.found);
        assert!(out.paths.is_empty());
        let note = out.note.expect("a reason");
        assert!(note.contains("no route from `save` to `handle`"), "{note}");
        assert!(note.contains("callers"), "{note}");
    }

    #[test]
    fn a_route_ends_on_a_labeled_external_leaf() {
        let out = path(&prep(), "root", "math.sqrt", None, None, None, None);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(
            route_names(&out),
            vec![vec!["root", "measured", "math.sqrt"]]
        );
        let leaf = out.paths[0].nodes.last().unwrap();
        assert_eq!(leaf.kind, "external");
        assert_eq!(leaf.id.kind.as_str(), "external");
        assert!(leaf.id.file.is_none(), "nothing to open: {:?}", leaf.id);
    }

    #[test]
    fn edge_kinds_narrow_the_search() {
        let src = "fn sink(n: int): int { return n }\n\
                   fn run(f: (int) -> int): int { return f(1) }\n\
                   fn entry(): int { return run(sink) }\n";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let both = path(&p, "entry", "sink", None, None, None, None);
        assert!(both.found);
        assert_eq!(both.paths[0].nodes[1].edge.as_deref(), Some("reference"));

        let calls = path(
            &p,
            "entry",
            "sink",
            None,
            Some(&["call".to_string()]),
            None,
            None,
        );
        assert!(!calls.found, "the only route is a passed reference");
    }

    #[test]
    fn a_missing_endpoint_and_a_bad_ranker_are_reported_rather_than_thrown() {
        let out = path(&prep(), "handle", "ghost", None, None, None, None);
        assert!(!out.found && out.paths.is_empty());
        assert!(out.from.is_none());
        assert!(
            out.note.as_deref().is_some_and(|n| n.contains("`ghost`")),
            "note: {:?}",
            out.note
        );

        let bad = path(
            &prep(),
            "handle",
            "save",
            None,
            None,
            None,
            Some("nonsense"),
        );
        assert_eq!(bad.ranker, "flow");
        assert!(
            bad.note.as_deref().is_some_and(|n| n.contains("nonsense")),
            "note: {:?}",
            bad.note
        );
    }
}
