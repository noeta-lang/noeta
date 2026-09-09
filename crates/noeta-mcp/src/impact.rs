//! `impact` and `callers` — the two tools that walk the call graph **backwards**, answering
//! "what breaks if I change this?".
//!
//! `impact` is the reverse transitive closure the watch loop already drives
//! ([`noeta_ide::impact::ImpactSession`]): every declaration whose behavior may change, in the
//! linked program's vocabulary, plus the `@test`/`@bench` functions among them, or one honest
//! `all` with the reason attribution failed. `callers` is the shallow form of the same walk over
//! [`noeta_ide::callgraph`]: one level at a time, each caller with the site it calls from and
//! whether it calls or references.
//!
//! Both address a declaration the way every other graph tool names it, and both emit the shared
//! [`NodeId`], so an impact answer joins a `symbols` outline and a `trace` walk on `id`.

use noeta_ide::callgraph::{self, Callee};
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{self, LinkStatus, Loc, NodeId, NodeKind, Prepared};
use crate::graph::{DeclIndex, Lookup};

/// Levels deeper than this are cut, whatever the caller asks for.
pub const MAX_CALLER_DEPTH: usize = 16;
pub const DEFAULT_CALLER_DEPTH: usize = 3;

// ---- impact -------------------------------------------------------------------------------

/// The `impact` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ImpactOutput {
    /// True when the answer is attributed to declarations; false when everything is impacted.
    pub attributed: bool,
    /// The declarations whose behavior may change — the seed plus the reverse closure of its
    /// callers and referencers. Empty under an inert edit.
    pub decls: Vec<NodeId>,
    /// The subset of `decls` a tier runner executes, each with the tier it was declared in.
    pub tier_functions: Vec<TierFunction>,
    /// Why a runner must still rerun everything, when it must: a module's top-level statements
    /// use one of the impacted declarations, and every run executes those. `decls` is the walk
    /// regardless, so the reachability question is answered even when the runner's is not.
    pub reason: Option<String>,
    /// Every declaration an ambiguous `symbol` leaf matched.
    pub candidates: Vec<NodeId>,
    pub note: Option<String>,
}

/// One impacted `@test`/`@bench` function.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TierFunction {
    pub id: NodeId,
    /// The tier block it was declared in (`test`, `bench`).
    pub tier: String,
}

