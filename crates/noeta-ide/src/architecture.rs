//! The **role-quotient graph**: the call graph with every declaration replaced by the `@role` it
//! bears, so a whole program's shape reads as a handful of nodes and edges.
//!
//! Two derivations sit here, one on top of the other.
//!
//! [`role_graph`] is the **bearer graph**: role-bearing declarations as nodes, and an edge from a
//! bearer to each nearest bearer it reaches, with every declaration that bears no role collapsed
//! away. A connection that exists only through a passed-as-value chain is marked, the way the
//! editor's swimlane view renders such an edge dashed. This is the same collapse the trace view's
//! Lanes mode performs in `editors/vscode-noeta/media/trace.js`, and the two must agree:
//! `bearer_edges_collapse_non_role_intermediates` pins the shape.
//!
//! [`architecture`] quotients that graph by role: one node per role, one edge per (role, role)
//! pair, each carrying how many bearer connections it aggregates and up to [`EXEMPLARS`] of them.
//! Declarations bearing no role are not dropped — they are counted as `unassigned` with exemplars,
//! so a program with no `@role` bindings anywhere still gets an answer that says what it holds.

use std::collections::{HashMap, HashSet};

use noeta_span::Span;

use crate::callgraph::{CallGraph, Callee};

/// How many example declarations an aggregated edge or the unassigned bucket carries.
pub const EXEMPLARS: usize = 3;

/// One role-bearing declaration.
#[derive(Debug, Clone)]
pub struct Bearer {
    /// The call-graph index.
    pub function: usize,
    pub name: String,
    /// Every role the declaration bears, in binding order.
    pub roles: Vec<String>,
    /// The lane the declaration sits in: its first role, the way the swimlane view places a card.
    pub lane: String,
    pub decl_span: Span,
}

/// One collapsed connection between two bearers.
#[derive(Debug, Clone)]
pub struct BearerEdge {
    /// The call-graph index of the source bearer.
    pub from: usize,
    /// The call-graph index of the target bearer.
    pub to: usize,
    /// True when every chain joining the two runs through a passed reference rather than a call.
    pub via_reference: bool,
}

/// The bearer graph: cards and the connections between them.
#[derive(Debug, Clone, Default)]
pub struct RoleGraph {
    pub bearers: Vec<Bearer>,
    pub edges: Vec<BearerEdge>,
    /// The roles in the order they were first reached — the swimlane column order.
    pub lanes: Vec<String>,
}

/// Collapse `graph` to its bearers. `roles_by_target` is the role index keyed by declaration name,
/// as [`crate::trace::roles_by_target`] builds it.
///
/// Every bearer is a walk root, so a bearer nothing reaches is still a node. From each root the
/// walk descends through non-bearers until it meets a bearer, records the edge, and continues from
/// there. A node is expanded once per root, which bounds the walk by the reachable set rather than
/// by the paths through it.
pub fn role_graph(graph: &CallGraph, roles_by_target: &HashMap<String, Vec<String>>) -> RoleGraph {
    let roles_of = |i: usize| -> Vec<String> {
        roles_by_target
            .get(&graph.functions[i].name)
            .cloned()
            .unwrap_or_default()
    };
    let mut bearers: Vec<Bearer> = Vec::new();
    let mut lanes: Vec<String> = Vec::new();
    let mut index_of: HashMap<usize, usize> = HashMap::new();
    for (i, f) in graph.functions.iter().enumerate() {
        let roles = roles_of(i);
        let Some(lane) = roles.first().cloned() else {
            continue;
        };
        if !lanes.contains(&lane) {
            lanes.push(lane.clone());
        }
        index_of.insert(i, bearers.len());
        bearers.push(Bearer {
            function: i,
            name: f.name.clone(),
            roles,
            lane,
            decl_span: f.name_span,
        });
    }

    // (from, to) → whether every chain found so far went through a reference.
    let mut edges: HashMap<(usize, usize), bool> = HashMap::new();
    for bearer in &bearers {
        // The frontier carries "reached from this bearer, through a chain that was (or was not)
        // reference-only so far".
        let mut queue: Vec<(usize, bool)> = vec![(bearer.function, false)];
        let mut seen: HashSet<usize> = [bearer.function].into_iter().collect();
        while let Some((node, via_reference)) = queue.pop() {
            for edge in graph.edges_from(Some(node)) {
                let Callee::Function(next) = &edge.callee else {
                    continue; // an external or dynamic leaf bears no role
                };
                let next = *next;
                let chain_ref = via_reference || !edge.call;
                if index_of.contains_key(&next) {
                    if next != bearer.function {
                        edges
                            .entry((bearer.function, next))
                            .and_modify(|held| *held = *held && chain_ref)
                            .or_insert(chain_ref);
                    }
                    continue; // a bearer ends the chain; its own walk carries it onward
                }
                if seen.insert(next) {
                    queue.push((next, chain_ref));
                }
            }
        }
    }

    let mut edges: Vec<BearerEdge> = edges
        .into_iter()
        .map(|((from, to), via_reference)| BearerEdge {
            from,
            to,
            via_reference,
        })
        .collect();
    edges.sort_by(|a, b| {
        graph.functions[a.from]
            .name
            .cmp(&graph.functions[b.from].name)
            .then_with(|| graph.functions[a.to].name.cmp(&graph.functions[b.to].name))
    });
    RoleGraph {
        bearers,
        edges,
        lanes,
    }
}

