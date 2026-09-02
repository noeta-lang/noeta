//! The role-aware **static trace** walker, shared by the LSP trace document and `noeta mcp`'s
//! `trace` tool:
//! unfold the full path a request would take from an architectural role — start at every function
//! bearing a `@role` binding (or a named function) and walk the [`callgraph`](crate::callgraph)
//! forward. Each node is a function with its own roles and how it was reached (a syntactic call
//! or a passed reference); external module calls and dynamic callees are labeled leaves, never
//! guesses. The `boundaries` list is the architectural answer on its own: every `(function,
//! role)` binding the walk reached.
//!
//! Wire-protocol-free like the rest of the engine: nodes carry **spans**; the MCP tool and the
//! LSP trace document resolve them to file/line their own way, over one shared walk that can
//! never disagree.

use std::collections::{HashMap, HashSet};

use noeta_ast::reflect::ReflectionInfo;
use noeta_span::Span;

use crate::callgraph::{CallEdge, CallGraph, Callee};

/// Walks deeper than this are cut (per-node `truncated`), whatever the caller asks for.
pub const MAX_DEPTH_CAP: usize = 16;
pub const DEFAULT_MAX_DEPTH: usize = 6;
/// The whole answer is capped at this many nodes — a pathological fan-out reports what fits and
/// says so, instead of flooding the reader.
pub const NODE_BUDGET: usize = 500;

/// A finished walk: one tree per root, plus the flattened role boundaries it crossed.
#[derive(Debug, Clone)]
pub struct Trace {
    /// One trace per starting function, in declaration order.
    pub roots: Vec<TraceNode>,
    /// Every `(function, role)` binding the walk reached, in encounter order.
    pub boundaries: Vec<Boundary>,
    /// True when the node budget cut the answer short.
    pub truncated: bool,
}

/// How a node was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceKind {
    Root,
    Call,
    /// Passed as a value — a callback or handler registration, still part of the flow.
    Reference,
}

/// One node of a trace: a function (or external/dynamic callee) and everything it leads to.
#[derive(Debug, Clone)]
pub struct TraceNode {
    /// The function's name (`handle`, `Counter.bump`), or the external/dynamic callee's label
    /// (`http.response`, `f.call`).
    pub name: String,
    pub kind: TraceKind,
    /// The architectural roles this function bears (`Enum.Variant`).
    pub roles: Vec<String>,
    /// The declared name's span — where the function lives. Absent on external/dynamic leaves.
    pub decl_span: Option<Span>,
    /// Where the call/reference happened (in the caller); absent on roots.
    pub site_span: Option<Span>,
    /// A native/module target outside the program — a leaf.
    pub external: bool,
    /// A call through a closure-valued binding — statically unresolvable, a leaf.
    pub dynamic: bool,
    /// This function is already on the current path (recursion) — expanded once, marked here.
    pub cycle: bool,
    /// Children were cut by the depth or node budget.
    pub truncated: bool,
    pub children: Vec<TraceNode>,
}

/// One `(function, role)` binding a walk reached.
#[derive(Debug, Clone)]
pub struct Boundary {
    pub target: String,
    pub role: String,
    /// The bearer's declared-name span.
    pub decl_span: Option<Span>,
}

/// How a `from` spec resolved to trace roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Roots {
    /// The spec named a role (every bearer) or a function.
    Functions(Vec<usize>),
    /// No spec — every role-bearing function is a root (the program's architectural surface).
    /// Empty when the program has no `@role` bindings on functions.
    AllRoleBearers(Vec<usize>),
    /// The spec matched no role binding and no function.
    NotFound,
}

/// The role index keyed the way the graph names functions: declaration name (`Type.method` for
/// methods) → its `Enum.Variant` bindings.
pub fn roles_by_target(info: &ReflectionInfo) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for r in &info.roles {
        map.entry(r.target.clone())
            .or_default()
            .push(format!("{}.{}", r.enum_name, r.variant));
    }
    map
}

/// Resolve `from` to root function indices: a role name (`EntryPoint` or `Semantic.EntryPoint`,
/// case-insensitive — every function bearing it), else a function name, else [`Roots::NotFound`].
/// With no `from`, every role-bearing function.
pub fn resolve_roots(graph: &CallGraph, info: &ReflectionInfo, from: Option<&str>) -> Roots {
    match from {
        Some(spec) => {
            let want = spec.trim().to_ascii_lowercase();
            let role_targets: Vec<usize> = info
                .roles
                .iter()
                .filter(|r| {
                    r.variant.to_ascii_lowercase() == want
                        || format!("{}.{}", r.enum_name, r.variant).to_ascii_lowercase() == want
                })
                .filter_map(|r| graph.function_named(&r.target))
                .collect();
            if !role_targets.is_empty() {
                Roots::Functions(dedup(role_targets))
            } else if let Some(idx) = graph.function_named(spec.trim()) {
                Roots::Functions(vec![idx])
            } else {
                Roots::NotFound
            }
        }
        None => Roots::AllRoleBearers(dedup(
            info.roles
                .iter()
                .filter_map(|r| graph.function_named(&r.target))
                .collect(),
        )),
    }
}

