//! `trace` — unfold the full static path a request would take from an
//! architectural role. `trace(from: "EntryPoint")` starts at every declaration bearing that
//! `@role` binding and walks the call graph forward: each node is a function with its own roles,
//! location, and how it was reached (a syntactic call or a passed reference — a handler
//! registration or callback is part of the flow too). External module calls (`http.response`,
//! `fs.read`) and dynamic callees appear as labeled leaves, never guesses.
//!
//! The `boundaries` summary is the architectural answer on its own: every `(function, role)`
//! binding the trace reached — "this entry point crosses into these persistence/trust
//! boundaries".
//!
//! The walk itself lives in [`noeta_ide::trace`] — the LSP's trace document runs
//! the same engine, so agent and editor can never disagree. This module owns only the MCP wire
//! shapes (span → file/line resolution, JSON schema) and the tool's notes.

use noeta_ast::Program;
use noeta_ide::callgraph;
use noeta_ide::trace as engine;
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{self, Loc, Prepared};

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

/// Answer `trace`: walk the call graph forward from `from` — a role (`EntryPoint` or
/// `Semantic.EntryPoint`, starting at every function bearing it) or a function name. With no
/// `from`, every role-bearing function is a root (the program's architectural surface).
pub fn trace(p: &Prepared, from: Option<&str>, max_depth: Option<usize>) -> TraceOutput {
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = match &linked.program {
        Ok(prog) => prog,
        Err(_) => &entry.0.program,
    };
    let checked = noeta_db::linked_checked_ide(&p.db, p.ws);
    let texts: Vec<&str> = p.sources.iter().map(|s| s.text()).collect();
    let graph = callgraph::build(program, &checked.expr_types, &checked.sites, &texts);
    let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
    let info = noeta_ast::reflect::build(program, &native_roles, &Default::default());

    let (roots, note): (Vec<usize>, Option<String>) =
        match engine::resolve_roots(&graph, &info, from) {
            engine::Roots::Functions(roots) => (roots, None),
            engine::Roots::NotFound => {
                let spec = from.unwrap_or_default();
                return not_found(format!(
                    "`{spec}` matches no role binding and no function — try `reflect` for the \
                     role index or `symbols` for the declarations"
                ));
            }
            engine::Roots::AllRoleBearers(all) => {
                if all.is_empty() {
                    return not_found(
                        "no `@role` bindings on any function — pass `from` (a function name) to \
                         trace from a specific start"
                            .to_string(),
                    );
                }
                (
                    all,
                    Some("no `from` given — tracing from every role-bearing function".to_string()),
                )
            }
        };

    let walked = engine::walk(
        &graph,
        &engine::roles_by_target(&info),
        &roots,
        max_depth.unwrap_or(engine::DEFAULT_MAX_DEPTH),
        engine::NODE_BUDGET,
    );
    TraceOutput {
        found: true,
        traces: walked.roots.iter().map(|n| to_wire(p, n)).collect(),
        boundaries: walked
            .boundaries
            .iter()
            .map(|b| {
                let at = b.decl_span.and_then(|span| analyze::locate_span(p, span));
                BoundaryHit {
                    target: b.target.clone(),
                    role: b.role.clone(),
                    file: at.as_ref().map(|(file, _)| file.clone()),
                    line: at.map(|(_, loc)| loc.start.line),
                }
            })
            .collect(),
        truncated: walked.truncated,
        note,
    }
}

fn not_found(note: String) -> TraceOutput {
    TraceOutput {
        found: false,
        traces: Vec::new(),
        boundaries: Vec::new(),
        truncated: false,
        note: Some(note),
    }
}

/// Resolve an engine node's spans to the tool's file/line wire shape, recursing into children.
fn to_wire(p: &Prepared, n: &engine::TraceNode) -> TraceNode {
    let at = n.decl_span.and_then(|span| analyze::locate_span(p, span));
    TraceNode {
        name: n.name.clone(),
        kind: match n.kind {
            engine::TraceKind::Root => "root",
            engine::TraceKind::Call => "call",
            engine::TraceKind::Reference => "reference",
        }
        .to_string(),
        roles: n.roles.clone(),
        file: at.as_ref().map(|(file, _)| file.clone()),
        line: at.map(|(_, loc)| loc.start.line),
        site: n
            .site_span
            .and_then(|span| analyze::locate_span(p, span))
            .map(|(_, loc)| loc.start),
        external: n.external,
        dynamic: n.dynamic,
        cycle: n.cycle,
        truncated: n.truncated,
        children: n.children.iter().map(|c| to_wire(p, c)).collect(),
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