/// One role of the quotient graph.
#[derive(Debug, Clone)]
pub struct RoleNode {
    /// The role's qualified binding (`Semantic.EntryPoint`).
    pub role: String,
    /// The declarations bearing it, in declaration order.
    pub bearers: Vec<usize>,
    /// Aggregated edges arriving at this role, from another role.
    pub in_degree: usize,
    /// Aggregated edges leaving this role, for another role.
    pub out_degree: usize,
}

/// One aggregated edge between two roles.
#[derive(Debug, Clone)]
pub struct RoleEdge {
    pub from: String,
    pub to: String,
    /// How many bearer-to-bearer connections this edge aggregates.
    pub count: usize,
    /// Up to [`EXEMPLARS`] of them, as call-graph indices.
    pub exemplars: Vec<(usize, usize)>,
    /// True when every connection aggregated here runs only through passed references.
    pub via_reference: bool,
}

/// The declarations bearing no role at all.
#[derive(Debug, Clone, Default)]
pub struct Unassigned {
    pub count: usize,
    /// Up to [`EXEMPLARS`] of them, as call-graph indices, most-connected first.
    pub exemplars: Vec<usize>,
}

/// One `(declaration, role)` binding — the summary `trace` reports as its boundaries.
#[derive(Debug, Clone)]
pub struct Boundary {
    pub target: String,
    pub role: String,
    pub decl_span: Span,
}

/// The program's architecture.
#[derive(Debug, Clone, Default)]
pub struct Architecture {
    pub roles: Vec<RoleNode>,
    pub edges: Vec<RoleEdge>,
    /// Every `(declaration, role)` binding, sorted by role then declaration.
    pub boundaries: Vec<Boundary>,
    pub unassigned: Unassigned,
    /// The bearer graph the quotient was taken over.
    pub bearer_graph: RoleGraph,
}