/// Walk the graph forward from `roots`, budgeted. `max_depth` is clamped to
/// [`MAX_DEPTH_CAP`]; pass [`DEFAULT_MAX_DEPTH`]/[`NODE_BUDGET`] for the standard budgets.
pub fn walk(
    graph: &CallGraph,
    roles_by_target: &HashMap<String, Vec<String>>,
    roots: &[usize],
    max_depth: usize,
    node_budget: usize,
) -> Trace {
    let mut tracer = Tracer {
        graph,
        roles_by_target,
        boundaries: Vec::new(),
        seen_boundaries: HashSet::new(),
        nodes_left: node_budget,
        max_depth: max_depth.clamp(1, MAX_DEPTH_CAP),
    };
    let mut path: Vec<usize> = Vec::new();
    let nodes: Vec<TraceNode> = roots
        .iter()
        .map(|&root| tracer.function_node(root, TraceKind::Root, None, 0, &mut path))
        .collect();
    let truncated = tracer.nodes_left == 0;
    Trace {
        roots: nodes,
        boundaries: tracer.boundaries,
        truncated,
    }
}

fn dedup(mut v: Vec<usize>) -> Vec<usize> {
    let mut seen = HashSet::new();
    v.retain(|i| seen.insert(*i));
    v
}

struct Tracer<'a> {
    graph: &'a CallGraph,
    roles_by_target: &'a HashMap<String, Vec<String>>,
    boundaries: Vec<Boundary>,
    seen_boundaries: HashSet<(String, String)>,
    nodes_left: usize,
    max_depth: usize,
}

impl Tracer<'_> {
    fn function_node(
        &mut self,
        idx: usize,
        kind: TraceKind,
        site_span: Option<Span>,
        depth: usize,
        path: &mut Vec<usize>,
    ) -> TraceNode {
        self.nodes_left = self.nodes_left.saturating_sub(1);
        let f = &self.graph.functions[idx];
        let roles = self
            .roles_by_target
            .get(&f.name)
            .cloned()
            .unwrap_or_default();
        for role in &roles {
            if self.seen_boundaries.insert((f.name.clone(), role.clone())) {
                self.boundaries.push(Boundary {
                    target: f.name.clone(),
                    role: role.clone(),
                    decl_span: Some(f.name_span),
                });
            }
        }

        let cycle = path.contains(&idx);
        let at_depth_limit = depth >= self.max_depth;
        let mut truncated = false;
        let children = if cycle || at_depth_limit || self.nodes_left == 0 {
            truncated = !cycle && self.graph.edges_from(Some(idx)).next().is_some();
            Vec::new()
        } else {
            path.push(idx);
            let edges: Vec<CallEdge> = self.graph.edges_from(Some(idx)).cloned().collect();
            let mut children = Vec::new();
            for edge in edges {
                if self.nodes_left == 0 {
                    truncated = true;
                    break;
                }
                children.push(self.edge_node(&edge, depth + 1, path));
            }
            path.pop();
            children
        };

        TraceNode {
            name: f.name.clone(),
            kind,
            roles,
            decl_span: Some(f.name_span),
            site_span,
            external: false,
            dynamic: false,
            cycle,
            // `truncated` is already exact: the cut-children branch above sets it only when
            // there *were* edges to cut (a leaf at the depth limit is a leaf, not a truncation —
            // the MCP original over-reported here), and the budget loop sets it on a mid-list cut.
            truncated,
            children,
        }
    }

    fn edge_node(&mut self, edge: &CallEdge, depth: usize, path: &mut Vec<usize>) -> TraceNode {
        let kind = if edge.call {
            TraceKind::Call
        } else {
            TraceKind::Reference
        };
        match &edge.callee {
            Callee::Function(idx) => self.function_node(*idx, kind, Some(edge.site), depth, path),
            Callee::External(name) => {
                self.nodes_left = self.nodes_left.saturating_sub(1);
                leaf(name, kind, edge.site, true, false)
            }
            Callee::Dynamic(name) => {
                self.nodes_left = self.nodes_left.saturating_sub(1);
                leaf(name, kind, edge.site, false, true)
            }
        }
    }
}

