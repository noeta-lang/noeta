//! `architecture` — the whole project as its `@role` graph.
//!
//! `trace` unfolds one entry point's flow. This is the summary above it: one node per role, one
//! edge per pair of roles, each carrying how many declaration-to-declaration connections it stands
//! for and a few of them by name. Everything bearing no role collapses out of the edges, so a
//! handler that reaches a store through four helpers is one edge from `EntryPoint` to
//! `PersistenceBoundary`.
//!
//! A project with no `@role` bindings gets the `unassigned` count and its most-connected
//! declarations, which is the answer that says what is there rather than that nothing is.
//!
//! The derivation lives in [`noeta_ide::architecture`], shared with the editor's swimlane view.

use noeta_ast::Program;
use noeta_ide::architecture::{self as engine};
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{self, LinkStatus, NodeId, Prepared};
use crate::context::{function_id, join};
use crate::graph::DeclIndex;
use crate::trace::BoundaryHit;

/// The `architecture` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ArchitectureOutput {
    /// One node per `@role` bound anywhere in the project, sorted by role.
    pub roles: Vec<RoleNode>,
    /// The aggregated edges between them, sorted by `from` then `to`.
    pub edges: Vec<RoleEdge>,
    /// The graph the aggregation was taken over: one entry per pair of role-bearing declarations
    /// joined by a chain of calls through declarations bearing no role. Walk it to answer which
    /// boundaries one entry point reaches.
    pub connections: Vec<Connection>,
    /// Every `(declaration, role)` binding — the same summary `trace` reports.
    pub boundaries: Vec<BoundaryHit>,
    /// The declarations bearing no role.
    pub unassigned: Unassigned,
    /// Whether the graph was built over the merged workspace program.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// One role.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RoleNode {
    /// The role's qualified binding (`Semantic.EntryPoint`).
    pub role: String,
    /// The declarations bearing it, each with the id every other graph tool reports.
    pub bearers: Vec<NodeId>,
    /// Connections arriving from another role.
    pub in_degree: usize,
    /// Connections leaving for another role.
    pub out_degree: usize,
}

/// One aggregated edge.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RoleEdge {
    pub from: String,
    pub to: String,
    /// How many declaration-to-declaration connections this edge stands for.
    pub count: usize,
    /// A few of them, so the edge can be read back to real code.
    pub exemplars: Vec<Exemplar>,
    /// True when every connection here runs only through a passed reference — a handler
    /// registration or a callback rather than a call.
    pub via_reference: bool,
}

/// One connection an aggregated edge stands for.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Exemplar {
    pub caller: NodeId,
    pub callee: NodeId,
}

/// One role-bearing declaration reaching another, with everything between them collapsed away.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Connection {
    pub from: NodeId,
    pub to: NodeId,
    /// True when every chain joining the two runs through a passed reference.
    pub via_reference: bool,
}

/// The declarations bearing no role.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Unassigned {
    pub count: usize,
    /// The most-connected of them.
    pub exemplars: Vec<NodeId>,
}

