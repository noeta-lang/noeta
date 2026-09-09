//! `context_map` — the declarations worth reading about a set of seeds, as much of them as a token
//! budget holds.
//!
//! An agent that has found one declaration needs the handful around it, not the file it sits in and
//! not the whole project. This ranks the call graph and the `use` import relation from the seeds
//! with personalized PageRank, then grows a connected subgraph outward until the budget is full,
//! emitting each declaration's signature grouped by file with the edge that pulled it in.
//!
//! Seeds are addressed the way every graph tool is addressed: a declaration name, a file or module
//! path, or a `@role`, which seeds every declaration bearing it.
//!
//! The ranking lives in [`noeta_ide::rank`], so the editor and the agent read one graph. This
//! module owns the wire shapes and the plumbing every graph tool shares: the linked program, the
//! call graph, the declaration inventory, and the node identity a tool's answer joins on.

use noeta_ast::Program;
use noeta_ide::callgraph::CallGraph;
use noeta_ide::rank::{self as engine, EdgeKind, ModuleId, Ranker};
use rmcp::schemars;
use serde::Serialize;

use crate::analyze::{self, LinkStatus, NodeId, NodeKind, Prepared};
use crate::graph::DeclIndex;

/// How many decimal places a score carries on the wire. A rank is a ratio, and six places is more
/// than enough to order one, while pinning the digits keeps two runs byte-identical.
const SCORE_PLACES: f64 = 1e6;

/// The `context_map` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ContextMapOutput {
    /// True when at least one seed resolved.
    pub found: bool,
    /// Which ranking chose the map: `ppr`, `degree` or `random`.
    pub ranker: String,
    pub budget_tokens: usize,
    /// What the emitted signatures cost, in the same unit.
    pub used_tokens: usize,
    /// True when a connected declaration did not fit.
    pub truncated: bool,
    /// The declarations and modules the seeds resolved to.
    pub seeds: Vec<NodeId>,
    /// Seed specs that named nothing here.
    pub missing_seeds: Vec<String>,
    /// Every declaration an ambiguous seed leaf matched.
    pub candidates: Vec<NodeId>,
    /// The map, grouped by the file each declaration is written in.
    pub files: Vec<FileGroup>,
    /// Whether the map was ranked over the merged workspace program.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// One file's share of the map.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FileGroup {
    /// The file, relative to the project root.
    pub file: String,
    /// The module path the file's location derives.
    pub module: String,
    /// The declarations from this file, strongest first.
    pub nodes: Vec<MapNode>,
}

/// One declaration in the map.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct MapNode {
    /// The declaration's identity — the same `id` `symbols`, `trace`, `impact` and `callers`
    /// report for it.
    pub id: NodeId,
    pub name: String,
    /// 1-based position in the whole map, strongest first.
    pub rank: usize,
    /// The ranking's score for this declaration.
    pub score: f64,
    /// The declaration's signature, up to its body.
    pub signature: String,
    /// What the signature cost against the budget.
    pub tokens: usize,
    pub line: Option<u32>,
    /// The `@role` bindings the declaration bears.
    pub roles: Vec<String>,
    /// The edge that pulled this declaration into the map. Absent on a seed.
    pub via: Option<Via>,
    /// This node comes from an unlinked program, so its qualification and its edges are unproven.
    pub unverified: bool,
}

/// How a declaration entered the map.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Via {
    /// The declaration or module already in the map that reaches this one.
    pub from: String,
    /// `call`, `reference` or `import`.
    pub kind: String,
}

