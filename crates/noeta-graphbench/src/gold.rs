//! Gold answers, computed from the compiler's own indices.
//!
//! The universe is every declaration the linked program holds: the call graph's function inventory
//! (top-level `fn`s, `Type.method`s, and the functions a `@test` block declares), plus the declared
//! types, plus one synthetic node for the program's top-level statements — the caller a call site
//! outside any declaration has. Each node carries the identity an arm has to reproduce: a qualified
//! name, the file, and the declaration name's byte span.
//!
//! Every structural answer here is a set (or a ranked list) of universe ids, derived by walking
//! those indices directly. There is no model, no heuristic and no sampling, so the answer is the
//! same on every run and on every machine.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use noeta_ast::{Program, Stmt};
use noeta_ide::callgraph::{CallGraph, Callee};
use noeta_span::Span;

use crate::corpus::Analysis;

/// What kind of declaration a universe node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Trait,
    TopLevel,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::Struct => "struct",
            NodeKind::Class => "class",
            NodeKind::Enum => "enum",
            NodeKind::Trait => "trait",
            NodeKind::TopLevel => "top-level",
        }
    }

    /// Whether this kind is a callable the graph can hold edges for.
    pub fn callable(self) -> bool {
        matches!(self, NodeKind::Function | NodeKind::Method)
    }
}

/// The synthetic node standing for the program's top-level statements.
pub const TOP_LEVEL: &str = "<top-level>";

/// One declaration, with the identity an answer is scored on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Node {
    pub id: usize,
    /// The name the graph knows it by — qualified for a linked top-level `fn`
    /// (`Shop.handlers.orders.place_order`), `Type.method` for a method.
    pub qualified: String,
    /// The last dotted segment, which is what a lexical search and `symbols` both speak.
    pub leaf: String,
    pub kind: NodeKind,
    pub file: String,
    pub module: String,
    /// The declaration name's byte span, the key the engine already has and no wire shape exposes.
    pub name_span: (u32, u32),
    /// The architectural roles this declaration bears, as `Enum.Variant`.
    pub roles: Vec<String>,
    /// Declared inside a `@test` block.
    pub is_test: bool,
    /// The call-graph index, for a callable.
    pub graph_index: Option<usize>,
}

/// The whole gold side of one project.
#[derive(Debug)]
pub struct Facts {
    pub nodes: Vec<Node>,
    /// Every `use` edge: `(importing file, imported module path)`.
    pub imports: Vec<(String, String)>,
    /// Every module path in the project, with the file that declares it.
    pub modules: Vec<(String, String)>,
    /// The call graph, kept for the walks the questions need.
    pub graph: CallGraph,
    /// `graph.functions` index to universe id.
    by_graph_index: HashMap<usize, usize>,
    /// Leaf name to every universe id carrying it — the collision index.
    by_leaf: HashMap<String, Vec<usize>>,
    roles_by_target: HashMap<String, Vec<String>>,
}

impl Facts {
    pub fn node(&self, id: usize) -> &Node {
        &self.nodes[id]
    }