/// Answer `architecture`.
pub fn architecture(p: &Prepared) -> ArchitectureOutput {
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
    let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
    let info = noeta_ast::reflect::build(program, &native_roles, &Default::default());
    let roles = noeta_ide::trace::roles_by_target(&info);
    let arch = engine::architecture(&graph, &roles);

    let id = |function: usize| function_id(p, &decls, &graph, function);
    let mut notes: Vec<String> = status.note().into_iter().collect();
    if arch.roles.is_empty() {
        notes.push(format!(
            "no `@role` bindings on any declaration — the {} declarations here are reported as \
             unassigned; bind roles with a `@role(...)` attribute to map the architecture",
            arch.unassigned.count
        ));
    }

    ArchitectureOutput {
        roles: arch
            .roles
            .iter()
            .map(|role| RoleNode {
                role: role.role.clone(),
                bearers: role.bearers.iter().map(|&f| id(f)).collect(),
                in_degree: role.in_degree,
                out_degree: role.out_degree,
            })
            .collect(),
        edges: arch
            .edges
            .iter()
            .map(|edge| RoleEdge {
                from: edge.from.clone(),
                to: edge.to.clone(),
                count: edge.count,
                exemplars: edge
                    .exemplars
                    .iter()
                    .map(|&(caller, callee)| Exemplar {
                        caller: id(caller),
                        callee: id(callee),
                    })
                    .collect(),
                via_reference: edge.via_reference,
            })
            .collect(),
        connections: arch
            .bearer_graph
            .edges
            .iter()
            .map(|edge| Connection {
                from: id(edge.from),
                to: id(edge.to),
                via_reference: edge.via_reference,
            })
            .collect(),
        boundaries: arch
            .boundaries
            .iter()
            .map(|b| {
                let at = analyze::locate_span(p, b.decl_span);
                BoundaryHit {
                    target: b.target.clone(),
                    role: b.role.clone(),
                    file: at.as_ref().map(|(file, _)| file.clone()),
                    line: at.map(|(_, loc)| loc.start.line),
                }
            })
            .collect(),
        unassigned: Unassigned {
            count: arch.unassigned.count,
            exemplars: arch.unassigned.exemplars.iter().map(|&f| id(f)).collect(),
        },
        linked: status.linked,
        link_diagnostics: status.link_diagnostics.clone(),
        note: join(notes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    /// Handler to service to store, with a sink reached only by being passed as a value.
    const SRC: &str = "\
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

fn defer(f: (int) -> int): int { return f(1) }

#[Route(\"/orders\")]
fn handle(n: int): int { return service(n) + defer(notify) }

echo handle(1)
";

    fn prep(src: &str) -> Prepared {
        prepare(&Some(src.to_string()), &None).unwrap()
    }

    #[test]
    fn the_role_quotient_aggregates_the_bearer_edges() {
        let out = architecture(&prep(SRC));
        let edges: Vec<(&str, &str, usize, bool)> = out
            .edges
            .iter()
            .map(|e| (e.from.as_str(), e.to.as_str(), e.count, e.via_reference))
            .collect();
        assert_eq!(
            edges,
            vec![
                // Collapsed through `service`, which bears no role.
                ("Semantic.EntryPoint", "Semantic.Persistence", 1, false),
                // Reached only by being handed to `defer` as a value.
                ("Semantic.EntryPoint", "Semantic.Sink", 1, true),
            ]
        );
        // The exemplar reads the edge back to real declarations, with joinable ids.
        let exemplar = &out.edges[0].exemplars[0];
        assert_eq!(exemplar.caller.name, "handle");
        assert_eq!(exemplar.callee.name, "save");
        assert_eq!(exemplar.callee.kind.as_str(), "function");
        assert!(exemplar.callee.file.is_some() && exemplar.callee.span.is_some());
        assert!(out.linked && out.link_diagnostics.is_empty());
        // The bearer graph the aggregation was taken over is on the wire too, which is what lets a
        // reader ask which boundaries *this* entry point reaches rather than which roles do.
        let connections: Vec<(&str, &str)> = out
            .connections
            .iter()
            .map(|c| (c.from.name.as_str(), c.to.name.as_str()))
            .collect();
        assert_eq!(connections, vec![("handle", "notify"), ("handle", "save")]);
    }

    #[test]
    fn a_role_nothing_reaches_carries_an_in_degree_of_zero() {
        let out = architecture(&prep(SRC));
        let roles: Vec<(&str, usize, usize)> = out
            .roles
            .iter()
            .map(|r| (r.role.as_str(), r.in_degree, r.out_degree))
            .collect();
        assert_eq!(
            roles,
            vec![
                ("Semantic.EntryPoint", 0, 2),
                ("Semantic.Persistence", 1, 0),
                ("Semantic.Sink", 1, 0),
            ]
        );
        let entry = &out.roles[0];
        assert_eq!(entry.bearers.len(), 1);
        assert_eq!(entry.bearers[0].name, "handle");
        // The boundary summary is the same shape `trace` reports.
        let hits: Vec<(&str, &str)> = out
            .boundaries
            .iter()
            .map(|b| (b.role.as_str(), b.target.as_str()))
            .collect();
        assert_eq!(
            hits,
            vec![
                ("Semantic.EntryPoint", "handle"),
                ("Semantic.Persistence", "save"),
                ("Semantic.Sink", "notify"),
            ]
        );
        assert!(out.boundaries[0].line.is_some());
    }

    #[test]
    fn a_project_with_no_roles_reports_what_it_holds() {
        let out = architecture(&prep(
            "fn leaf(): int { return 1 }\n\
             fn middle(): int { return leaf() }\n\
             fn other(): int { return leaf() }\n\
             fn top(): int { return middle() + leaf() }\n\
             echo top()\n",
        ));
        assert!(out.roles.is_empty() && out.edges.is_empty() && out.boundaries.is_empty());
        assert_eq!(out.unassigned.count, 4);
        assert_eq!(out.unassigned.exemplars[0].name, "leaf");
        assert!(out.unassigned.exemplars[0].span.is_some());
        let note = out.note.expect("an empty graph explains itself");
        assert!(note.contains("no `@role` bindings"), "{note}");
        assert!(note.contains("unassigned"), "{note}");
    }
}