fn leaf(name: &str, kind: TraceKind, site: Span, external: bool, dynamic: bool) -> TraceNode {
    TraceNode {
        name: name.to_string(),
        kind,
        roles: Vec::new(),
        decl_span: None,
        site_span: Some(site),
        external,
        dynamic,
        cycle: false,
        truncated: false,
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    const SRC: &str = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

use std.{math}

#[Route(\"/orders\")]
fn handle(n: int): int {
  v = validate(n)
  return save(v)
}

fn validate(n: int): int {
  s = math.sqrt(4.0)
  echo s
  return n + 1
}

fn save(n: int): int {
  return n
}
";

    fn setup(src: &str) -> (CallGraph, ReflectionInfo) {
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
        // A pure checker-test fixture with no installed extensions — no native roles to thread.
        let info = noeta_ast::reflect::build(&parsed.program, &[], &Default::default());
        (graph, info)
    }

    #[test]
    fn role_spec_resolves_to_every_bearer_and_walks_the_flow() {
        let (graph, info) = setup(SRC);
        let Roots::Functions(roots) = resolve_roots(&graph, &info, Some("EntryPoint")) else {
            panic!("role resolves");
        };
        let trace = walk(
            &graph,
            &roles_by_target(&info),
            &roots,
            DEFAULT_MAX_DEPTH,
            NODE_BUDGET,
        );
        assert_eq!(trace.roots.len(), 1);
        let root = &trace.roots[0];
        assert_eq!(root.name, "handle");
        assert_eq!(root.kind, TraceKind::Root);
        assert_eq!(root.roles, vec!["Semantic.EntryPoint"]);
        let names: Vec<&str> = root.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["validate", "save"]);
        // validate's external math call is a labeled leaf with a site span.
        let validate = &root.children[0];
        let math = validate
            .children
            .iter()
            .find(|c| c.name == "math.sqrt")
            .expect("external leaf");
        assert!(math.external && math.decl_span.is_none() && math.site_span.is_some());
        // The boundary summary carries the entry point.
        assert!(
            trace
                .boundaries
                .iter()
                .any(|b| b.target == "handle" && b.role == "Semantic.EntryPoint")
        );
    }

    #[test]
    fn unknown_spec_is_not_found_and_no_spec_takes_all_bearers() {
        let (graph, info) = setup(SRC);
        assert_eq!(resolve_roots(&graph, &info, Some("nope")), Roots::NotFound);
        let Roots::AllRoleBearers(all) = resolve_roots(&graph, &info, None) else {
            panic!("no spec resolves to bearers");
        };
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn recursion_is_marked_a_cycle_not_walked_forever() {
        let (graph, info) = setup("fn spin(n: int): int { return spin(n) }\n");
        let Roots::Functions(roots) = resolve_roots(&graph, &info, Some("spin")) else {
            panic!("fn name resolves");
        };
        let trace = walk(
            &graph,
            &HashMap::new(),
            &roots,
            DEFAULT_MAX_DEPTH,
            NODE_BUDGET,
        );
        let root = &trace.roots[0];
        assert_eq!(root.children.len(), 1);
        assert!(root.children[0].cycle, "self-call marked, not expanded");
        assert!(root.children[0].children.is_empty());
    }
}

/// Why a [`LocatedTrace`] has the shape it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceStatus {
    Ok,
    /// The program binds no `@role` on any function — nothing to trace.
    NoRoles,
    /// The `from` spec matched no role binding and no function.
    NotFound,
}

impl TraceStatus {
    /// The wire tag.
    pub fn as_str(self) -> &'static str {
        match self {
            TraceStatus::Ok => "ok",
            TraceStatus::NoRoles => "noRoles",
            TraceStatus::NotFound => "notFound",
        }
    }
}

/// A resolved editor location (the structured twin of the text renderer's `path:line`).
#[derive(Debug, Clone)]
pub struct TraceLoc {
    pub uri: String,
    pub line: u32,
    pub character: u32,
}

/// One node of the located trace tree (see [`TraceNode`] for field semantics).
#[derive(Debug, Clone)]
pub struct LocatedTraceNode {
    pub name: String,
    pub kind: TraceKind,
    pub roles: Vec<String>,
    pub loc: Option<TraceLoc>,
    pub external: bool,
    pub dynamic: bool,
    pub cycle: bool,
    pub truncated: bool,
    pub children: Vec<LocatedTraceNode>,
}

/// One located boundary a walk reached.
#[derive(Debug, Clone)]
pub struct LocatedBoundary {
    pub role: String,
    pub target: String,
    pub loc: Option<TraceLoc>,
}

/// The structured, located trace the editor's trace view renders (the text document is the same
/// walk rendered for terminals/agents).
#[derive(Debug, Clone)]
pub struct LocatedTrace {
    pub from: Option<String>,
    pub status: TraceStatus,
    pub truncated: bool,
    pub boundaries: Vec<LocatedBoundary>,
    pub roots: Vec<LocatedTraceNode>,
}

impl LocatedTrace {
    /// An empty trace carrying only its `status` (the not-ok cases).
    pub fn empty(from: Option<&str>, status: TraceStatus) -> LocatedTrace {
        LocatedTrace {
            from: from.map(str::to_string),
            status,
            truncated: false,
            boundaries: Vec::new(),
            roots: Vec::new(),
        }
    }
}