/// Answer `context_map`.
pub fn context_map(
    p: &Prepared,
    seeds: &[String],
    budget_tokens: Option<usize>,
    edge_kinds: Option<&[String]>,
    ranker: Option<&str>,
    seed: Option<u64>,
) -> ContextMapOutput {
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

    let mut notes: Vec<String> = status.note().into_iter().collect();
    let ranker = match parse_ranker(ranker) {
        Ok(ranker) => ranker,
        Err(message) => {
            notes.push(message);
            Ranker::default()
        }
    };
    let (kinds, kind_note) = parse_kinds(edge_kinds);
    notes.extend(kind_note);
    if seeds.is_empty() {
        notes
            .push("`seeds` needs at least one declaration, file or role to start from".to_string());
    }

    let rank_graph = engine::build(&graph, program, &module_ids(p));
    let map = engine::context_map(
        &rank_graph,
        &graph,
        &roles,
        &texts,
        &engine::Request {
            seeds,
            budget_tokens: budget_tokens.unwrap_or(engine::DEFAULT_BUDGET_TOKENS),
            edge_kinds: kinds,
            ranker,
            seed: seed.unwrap_or(0),
        },
    );

    for spec in &map.missing {
        notes.push(format!(
            "`{spec}` matches no declaration, file or role — try `symbols` for the declarations"
        ));
    }
    for (spec, candidates) in &map.ambiguous {
        notes.push(format!(
            "`{spec}` names {} declarations — pass one of: {}",
            candidates.len(),
            candidates.join(", ")
        ));
    }

    // Grouped by file, each group in the map's own order, the groups themselves ordered by their
    // strongest declaration so the file a reader wants first is first.
    let mut files: Vec<FileGroup> = Vec::new();
    for entry in &map.nodes {
        let node = &rank_graph.nodes[entry.node];
        let file = p
            .file_name(node.source)
            .unwrap_or_else(|| node.file.clone());
        let wire = MapNode {
            id: node_id(p, &decls, node),
            name: node.name.clone(),
            rank: entry.rank,
            score: round(entry.score),
            signature: entry.signature.clone(),
            tokens: entry.tokens,
            line: node
                .decl_span
                .and_then(|span| analyze::locate_span(p, span))
                .map(|(_, loc)| loc.start.line),
            roles: roles.get(&node.name).cloned().unwrap_or_default(),
            via: entry.via.map(|via| Via {
                from: rank_graph.nodes[via.from].name.clone(),
                kind: via.kind.to_string(),
            }),
            unverified: !status.linked,
        };
        match files.iter_mut().find(|g| g.file == file) {
            Some(group) => group.nodes.push(wire),
            None => files.push(FileGroup {
                module: p
                    .modules
                    .get(node.source)
                    .map(|m| m.namespace.clone())
                    .unwrap_or_default(),
                file,
                nodes: vec![wire],
            }),
        }
    }

    ContextMapOutput {
        found: !map.seeds.is_empty(),
        ranker: map.ranker.to_string(),
        budget_tokens: map.budget_tokens,
        used_tokens: map.used_tokens,
        truncated: map.truncated,
        seeds: map
            .seeds
            .iter()
            .map(|&i| node_id(p, &decls, &rank_graph.nodes[i]))
            .collect(),
        missing_seeds: map.missing.clone(),
        candidates: map
            .ambiguous
            .iter()
            .flat_map(|(_, candidates)| candidates.iter())
            .filter_map(|name| match decls.lookup(name) {
                crate::graph::Lookup::Found(decl) => Some(decl.id(p)),
                _ => None,
            })
            .collect(),
        files,
        linked: status.linked,
        link_diagnostics: status.link_diagnostics.clone(),
        note: join(notes),
    }
}

/// The rank graph's per-source identity: the module path the linker derived and the file name every
/// other tool reports.
pub(crate) fn module_ids(p: &Prepared) -> Vec<ModuleId> {
    (0..p.sources.len())
        .map(|i| ModuleId {
            name: p
                .modules
                .get(i)
                .map(|m| m.namespace.clone())
                .unwrap_or_default(),
            file: p.file_name(i).unwrap_or_default(),
        })
        .collect()
}

/// A rank-graph node's identity, taken from the declaration inventory where it has one so the kind
/// is exact. A module node is the file's own identity, the shape `module_graph` reports.
fn node_id(p: &Prepared, decls: &DeclIndex, node: &engine::Node) -> NodeId {
    match node.decl_span.and_then(|span| decls.at_name_span(span)) {
        Some(decl) => decl.id(p),
        None => match node.decl_span {
            Some(span) => p.node_id(&node.name, kind_of(node.sort), span),
            None => {
                let len = p.sources.get(node.source).map_or(0, |s| s.text().len()) as u32;
                p.node_id(
                    &node.name,
                    NodeKind::Module,
                    noeta_span::Span::new_in(noeta_span::SourceId(node.source as u32), 0, len),
                )
            }
        },
    }
}