    /// Every declaration answering to this leaf name.
    pub fn by_leaf(&self, leaf: &str) -> &[usize] {
        self.by_leaf.get(leaf).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Whether the leaf name names more than one declaration.
    pub fn collides(&self, leaf: &str) -> bool {
        self.by_leaf(leaf).len() > 1
    }

    pub fn universe_of(&self, graph_index: usize) -> Option<usize> {
        self.by_graph_index.get(&graph_index).copied()
    }

    /// The top-level node's universe id.
    pub fn top_level(&self) -> usize {
        self.nodes
            .iter()
            .position(|n| n.kind == NodeKind::TopLevel)
            .expect("the universe always carries the top-level node")
    }

    /// Who calls (or references) this node, as universe ids.
    pub fn callers(&self, id: usize) -> BTreeSet<usize> {
        let Some(target) = self.nodes[id].graph_index else {
            return BTreeSet::new();
        };
        let mut out = BTreeSet::new();
        for edge in &self.graph.edges {
            if edge.callee != Callee::Function(target) {
                continue;
            }
            match edge.caller {
                Some(caller) => {
                    if let Some(universe) = self.universe_of(caller) {
                        out.insert(universe);
                    }
                }
                None => {
                    out.insert(self.top_level());
                }
            }
        }
        out
    }

    /// Everything this node reaches within `depth` hops, as universe ids (the node itself excluded).
    pub fn callees(&self, id: usize, depth: usize) -> BTreeSet<usize> {
        let Some(start) = self.nodes[id].graph_index else {
            return BTreeSet::new();
        };
        let mut seen: HashSet<usize> = HashSet::from([start]);
        let mut queue = VecDeque::from([(start, 0usize)]);
        let mut out = BTreeSet::new();
        while let Some((at, hops)) = queue.pop_front() {
            if hops == depth {
                continue;
            }
            for edge in self.graph.edges_from(Some(at)) {
                if let Callee::Function(next) = edge.callee
                    && seen.insert(next)
                {
                    if let Some(universe) = self.universe_of(next) {
                        out.insert(universe);
                    }
                    queue.push_back((next, hops + 1));
                }
            }
        }
        out
    }

    /// The `external`/`dynamic` labels this node's walk terminates at — the leaves a correct answer
    /// names rather than guesses.
    pub fn labeled_leaves(&self, id: usize, depth: usize) -> BTreeSet<String> {
        let Some(start) = self.nodes[id].graph_index else {
            return BTreeSet::new();
        };
        let mut seen: HashSet<usize> = HashSet::from([start]);
        let mut queue = VecDeque::from([(start, 0usize)]);
        let mut out = BTreeSet::new();
        while let Some((at, hops)) = queue.pop_front() {
            if hops == depth {
                continue;
            }
            for edge in self.graph.edges_from(Some(at)) {
                match &edge.callee {
                    Callee::Function(next) => {
                        if seen.insert(*next) {
                            queue.push_back((*next, hops + 1));
                        }
                    }
                    Callee::External(name) | Callee::Dynamic(name) => {
                        out.insert(name.clone());
                    }
                }
            }
        }
        out
    }

    /// The shortest call path from `from` to `to`, as universe ids including both ends.
    pub fn shortest_path(&self, from: usize, to: usize) -> Option<Vec<usize>> {
        let (start, goal) = (self.nodes[from].graph_index?, self.nodes[to].graph_index?);
        let mut previous: HashMap<usize, usize> = HashMap::new();
        let mut seen: HashSet<usize> = HashSet::from([start]);
        let mut queue = VecDeque::from([start]);
        while let Some(at) = queue.pop_front() {
            if at == goal {
                let mut path = vec![at];
                let mut cursor = at;
                while let Some(before) = previous.get(&cursor) {
                    path.push(*before);
                    cursor = *before;
                }
                path.reverse();
                return path.into_iter().map(|i| self.universe_of(i)).collect();
            }
            for edge in self.graph.edges_from(Some(at)) {
                if let Callee::Function(next) = edge.callee
                    && seen.insert(next)
                {
                    previous.insert(next, at);
                    queue.push_back(next);
                }
            }
        }
        None
    }

    /// The `(target, role)` boundaries a trace from `id` crosses, as `Type.method`-style targets.
    pub fn boundaries(&self, id: usize, depth: usize) -> BTreeSet<String> {
        let Some(start) = self.nodes[id].graph_index else {
            return BTreeSet::new();
        };
        let walked = noeta_ide::trace::walk(
            &self.graph,
            &self.roles_by_target,
            &[start],
            depth,
            noeta_ide::trace::NODE_BUDGET,
        );
        walked.boundaries.iter().map(|b| b.target.clone()).collect()
    }

    /// Which `@test` functions must rerun when `id` changes: every test whose forward reach
    /// contains it. The reverse closure the watch-mode impact engine walks, restricted to the tests.
    pub fn impacted_tests(&self, id: usize) -> BTreeSet<usize> {
        let Some(target) = self.nodes[id].graph_index else {
            return BTreeSet::new();
        };
        let mut reaching: HashSet<usize> = HashSet::from([target]);
        let mut queue = VecDeque::from([target]);
        while let Some(at) = queue.pop_front() {
            for edge in &self.graph.edges {
                if edge.callee == Callee::Function(at)
                    && let Some(caller) = edge.caller
                    && reaching.insert(caller)
                {
                    queue.push_back(caller);
                }
            }
        }
        self.nodes
            .iter()
            .filter(|n| n.is_test)
            .filter(|n| n.graph_index.is_some_and(|i| reaching.contains(&i)))
            .map(|n| n.id)
            .collect()
    }

    /// Which files import this module path.
    pub fn importers(&self, module: &str) -> BTreeSet<String> {
        self.imports
            .iter()
            .filter(|(_, imported)| imports_module(imported, module))
            .map(|(file, _)| file.clone())
            .collect()
    }

    /// Every role-bearing declaration, as `(universe id, role)`.
    pub fn role_bearers(&self) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        for node in &self.nodes {
            for role in &node.roles {
                out.push((node.id, role.clone()));
            }
        }
        out
    }
}