/// Answer `impact`. `symbol` asks what changing a named declaration reaches; `edit` asks what one
/// file's new text reaches, diffed against what is on disk. Exactly one is required.
pub fn impact(
    p: &Prepared,
    entry: Option<&str>,
    symbol: Option<&str>,
    edit_file: Option<&str>,
    edit_source: Option<&str>,
) -> ImpactOutput {
    let decls = decl_index(p);
    let Some(entry) = entry else {
        return note("`impact` needs a `file` — the project this edit belongs to".to_string());
    };
    let Some(mut session) = noeta_ide::impact::ImpactSession::new(std::path::Path::new(entry))
    else {
        return note(format!(
            "{} does not anchor a project the impact engine can analyze",
            p.relative(entry)
        ));
    };

    let answer = match (symbol, edit_file, edit_source) {
        (Some(name), None, None) => match decls.lookup(name) {
            Lookup::Found(decl) => session.reach_of_decls(std::slice::from_ref(&decl.name)),
            Lookup::Ambiguous(all) => {
                return ImpactOutput {
                    attributed: false,
                    decls: Vec::new(),
                    tier_functions: Vec::new(),
                    reason: None,
                    candidates: all.iter().map(|d| d.id(p)).collect(),
                    note: Some(format!(
                        "`{name}` names {} declarations in this workspace — pass one of the \
                         candidate names",
                        all.len()
                    )),
                };
            }
            Lookup::Missing => {
                let sources = crate::graph::source_index(p);
                return note(
                    crate::graph::outside_graph_note(p, &sources, name).unwrap_or_else(|| {
                        format!("no declaration named `{name}` in this workspace")
                    }),
                );
            }
        },
        (None, Some(file), Some(source)) => {
            let path = std::path::Path::new(file);
            let canonical = match path.canonicalize() {
                Ok(canonical) => canonical,
                Err(e) => return note(format!("cannot open {}: {e}", p.relative(file))),
            };
            session.reach_of_sources(&[(canonical, source.to_string())])
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            return note("`edit` needs both `file` and `new_source`".to_string());
        }
        _ => {
            return note(
                "provide `symbol` (a declaration name) or `edit` (a file and its new source)"
                    .to_string(),
            );
        }
    };

    let reach = match answer {
        Ok(reach) => reach,
        // A project-shaped valve — unreadable members, a project that does not link or check.
        // There is no walk to report, only the reason.
        Err(reason) => {
            return ImpactOutput {
                attributed: false,
                decls: Vec::new(),
                tier_functions: Vec::new(),
                reason: Some(reason),
                candidates: Vec::new(),
                note: None,
            };
        }
    };

    let mut ids = Vec::with_capacity(reach.decls.len());
    let mut tier_functions = Vec::new();
    for name in &reach.decls {
        match decls.lookup(name) {
            Lookup::Found(decl) => {
                if let Some(tier) = &decl.tier {
                    tier_functions.push(TierFunction {
                        id: decl.id(p),
                        tier: tier.clone(),
                    });
                }
                ids.push(decl.id(p));
            }
            // A closure name the graph carries that no declaration owns still belongs in the
            // answer: dropping it would make the list read as complete while it is not.
            _ => ids.push(p.unlocated_id(name, NodeKind::Function)),
        }
    }
    // A module whose top-level statements use an impacted declaration is itself impacted — every
    // run executes them. It joins the answer as that module's node, the way `callers` reports the
    // same edge, rather than aborting the walk that found it.
    for (source, _) in &reach.top_level_uses {
        let id = top_level_id(p, *source);
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let reason = reach.top_level_uses.first().map(|(source, used)| {
        format!(
            "the top level of `{}` uses `{used}`, and every run executes it — a runner cannot \
             narrow past that, though the declarations below are still what changing it reaches",
            top_level_id(p, *source).name
        )
    });
    ImpactOutput {
        attributed: reason.is_none(),
        decls: ids,
        tier_functions,
        reason,
        candidates: Vec::new(),
        note: None,
    }
}

fn note(note: String) -> ImpactOutput {
    ImpactOutput {
        attributed: false,
        decls: Vec::new(),
        tier_functions: Vec::new(),
        reason: None,
        candidates: Vec::new(),
        note: Some(note),
    }
}

/// The declaration inventory over the linked program, or the entry's own parse when it does not
/// link — how a `symbol` becomes a post-link name the impact engine speaks.
fn decl_index(p: &Prepared) -> DeclIndex {
    let linked = noeta_db::linked(&p.db, p.ws);
    match &linked.program {
        Ok(program) => DeclIndex::build(program),
        Err(_) => DeclIndex::build(&noeta_db::ast(&p.db, analyze::entry_program(p)).0.program),
    }
}

// ---- callers ------------------------------------------------------------------------------

/// The `callers` result: the reverse walk, one level per hop away from the target.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CallersOutput {
    pub found: bool,
    /// The declaration the walk started from.
    pub target: Option<NodeId>,
    /// One entry per hop: `depth: 1` holds the direct callers, `2` their callers, and so on.
    pub levels: Vec<CallerLevel>,
    /// Every declaration an ambiguous `symbol` leaf matched.
    pub candidates: Vec<NodeId>,
    /// True when `depth` cut the walk short.
    pub truncated: bool,
    /// Whether the walk ran over the merged workspace program.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// One hop of the reverse walk.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CallerLevel {
    /// Hops away from the target: 1 is a direct caller.
    pub depth: usize,
    pub callers: Vec<CallerEdge>,
}