/// Quotient the bearer graph by role.
///
/// A declaration bearing two roles contributes to both, so an edge into a bearer of
/// `PersistenceBoundary` and `Sink` is an edge into each.
pub fn architecture(
    graph: &CallGraph,
    roles_by_target: &HashMap<String, Vec<String>>,
) -> Architecture {
    let bearer_graph = role_graph(graph, roles_by_target);

    let mut roles: Vec<RoleNode> = Vec::new();
    let mut role_at: HashMap<String, usize> = HashMap::new();
    for bearer in &bearer_graph.bearers {
        for role in &bearer.roles {
            let at = match role_at.get(role) {
                Some(&at) => at,
                None => {
                    role_at.insert(role.clone(), roles.len());
                    roles.push(RoleNode {
                        role: role.clone(),
                        bearers: Vec::new(),
                        in_degree: 0,
                        out_degree: 0,
                    });
                    roles.len() - 1
                }
            };
            roles[at].bearers.push(bearer.function);
        }
    }

    let roles_of = |function: usize| -> Vec<String> {
        bearer_graph
            .bearers
            .iter()
            .find(|b| b.function == function)
            .map(|b| b.roles.clone())
            .unwrap_or_default()
    };
    let mut aggregated: HashMap<(String, String), RoleEdge> = HashMap::new();
    for edge in &bearer_graph.edges {
        for from in roles_of(edge.from) {
            for to in roles_of(edge.to) {
                let entry = aggregated
                    .entry((from.clone(), to.clone()))
                    .or_insert_with(|| RoleEdge {
                        from: from.clone(),
                        to: to.clone(),
                        count: 0,
                        exemplars: Vec::new(),
                        via_reference: true,
                    });
                entry.count += 1;
                entry.via_reference = entry.via_reference && edge.via_reference;
                if entry.exemplars.len() < EXEMPLARS {
                    entry.exemplars.push((edge.from, edge.to));
                }
            }
        }
    }
    let mut edges: Vec<RoleEdge> = aggregated.into_values().collect();
    edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
    for edge in &edges {
        if let Some(&at) = role_at.get(&edge.from) {
            roles[at].out_degree += edge.count;
        }
        if let Some(&at) = role_at.get(&edge.to) {
            roles[at].in_degree += edge.count;
        }
    }
    roles.sort_by(|a, b| a.role.cmp(&b.role));

    let mut boundaries: Vec<Boundary> = bearer_graph
        .bearers
        .iter()
        .flat_map(|b| {
            b.roles.iter().map(|role| Boundary {
                target: b.name.clone(),
                role: role.clone(),
                decl_span: b.decl_span,
            })
        })
        .collect();
    boundaries.sort_by(|a, b| a.role.cmp(&b.role).then_with(|| a.target.cmp(&b.target)));

    Architecture {
        roles,
        edges,
        boundaries,
        unassigned: unassigned(graph, roles_by_target),
        bearer_graph,
    }
}