/// A `use A.b.C` names module `A.b`; a `use A.b` names module `A.b` too. Both count as importing
/// `A.b`, and neither counts as importing `A`.
fn imports_module(imported: &str, module: &str) -> bool {
    imported == module
}

/// Build the gold facts for one prepared project.
pub fn facts(analysis: &Analysis) -> Result<Facts, String> {
    let linked = noeta_db::linked(&analysis.db, analysis.ws);
    let program: &Program = linked
        .program
        .as_ref()
        .map_err(|d| format!("{} does not link ({} diagnostics)", analysis.name, d.len()))?;
    let checked = noeta_db::linked_checked_ide(&analysis.db, analysis.ws);
    let texts = analysis.texts();
    let graph = noeta_ide::callgraph::build(program, &checked.expr_types, &checked.sites, &texts);
    let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
    let info = noeta_ast::reflect::build(program, &native_roles, &Default::default());
    let roles_by_target = noeta_ide::trace::roles_by_target(&info);
    let test_spans = test_fn_spans(program);

    let mut nodes: Vec<Node> = Vec::new();
    let mut by_graph_index = HashMap::new();
    for (index, function) in graph.functions.iter().enumerate() {
        let id = nodes.len();
        by_graph_index.insert(index, id);
        let leaf = leaf_of(&function.name);
        nodes.push(Node {
            id,
            qualified: function.name.clone(),
            leaf,
            kind: if function.method {
                NodeKind::Method
            } else {
                NodeKind::Function
            },
            file: file_of(analysis, function.name_span),
            module: module_of(analysis, function.name_span),
            name_span: (function.name_span.start, function.name_span.end),
            roles: roles_by_target
                .get(&function.name)
                .cloned()
                .unwrap_or_default(),
            is_test: test_spans.contains(&function.name_span),
            graph_index: Some(index),
        });
    }
    for (name, kind, span) in type_decls(program) {
        let id = nodes.len();
        nodes.push(Node {
            id,
            qualified: name.clone(),
            leaf: leaf_of(&name),
            kind,
            file: file_of(analysis, span),
            module: module_of(analysis, span),
            name_span: (span.start, span.end),
            roles: roles_by_target.get(&name).cloned().unwrap_or_default(),
            is_test: test_spans.contains(&span),
            graph_index: None,
        });
    }
    let top = nodes.len();
    nodes.push(Node {
        id: top,
        qualified: TOP_LEVEL.to_string(),
        leaf: TOP_LEVEL.to_string(),
        kind: NodeKind::TopLevel,
        file: analysis.entry.display().to_string(),
        module: analysis
            .module_of(noeta_span::SourceId::FIRST)
            .unwrap_or_default(),
        name_span: (0, 0),
        roles: Vec::new(),
        is_test: false,
        graph_index: None,
    });

    let mut by_leaf: HashMap<String, Vec<usize>> = HashMap::new();
    for node in &nodes {
        by_leaf.entry(node.leaf.clone()).or_default().push(node.id);
    }

    let (imports, modules) = import_edges(analysis);
    Ok(Facts {
        nodes,
        imports,
        modules,
        graph,
        by_graph_index,
        by_leaf,
        roles_by_target,
    })
}

fn leaf_of(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_string()
}