fn kind_of(sort: engine::NodeSort) -> NodeKind {
    match sort {
        engine::NodeSort::Function => NodeKind::Function,
        engine::NodeSort::Method => NodeKind::Method,
        engine::NodeSort::Module => NodeKind::Module,
    }
}

/// A declaration's identity from its call-graph node — the join every graph tool makes.
pub(crate) fn function_id(
    p: &Prepared,
    decls: &DeclIndex,
    graph: &CallGraph,
    function: usize,
) -> NodeId {
    let node = &graph.functions[function];
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

/// Parse the `ranker` argument, naming what is on offer when it is not one of them.
fn parse_ranker(ranker: Option<&str>) -> Result<Ranker, String> {
    match ranker {
        None => Ok(Ranker::default()),
        Some(name) => name.parse::<Ranker>().map_err(|e| e.to_string()),
    }
}

/// Parse the `edge_kinds` filter, reporting each spelling that is not a kind and keeping the rest.
pub(crate) fn parse_kinds(kinds: Option<&[String]>) -> (Vec<EdgeKind>, Option<String>) {
    let Some(kinds) = kinds else {
        return (Vec::new(), None);
    };
    let mut parsed = Vec::new();
    let mut bad = Vec::new();
    for kind in kinds {
        match kind.parse::<EdgeKind>() {
            Ok(kind) => parsed.push(kind),
            Err(message) => bad.push(message),
        }
    }
    (parsed, join(bad))
}

/// One note out of several, or none.
pub(crate) fn join(notes: Vec<String>) -> Option<String> {
    if notes.is_empty() {
        None
    } else {
        Some(notes.join("; "))
    }
}

fn round(score: f64) -> f64 {
    (score * SCORE_PLACES).round() / SCORE_PLACES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    /// A leaf with a chain of its own, and a hub with callees that have nothing to do with it.
    const SRC: &str = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

fn deep(): int { return 1 }
fn helper(): int { return deep() }

#[Route(\"/leaf\")]
fn leaf(): int { return helper() }

fn far_one(): int { return 1 }
fn far_two(): int { return 1 }
fn far_three(): int { return 1 }
fn hub(): int { return far_one() + far_two() + far_three() }
fn caller_a(): int { return hub() }
fn caller_b(): int { return hub() }
echo leaf() + caller_a() + caller_b()
";

    fn prep() -> Prepared {
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    fn names(out: &ContextMapOutput) -> Vec<String> {
        let mut nodes: Vec<&MapNode> = out.files.iter().flat_map(|f| f.nodes.iter()).collect();
        nodes.sort_by_key(|n| n.rank);
        nodes.iter().map(|n| n.name.clone()).collect()
    }

    #[test]
    fn a_seeded_map_leads_with_the_seed_and_carries_its_neighborhood() {
        let out = context_map(&prep(), &["leaf".to_string()], None, None, None, None);
        assert!(out.found, "note: {:?}", out.note);
        assert_eq!(out.ranker, "ppr");
        let order = names(&out);
        assert_eq!(order[0], "leaf");
        let at = |name: &str| order.iter().position(|n| n == name).unwrap_or(usize::MAX);
        assert!(at("helper") < at("hub"), "{order:?}");
        assert!(at("deep") < at("far_one"), "{order:?}");
        // Every node carries an identity, and it is the one the other graph tools report.
        let first = &out.files[0].nodes[0];
        assert_eq!(first.id.name, "leaf");
        assert_eq!(first.id.kind.as_str(), "function");
        assert!(first.id.span.is_some() && first.line.is_some());
        assert!(first.via.is_none(), "the seed has no route into the map");
        assert_eq!(first.roles, vec!["Semantic.EntryPoint"]);
        // And the one after it names the edge that pulled it in.
        let helper = out.files[0]
            .nodes
            .iter()
            .find(|n| n.name == "helper")
            .expect("helper is in the map");
        let via = helper.via.as_ref().expect("a non-seed carries its route");
        assert_eq!((via.from.as_str(), via.kind.as_str()), ("leaf", "call"));
        assert!(out.linked && out.link_diagnostics.is_empty());
        assert!(!first.unverified);
    }

    /// The ablation knob is real: `degree` takes the most-connected declaration where `ppr` takes
    /// the seed's own neighbor, and `random` is reproducible from its seed.
    /// The identity is joinable: the id a map node carries for a declaration is byte for byte the
    /// id `callers` reports for the same declaration, over a real package where names are
    /// namespace-qualified. Nothing in the map is worth much if an agent cannot join it.
    #[test]
    fn a_map_node_and_callers_report_one_identity() {
        noeta_stdlib::registry::default_seeded();
        let root = noeta_test_temp::TempDir::new("mcp-context-identity");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("noeta.toml"),
            "[package]\nname = \"local/mapped\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("store.noe"),
            "pub fn load(): int { return 7 }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src").join("main.noe"),
            "use mapped.store\nfn entry(): int { return store.load() }\necho entry()\n",
        )
        .unwrap();
        let entry = root.join("src").join("main.noe").display().to_string();

        let out = context_map(
            &prepare(&None, &Some(entry.clone())).unwrap(),
            &["entry".to_string()],
            None,
            None,
            None,
            None,
        );
        assert!(out.linked, "note: {:?}", out.note);
        let load = out
            .files
            .iter()
            .flat_map(|f| f.nodes.iter())
            .find(|n| n.name.ends_with(".load"))
            .expect("the map reaches the imported module's declaration");
        assert_eq!(load.name, "mapped.store.load", "the post-link name");

        let walked = crate::impact::callers(
            &prepare(&None, &Some(entry)).unwrap(),
            "mapped.store.load",
            None,
        );
        assert_eq!(
            load.id,
            walked.target.expect("callers found it"),
            "one declaration, one identity"
        );
    }

    #[test]
    fn the_ranker_argument_changes_the_ranking() {
        let by_degree = context_map(
            &prep(),
            &["leaf".to_string()],
            None,
            None,
            Some("degree"),
            None,
        );
        assert_eq!(by_degree.ranker, "degree");
        assert_eq!(names(&by_degree)[1], "hub", "{:?}", names(&by_degree));

        let first = context_map(
            &prep(),
            &["leaf".to_string()],
            None,
            None,
            Some("random"),
            Some(7),
        );
        let again = context_map(
            &prep(),
            &["leaf".to_string()],
            None,
            None,
            Some("random"),
            Some(7),
        );
        assert_eq!(names(&first), names(&again), "a seeded run repeats");
        assert_eq!(first.ranker, "random");

        // An unknown ranker falls back to the default and says so rather than failing the call.
        let bad = context_map(
            &prep(),
            &["leaf".to_string()],
            None,
            None,
            Some("nonsense"),
            None,
        );
        assert_eq!(bad.ranker, "ppr");
        assert!(
            bad.note.as_deref().is_some_and(|n| n.contains("nonsense")),
            "note: {:?}",
            bad.note
        );
    }

    #[test]
    fn the_budget_bounds_the_answer_and_says_when_it_bit() {
        let full = context_map(&prep(), &["leaf".to_string()], Some(4096), None, None, None);
        assert!(!full.truncated && full.used_tokens <= 4096);
        let small = context_map(
            &prep(),
            &["leaf".to_string()],
            Some(full.used_tokens / 3),
            None,
            None,
            None,
        );
        assert!(small.truncated, "the budget cut the map");
        assert!(small.used_tokens <= full.used_tokens / 3);
        assert!(names(&small).len() < names(&full).len());
        assert_eq!(names(&small)[0], "leaf");
    }

    #[test]
    fn a_role_seeds_every_bearer_and_an_unknown_seed_is_named() {
        let by_role = context_map(&prep(), &["EntryPoint".to_string()], None, None, None, None);
        assert!(by_role.found);
        assert_eq!(
            by_role
                .seeds
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["leaf"]
        );

        let unknown = context_map(&prep(), &["ghost".to_string()], None, None, None, None);
        assert!(!unknown.found);
        assert!(unknown.files.is_empty());
        assert_eq!(unknown.missing_seeds, vec!["ghost".to_string()]);
        assert!(
            unknown.note.as_deref().is_some_and(|n| n.contains("ghost")),
            "note: {:?}",
            unknown.note
        );
    }
}