/// One reverse edge: who uses what, where, and how.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CallerEdge {
    /// The using declaration. A use written in a module's top-level statements reports the module.
    pub id: NodeId,
    /// The node it uses at this site — the target, or something the previous level named.
    pub calls: NodeId,
    /// `call` when the use is syntactically `f(...)`, `reference` when the function is passed as
    /// a value (a callback or a handler registration, part of the flow all the same).
    pub kind: EdgeKind,
    /// Where the use sits, in the using declaration's file.
    pub site: Option<Loc>,
    /// The file the use sits in, relative to the project root.
    pub file: Option<String>,
}

/// How a use reaches its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Call,
    Reference,
}

/// Answer `callers`: walk the call graph backwards from `symbol`, `depth` levels at most.
pub fn callers(p: &Prepared, symbol: &str, depth: Option<usize>) -> CallersOutput {
    let status = LinkStatus::of(p);
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program = match &linked.program {
        Ok(program) => program,
        Err(_) => &entry.0.program,
    };
    let decls = DeclIndex::build(program);
    let target = match decls.lookup(symbol) {
        Lookup::Found(decl) => decl,
        Lookup::Ambiguous(all) => {
            return CallersOutput {
                found: false,
                target: None,
                levels: Vec::new(),
                candidates: all.iter().map(|d| d.id(p)).collect(),
                truncated: false,
                linked: status.linked,
                link_diagnostics: status.link_diagnostics,
                note: Some(format!(
                    "`{symbol}` names {} declarations in this workspace — pass one of the \
                     candidate names",
                    all.len()
                )),
            };
        }
        Lookup::Missing => {
            // A name the graph does not hold may still be declared: the graph is the program the
            // entry links, so a module's `@test` block — referenced by nothing — is outlined by
            // `symbols` and absent here. Say which of the two it is.
            let sources = crate::graph::source_index(p);
            let note = crate::graph::outside_graph_note(p, &sources, symbol).unwrap_or_else(|| {
                format!(
                    "no declaration named `{symbol}` in this workspace — try `symbols` for the \
                     declarations"
                )
            });
            return CallersOutput {
                found: false,
                target: None,
                levels: Vec::new(),
                candidates: Vec::new(),
                truncated: false,
                linked: status.linked,
                link_diagnostics: status.link_diagnostics,
                note: Some(note),
            };
        }
    };

    let checked = noeta_db::linked_checked_ide(&p.db, p.ws);
    let texts: Vec<&str> = p.sources.iter().map(|s| s.text()).collect();
    let graph = callgraph::build(program, &checked.expr_types, &checked.sites, &texts);
    let depth = depth.unwrap_or(DEFAULT_CALLER_DEPTH).min(MAX_CALLER_DEPTH);

    // A frontier of graph node names, walked one hop at a time. Nodes already reported are not
    // expanded again, so a diamond reports each caller once rather than once per path.
    let mut frontier: std::collections::BTreeSet<String> =
        [target.name.clone()].into_iter().collect();
    let mut seen: std::collections::BTreeSet<String> = frontier.iter().cloned().collect();
    let mut levels = Vec::new();
    let mut truncated = false;
    for hop in 1..=depth {
        let mut edges = Vec::new();
        let mut next: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for edge in &graph.edges {
            let Some(used) = callee_name(&graph, &edge.callee) else {
                continue;
            };
            if !frontier.contains(used.as_str()) {
                continue;
            }
            let calls = match &edge.callee {
                Callee::Function(i) => id_of(p, &decls, &graph.functions[*i]),
                Callee::External(name) => p.unlocated_id(name, NodeKind::External),
                Callee::Dynamic(name) => p.unlocated_id(name, NodeKind::Dynamic),
            };
            let id = match edge.caller {
                Some(i) => {
                    let node = &graph.functions[i];
                    if seen.insert(node.name.clone()) {
                        next.insert(node.name.clone());
                    }
                    id_of(p, &decls, node)
                }
                None => top_level_id(p, edge.site.source),
            };
            let at = analyze::locate_span(p, edge.site);
            edges.push(CallerEdge {
                id,
                calls,
                kind: if edge.call {
                    EdgeKind::Call
                } else {
                    EdgeKind::Reference
                },
                site: at.map(|(_, loc)| loc.start),
                file: p.file_name(edge.site.source.0 as usize),
            });
        }
        if edges.is_empty() {
            break;
        }
        levels.push(CallerLevel {
            depth: hop,
            callers: edges,
        });
        if next.is_empty() {
            break;
        }
        if hop == depth {
            truncated = true;
        }
        frontier = next;
    }

    CallersOutput {
        found: true,
        target: Some(target.id(p)),
        levels,
        candidates: Vec::new(),
        truncated,
        linked: status.linked,
        note: status.note(),
        link_diagnostics: status.link_diagnostics,
    }
}

