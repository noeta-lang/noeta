//! R3, the headline: `trace` — unfold the full static path a request would take from an
//! architectural role. `trace(from: "EntryPoint")` starts at every declaration bearing that
//! `@role` binding and walks the [`noeta_ide::callgraph`] forward: each node is a function with
//! its own roles, location, and how it was reached (a syntactic call or a passed reference — a
//! handler registration or callback is part of the flow too). External module calls
//! (`http.response`, `fs.read`) and dynamic callees appear as labeled leaves, never guesses.
//!
//! The `boundaries` summary is the architectural answer on its own: every `(function, role)`
//! binding the trace reached — "this entry point crosses into these persistence/trust
//! boundaries".

use std::collections::{HashMap, HashSet};

use noeta_ast::Program;
use noeta_ide::callgraph::{self, CallGraph, Callee};
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{self, Loc, Prepared};

/// Traces deeper than this are cut (per-node `truncated`), whatever `max_depth` asks for.
const MAX_DEPTH_CAP: usize = 16;
const DEFAULT_MAX_DEPTH: usize = 6;
/// The whole answer is capped at this many nodes — a pathological fan-out reports what fits and
/// says so, instead of flooding the agent.
const NODE_BUDGET: usize = 500;

/// The `trace` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TraceOutput {
    pub found: bool,
    /// One trace per starting function, in declaration order.
    pub traces: Vec<TraceNode>,
    /// Every `(function, role)` binding the traces reached — the role boundaries this flow
    /// crosses, in encounter order.
    pub boundaries: Vec<BoundaryHit>,
    /// True when the node budget cut the answer short.
    pub truncated: bool,
    pub note: Option<String>,
}

/// One node of a trace: a function (or external/dynamic callee) and everything it leads to.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TraceNode {
    /// The function's name (`handle`, `Counter.bump`), or the external/dynamic callee's label
    /// (`http.response`, `f.call`).
    pub name: String,
    /// How this node was reached: `root` | `call` | `reference` (passed as a value — a callback
    /// or handler registration).
    pub kind: String,
    /// The architectural roles this function bears (`Enum.Variant`).
    pub roles: Vec<String>,
    /// Where the function is declared.
    pub file: Option<String>,
    pub line: Option<u32>,
    /// Where the call/reference happened (in the caller), absent on roots.
    pub site: Option<Loc>,
    /// A native/module target outside the program — a leaf.
    pub external: bool,
    /// A call through a closure-valued binding — statically unresolvable, a leaf.
    pub dynamic: bool,
    /// This function is already on the current path (recursion) — expanded once, marked here.
    pub cycle: bool,
    /// Children were cut by `max_depth` or the node budget.
    pub truncated: bool,
    pub children: Vec<TraceNode>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoundaryHit {
    pub target: String,
    pub role: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

struct Tracer<'a> {
    p: &'a Prepared,
    graph: &'a CallGraph,
    roles_by_target: &'a HashMap<String, Vec<String>>,
    boundaries: Vec<BoundaryHit>,
    seen_boundaries: HashSet<(String, String)>,
    nodes_left: usize,
    max_depth: usize,
}