fn file_of(analysis: &Analysis, span: Span) -> String {
    analysis
        .file_of(span.source)
        .unwrap_or("<unknown>")
        .to_string()
}

fn module_of(analysis: &Analysis, span: Span) -> String {
    analysis.module_of(span.source).unwrap_or_default()
}

/// Every declared struct/class/enum/trait, with the name it is known by after linking.
fn type_decls(program: &Program) -> Vec<(String, NodeKind, Span)> {
    let mut out = Vec::new();
    collect_type_decls(&program.stmts, &mut out);
    out
}

fn collect_type_decls(stmts: &[Stmt], out: &mut Vec<(String, NodeKind, Span)>) {
    for stmt in stmts {
        match stmt {
            Stmt::Struct(decl) => {
                out.push((decl.name.to_string(), NodeKind::Struct, decl.name_span))
            }
            Stmt::Class(decl) => out.push((decl.name.to_string(), NodeKind::Class, decl.name_span)),
            Stmt::Enum(decl) => out.push((decl.name.to_string(), NodeKind::Enum, decl.name_span)),
            Stmt::Trait(decl) => out.push((decl.name.to_string(), NodeKind::Trait, decl.name_span)),
            Stmt::TierBlock { items, .. } => collect_type_decls(items, out),
            _ => {}
        }
    }
}

/// The name spans of every function a `@test` block declares, at any nesting.
fn test_fn_spans(program: &Program) -> HashSet<Span> {
    let mut out = HashSet::new();
    collect_test_spans(&program.stmts, false, &mut out);
    out
}

fn collect_test_spans(stmts: &[Stmt], inside: bool, out: &mut HashSet<Span>) {
    for stmt in stmts {
        match stmt {
            Stmt::TierBlock { tier, items, .. } => {
                collect_test_spans(items, inside || tier == "test", out);
            }
            Stmt::Fn(decl) if inside => {
                out.insert(decl.name_span);
            }
            Stmt::Struct(decl) if inside => {
                out.insert(decl.name_span);
                for method in &decl.methods {
                    out.insert(method.name_span);
                }
            }
            Stmt::Class(decl) if inside => {
                out.insert(decl.name_span);
                for method in &decl.methods {
                    out.insert(method.name_span);
                }
            }
            _ => {}
        }
    }
}

/// The `use` graph and the module roster: `(importing file, imported module)` edges, and
/// `(module path, declaring file)` pairs.
type ImportGraph = (Vec<(String, String)>, Vec<(String, String)>);

/// Read the `use` graph off each member file's **own** AST, and the module path it derives.
///
/// Per file rather than off the merged program, because the linker resolves imports away: the
/// merged program is what every declaration became, not what any file wrote.
fn import_edges(analysis: &Analysis) -> ImportGraph {
    let mut modules: Vec<(String, String)> = Vec::new();
    for (index, source) in analysis.sources.iter().enumerate() {
        if let Some(module) = analysis.module_of(noeta_span::SourceId(index as u32))
            && !module.is_empty()
        {
            modules.push((module, source.name().to_string()));
        }
    }
    let known: HashSet<&str> = modules.iter().map(|(m, _)| m.as_str()).collect();
    let mut imports = Vec::new();
    for member in analysis.ws.members(&analysis.db) {
        let parsed = noeta_db::ast(&analysis.db, *member);
        for stmt in &parsed.0.program.stmts {
            let Stmt::Use { path, names, span } = stmt else {
                continue;
            };
            let file = file_of(analysis, *span);
            let dotted = path.join(".");
            // `use Shop.models.Order` names module `Shop.models`; `use Feed.parse.csv` names the
            // whole module `Feed.parse.csv`. The project's own module list decides which reading
            // applies, so an import edge lands on a module that exists rather than on a prefix
            // that does not.
            if known.contains(dotted.as_str()) {
                imports.push((file.clone(), dotted.clone()));
            }
            for name in names {
                let extended = format!("{dotted}.{}", name.name);
                if known.contains(extended.as_str()) {
                    imports.push((file.clone(), extended));
                }
            }
        }
    }
    imports.sort();
    imports.dedup();
    modules.sort();
    (imports, modules)
}
