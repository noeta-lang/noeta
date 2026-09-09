//! `trace` — unfold the full static path a request would take from an
//! architectural role. `trace(from: "EntryPoint")` starts at every declaration bearing that
//! `@role` binding and walks the call graph forward: each node is a function with its own roles,
//! location, and how it was reached (a syntactic call or a passed reference — a handler
//! registration or callback is part of the flow too). External module calls (`http.response`,
//! `fs.read`) and dynamic callees appear as labeled leaves, never guesses.
//!
//! Each function unfolds **once**: a second arrival marks the node `shared` and stops, so the
//! answer stays proportional to the reachable functions rather than to the paths through them.
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

use crate::analyze::{self, LinkStatus, Loc, NodeId, NodeKind, Prepared};
use crate::graph::DeclIndex;

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
    /// Whether the walk ran over the merged workspace program. When false the graph came from the
    /// entry file's own parse: names are unqualified, and a call into a sibling module is reported
    /// as an external leaf. Every node of such a walk carries `unverified`.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// One node of a trace: a function (or external/dynamic callee) and everything it leads to.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TraceNode {
    /// The node's stable identity — the same `id` `symbols`, `reflect`, `impact` and `callers`
    /// report for this declaration, and the only spelling of its name, file and span. An external
    /// or dynamic callee has a name and a kind and no file or span, having no declaration here.
    pub id: NodeId,
    /// How this node was **reached**: `root` | `call` | `reference` (passed as a value — a
    /// callback or handler registration). What the node *is* is `id.kind`.
    pub kind: String,
    /// The architectural roles this function bears (`Enum.Variant`).
    pub roles: Vec<String>,
    /// Where the call/reference happened (in the caller), absent on roots.
    pub site: Option<Loc>,
    /// A native/module target outside the program — a leaf. False for a target that names one of
    /// this project's own modules, which is `unresolved` in `id.kind` instead.
    pub external: bool,
    /// A call through a closure-valued binding — statically unresolvable, a leaf.
    pub dynamic: bool,
    /// This function is already on the current path (recursion) — expanded once, marked here.
    pub cycle: bool,
    /// This function was expanded earlier in the answer — its subtree is written once, and every
    /// later arrival is this reference.
    pub shared: bool,
    /// Children were cut by `max_depth` or the node budget.
    pub truncated: bool,
    /// This node comes from an unlinked program, so its `external`/`dynamic` classification is
    /// unproven — a real intra-project call can land here as an external leaf.
    pub unverified: bool,
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
    let status = LinkStatus::of(p);
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
    // The declaration inventory over the same program: a trace node's identity is the declaration
    // it names, so a trace joins `symbols` and `callers` on `id` rather than on (name, file, line).
    let decls = DeclIndex::build(program);

    let (roots, walk_note): (Vec<usize>, Option<String>) =
        match engine::resolve_roots(&graph, &info, from) {
            engine::Roots::Functions(roots) => (roots, None),
            engine::Roots::Ambiguous(candidates) => {
                let spec = from.unwrap_or_default();
                return not_found(
                    format!(
                        "`{spec}` names {} functions — pass one of: {}",
                        candidates.len(),
                        candidates.join(", ")
                    ),
                    &status,
                );
            }
            engine::Roots::NotFound { near } => {
                let spec = from.unwrap_or_default();
                let hint = if near.is_empty() {
                    "try `reflect` for the role index or `symbols` for the declarations".to_string()
                } else {
                    format!("did you mean {}?", near.join(", "))
                };
                // A name the graph does not hold may still be declared: the graph is the program
                // the entry links, and a module's `@test` block is referenced by nothing, so it is
                // outlined by `symbols` and absent here. Say which it is.
                let note =
                    match crate::graph::outside_graph_note(p, &crate::graph::source_index(p), spec)
                    {
                        Some(outside) => outside,
                        None => {
                            format!("`{spec}` matches no role binding and no function — {hint}")
                        }
                    };
                return not_found(note, &status);
            }
            engine::Roots::AllRoleBearers(all) => {
                if all.is_empty() {
                    return not_found(
                        "no `@role` bindings on any function — pass `from` (a function name) to \
                         trace from a specific start"
                            .to_string(),
                        &status,
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
        traces: walked
            .roots
            .iter()
            .map(|n| to_wire(p, n, &decls, status.linked))
            .collect(),
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
        // The link note comes first: a walk over an unlinked program is a different answer, and
        // saying so has to outrank "no `from` given".
        note: match (status.note(), walk_note) {
            (Some(link), Some(walk)) => Some(format!("{link}; {walk}")),
            (Some(link), None) => Some(link),
            (None, walk) => walk,
        },
        linked: status.linked,
        link_diagnostics: status.link_diagnostics.clone(),
    }
}

fn not_found(note: String, status: &LinkStatus) -> TraceOutput {
    TraceOutput {
        found: false,
        traces: Vec::new(),
        boundaries: Vec::new(),
        truncated: false,
        note: Some(match status.note() {
            Some(link) => format!("{link}; {note}"),
            None => note,
        }),
        linked: status.linked,
        link_diagnostics: status.link_diagnostics.clone(),
    }
}

/// Resolve an engine node's spans to the tool's file/line wire shape, recursing into children.
fn to_wire(p: &Prepared, n: &engine::TraceNode, decls: &DeclIndex, linked: bool) -> TraceNode {
    // An `external` target that names one of this project's own modules is not external: it is a
    // real intra-project call the unlinked program could not resolve. Reporting it as external is
    // a wrong architecture an agent cannot detect, so it degrades to `unresolved` instead.
    let unresolved = n.external && p.names_a_project_module(&n.name);
    let kind = if unresolved {
        NodeKind::Unresolved
    } else if n.external {
        NodeKind::External
    } else if n.dynamic {
        NodeKind::Dynamic
    } else if n.name.contains('.') && n.decl_span.is_some() {
        NodeKind::Method
    } else {
        NodeKind::Function
    };
    let id = match n.decl_span.and_then(|span| decls.at_name_span(span)) {
        Some(decl) => decl.id(p),
        None => match n.decl_span {
            Some(span) => p.node_id(&n.name, kind, span),
            None => p.unlocated_id(&n.name, kind),
        },
    };
    TraceNode {
        id,
        kind: match n.kind {
            engine::TraceKind::Root => "root",
            engine::TraceKind::Call => "call",
            engine::TraceKind::Reference => "reference",
        }
        .to_string(),
        roles: n.roles.clone(),
        site: n
            .site_span
            .and_then(|span| analyze::locate_span(p, span))
            .map(|(_, loc)| loc.start),
        external: n.external && !unresolved,
        dynamic: n.dynamic,
        cycle: n.cycle,
        shared: n.shared,
        truncated: n.truncated,
        unverified: !linked,
        children: n
            .children
            .iter()
            .map(|c| to_wire(p, c, decls, linked))
            .collect(),
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
        assert_eq!(root.id.name, "handle");
        assert_eq!(root.kind, "root");
        assert_eq!(root.roles, vec!["Semantic.EntryPoint"]);
        // handle → validate and handle → save, both syntactic calls.
        let child_names: Vec<&str> = root.children.iter().map(|c| c.id.name.as_str()).collect();
        assert!(
            child_names.contains(&"validate"),
            "children: {child_names:?}"
        );
        assert!(child_names.contains(&"save"));
        // The persistence boundary shows on the node the trace reached…
        let save = root.children.iter().find(|c| c.id.name == "save").unwrap();
        assert_eq!(save.roles, vec!["Semantic.Persistence"]);
        assert_eq!(save.kind, "call");
        assert!(save.site.is_some(), "call site located");
        // …and validate's external math call is a labeled leaf.
        let validate = root
            .children
            .iter()
            .find(|c| c.id.name == "validate")
            .unwrap();
        assert!(
            validate
                .children
                .iter()
                .any(|c| c.id.name == "math.sqrt" && c.external),
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
        assert_eq!(by_role.traces[0].id.name, "handle");

        let by_name = trace(&prep(), Some("validate"), None);
        assert!(by_name.found);
        assert_eq!(by_name.traces[0].id.name, "validate");
        assert!(by_name.traces[0].roles.is_empty());
    }

    #[test]
    fn omitted_from_traces_every_role_bearing_function() {
        let out = trace(&prep(), None, None);
        assert!(out.found);
        let roots: Vec<&str> = out.traces.iter().map(|t| t.id.name.as_str()).collect();
        assert!(roots.contains(&"handle") && roots.contains(&"save"));
        assert!(out.note.unwrap().contains("every role-bearing function"));
    }

    #[test]
    fn unknown_start_reports_not_found() {
        let out = trace(&prep(), Some("ghost"), None);
        assert!(!out.found);
        assert!(out.note.unwrap().contains("ghost"));
    }

    /// **D5.** One unresolvable `use` breaks the link, and every graph tool then answers off the
    /// entry file's own parse. The walk stayed `found: true` with a `null` note, silently swapped
    /// every node's identity from qualified to bare, and rendered a real call into a sibling
    /// module as `external: true` with no file — a wrong architecture an agent cannot detect.
    #[test]
    fn a_broken_use_reports_the_failed_link_and_never_calls_a_sibling_external() {
        noeta_stdlib::registry::default_seeded();
        let root = noeta_test_temp::TempDir::new("mcp-trace-link-status");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/broken\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("alpha.noe"),
            "pub fn alpha_only(): int { return 1 }\n",
        )
        .unwrap();
        let main = "use broken.alpha\n\
                    fn entry(): int { return alpha.alpha_only() }\n\
                    echo entry()\n";

        // The control: the same project links, and the call resolves to a located function.
        std::fs::write(root.join("src").join("main.noe"), main).unwrap();
        let entry = root.join("src").join("main.noe").display().to_string();
        // Addressed by the post-link name, which is what a linked walk speaks.
        let clean = trace(
            &prepare(&None, &Some(entry.clone())).unwrap(),
            Some("broken.main.entry"),
            None,
        );
        assert!(clean.linked, "note: {:?}", clean.note);
        assert!(clean.link_diagnostics.is_empty());
        let child = &clean.traces[0].children[0];
        assert_eq!(child.id.name, "broken.alpha.alpha_only");
        assert!(!child.external && !child.unverified, "{child:?}");

        // Now break the link with one import of a module that does not exist.
        std::fs::write(
            root.join("src").join("main.noe"),
            format!("use broken.nonexistent\n{main}"),
        )
        .unwrap();
        // The fallback speaks bare names, which is itself part of what `linked: false` warns about.
        let out = trace(&prepare(&None, &Some(entry)).unwrap(), Some("entry"), None);
        assert!(!out.linked, "a broken `use` must flip `linked` to false");
        assert!(
            !out.link_diagnostics.is_empty(),
            "the diagnostics that stopped the link are reported"
        );
        assert!(
            out.note
                .as_deref()
                .is_some_and(|n| n.contains("did not link")),
            "note: {:?}",
            out.note
        );
        assert!(out.found, "the fallback still answers");
        let root_node = &out.traces[0];
        assert!(
            root_node.unverified,
            "every node of a fallback walk is marked"
        );
        let child = &root_node.children[0];
        assert!(
            !child.external,
            "an intra-project function must never be reported external: {child:?}"
        );
        assert_eq!(
            child.id.kind.as_str(),
            "unresolved",
            "it degrades to `unresolved` instead: {child:?}"
        );
        assert!(child.unverified);
    }

    #[test]
    fn a_near_miss_names_the_declaration_it_almost_matched() {
        let out = trace(&prep(), Some("valid"), None);
        assert!(!out.found);
        let note = out.note.unwrap();
        assert!(note.contains("did you mean validate?"), "{note}");
    }

    #[test]
    fn a_function_reached_twice_is_expanded_once() {
        let src = "fn leaf(): int { return 1 }\n\
                   fn deep(): int { return leaf() }\n\
                   fn a(): int { return deep() }\n\
                   fn b(): int { return deep() }\n\
                   fn entry(): int { return a() + b() }\n\
                   echo entry()\n";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = trace(&p, Some("entry"), None);
        assert!(out.found);
        let root = &out.traces[0];
        let under_a = &root.children[0].children[0];
        assert_eq!(under_a.id.name, "deep");
        assert_eq!(under_a.children.len(), 1, "expanded on first arrival");
        let under_b = &root.children[1].children[0];
        assert_eq!(under_b.id.name, "deep");
        assert!(under_b.shared && under_b.children.is_empty());
    }

    #[test]
    fn recursion_is_marked_as_a_cycle_not_expanded_forever() {
        let src = "fn ping(n: int): int { return pong(n) }\nfn pong(n: int): int { return ping(n) }\necho ping(1)\n";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = trace(&p, Some("ping"), None);
        assert!(out.found);
        let pong = &out.traces[0].children[0];
        assert_eq!(pong.id.name, "pong");
        let back = &pong.children[0];
        assert_eq!(back.id.name, "ping");
        assert!(back.cycle, "the back-edge is marked, not expanded");
        assert!(back.children.is_empty());
    }
}