/// Answer `trace`: walk the call graph forward from `from` — a role (`EntryPoint` or
/// `Semantic.EntryPoint`, starting at every function bearing it) or a function name. With no
/// `from`, every role-bearing function is a root (the program's architectural surface).
pub fn trace(p: &Prepared, from: Option<&str>, max_depth: Option<usize>) -> TraceOutput {
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = match &linked.0 {
        Ok(prog) => prog,
        Err(_) => &entry.0.program,
    };
    let checked = noeta_db::linked_checked_ide(&p.db, p.ws);
    let texts: Vec<&str> = p.sources.iter().map(|s| s.text()).collect();
    let graph = callgraph::build(program, &checked.expr_types, &texts);

    let info = noeta_ast::reflect::build(program);
    let mut roles_by_target: HashMap<String, Vec<String>> = HashMap::new();
    for r in &info.roles {
        roles_by_target
            .entry(r.target.clone())
            .or_default()
            .push(format!("{}.{}", r.enum_name, r.variant));
    }

    // Resolve the roots: a role name → every function bearing it; else a function name; with no
    // `from`, every role-bearing function.
    let (roots, note): (Vec<usize>, Option<String>) = match from {
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
                (dedup(role_targets), None)
            } else if let Some(idx) = graph.function_named(spec.trim()) {
                (vec![idx], None)
            } else {
                return TraceOutput {
                    found: false,
                    traces: Vec::new(),
                    boundaries: Vec::new(),
                    truncated: false,
                    note: Some(format!(
                        "`{spec}` matches no role binding and no function — try `reflect` for \
                         the role index or `symbols` for the declarations"
                    )),
                };
            }
        }
        None => {
            let all: Vec<usize> = info
                .roles
                .iter()
                .filter_map(|r| graph.function_named(&r.target))
                .collect();
            if all.is_empty() {
                return TraceOutput {
                    found: false,
                    traces: Vec::new(),
                    boundaries: Vec::new(),
                    truncated: false,
                    note: Some(
                        "no `@role` bindings on any function — pass `from` (a function name) to \
                         trace from a specific start"
                            .to_string(),
                    ),
                };
            }
            (
                dedup(all),
                Some("no `from` given — tracing from every role-bearing function".to_string()),
            )
        }
    };

    let mut tracer = Tracer {
        p,
        graph: &graph,
        roles_by_target: &roles_by_target,
        boundaries: Vec::new(),
        seen_boundaries: HashSet::new(),
        nodes_left: NODE_BUDGET,
        max_depth: max_depth
            .unwrap_or(DEFAULT_MAX_DEPTH)
            .clamp(1, MAX_DEPTH_CAP),
    };
    let mut path: Vec<usize> = Vec::new();
    let traces: Vec<TraceNode> = roots
        .iter()
        .map(|&root| tracer.function_node(root, "root", None, 0, &mut path))
        .collect();
    let truncated = tracer.nodes_left == 0;
    TraceOutput {
        found: true,
        traces,
        boundaries: tracer.boundaries,
        truncated,
        note,
    }
}

fn dedup(mut v: Vec<usize>) -> Vec<usize> {
    let mut seen = HashSet::new();
    v.retain(|i| seen.insert(*i));
    v
}