/// The declarations bearing no role, with the most-connected of them as exemplars — the answer a
/// program with no `@role` bindings gets instead of an empty graph.
fn unassigned(graph: &CallGraph, roles_by_target: &HashMap<String, Vec<String>>) -> Unassigned {
    let mut degree: Vec<(usize, usize)> = Vec::new();
    for (i, f) in graph.functions.iter().enumerate() {
        if roles_by_target.contains_key(&f.name) {
            continue;
        }
        let out = graph.edges_from(Some(i)).count();
        let incoming = graph
            .edges
            .iter()
            .filter(|e| e.callee == Callee::Function(i))
            .count();
        degree.push((i, out + incoming));
    }
    let count = degree.len();
    degree.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| graph.functions[a.0].name.cmp(&graph.functions[b.0].name))
    });
    Unassigned {
        count,
        exemplars: degree.into_iter().take(EXEMPLARS).map(|(i, _)| i).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    fn setup(src: &str) -> (CallGraph, HashMap<String, Vec<String>>) {
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
        let info = noeta_ast::reflect::build(&parsed.program, &[], &Default::default());
        let roles = crate::trace::roles_by_target(&info);
        (graph, roles)
    }

    /// Handler → service → store, with a role on the first and the last and nothing on the middle.
    const LAYERS: &str = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

@attribute
@role(Semantic.Persistence)
struct Store { table: string }

@attribute
@role(Semantic.Sink)
struct Emits { channel: string }

#[Store(\"orders\")]
fn save(n: int): int { return n }

#[Emits(\"log\")]
fn notify(n: int): int { return n }

fn service(n: int): int { return save(n) }

#[Route(\"/orders\")]
fn handle(n: int): int { return service(n) + notify(n) }

echo handle(1)
";

    fn edge_names(arch: &Architecture) -> Vec<(&str, &str, usize)> {
        arch.edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.count))
            .collect()
    }

    /// A three-layer program yields exactly the two aggregated edges, each counting one bearer
    /// connection, and the non-role intermediate is nowhere in the quotient.
    #[test]
    fn three_layers_yield_two_aggregated_edges() {
        let (graph, roles) = setup(LAYERS);
        let arch = architecture(&graph, &roles);
        assert_eq!(
            edge_names(&arch),
            vec![
                ("Semantic.EntryPoint", "Semantic.Persistence", 1),
                ("Semantic.EntryPoint", "Semantic.Sink", 1),
            ]
        );
        // The exemplar names the two real declarations the edge stands for.
        let persistence = &arch.edges[0];
        assert_eq!(persistence.exemplars.len(), 1);
        let (from, to) = persistence.exemplars[0];
        assert_eq!(graph.functions[from].name, "handle");
        assert_eq!(graph.functions[to].name, "save");
        assert!(
            !persistence.via_reference,
            "a chain of calls, not a callback"
        );
        // `service` carries no role, so it is not a node — it is counted as unassigned.
        assert!(
            arch.roles.iter().all(|r| r.role.starts_with("Semantic.")),
            "{:?}",
            arch.roles.iter().map(|r| &r.role).collect::<Vec<_>>()
        );
        assert_eq!(arch.unassigned.count, 1);
        assert_eq!(
            graph.functions[arch.unassigned.exemplars[0]].name,
            "service"
        );
        // The boundary summary is every binding, sorted.
        let bindings: Vec<(&str, &str)> = arch
            .boundaries
            .iter()
            .map(|b| (b.role.as_str(), b.target.as_str()))
            .collect();
        assert_eq!(
            bindings,
            vec![
                ("Semantic.EntryPoint", "handle"),
                ("Semantic.Persistence", "save"),
                ("Semantic.Sink", "notify"),
            ]
        );
    }

    /// A role nothing reaches is a node with an in-degree of zero, not an omission.
    #[test]
    fn a_role_nothing_reaches_has_an_in_degree_of_zero() {
        let (graph, roles) = setup(LAYERS);
        let arch = architecture(&graph, &roles);
        let entry = arch
            .roles
            .iter()
            .find(|r| r.role == "Semantic.EntryPoint")
            .expect("the entry point is a node");
        assert_eq!(entry.in_degree, 0, "nothing calls the handler");
        assert_eq!(entry.out_degree, 2);
        let store = arch
            .roles
            .iter()
            .find(|r| r.role == "Semantic.Persistence")
            .expect("the store is a node");
        assert_eq!((store.in_degree, store.out_degree), (1, 0));
    }

    /// The collapse is the swimlane view's: a chain of non-role calls between two bearers becomes
    /// one edge, and a connection that only ever runs through a passed value is marked.
    #[test]
    fn bearer_edges_collapse_non_role_intermediates() {
        let (graph, roles) = setup(
            "@attribute\n\
             @role(Semantic.EntryPoint)\n\
             struct Route { path: string }\n\
             \n\
             @attribute\n\
             @role(Semantic.Sink)\n\
             struct Emits { channel: string }\n\
             \n\
             #[Emits(\"log\")]\n\
             fn sink(n: int): int { return n }\n\
             \n\
             #[Emits(\"audit\")]\n\
             fn audited(n: int): int { return n }\n\
             \n\
             fn middle_two(n: int): int { return sink(n) }\n\
             fn middle_one(n: int): int { return middle_two(n) }\n\
             fn run(f: (int) -> int): int { return f(1) }\n\
             \n\
             #[Route(\"/x\")]\n\
             fn handle(n: int): int { return middle_one(n) + run(audited) }\n",
        );
        let collapsed = role_graph(&graph, &roles);
        let names: Vec<&str> = collapsed.bearers.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["sink", "audited", "handle"],
            "only bearers are cards"
        );
        let edges: Vec<(&str, &str, bool)> = collapsed
            .edges
            .iter()
            .map(|e| {
                (
                    graph.functions[e.from].name.as_str(),
                    graph.functions[e.to].name.as_str(),
                    e.via_reference,
                )
            })
            .collect();
        assert_eq!(
            edges,
            vec![
                // Reached only by being passed as a value.
                ("handle", "audited", true),
                // Two non-role intermediates, collapsed into one edge.
                ("handle", "sink", false),
            ]
        );
        assert_eq!(
            collapsed.lanes,
            vec!["Semantic.Sink", "Semantic.EntryPoint"]
        );
    }

    /// A program with no `@role` bindings reports what it holds instead of nothing.
    #[test]
    fn a_program_with_no_roles_reports_only_unassigned() {
        let (graph, roles) = setup(
            "fn leaf(): int { return 1 }\n\
             fn middle(): int { return leaf() }\n\
             fn extra(): int { return leaf() }\n\
             fn top(): int { return middle() + leaf() }\n",
        );
        let arch = architecture(&graph, &roles);
        assert!(arch.roles.is_empty() && arch.edges.is_empty());
        assert!(arch.boundaries.is_empty());
        assert_eq!(arch.unassigned.count, 4);
        // Most-connected first: three declarations use `leaf`, and nothing uses it more.
        assert_eq!(graph.functions[arch.unassigned.exemplars[0]].name, "leaf");
    }
}