/// The graph name an edge points at, for matching against the frontier. External and dynamic
/// targets are named too, so `callers` answers for a stdlib call as readily as a local one.
fn callee_name(graph: &callgraph::CallGraph, callee: &Callee) -> Option<String> {
    match callee {
        Callee::Function(i) => graph.functions.get(*i).map(|f| f.name.clone()),
        Callee::External(name) | Callee::Dynamic(name) => Some(name.clone()),
    }
}

/// A graph function node's identity, taken from the declaration inventory where it has one so the
/// kind is exact.
fn id_of(p: &Prepared, decls: &DeclIndex, node: &callgraph::FnNode) -> NodeId {
    match decls.at_name_span(node.name_span) {
        Some(decl) => decl.id(p),
        None => p.node_id(
            &node.name,
            if node.method {
                NodeKind::Method
            } else {
                NodeKind::Function
            },
            node.name_span,
        ),
    }
}

/// The identity of a module's **top-level statements** — the caller of a use written outside any
/// declaration. Every run executes them, which is why the impact engine widens to a full rerun
/// when one of them uses a changed declaration.
fn top_level_id(p: &Prepared, source: noeta_span::SourceId) -> NodeId {
    let index = source.0 as usize;
    let namespace = p
        .modules
        .get(index)
        .map(|m| m.namespace.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| p.file_name(index))
        .unwrap_or_else(|| "<top level>".to_string());
    let len = p.sources.get(index).map_or(0, |s| s.text().len()) as u32;
    p.node_id(
        &namespace,
        NodeKind::Module,
        noeta_span::Span::new_in(source, 0, len),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    /// A three-module project: `store.noe` holds the leaf, `service.noe` calls it, `main.noe`
    /// calls the service and declares a `@test` that reaches the leaf through both. `other.noe`
    /// is the control — nothing in it touches the chain.
    fn project(name: &str) -> noeta_test_temp::TempPath {
        let root = noeta_test_temp::TempDir::new(&format!("mcp-{name}"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/chain\"\nversion = \"0.1.0\"\n\n[directives]\ntest = \"std\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("store.noe"),
            "pub fn load(): int { return 7 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("service.noe"),
            "use chain.store\npub fn fetch(): int { return store.load() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("other.noe"),
            "pub fn unrelated(): int { return 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            // `echo boot()` is the top-level statement: a top-level use of an IMPACTED
            // declaration is the impact engine's setup valve (every run executes it, so the run
            // may differ wholesale), and `boot` is deliberately outside the chain so the closure
            // stays attributed. It also gives the top-level-caller case something to find.
            "use chain.service\n\
             fn handle(): int { return service.fetch() }\n\
             fn boot(): int { return 0 }\n\
             echo boot()\n\
             @test {\n  fn handles(): void { assert(handle() == 7) }\n}\n",
        )
        .unwrap();
        root.into_child("src/main.noe")
    }

    fn prep(entry: &str) -> Prepared {
        noeta_stdlib::registry::default_seeded();
        prepare(&None, &Some(entry.to_string())).expect("prepare")
    }

    /// **D4.** Changing the leaf impacts exactly the two declarations that reach it and the one
    /// `@test` that exercises them. Nothing walked an edge backwards before this tool existed.
    #[test]
    fn changing_a_leaf_impacts_the_declarations_that_reach_it() {
        let entry = project("impact_chain");
        let file = entry.display().to_string();
        let p = prep(&file);
        let out = impact(&p, Some(&file), Some("chain.store.load"), None, None);
        assert!(
            out.attributed,
            "reason: {:?} note: {:?}",
            out.reason, out.note
        );
        let names: std::collections::BTreeSet<&str> =
            out.decls.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains("chain.service.fetch"), "{names:?}");
        assert!(names.contains("chain.main.handle"), "{names:?}");
        // The `@test` fn carries its source's module prefix like every other declaration, so the
        // name an impact answer reports is the one `symbols` and `trace` report for it.
        assert!(
            names.contains("chain.main.handles"),
            "the `@test` is impacted: {names:?}"
        );
        assert!(
            !names.contains("chain.other.unrelated"),
            "an unrelated declaration is not impacted: {names:?}"
        );
        // The tier functions are called out, so a runner filter needs no second pass.
        let tiers: Vec<(&str, &str)> = out
            .tier_functions
            .iter()
            .map(|t| (t.id.name.as_str(), t.tier.as_str()))
            .collect();
        assert_eq!(tiers, vec![("chain.main.handles", "test")]);
        // The ids join with the outline.
        let outlined = crate::understand::symbols(&p, crate::understand::SymbolScope::Workspace);
        let fetch = out
            .decls
            .iter()
            .find(|d| d.name == "chain.service.fetch")
            .unwrap();
        assert!(
            outlined.symbols.iter().any(|s| &s.id == fetch),
            "the impact id joins the workspace outline"
        );
        // And so does the `@test` fn, which is the case one vocabulary is easiest to lose: the
        // linker qualifies the top level, tier activation hoists a tier declaration into it, and
        // naming it bare there would leave `impact` reporting a node `symbols` never mentions.
        let handles = out
            .decls
            .iter()
            .find(|d| d.name == "chain.main.handles")
            .expect("the impacted test");
        let outlined_test = outlined
            .symbols
            .iter()
            .find(|s| &s.id == handles)
            .expect("the impact id joins the outline's `@test` node");
        assert_eq!(outlined_test.tier.as_deref(), Some("test"));
    }

    /// **The top-level use keeps its closure.** A module's top-level statements run on every
    /// pass, so a *runner* cannot narrow past one that uses an impacted declaration — and the walk
    /// used to be abandoned at that edge, so `impact` answered `decls: []` for every symbol in
    /// every real project. The verdict stays; the answer arrives with it.
    #[test]
    fn a_top_level_use_reports_the_closure_it_widens_over() {
        let root = noeta_test_temp::TempDir::new("mcp-impact-toplevel");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/top\"\nversion = \"0.1.0\"\n\n[directives]\ntest = \"std\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("store.noe"),
            "pub fn load(): int { return 7 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            "use top.store\n\
             fn handle(): int { return store.load() }\n\
             echo handle()\n\
             @test {\n  fn handles(): void { assert(handle() == 7) }\n}\n",
        )
        .unwrap();
        let entry = root.into_child("src/main.noe");
        let file = entry.display().to_string();
        let p = prep(&file);

        let out = impact(&p, Some(&file), Some("top.store.load"), None, None);
        assert!(
            !out.attributed,
            "a top-level use is still a full rerun for a runner"
        );
        let reason = out.reason.as_deref().unwrap_or_default();
        assert!(
            reason.contains("top.main") && reason.contains("top.main.handle"),
            "the reason names the module and what it used: {reason}"
        );

        let names: std::collections::BTreeSet<&str> =
            out.decls.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains("top.store.load"), "{names:?}");
        assert!(names.contains("top.main.handle"), "the caller: {names:?}");
        assert!(names.contains("top.main.handles"), "the `@test`: {names:?}");
        // The top-level use is a node of its own — the module, the way `callers` reports it —
        // rather than the point the walk gave up at.
        let module = out
            .decls
            .iter()
            .find(|d| d.kind == NodeKind::Module)
            .unwrap_or_else(|| panic!("the module node is missing: {names:?}"));
        assert_eq!(module.name, "top.main");
        assert_eq!(
            out.tier_functions
                .iter()
                .map(|t| (t.id.name.as_str(), t.tier.as_str()))
                .collect::<Vec<_>>(),
            vec![("top.main.handles", "test")]
        );
    }

    /// The negative: an edit that touches nothing the chain reaches impacts nothing.
    #[test]
    fn an_unrelated_edit_impacts_nothing() {
        let entry = project("impact_unrelated");
        let file = entry.display().to_string();
        let p = prep(&file);
        let other = std::path::Path::new(&file)
            .parent()
            .unwrap()
            .join("other.noe");
        let out = impact(
            &p,
            Some(&file),
            None,
            Some(&other.display().to_string()),
            Some("pub fn unrelated(): int { return 2 }\n"),
        );
        assert!(
            out.attributed,
            "reason: {:?} note: {:?}",
            out.reason, out.note
        );
        let names: std::collections::BTreeSet<&str> =
            out.decls.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["chain.other.unrelated"].into_iter().collect(),
            "only the edited declaration itself"
        );
        assert!(out.tier_functions.is_empty(), "no test reaches it");
    }

    /// `callers` reports the reverse walk one level at a time, with the site and the edge kind.
    #[test]
    fn callers_walks_back_level_by_level() {
        let entry = project("callers_chain");
        let file = entry.display().to_string();
        let p = prep(&file);
        let out = callers(&p, "chain.store.load", Some(4));
        assert!(out.found, "note: {:?}", out.note);
        assert!(out.linked);
        assert_eq!(out.target.expect("target").name, "chain.store.load");

        let level = |depth: usize| {
            out.levels
                .iter()
                .find(|l| l.depth == depth)
                .unwrap_or_else(|| panic!("no level {depth}: {:?}", out.levels))
        };
        let names = |depth: usize| -> std::collections::BTreeSet<&str> {
            level(depth)
                .callers
                .iter()
                .map(|c| c.id.name.as_str())
                .collect()
        };
        assert_eq!(
            names(1),
            ["chain.service.fetch"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(names(2).contains("chain.main.handle"), "{:?}", names(2));
        // Each edge says where and how.
        let direct = &level(1).callers[0];
        assert_eq!(direct.kind, EdgeKind::Call);
        assert!(direct.site.is_some(), "the call site is located");
        assert_eq!(direct.file.as_deref(), Some("src/service.noe"));
        assert_eq!(direct.calls.name, "chain.store.load");
    }

    /// A use written in a module's top-level statements is a caller too, reported as the module.
    #[test]
    fn a_top_level_use_is_reported_as_its_module() {
        let entry = project("callers_top_level");
        let p = prep(&entry.display().to_string());
        let out = callers(&p, "chain.main.boot", Some(1));
        assert!(out.found, "note: {:?}", out.note);
        let module = out.levels[0]
            .callers
            .iter()
            .find(|c| c.id.kind == NodeKind::Module)
            .unwrap_or_else(|| panic!("`echo boot()` is a top-level use: {:?}", out.levels));
        assert_eq!(module.id.name, "chain.main");
    }

    #[test]
    fn an_ambiguous_leaf_reports_every_candidate() {
        let root = noeta_test_temp::TempDir::new("mcp-callers-ambiguous");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/two\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("alpha.noe"),
            "pub fn shared(): int { return 1 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("beta.noe"),
            "pub fn shared(): int { return 2 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            "use two.alpha\nuse two.beta\necho alpha.shared() + beta.shared()\n",
        )
        .unwrap();
        let entry = root.into_child("src/main.noe");
        let p = prep(&entry.display().to_string());
        let out = callers(&p, "shared", None);
        assert!(!out.found);
        let names: std::collections::BTreeSet<&str> =
            out.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["two.alpha.shared", "two.beta.shared"]
                .into_iter()
                .collect()
        );
    }
}