impl Tracer<'_> {
    fn function_node(
        &mut self,
        idx: usize,
        kind: &str,
        site: Option<Loc>,
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
        let at = analyze::locate_span(self.p, f.name_span);
        for role in &roles {
            if self.seen_boundaries.insert((f.name.clone(), role.clone())) {
                self.boundaries.push(BoundaryHit {
                    target: f.name.clone(),
                    role: role.clone(),
                    file: at.as_ref().map(|(file, _)| file.clone()),
                    line: at.as_ref().map(|(_, loc)| loc.start.line),
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
            let edges: Vec<_> = self.graph.edges_from(Some(idx)).cloned().collect();
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
            kind: kind.to_string(),
            roles,
            file: at.as_ref().map(|(file, _)| file.clone()),
            line: at.map(|(_, loc)| loc.start.line),
            site,
            external: false,
            dynamic: false,
            cycle,
            truncated: truncated || (at_depth_limit && !cycle),
            children,
        }
    }

    fn edge_node(
        &mut self,
        edge: &callgraph::CallEdge,
        depth: usize,
        path: &mut Vec<usize>,
    ) -> TraceNode {
        let kind = if edge.call { "call" } else { "reference" };
        let site = analyze::locate_span(self.p, edge.site).map(|(_, loc)| loc.start);
        match &edge.callee {
            Callee::Function(idx) => self.function_node(*idx, kind, site, depth, path),
            Callee::External(name) => {
                self.nodes_left = self.nodes_left.saturating_sub(1);
                leaf(name, kind, site, true, false)
            }
            Callee::Dynamic(name) => {
                self.nodes_left = self.nodes_left.saturating_sub(1);
                leaf(name, kind, site, false, true)
            }
        }
    }
}

fn leaf(name: &str, kind: &str, site: Option<Loc>, external: bool, dynamic: bool) -> TraceNode {
    TraceNode {
        name: name.to_string(),
        kind: kind.to_string(),
        roles: Vec::new(),
        file: None,
        line: None,
        site,
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
    use crate::analyze::prepare;

    /// An entry point flowing through a helper into a persistence boundary and an external call.
    const SRC: &str = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

@attribute
@role(Semantic.Persistence)
struct Store { table: string }

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

#[Store(\"orders\")]
fn save(n: int): int {
  echo n
  return n
}
";

    fn prep() -> Prepared {
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    #[test]
    fn entrypoint_role_unfolds_the_request_path() {
        let out = trace(&prep(), Some("EntryPoint"), None);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(out.traces.len(), 1);
        let root = &out.traces[0];
        assert_eq!(root.name, "handle");
        assert_eq!(root.kind, "root");
        assert_eq!(root.roles, vec!["Semantic.EntryPoint"]);
        // handle → validate and handle → save, both syntactic calls.
        let child_names: Vec<&str> = root.children.iter().map(|c| c.name.as_str()).collect();
        assert!(
            child_names.contains(&"validate"),
            "children: {child_names:?}"
        );
        assert!(child_names.contains(&"save"));
        // The persistence boundary shows on the node the trace reached…
        let save = root.children.iter().find(|c| c.name == "save").unwrap();
        assert_eq!(save.roles, vec!["Semantic.Persistence"]);
        assert_eq!(save.kind, "call");
        assert!(save.site.is_some(), "call site located");
        // …and validate's external math call is a labeled leaf.
        let validate = root.children.iter().find(|c| c.name == "validate").unwrap();
        assert!(
            validate
                .children
                .iter()
                .any(|c| c.name == "math.sqrt" && c.external),
            "validate children: {:?}",
            validate.children
        );
        // The boundary summary answers the architectural question directly.
        let hits: Vec<(&str, &str)> = out
            .boundaries
            .iter()
            .map(|b| (b.target.as_str(), b.role.as_str()))
            .collect();
        assert!(hits.contains(&("handle", "Semantic.EntryPoint")));
        assert!(hits.contains(&("save", "Semantic.Persistence")));
    }

    #[test]
    fn qualified_role_and_function_name_both_resolve() {
        let by_role = trace(&prep(), Some("Semantic.EntryPoint"), None);
        assert!(by_role.found);
        assert_eq!(by_role.traces[0].name, "handle");

        let by_name = trace(&prep(), Some("validate"), None);
        assert!(by_name.found);
        assert_eq!(by_name.traces[0].name, "validate");
        assert!(by_name.traces[0].roles.is_empty());
    }

    #[test]
    fn omitted_from_traces_every_role_bearing_function() {
        let out = trace(&prep(), None, None);
        assert!(out.found);
        let roots: Vec<&str> = out.traces.iter().map(|t| t.name.as_str()).collect();
        assert!(roots.contains(&"handle") && roots.contains(&"save"));
        assert!(out.note.unwrap().contains("every role-bearing function"));
    }

    #[test]
    fn unknown_start_reports_not_found() {
        let out = trace(&prep(), Some("ghost"), None);
        assert!(!out.found);
        assert!(out.note.unwrap().contains("ghost"));
    }

    #[test]
    fn recursion_is_marked_as_a_cycle_not_expanded_forever() {
        let src = "fn ping(n: int): int { return pong(n) }\nfn pong(n: int): int { return ping(n) }\necho ping(1)\n";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = trace(&p, Some("ping"), None);
        assert!(out.found);
        let pong = &out.traces[0].children[0];
        assert_eq!(pong.name, "pong");
        let back = &pong.children[0];
        assert_eq!(back.name, "ping");
        assert!(back.cycle, "the back-edge is marked, not expanded");
        assert!(back.children.is_empty());
    }
}
