//! The unified documentation model (docs-browser arc, slice **0**).
//!
//! One navigable, searchable model of *everything worth documenting*, assembled over the compiler
//! so that every tool — `noeta lsp` (the editor's docs browser), `noeta mcp` (the agent's docs
//! tool), and `noeta doc` (the CLI) — reads the **same** tree and can never drift. This module is
//! the single interface those adapters go through; none of them assembles docs itself.
//!
//! The model is a lazily-unfolded tree of [`DocNode`]s under a small set of **roots** (one per
//! *corpus*): the project's own `@doc` blocks (this slice), and later the language guides and the
//! generated stdlib/package API reference. Each corpus is a self-contained arm of the same
//! dispatch — [`children`], [`page`], [`search`] — keyed off the [`DocId`]'s root segment, so a
//! new root is a new arm, not a new interface.
//!
//! Assembly is pure over a parsed [`Program`] plus a [`DocEnv`] — the small seam through which the
//! host resolves a [`Span`] to an editor location and names a source. That keeps the tree logic
//! unit-testable against source (a stub `DocEnv`), exactly like [`crate::symbols::outline`], while
//! the [`DocumentStore`](crate::DocumentStore) supplies the real salsa-backed environment.

use std::collections::HashMap;

use noeta_ast::Program;
use noeta_check::{DocTarget, dedent_doc, resolve_docs};
use noeta_span::{SourceId, Span};

use crate::api;
use crate::guide;
use crate::offsets::Range;
use crate::symbols::{SymbolKind, SymbolNode, outline};

/// A stable, wire-safe handle to a node in the doc tree. The first `/`-segment is the **root**
/// (`project`, later `guide`/`api`); the rest is that corpus's own addressing. Round-trips through
/// an adapter's wire and back into [`page`]/[`children`] unchanged. Segments below the root are
/// Noeta identifiers or numeric indices (never contain `/`), so splitting on `/` is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocId(pub String);

impl DocId {
    fn new(s: impl Into<String>) -> Self {
        DocId(s.into())
    }
    /// The root segment (`"project"`, `"guide"`, `"api"`), or the whole id if it has no `/`.
    pub fn root(&self) -> &str {
        self.0.split('/').next().unwrap_or(&self.0)
    }
    fn segments(&self) -> Vec<&str> {
        self.0.split('/').collect()
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The root of the project corpus: the active workspace's own declarations and `@doc` blocks.
pub const PROJECT_ROOT: &str = "project";

/// The root of the language-guide corpus: the embedded `docs/*.md` wiki (see [`crate::guide`]).
pub const GUIDE_ROOT: &str = "guide";

/// The root of the API-reference corpus: the stdlib/native surface from the registry (see
/// [`crate::api`]). Like the guide, workspace-independent.
pub const API_ROOT: &str = "api";

/// The root of the dependencies corpus: the project's **direct** third-party packages, read from
/// the manifest's `[dependencies]` (never transitive/shadow deps — those are not in the table).
/// Workspace-dependent: a source dependency's `.noe` API is browsed from the linked program; a
/// native or not-yet-fetched dependency shows a placeholder pointing at `noeta doc`.
pub const DEPS_ROOT: &str = "deps";

/// The kind of a doc node — its icon and how an adapter presents it. Spans every corpus; the
/// project corpus uses the declaration kinds, later corpora add [`DocKind::Guide`] and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    /// A corpus root (`Project`, `Guide`, `Api`).
    Root,
    /// A source module (one `.noe` file of the project).
    Module,
    Function,
    Struct,
    Class,
    Enum,
    Variant,
    Field,
    Method,
    Interface,
    /// A user-defined `trait` (L1 user traits).
    Trait,
    /// Free-floating section prose between declarations (an unattached `@doc` block).
    Section,
    /// A language-guide page (a `docs/*.md` wiki page).
    Guide,
    /// A dependency package (a direct entry of the manifest's `[dependencies]`).
    Package,
}

impl DocKind {
    /// The wire tag an adapter maps onto its own protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Root => "root",
            DocKind::Module => "module",
            DocKind::Function => "function",
            DocKind::Struct => "struct",
            DocKind::Class => "class",
            DocKind::Enum => "enum",
            DocKind::Variant => "variant",
            DocKind::Field => "field",
            DocKind::Method => "method",
            DocKind::Interface => "interface",
            DocKind::Trait => "trait",
            DocKind::Section => "section",
            DocKind::Guide => "guide",
            DocKind::Package => "package",
        }
    }

    fn from_symbol(kind: SymbolKind) -> Self {
        match kind {
            SymbolKind::Function => DocKind::Function,
            SymbolKind::Struct => DocKind::Struct,
            SymbolKind::Class => DocKind::Class,
            SymbolKind::Enum => DocKind::Enum,
            SymbolKind::EnumMember => DocKind::Variant,
            SymbolKind::Field => DocKind::Field,
            SymbolKind::Method => DocKind::Method,
            SymbolKind::Interface => DocKind::Interface,
            SymbolKind::Trait => DocKind::Trait,
        }
    }
}

/// A resolved source location — where a node's declaration lives, for "go to source".
#[derive(Debug, Clone, PartialEq)]
pub struct DocLoc {
    pub uri: String,
    pub range: Range,
}

/// One node of the doc tree: what an adapter renders in the navigation view. Carries enough to
/// display a row (title, kind, dim `detail`) and to know whether it can be expanded ([`expandable`])
/// or opened as a page ([`has_page`]), plus its source location when it has one.
///
/// [`expandable`]: DocNode::expandable
/// [`has_page`]: DocNode::has_page
#[derive(Debug, Clone, PartialEq)]
pub struct DocNode {
    pub id: DocId,
    pub title: String,
    pub kind: DocKind,
    /// A short signature-like detail shown dim next to the title (a function's parameters/return,
    /// a field's type), when useful.
    pub detail: Option<String>,
    /// Whether [`page`] yields a body worth opening (a signature and/or prose).
    pub has_page: bool,
    /// Whether [`children`] yields sub-nodes (an unexpandable leaf otherwise).
    pub expandable: bool,
    pub location: Option<DocLoc>,
}

/// A cross-reference from a page to another doc node — a clickable "see also" link. Populated
/// lightly in this slice (empty); later wired to the call graph and prose↔API links.
#[derive(Debug, Clone, PartialEq)]
pub struct DocXref {
    pub id: DocId,
    pub title: String,
}

/// A rendered doc page: the body an adapter shows when a node is opened. `signature` is the
/// declaration's code (rendered in a ```` ```noeta ```` block), `markdown` its `@doc` prose (may
/// be empty). Together with `location` and `xrefs`, this is the whole page.
#[derive(Debug, Clone, PartialEq)]
pub struct DocPage {
    pub id: DocId,
    pub title: String,
    pub kind: DocKind,
    pub signature: Option<String>,
    pub markdown: String,
    pub location: Option<DocLoc>,
    pub xrefs: Vec<DocXref>,
}

/// One search hit: the node to open, plus a short snippet of the matching text and a score (higher
/// is better) the adapter can present in ranked order.
#[derive(Debug, Clone, PartialEq)]
pub struct DocHit {
    pub id: DocId,
    pub title: String,
    pub kind: DocKind,
    pub snippet: String,
    pub score: i32,
}

/// How a direct dependency's API is available to the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// A `.noe` source package whose modules are linked into the program — its API is browsed in
    /// full, offline.
    Source,
    /// A native (Rust-backed) package: its API is generated at publish time and lives on the
    /// registry, not in the local store, so the editor can only point at `noeta doc`.
    Native,
    /// Declared in the manifest but not resolved on disk yet (not fetched / a resolution error).
    Unresolved,
}

/// One direct dependency of the project (a `[dependencies]` entry): the import root it is used
/// under, a short human detail (its source/version), and how its API is available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepInfo {
    /// The dependency-table key — the root a consumer writes `use <root>.…` under.
    pub root: String,
    /// A dim detail for the row (e.g. `path ../geom`, `git …@v1`, `^1.2`), with a kind hint.
    pub detail: String,
    pub kind: DepKind,
}

/// The host seam the pure assembly resolves through: map a declaration [`Span`] to an editor
/// [`DocLoc`], name a project source, and (for the dependencies corpus) enumerate the direct
/// dependencies and name a dependency module's source. The
/// [`DocumentStore`](crate::DocumentStore) implements this over its salsa database; tests stub it.
pub trait DocEnv {
    fn locate(&self, span: Span) -> Option<DocLoc>;
    /// The display name of a project source (a file basename), or `None` if the source is not part
    /// of the project corpus (a dependency module, excluded from the project tree — it appears
    /// under the dependencies corpus instead).
    fn source_name(&self, source: SourceId) -> Option<String>;

    /// The project's direct dependencies (manifest `[dependencies]`), for the dependencies corpus
    /// root — every direct entry, whatever its kind. Empty by default (stubs / no workspace). A
    /// source dependency's browsable modules are supplied separately, as [`DocCtx::deps`].
    fn dependencies(&self) -> Vec<DepInfo> {
        Vec::new()
    }
}

/// One direct **source** dependency module: its own parsed [`Program`] (a dependency module is a
/// separate salsa input, not merged into the workspace program, so it must be threaded in
/// explicitly), the import `root` it is used under, its display name, and its [`SourceId`] (for
/// stable ids and go-to-source). The store supplies these; the pure model walks them exactly like a
/// project module.
#[derive(Debug)]
pub struct DepDoc<'a> {
    pub root: String,
    pub module_name: String,
    pub source: SourceId,
    pub program: &'a Program,
}

/// One workspace member module in a [`DocCtx`]: its own (unlinked) program, whose spans carry
/// the member's [`SourceId`]. The project corpus documents **every member**, each from its own
/// AST — not the current entry's import closure, which would hide any sibling the entry happens
/// not to import (a `hotpath.noe` next to `main.noe`). The same per-module shape the
/// dependencies corpus ([`DepDoc`]) always had.
#[derive(Debug)]
pub struct MemberDoc<'a> {
    pub source: SourceId,
    pub program: &'a Program,
}

/// The context a doc request resolves in: the workspace's [`DocEnv`] and its member modules when
/// a workspace is open (empty otherwise), plus the direct source dependencies' own programs. The
/// **project** corpus needs the env+members (it yields nothing without them); the
/// **dependencies** corpus needs `deps`; the **language guide** corpus is workspace-independent
/// and is served in either case — so the guide is always browsable, even with no `.noe` file
/// open.
pub struct DocCtx<'a> {
    pub env: Option<&'a dyn DocEnv>,
    pub members: Vec<MemberDoc<'a>>,
    pub deps: Vec<DepDoc<'a>>,
}

impl std::fmt::Debug for DocCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn DocEnv`/`Program` are not `Debug`; report only whether a workspace is present.
        f.debug_struct("DocCtx")
            .field("workspace", &self.env.is_some())
            .finish()
    }
}

impl<'a> DocCtx<'a> {
    /// A context backed by an open workspace, with no dependency modules.
    pub fn new(env: &'a dyn DocEnv, members: Vec<MemberDoc<'a>>) -> Self {
        DocCtx {
            env: Some(env),
            members,
            deps: Vec::new(),
        }
    }

    /// A context backed by an open workspace, including its direct source dependencies' programs.
    pub fn with_deps(
        env: &'a dyn DocEnv,
        members: Vec<MemberDoc<'a>>,
        deps: Vec<DepDoc<'a>>,
    ) -> Self {
        DocCtx {
            env: Some(env),
            members,
            deps,
        }
    }

    /// A workspace-less context — only the guide corpus resolves.
    pub fn empty() -> Self {
        DocCtx {
            env: None,
            members: Vec::new(),
            deps: Vec::new(),
        }
    }

    /// The `(env, members)` pair when a workspace is open, for the project corpus arms.
    fn project(&self) -> Option<(&'a dyn DocEnv, &[MemberDoc<'a>])> {
        Some((self.env?, self.members.as_slice()))
    }
}

/// The corpus roots, in display order: the active project, then the language guides. The API
/// reference joins as a third root in a later arc, each an additional root here plus a dispatch arm.
pub fn roots() -> Vec<DocNode> {
    vec![
        DocNode {
            id: DocId::new(PROJECT_ROOT),
            title: "Project".to_string(),
            kind: DocKind::Root,
            detail: None,
            has_page: false,
            expandable: true,
            location: None,
        },
        DocNode {
            id: DocId::new(DEPS_ROOT),
            title: "Dependencies".to_string(),
            kind: DocKind::Root,
            detail: None,
            has_page: false,
            expandable: true,
            location: None,
        },
        DocNode {
            id: DocId::new(GUIDE_ROOT),
            title: "Language Guide".to_string(),
            kind: DocKind::Root,
            detail: None,
            has_page: false,
            expandable: true,
            location: None,
        },
        DocNode {
            id: DocId::new(API_ROOT),
            title: "API Reference".to_string(),
            kind: DocKind::Root,
            detail: None,
            has_page: false,
            expandable: true,
            location: None,
        },
    ]
}

/// The children of `id`, one lazily-unfolded level (mirrors the Architecture view's lazy tree):
/// the project root → its source modules; a module → its declarations (and section prose); a
/// declaration → its members (fields/variants/methods). An unknown or leaf id yields an empty
/// vec, never an error.
pub fn children(ctx: &DocCtx, id: &DocId) -> Vec<DocNode> {
    if id.root() == GUIDE_ROOT {
        return guide_children(id);
    }
    if id.root() == API_ROOT {
        return api_children(id);
    }
    if id.root() == DEPS_ROOT {
        return deps_children(ctx, id);
    }
    if id.root() != PROJECT_ROOT {
        return Vec::new();
    }
    let Some((env, members)) = ctx.project() else {
        return Vec::new(); // the project corpus needs an open workspace
    };
    let tree = ProjectTree::build(env, members);
    let seg = id.segments();
    match seg.as_slice() {
        [_root] => tree.modules.iter().map(|m| m.node.clone()).collect(),
        [_root, s] => match tree.module(s) {
            Some(m) => {
                let mut kids: Vec<DocNode> = m.decls.iter().map(|d| d.node.clone()).collect();
                kids.extend(m.sections.iter().map(|s| s.node.clone()));
                kids
            }
            None => Vec::new(),
        },
        [_root, s, decl] => match tree.module(s).and_then(|m| m.decl(decl)) {
            Some(d) => d.members.iter().map(|m| m.node.clone()).collect(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The rendered page for `id` — the node's signature and `@doc` prose — or `None` if the id names
/// nothing in the current program.
pub fn page(ctx: &DocCtx, id: &DocId) -> Option<DocPage> {
    if id.root() == GUIDE_ROOT {
        return guide_page(id);
    }
    if id.root() == API_ROOT {
        return api_page(id);
    }
    if id.root() == DEPS_ROOT {
        return deps_page(ctx, id);
    }
    if id.root() != PROJECT_ROOT {
        return None;
    }
    let (env, members) = ctx.project()?; // the project corpus needs an open workspace
    ProjectTree::build(env, members).page_of(id)
}

/// Rank every project node by how well it matches `query` (case-insensitive): a title hit scores
/// higher than a body/detail hit. Returns hits sorted best-first, capped at [`SEARCH_LIMIT`].
pub fn search(ctx: &DocCtx, query: &str) -> Vec<DocHit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<DocHit> = Vec::new();
    // The project and dependency corpora contribute only when a workspace is open.
    if let Some((env, members)) = ctx.project() {
        score_tree_into(&ProjectTree::build(env, members), &needle, &mut hits);
        // Source dependencies: the browse-able `.noe` packages threaded in as `ctx.deps`. Group by
        // root so each dependency contributes one tree (a dep may span several modules).
        let mut roots: Vec<&str> = ctx.deps.iter().map(|d| d.root.as_str()).collect();
        roots.sort_unstable();
        roots.dedup();
        for root in roots {
            score_tree_into(&dep_tree(ctx, root), &needle, &mut hits);
        }
    }
    // Merge in the language-guide and API-reference corpora so one search spans every corpus. Scores
    // are on each ranker's own scale; close enough for a combined best-first order at this size.
    hits.extend(guide_search(query));
    hits.extend(api_search(&needle));
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    hits.truncate(SEARCH_LIMIT);
    hits
}

/// Rank every node of one built tree (project or a dependency) against `needle` (already
/// lowercased): a title hit outscores a detail hit outscores a prose hit. Pushes matches into
/// `hits`. Shared by the project and dependency corpora so both rank identically.
fn score_tree_into(tree: &ProjectTree, needle: &str, hits: &mut Vec<DocHit>) {
    tree.for_each(&mut |node: &DocNode, prose: &str| {
        let title_l = node.title.to_lowercase();
        let detail_l = node.detail.as_deref().unwrap_or("").to_lowercase();
        let prose_l = prose.to_lowercase();
        let mut score = 0;
        if title_l == needle {
            score += 100;
        } else if title_l.contains(needle) {
            score += 40;
        }
        if detail_l.contains(needle) {
            score += 8;
        }
        if prose_l.contains(needle) {
            score += 5;
        }
        if score > 0 {
            hits.push(DocHit {
                id: node.id.clone(),
                title: node.title.clone(),
                kind: node.kind,
                snippet: snippet_of(prose, node.detail.as_deref()),
                score,
            });
        }
    });
}

/// The [`DocId`] documenting the declaration whose name span is `name_span`, if it is a project
/// node — the bridge for "show docs for the symbol under the cursor" (the store resolves the
/// cursor to a name span; this maps that span to its doc node).
pub fn id_for_name_span(
    env: &impl DocEnv,
    members: &[MemberDoc],
    name_span: Span,
) -> Option<DocId> {
    let tree = ProjectTree::build(env, members);
    let mut found = None;
    tree.for_each_with_span(&mut |node: &DocNode, span: Option<Span>| {
        if span == Some(name_span) {
            found = Some(node.id.clone());
        }
    });
    found
}

// ---- The language-guide corpus dispatch (slice 2). Static, workspace-independent. --------------

/// The children of a guide id: the guide root lists one node per `docs/*.md` page; a page is a
/// leaf (its full markdown is the page body).
fn guide_children(id: &DocId) -> Vec<DocNode> {
    if id.segments().len() != 1 {
        return Vec::new(); // a page node is a leaf
    }
    guide::index()
        .into_iter()
        .map(|(slug, title)| DocNode {
            id: DocId::new(format!("{GUIDE_ROOT}/{slug}")),
            title,
            kind: DocKind::Guide,
            detail: None,
            has_page: true,
            expandable: false,
            location: None,
        })
        .collect()
}

/// The page for a `guide/<slug>` id: the full markdown body of the guide page.
fn guide_page(id: &DocId) -> Option<DocPage> {
    let segments = id.segments();
    let [_root, slug] = segments.as_slice() else {
        return None;
    };
    let (title, body) = guide::lookup(slug)?;
    Some(DocPage {
        id: id.clone(),
        title,
        kind: DocKind::Guide,
        signature: None,
        markdown: body.to_string(),
        location: None,
        xrefs: Vec::new(),
    })
}

/// Guide search hits as [`DocHit`]s addressed to their page node, best hit per page (a page's
/// sections collapse to one result).
fn guide_search(query: &str) -> Vec<DocHit> {
    let mut best: Vec<DocHit> = Vec::new();
    for hit in guide::search(query, SEARCH_LIMIT) {
        let id = DocId::new(format!("{GUIDE_ROOT}/{}", hit.page));
        match best.iter_mut().find(|h| h.id == id) {
            Some(existing) => {
                if (hit.score as i32) > existing.score {
                    existing.score = hit.score as i32;
                    existing.snippet = hit.snippet;
                }
            }
            None => best.push(DocHit {
                id,
                title: hit.title,
                kind: DocKind::Guide,
                snippet: hit.snippet,
                score: hit.score as i32,
            }),
        }
    }
    best
}

// ---- The API-reference corpus dispatch (Arc 2). Static, workspace-independent. ----------------

/// The compact signature detail shown next to a function node — the rendered signature without its
/// leading `fn ` (`sqrt(x: float): float`).
fn api_detail(signature: &str) -> String {
    signature
        .strip_prefix("fn ")
        .unwrap_or(signature)
        .to_string()
}

/// Auto-derived cross-references from an API symbol back to the language-guide pages that mention it
/// (Arc 2 A3): a guide page using `math.sqrt(…)` links from the `std.math.sqrt` API page. Matches
/// the call form `<short>.<name>` (`math.sqrt`) in guide bodies; capped to keep a page's "see also"
/// tidy. One direction only — the guide is the source of truth for what links where, so the
/// backlink is derived, never hand-maintained.
fn guide_xrefs_for(qualified: &str, name: &str) -> Vec<DocXref> {
    let short = qualified.rsplit('.').next().unwrap_or(qualified);
    let needle = format!("{short}.{name}");
    guide::pages_mentioning(&needle)
        .into_iter()
        .take(6)
        .map(|(slug, title)| DocXref {
            id: DocId::new(format!("{GUIDE_ROOT}/{slug}")),
            title,
        })
        .collect()
}

/// A function/method leaf node under an API module/type.
fn api_fn_node(qualified: &str, f: &api::ApiFn, kind: DocKind) -> DocNode {
    DocNode {
        id: DocId::new(format!("{API_ROOT}/{qualified}/{}", f.name)),
        title: f.name.clone(),
        kind,
        detail: Some(api_detail(&f.signature)),
        has_page: true,
        expandable: false,
        location: None,
    }
}

/// The children of an API id: the API root lists modules then extern types; a module lists its
/// functions, a type its methods; a function/method is a leaf.
fn api_children(id: &DocId) -> Vec<DocNode> {
    match id.segments().as_slice() {
        [_root] => {
            let mut nodes: Vec<DocNode> = api::modules()
                .into_iter()
                .map(|m| DocNode {
                    id: DocId::new(format!("{API_ROOT}/{}", m.qualified)),
                    title: m.qualified,
                    kind: DocKind::Module,
                    detail: None,
                    // A module opens an overview page (its function list), like docs.rs.
                    has_page: true,
                    expandable: true,
                    location: None,
                })
                .collect();
            nodes.extend(api::types().into_iter().map(|t| DocNode {
                id: DocId::new(format!("{API_ROOT}/{}", t.qualified)),
                title: t.qualified,
                kind: DocKind::Struct,
                detail: None,
                has_page: true,
                expandable: true,
                location: None,
            }));
            // A namespace that holds only nominal declarations is still a container the tree must
            // offer — `std.http` registers its functions under `std.http.client`/`.server`, so
            // `Framing` and `Frame` live in a namespace with no module of its own and would
            // otherwise be unreachable from the root no matter how well they document themselves.
            let listed: std::collections::HashSet<&str> =
                nodes.iter().map(|n| n.title.as_str()).collect();
            let mut orphans: Vec<String> = api::decls()
                .into_iter()
                .map(|d| d.module)
                .filter(|m| !listed.contains(m.as_str()))
                .collect();
            orphans.sort();
            orphans.dedup();
            let orphans: Vec<DocNode> = orphans
                .into_iter()
                .map(|m| DocNode {
                    id: DocId::new(format!("{API_ROOT}/{m}")),
                    title: m,
                    kind: DocKind::Module,
                    detail: None,
                    has_page: true,
                    expandable: true,
                    location: None,
                })
                .collect();
            nodes.extend(orphans);
            nodes
        }
        // A qualified name is either a module (→ functions and nominal declarations) or an extern
        // type (→ methods).
        [_root, qualified] => {
            if let Some(m) = api::module(qualified) {
                m.functions
                    .iter()
                    .map(|f| api_fn_node(qualified, f, DocKind::Function))
                    .chain(api_decl_nodes(qualified))
                    .collect()
            } else if let Some(t) = api::type_(qualified) {
                t.methods
                    .iter()
                    .map(|f| api_fn_node(qualified, f, DocKind::Method))
                    .collect()
            } else {
                // Not a module and not an extern type — but a unit may declare a trait under a
                // namespace it registers no functions in, and that namespace is still a real page.
                api_decl_nodes(qualified).collect()
            }
        }
        _ => Vec::new(),
    }
}

/// The nominal declarations (traits, enums, classes, structs) namespaced under `qualified`, as tree
/// leaves. They sit beside the module's functions for the same reason they sit on its `docs.json`
/// page: `use std.vec` brings `vec.Kernels` into reach exactly as it brings `vec.add`, so a browser
/// that lists one and not the other misrepresents what the module offers.
fn api_decl_nodes(qualified: &str) -> impl Iterator<Item = DocNode> + use<> {
    let qualified = qualified.to_string();
    api::decls()
        .into_iter()
        .filter(move |d| d.module == qualified)
        .map(|d| DocNode {
            id: DocId::new(format!("{API_ROOT}/{}/{}", d.module, d.name)),
            title: d.name.clone(),
            kind: decl_kind(d.kind),
            detail: Some(api_detail(&d.signature)),
            has_page: true,
            expandable: false,
            location: None,
        })
}

/// An [`api::ApiDecl`]'s docs.json kind as the browser's own.
fn decl_kind(kind: &str) -> DocKind {
    match kind {
        "trait" => DocKind::Trait,
        "enum" => DocKind::Enum,
        "class" => DocKind::Class,
        _ => DocKind::Struct,
    }
}

/// The page for an API id: a module/type **overview** for a two-segment id (`api/std.math`), or a
/// single function/method page for a three-segment id (`api/std.math/sqrt`).
fn api_page(id: &DocId) -> Option<DocPage> {
    match id.segments().as_slice() {
        [_root, qualified] => api_overview_page(qualified),
        [_root, qualified, name] => api_member_page(id, qualified, name),
        _ => None,
    }
}

/// A module's (or extern type's) overview page: its members listed with signatures and one-line
/// summaries, like a docs.rs module page. Gives the tree's module/type rows something to open and
/// gives a module-name search hit a destination.
fn api_overview_page(qualified: &str) -> Option<DocPage> {
    // The namespace's nominal declarations, listed under every overview that has any — a module's,
    // and a declaration-only namespace's (`std.http`, whose functions live under `std.http.client`),
    // which is a real page with nothing else on it.
    let decls: Vec<api::ApiDecl> = api::decls()
        .into_iter()
        .filter(|d| d.module == qualified)
        .collect();
    let (kind, members): (DocKind, Vec<api::ApiFn>) = if let Some(m) = api::module(qualified) {
        (DocKind::Module, m.functions)
    } else if let Some(t) = api::type_(qualified) {
        (DocKind::Struct, t.methods)
    } else if !decls.is_empty() {
        (DocKind::Module, Vec::new())
    } else {
        // Not a module, not an extern type, and nothing declared under it: no such page.
        return None;
    };
    let noun = if kind == DocKind::Module {
        "Functions"
    } else {
        "Methods"
    };
    let mut markdown = String::new();
    if members.is_empty() && decls.is_empty() {
        markdown.push_str(&format!("_No {}._", noun.to_lowercase()));
    }
    if !members.is_empty() {
        markdown.push_str(&format!("### {noun}\n\n"));
        for f in &members {
            let summary = f.doc.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            if summary.is_empty() {
                markdown.push_str(&format!("- `{}`\n", f.signature));
            } else {
                markdown.push_str(&format!("- `{}` — {}\n", f.signature, summary.trim()));
            }
        }
    }
    if !decls.is_empty() {
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        markdown.push_str("### Types and traits\n\n");
        for d in &decls {
            let summary = d.doc.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            if summary.is_empty() {
                markdown.push_str(&format!("- `{} {}`\n", d.kind, d.name));
            } else {
                markdown.push_str(&format!("- `{} {}` — {}\n", d.kind, d.name, summary.trim()));
            }
        }
    }
    Some(DocPage {
        id: DocId::new(format!("{API_ROOT}/{qualified}")),
        title: qualified.to_string(),
        kind,
        signature: None,
        markdown,
        location: None,
        xrefs: Vec::new(),
    })
}

/// A single function/method page: the rendered signature, its prose, and cross-references to guide
/// pages that mention it.
fn api_member_page(id: &DocId, qualified: &str, name: &str) -> Option<DocPage> {
    let (signature, markdown, kind) = match api::function(qualified, name) {
        Some(f) => (f.signature, f.doc, DocKind::Function),
        None => match api::method(qualified, name) {
            Some(f) => (f.signature, f.doc, DocKind::Method),
            // Not a function or method of this container, so the remaining thing it can name is one
            // of the namespace's nominal declarations.
            None => {
                let d = api::decls()
                    .into_iter()
                    .find(|d| d.module == qualified && d.name == name)?;
                (d.signature, d.doc, decl_kind(d.kind))
            }
        },
    };
    Some(DocPage {
        id: id.clone(),
        title: format!("{qualified}.{name}"),
        kind,
        signature: Some(signature),
        markdown,
        location: None,
        xrefs: guide_xrefs_for(qualified, name),
    })
}

/// Score one API function/method against `needle`, pushing a hit when it matches.
fn api_score_into(
    qualified: &str,
    f: &api::ApiFn,
    kind: DocKind,
    needle: &str,
    hits: &mut Vec<DocHit>,
) {
    let name_l = f.name.to_lowercase();
    let mut score = 0;
    if name_l == needle {
        score += 100;
    } else if name_l.contains(needle) {
        score += 40;
    }
    if f.signature.to_lowercase().contains(needle) {
        score += 8;
    }
    if f.doc.to_lowercase().contains(needle) {
        score += 5;
    }
    if score > 0 {
        hits.push(DocHit {
            id: DocId::new(format!("{API_ROOT}/{qualified}/{}", f.name)),
            title: format!("{qualified}.{}", f.name),
            kind,
            snippet: if f.doc.is_empty() {
                api_detail(&f.signature)
            } else {
                f.doc.clone()
            },
            score,
        });
    }
}

/// Score an API module/type container by its qualified name, so filtering or searching a module
/// name (`crypto` → `std.crypto`) surfaces the module itself — not only its functions, whose names
/// rarely repeat the module name. Pushes the container's overview node as a hit when it matches.
fn api_container_score_into(qualified: &str, kind: DocKind, needle: &str, hits: &mut Vec<DocHit>) {
    let q = qualified.to_lowercase();
    // Match against the short segment (`crypto`) and the whole qualified name (`std.crypto`).
    let short = qualified
        .rsplit('.')
        .next()
        .unwrap_or(qualified)
        .to_lowercase();
    let score = if short == needle || q == needle {
        90
    } else if short.contains(needle) || q.contains(needle) {
        35
    } else {
        0
    };
    if score > 0 {
        hits.push(DocHit {
            id: DocId::new(format!("{API_ROOT}/{qualified}")),
            title: qualified.to_string(),
            kind,
            snippet: String::new(),
            score,
        });
    }
}

/// API-reference search: rank module/type containers by their qualified name, and their member
/// functions/methods by name (highest), signature, and prose. `needle` is already lowercased.
fn api_search(needle: &str) -> Vec<DocHit> {
    let mut hits = Vec::new();
    for m in api::modules() {
        api_container_score_into(&m.qualified, DocKind::Module, needle, &mut hits);
        for f in &m.functions {
            api_score_into(&m.qualified, f, DocKind::Function, needle, &mut hits);
        }
    }
    for t in api::types() {
        api_container_score_into(&t.qualified, DocKind::Struct, needle, &mut hits);
        for f in &t.methods {
            api_score_into(&t.qualified, f, DocKind::Method, needle, &mut hits);
        }
    }
    // Nominal declarations are searched as members of their namespace, the same shape a function
    // is — searching "Mergeable" should find the trait, not silently nothing.
    for d in api::decls() {
        api_score_into(
            &d.module,
            &api::ApiFn {
                name: d.name,
                signature: d.signature,
                doc: d.doc,
            },
            decl_kind(d.kind),
            needle,
            &mut hits,
        );
    }
    hits
}

/// Cap on the number of search hits returned.
pub const SEARCH_LIMIT: usize = 50;

fn snippet_of(prose: &str, detail: Option<&str>) -> String {
    let src = if !prose.trim().is_empty() {
        prose.trim()
    } else {
        detail.unwrap_or("").trim()
    };
    let one_line = src.split('\n').next().unwrap_or("").trim();
    if one_line.chars().count() > 140 {
        let cut: String = one_line.chars().take(137).collect();
        format!("{cut}…")
    } else {
        one_line.to_string()
    }
}

// ---- The dependencies corpus dispatch (docs-browser-ui). Workspace-dependent. -----------------

/// A dependency package row under the `deps` root.
fn dep_node(info: DepInfo) -> DocNode {
    // A source package expands to its modules; a native/unresolved package is a leaf that opens a
    // placeholder page explaining how to reach its API.
    let expandable = info.kind == DepKind::Source;
    DocNode {
        id: DocId::new(format!("{DEPS_ROOT}/{}", info.root)),
        title: info.root,
        kind: DocKind::Package,
        detail: Some(info.detail),
        has_page: true,
        expandable,
        location: None,
    }
}

/// The declaration tree of one **source** dependency: the same [`ModuleEntry`]/[`DeclEntry`] shape
/// as the project corpus (so pages, prose, sections and go-to-source render identically), assembled
/// from the dependency's own module programs (threaded in as [`DocCtx::deps`]) and rooted at
/// `deps/<root>`.
fn dep_tree(ctx: &DocCtx, root: &str) -> ProjectTree {
    let prefix = format!("{DEPS_ROOT}/{root}");
    let modules = ctx
        .deps
        .iter()
        .filter(|d| d.root == root)
        .map(|d| module_entry(ctx.env, &prefix, d.source, d.module_name.clone(), d.program))
        .collect();
    ProjectTree { modules }
}

/// One module as a [`ModuleEntry`] — the shared builder behind the project AND dependency
/// corpora: outline the module's own program's declarations, attach its `@doc` prose and
/// sections, keyed by `<prefix>/<source-id>` (`project/…` or `deps/<root>/…`). One builder means
/// a project module and a dependency module render identically.
fn module_entry(
    env: Option<&dyn DocEnv>,
    prefix: &str,
    source: SourceId,
    title: String,
    program: &Program,
) -> ModuleEntry {
    let docs = ResolvedDocs::collect(program);
    let key = source.0.to_string();
    let module_id = format!("{prefix}/{key}");
    let decls: Vec<DeclEntry> = outline(program)
        .iter()
        .filter(|sym| sym.name_span.source == source)
        .map(|sym| decl_entry(env, prefix, &key, sym, &docs))
        .collect();
    let sections: Vec<SectionEntry> = docs
        .sections
        .get(&source)
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, (span, text))| SectionEntry {
            node: DocNode {
                id: DocId::new(format!("{module_id}/~{i}")),
                title: section_title(text),
                kind: DocKind::Section,
                detail: None,
                has_page: true,
                expandable: false,
                location: env.and_then(|e| e.locate(*span)),
            },
            prose: text.clone(),
        })
        .collect();
    ModuleEntry {
        node: DocNode {
            id: DocId::new(module_id),
            title,
            kind: DocKind::Module,
            detail: None,
            // A module always opens an overview page (prose and/or a contents list).
            has_page: true,
            expandable: !decls.is_empty() || !sections.is_empty(),
            location: None,
        },
        prose: docs.module.get(&source).cloned().unwrap_or_default(),
        key,
        decls,
        sections,
    }
}

/// The children of a `deps` id: the root lists the direct dependencies; a source dependency lists
/// its modules → declarations → members exactly like the project tree.
fn deps_children(ctx: &DocCtx, id: &DocId) -> Vec<DocNode> {
    let Some(env) = ctx.env else {
        return Vec::new(); // the dependencies corpus needs an open workspace
    };
    let seg = id.segments();
    match seg.as_slice() {
        [_root] => env.dependencies().into_iter().map(dep_node).collect(),
        [_root, root] => dep_tree(ctx, root)
            .modules
            .iter()
            .map(|m| m.node.clone())
            .collect(),
        [_root, root, s] => match dep_tree(ctx, root).module(s) {
            Some(m) => {
                let mut kids: Vec<DocNode> = m.decls.iter().map(|d| d.node.clone()).collect();
                kids.extend(m.sections.iter().map(|s| s.node.clone()));
                kids
            }
            None => Vec::new(),
        },
        [_root, root, s, decl] => match dep_tree(ctx, root).module(s).and_then(|m| m.decl(decl)) {
            Some(d) => d.members.iter().map(|m| m.node.clone()).collect(),
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The page for a `deps` id: a package's overview/placeholder for a two-segment id, or a
/// module/declaration/member page (via the dependency tree) for a deeper id.
fn deps_page(ctx: &DocCtx, id: &DocId) -> Option<DocPage> {
    let env = ctx.env?;
    match id.segments().as_slice() {
        [_root, root] => dep_overview_page(ctx, env, root),
        [_root, root, ..] => dep_tree(ctx, root).page_of(id),
        _ => None, // the bare `deps` root has no page
    }
}

/// A dependency's landing page: a source package lists its modules; a native/unresolved package
/// explains, honestly, where its API is and is not available.
fn dep_overview_page(ctx: &DocCtx, env: &dyn DocEnv, root: &str) -> Option<DocPage> {
    let info = env.dependencies().into_iter().find(|d| d.root == root)?;
    let markdown = match info.kind {
        DepKind::Source => {
            let tree = dep_tree(ctx, root);
            if tree.modules.is_empty() {
                "_This package contributes no browsable modules._".to_string()
            } else {
                let mut md = String::from("### Modules\n\n");
                for m in &tree.modules {
                    md.push_str(&format!("- `{}`\n", m.node.title));
                }
                md
            }
        }
        DepKind::Native => format!(
            "**Native package.** Its API reference is generated when the package is published and \
             lives on the registry, not in the local store — so the editor can't browse it here. \
             View it with:\n\n```\nnoeta doc {root}\n```",
        ),
        DepKind::Unresolved => format!(
            "**Not resolved yet.** `{root}` is declared in `[dependencies]` but its source isn't on \
             disk — build the project (or run `noeta doc {root}`) to fetch it, then reopen the \
             docs.",
        ),
    };
    Some(DocPage {
        id: DocId::new(format!("{DEPS_ROOT}/{root}")),
        title: root.to_string(),
        kind: DocKind::Package,
        signature: None,
        markdown,
        location: None,
        xrefs: Vec::new(),
    })
}

// ---- The eagerly-built project tree (one pass per request, like the Architecture view). --------

struct MemberEntry {
    node: DocNode,
    name_span: Span,
    prose: String,
}

struct DeclEntry {
    node: DocNode,
    name_span: Span,
    prose: String,
    members: Vec<MemberEntry>,
}

struct SectionEntry {
    node: DocNode,
    prose: String,
}

struct ModuleEntry {
    /// The source index string used in ids (`SourceId.0`).
    key: String,
    node: DocNode,
    prose: String,
    decls: Vec<DeclEntry>,
    sections: Vec<SectionEntry>,
}

struct ProjectTree {
    modules: Vec<ModuleEntry>,
}

impl ModuleEntry {
    fn decl(&self, name: &str) -> Option<&DeclEntry> {
        self.decls.iter().find(|d| last_segment(&d.node.id) == name)
    }
}

impl ProjectTree {
    fn module(&self, key: &str) -> Option<&ModuleEntry> {
        self.modules.iter().find(|m| m.key == key)
    }

    /// The rendered page for `id`, found by id-equality over the tree (works for any root prefix —
    /// project or a dependency). A module page is prose-only; a declaration/member page carries its
    /// signature detail; a section page is its prose. `None` if the tree has no such node.
    fn page_of(&self, id: &DocId) -> Option<DocPage> {
        for m in &self.modules {
            if m.node.id == *id {
                // A module always opens an overview: its `@doc` prose (if any) followed by a
                // contents list of its declarations — so clicking a source file is never a dead end.
                return Some(page_from(&m.node, None, &module_overview(m)));
            }
            for d in &m.decls {
                if d.node.id == *id {
                    return Some(page_from(&d.node, d.node.detail.clone(), &d.prose));
                }
                for mem in &d.members {
                    if mem.node.id == *id {
                        return Some(page_from(&mem.node, mem.node.detail.clone(), &mem.prose));
                    }
                }
            }
            for s in &m.sections {
                if s.node.id == *id {
                    return Some(page_from(&s.node, None, &s.prose));
                }
            }
        }
        None
    }

    /// Visit every node paired with its prose (for search).
    fn for_each(&self, f: &mut impl FnMut(&DocNode, &str)) {
        for m in &self.modules {
            f(&m.node, &m.prose);
            for d in &m.decls {
                f(&d.node, &d.prose);
                for mem in &d.members {
                    f(&mem.node, &mem.prose);
                }
            }
            for s in &m.sections {
                f(&s.node, &s.prose);
            }
        }
    }

    /// Visit every declaration/member node paired with its name span (for span→id lookup). Module
    /// and section nodes carry no declaration name span and are visited with `None`.
    fn for_each_with_span(&self, f: &mut impl FnMut(&DocNode, Option<Span>)) {
        for m in &self.modules {
            f(&m.node, None);
            for d in &m.decls {
                f(&d.node, Some(d.name_span));
                for mem in &d.members {
                    f(&mem.node, Some(mem.name_span));
                }
            }
            for s in &m.sections {
                f(&s.node, None);
            }
        }
    }

    /// The project tree: modules named by [`DocEnv::source_name`], rooted at `project`.
    /// The project corpus: one module row per **workspace member**, each documented from its own
    /// program — the whole project, not any one entry's import closure (a sibling the current
    /// file never imports is still a source file worth documenting). Shares [`module_entry`]
    /// with the dependencies corpus, so a project module renders — signatures, `@doc` prose,
    /// sections, go-to-source — identically to a dependency's.
    fn build(env: &dyn DocEnv, members: &[MemberDoc]) -> ProjectTree {
        let modules = members
            .iter()
            .filter_map(|m| {
                let title = env.source_name(m.source)?;
                Some(module_entry(
                    Some(env),
                    PROJECT_ROOT,
                    m.source,
                    title,
                    m.program,
                ))
            })
            .collect();
        ProjectTree { modules }
    }
}

/// A module's overview page body: its `@doc` prose (when present) followed by a "Contents" list of
/// its declarations with their signature detail — so a module row always opens something useful,
/// even when the file carries no module-level `@doc`.
fn module_overview(m: &ModuleEntry) -> String {
    let mut md = String::new();
    if !m.prose.trim().is_empty() {
        md.push_str(m.prose.trim());
        md.push_str("\n\n");
    }
    if !m.decls.is_empty() {
        md.push_str("### Contents\n\n");
        for d in &m.decls {
            let kind = d.node.kind.as_str();
            match &d.node.detail {
                Some(detail) => {
                    md.push_str(&format!("- `{}` {} — `{}`\n", d.node.title, kind, detail))
                }
                None => md.push_str(&format!("- `{}` {}\n", d.node.title, kind)),
            }
        }
    } else if m.prose.trim().is_empty() {
        md.push_str("_This module has no documented declarations._");
    }
    md
}

/// Build a [`DocPage`] from a tree node plus its (already-resolved) signature and prose.
fn page_from(node: &DocNode, signature: Option<String>, markdown: &str) -> DocPage {
    DocPage {
        id: node.id.clone(),
        title: node.title.clone(),
        kind: node.kind,
        signature,
        markdown: markdown.to_string(),
        location: node.location.clone(),
        xrefs: Vec::new(),
    }
}

fn decl_entry(
    env: Option<&dyn DocEnv>,
    prefix: &str,
    mod_key: &str,
    sym: &SymbolNode,
    docs: &ResolvedDocs,
) -> DeclEntry {
    let id = DocId::new(format!("{prefix}/{mod_key}/{}", sym.name));
    let members: Vec<MemberEntry> = sym
        .children
        .iter()
        .map(|child| MemberEntry {
            node: DocNode {
                id: DocId::new(format!("{}/{}", id.as_str(), child.name)),
                title: child.name.clone(),
                kind: DocKind::from_symbol(child.kind),
                detail: child.detail.clone(),
                has_page: true,
                expandable: false,
                location: env.and_then(|e| e.locate(child.name_span)),
            },
            name_span: child.name_span,
            prose: docs.decl.get(&child.name_span).cloned().unwrap_or_default(),
        })
        .collect();
    DeclEntry {
        node: DocNode {
            id,
            title: sym.name.clone(),
            kind: DocKind::from_symbol(sym.kind),
            detail: sym.detail.clone(),
            has_page: true,
            expandable: !members.is_empty(),
            location: env.and_then(|e| e.locate(sym.name_span)),
        },
        name_span: sym.name_span,
        prose: docs.decl.get(&sym.name_span).cloned().unwrap_or_default(),
        members,
    }
}

/// The `@doc` prose of a program, bucketed by what it documents — declarations (by name span),
/// module docs (by source), and free-floating sections (by source, in order).
struct ResolvedDocs {
    decl: HashMap<Span, String>,
    module: HashMap<SourceId, String>,
    sections: HashMap<SourceId, Vec<(Span, String)>>,
}

impl ResolvedDocs {
    fn collect(program: &Program) -> ResolvedDocs {
        let mut decl = HashMap::new();
        let mut module = HashMap::new();
        let mut sections: HashMap<SourceId, Vec<(Span, String)>> = HashMap::new();
        for block in resolve_docs(program) {
            let text = dedent_doc(&block.text).trim().to_string();
            match block.target {
                DocTarget::Decl { name_span, .. } => {
                    decl.insert(name_span, text);
                }
                DocTarget::Module => {
                    module.entry(block.span.source).or_insert(text);
                }
                DocTarget::Section => {
                    sections
                        .entry(block.span.source)
                        .or_default()
                        .push((block.span, text));
                }
            }
        }
        ResolvedDocs {
            decl,
            module,
            sections,
        }
    }
}

fn last_segment(id: &DocId) -> &str {
    id.0.rsplit('/').next().unwrap_or(&id.0)
}

fn section_title(text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Section")
        .trim_start_matches('#')
        .trim();
    if first.chars().count() > 60 {
        let cut: String = first.chars().take(57).collect();
        format!("{cut}…")
    } else if first.is_empty() {
        "Section".to_string()
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::Source;

    /// A stub environment: names the single test source and resolves no locations.
    struct StubEnv;
    impl DocEnv for StubEnv {
        fn locate(&self, _span: Span) -> Option<DocLoc> {
            None
        }
        fn source_name(&self, source: SourceId) -> Option<String> {
            (source == SourceId::FIRST).then(|| "t.noe".to_string())
        }
    }

    fn program_of(src: &str) -> Program {
        let source = Source::new(SourceId::FIRST, "t.noe", src);
        let lexed = lex(&source);
        parse(&source, &lexed.tokens).program
    }

    /// The one-module member list most tests need: `program` as the workspace's only member.
    fn one_member(program: &Program) -> Vec<MemberDoc<'_>> {
        vec![MemberDoc {
            source: SourceId::FIRST,
            program,
        }]
    }

    #[test]
    fn the_roots_are_project_deps_guide_api() {
        let roots = roots();
        assert_eq!(roots.len(), 4);
        assert_eq!(roots[0].id.as_str(), "project");
        assert_eq!(roots[1].id.as_str(), "deps");
        assert_eq!(roots[2].id.as_str(), "guide");
        assert_eq!(roots[3].id.as_str(), "api");
        assert!(
            roots
                .iter()
                .all(|r| r.kind == DocKind::Root && r.expandable)
        );
    }

    /// A stub env with no project sources of its own — the dependencies corpus is driven by the
    /// `DepInfo` list here plus the dependency programs threaded through [`DocCtx::with_deps`].
    struct DepEnv;
    impl DocEnv for DepEnv {
        fn locate(&self, _span: Span) -> Option<DocLoc> {
            None
        }
        fn source_name(&self, _source: SourceId) -> Option<String> {
            None // nothing in the project corpus
        }
        fn dependencies(&self) -> Vec<DepInfo> {
            vec![
                DepInfo {
                    root: "geom".to_string(),
                    detail: "path ../geom".to_string(),
                    kind: DepKind::Source,
                },
                DepInfo {
                    root: "imgfx".to_string(),
                    detail: "^1.0 · native".to_string(),
                    kind: DepKind::Native,
                },
            ]
        }
    }

    #[test]
    fn the_deps_corpus_browses_a_source_package_and_placeholders_a_native_one() {
        let program = program_of("@doc { A point. }\nstruct Point {\n  x: int\n  y: int\n}");
        // The dep module `geom/shapes.noe` is the parsed program above, attributed to its source.
        let env = DepEnv;
        let deps = vec![DepDoc {
            root: "geom".to_string(),
            module_name: "shapes.noe".to_string(),
            source: SourceId::FIRST,
            program: &program,
        }];
        let ctx = DocCtx::with_deps(&env, Vec::new(), deps); // dep-only: no project members

        // Root: one row per direct dependency.
        let deps = children(&ctx, &DocId::new("deps"));
        let names: Vec<&str> = deps.iter().map(|d| d.title.as_str()).collect();
        assert_eq!(names, vec!["geom", "imgfx"]);
        let geom = &deps[0];
        assert_eq!(geom.kind, DocKind::Package);
        assert!(geom.expandable, "a source package expands");
        let imgfx = &deps[1];
        assert!(!imgfx.expandable, "a native package is a leaf");

        // A source package unfolds to modules → decls → members, like the project corpus.
        let modules = children(&ctx, &geom.id);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].title, "shapes.noe");
        let decls = children(&ctx, &modules[0].id);
        let point = decls.iter().find(|d| d.title == "Point").expect("Point");
        assert_eq!(point.kind, DocKind::Struct);
        let fields = children(&ctx, &point.id);
        assert_eq!(
            fields.iter().map(|f| f.title.as_str()).collect::<Vec<_>>(),
            vec!["x", "y"]
        );

        // The decl page carries its dependency `@doc` prose.
        let decl_page = page(&ctx, &point.id).expect("dep decl page");
        assert_eq!(decl_page.title, "Point");
        assert_eq!(decl_page.markdown, "A point.");

        // The native package's page is an honest placeholder pointing at `noeta doc`.
        let native_page = page(&ctx, &imgfx.id).expect("native placeholder page");
        assert_eq!(native_page.kind, DocKind::Package);
        assert!(native_page.markdown.contains("noeta doc imgfx"));

        // Search spans the dependency corpus (a source dep's decls are searchable).
        let hits = search(&ctx, "Point");
        assert!(
            hits.iter()
                .any(|h| h.id.as_str() == "deps/geom/0/Point" && h.title == "Point"),
            "dependency decl is a search hit"
        );
    }

    #[test]
    fn the_api_root_browses_registry_modules_and_functions() {
        // Workspace-independent, like the guide.
        let ctx = DocCtx::empty();
        let modules = children(&ctx, &DocId::new("api"));
        assert!(modules.len() > 3, "the registry has several modules");
        let math = modules
            .iter()
            .find(|m| m.title == "std.math")
            .expect("std.math is a module");
        // A module is expandable AND opens an overview page (its function list).
        assert!(math.expandable && math.has_page);
        let overview = page(&ctx, &math.id).expect("the module overview renders");
        assert_eq!(overview.kind, DocKind::Module);
        assert!(
            overview.markdown.contains("sqrt"),
            "overview lists functions"
        );

        let fns = children(&ctx, &math.id);
        let sqrt = fns.iter().find(|f| f.title == "sqrt").expect("math.sqrt");
        assert_eq!(sqrt.kind, DocKind::Function);
        assert!(sqrt.has_page && !sqrt.expandable);
        assert_eq!(sqrt.detail.as_deref(), Some("sqrt(x: float): float"));

        let rendered = page(&ctx, &sqrt.id).expect("the function page renders");
        assert_eq!(
            rendered.signature.as_deref(),
            Some("fn sqrt(x: float): float")
        );
        assert!(rendered.markdown.contains("square root"));

        // Search spans the API corpus.
        let hits = search(&ctx, "sqrt");
        assert!(hits.iter().any(|h| h.id.as_str() == "api/std.math/sqrt"));

        // A module-name query surfaces the module container itself (its functions rarely repeat the
        // module name), so the tree filter can narrow to a whole module.
        let math_hits = search(&ctx, "math");
        assert!(
            math_hits.iter().any(|h| h.id.as_str() == "api/std.math"),
            "the std.math container is a hit for `math`"
        );
    }

    #[test]
    fn the_api_root_includes_extern_types_and_their_methods() {
        let ctx = DocCtx::empty();
        let roots = children(&ctx, &DocId::new("api"));
        // A well-known extern type surfaces alongside the modules (e.g. std.id.Uuid).
        let uuid = roots
            .iter()
            .find(|n| n.title == "std.id.Uuid")
            .expect("std.id.Uuid extern type present");
        assert_eq!(uuid.kind, DocKind::Struct);
        assert!(uuid.expandable);
        let methods = children(&ctx, &uuid.id);
        assert!(!methods.is_empty(), "the type exposes methods");
        assert!(
            methods
                .iter()
                .all(|m| m.kind == DocKind::Method && m.has_page)
        );
        // A method page renders (signature at least).
        let m0 = &methods[0];
        let rendered = page(&ctx, &m0.id).expect("method page renders");
        assert_eq!(rendered.kind, DocKind::Method);
        assert!(rendered.signature.is_some());
    }

    #[test]
    fn a_modules_native_traits_and_types_browse_beside_its_functions() {
        // The browser read `modules()`/`types()` only, so `std.vec` listed nineteen functions and
        // neither of the two traits the module exists to have you `impl` — the same hole
        // `noeta doc --api` had, through the same corpus.
        let ctx = DocCtx::empty();
        let vec_mod = children(&ctx, &DocId::new("api"))
            .into_iter()
            .find(|n| n.title == "std.vec")
            .expect("std.vec module present");
        let kids = children(&ctx, &vec_mod.id);
        let kernels = kids
            .iter()
            .find(|n| n.title == "Kernels")
            .expect("the Kernels trait browses under std.vec");
        assert_eq!(kernels.kind, DocKind::Trait);
        assert!(kernels.has_page);
        assert!(
            kids.iter().any(|n| n.kind == DocKind::Function),
            "the module's functions are still there"
        );

        // Its page carries the whole declaration and its prose.
        let rendered = page(&ctx, &kernels.id).expect("the trait page renders");
        assert_eq!(rendered.kind, DocKind::Trait);
        assert!(
            rendered
                .signature
                .as_deref()
                .unwrap()
                .contains("trait Kernels {")
        );
        assert!(rendered.markdown.contains("Element-wise arithmetic"));

        // An enum and a struct reach the tree with their own kinds, not a generic one.
        let http = children(&ctx, &DocId::new("api"))
            .into_iter()
            .find(|n| n.title == "std.http")
            .expect("std.http module present");
        let http_kids = children(&ctx, &http.id);
        let framing = http_kids.iter().find(|n| n.title == "Framing").unwrap();
        assert_eq!(framing.kind, DocKind::Enum);
        let frame = http_kids.iter().find(|n| n.title == "Frame").unwrap();
        assert_eq!(frame.kind, DocKind::Struct);

        // `std.http` registers no functions of its own (they live under `std.http.client`), so it
        // is a namespace that exists *only* because declarations are namespaced under it. Its row
        // promises a page, and that page must render rather than 404.
        let overview = page(&ctx, &http.id).expect("a declaration-only namespace has a page");
        assert!(overview.markdown.contains("### Types and traits"));
        assert!(overview.markdown.contains("`enum Framing`"));
    }

    #[test]
    fn searching_finds_a_native_trait_by_name() {
        let ctx = DocCtx::empty();
        let hits = search(&ctx, "satkernels");
        assert!(
            hits.iter().any(|h| h.title.contains("SatKernels")),
            "the trait is searchable: {:?}",
            hits.iter().map(|h| &h.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_guide_root_lists_pages_that_render_full_markdown() {
        let program = program_of("fn f() {}");
        let env = StubEnv;
        // The guide corpus is workspace-independent — env/program are ignored for guide ids.
        let pages = children(
            &DocCtx::new(&env, one_member(&program)),
            &DocId::new("guide"),
        );
        assert!(pages.len() > 5, "guide root lists the wiki pages");
        assert!(
            pages
                .iter()
                .all(|p| p.kind == DocKind::Guide && p.has_page && !p.expandable)
        );
        let page_node = &pages[0];
        let page = page(&DocCtx::new(&env, one_member(&program)), &page_node.id)
            .expect("a guide page renders");
        assert_eq!(page.kind, DocKind::Guide);
        assert!(!page.markdown.is_empty());
        // A guide page is a leaf.
        assert!(children(&DocCtx::new(&env, one_member(&program)), &page_node.id).is_empty());
    }

    #[test]
    fn the_guide_corpus_is_browsable_with_no_workspace() {
        // With no `.noe` file open (an empty context), the guide still browses; only the project
        // corpus needs a workspace.
        let ctx = DocCtx::empty();
        let guide_pages = children(&ctx, &DocId::new("guide"));
        assert!(guide_pages.len() > 5, "guide browses without a workspace");
        // The project corpus yields nothing without a workspace.
        assert!(children(&ctx, &DocId::new("project")).is_empty());
        assert!(page(&ctx, &DocId::new("project/0/whatever")).is_none());
        let rendered =
            page(&ctx, &guide_pages[0].id).expect("a guide page renders with no workspace");
        assert!(!rendered.markdown.is_empty());
        // Search still returns guide hits.
        let hits = search(&ctx, "standard library");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.kind == DocKind::Guide));
    }

    #[test]
    fn search_spans_both_the_project_and_the_guide_corpus() {
        // `Widget` exists only in the project; a common guide term surfaces guide hits too.
        let program = program_of("struct Widget { size: int }");
        let env = StubEnv;
        let project_hit = search(&DocCtx::new(&env, one_member(&program)), "Widget");
        assert!(
            project_hit
                .iter()
                .any(|h| h.kind == DocKind::Struct && h.title == "Widget")
        );
        let guide_hits = search(&DocCtx::new(&env, one_member(&program)), "standard library");
        assert!(
            guide_hits.iter().any(|h| h.kind == DocKind::Guide),
            "a guide-only query returns guide hits"
        );
    }

    #[test]
    fn project_children_are_modules_then_decls_then_members() {
        let program = program_of(
            "fn greet(name: str): str { return name }\nstruct Point {\n  x: int\n  y: int\n}",
        );
        let env = StubEnv;

        // Level 1: one module for the single source.
        let modules = children(
            &DocCtx::new(&env, one_member(&program)),
            &DocId::new("project"),
        );
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].kind, DocKind::Module);
        assert_eq!(modules[0].title, "t.noe");
        assert!(modules[0].expandable);

        // Level 2: the module's declarations.
        let decls = children(&DocCtx::new(&env, one_member(&program)), &modules[0].id);
        let names: Vec<&str> = decls.iter().map(|d| d.title.as_str()).collect();
        assert_eq!(names, vec!["greet", "Point"]);
        let point = decls.iter().find(|d| d.title == "Point").unwrap();
        assert_eq!(point.kind, DocKind::Struct);
        assert!(point.expandable);

        // Level 3: the struct's fields.
        let fields = children(&DocCtx::new(&env, one_member(&program)), &point.id);
        let fnames: Vec<&str> = fields.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(fnames, vec!["x", "y"]);
        assert_eq!(fields[0].kind, DocKind::Field);
        assert_eq!(fields[0].detail.as_deref(), Some("int"));
    }

    #[test]
    fn a_decl_page_carries_its_doc_prose_and_signature() {
        let program = program_of(
            "@doc {\n  Greets a person by name.\n}\nfn greet(name: str): str { return name }",
        );
        let env = StubEnv;
        let modules = children(
            &DocCtx::new(&env, one_member(&program)),
            &DocId::new("project"),
        );
        let decls = children(&DocCtx::new(&env, one_member(&program)), &modules[0].id);
        let greet = &decls[0];

        let page = page(&DocCtx::new(&env, one_member(&program)), &greet.id).unwrap();
        assert_eq!(page.title, "greet");
        assert_eq!(page.kind, DocKind::Function);
        assert_eq!(page.markdown, "Greets a person by name.");
        assert_eq!(page.signature.as_deref(), Some("(name: str) -> str"));
    }

    #[test]
    fn a_module_doc_becomes_the_module_page() {
        // A doc block whose next statement is *not* a declaration (here a `use`) is the module doc,
        // not a declaration's doc — the adjacency rule in `resolve_texts`.
        let program = program_of("@doc {\n  The geometry module.\n}\nuse std.math\nfn f() {}");
        let env = StubEnv;
        let modules = children(
            &DocCtx::new(&env, one_member(&program)),
            &DocId::new("project"),
        );
        assert!(modules[0].has_page);
        let page = page(&DocCtx::new(&env, one_member(&program)), &modules[0].id).unwrap();
        // The module overview leads with its `@doc` prose, then a contents list of its decls.
        assert!(page.markdown.starts_with("The geometry module."));
        assert!(page.markdown.contains("### Contents"));
        assert!(page.markdown.contains("`f` function"));
        assert_eq!(page.kind, DocKind::Module);
    }

    #[test]
    fn search_ranks_a_name_hit_above_a_prose_hit() {
        let program =
            program_of("@doc {\n  A helper about widgets.\n}\nfn helper() {}\nfn widget() {}");
        let env = StubEnv;
        let hits = search(&DocCtx::new(&env, one_member(&program)), "widget");
        // `widget` (name match) outranks `helper` (prose-only match), and both appear.
        assert!(hits.len() >= 2);
        assert_eq!(hits[0].title, "widget");
        assert!(hits.iter().any(|h| h.title == "helper"));
        assert!(hits[0].score > hits.iter().find(|h| h.title == "helper").unwrap().score);
    }

    #[test]
    fn id_for_name_span_finds_the_documented_decl() {
        let program = program_of("fn greet() {}\nfn other() {}");
        let env = StubEnv;
        // The name span of `greet` is where the identifier sits in source.
        let greet_span = outline(&program)[0].name_span;
        let id = id_for_name_span(&env, &one_member(&program), greet_span).unwrap();
        assert_eq!(id.as_str(), "project/0/greet");
    }

    #[test]
    fn section_prose_becomes_a_leaf_node() {
        // The first non-attached block is the module doc; a later non-attached block is a section.
        // Each block's next statement is a `use` (not a declaration), so neither attaches to a decl.
        let program = program_of(
            "@doc {\n  Module intro.\n}\nuse std.math\nfn a() {}\n@doc {\n  ## Notes\n  Some free prose.\n}\nuse std.list\nfn b() {}",
        );
        let env = StubEnv;
        let modules = children(
            &DocCtx::new(&env, one_member(&program)),
            &DocId::new("project"),
        );
        let kids = children(&DocCtx::new(&env, one_member(&program)), &modules[0].id);
        let section = kids.iter().find(|k| k.kind == DocKind::Section).unwrap();
        assert_eq!(section.title, "Notes");
        let page = page(&DocCtx::new(&env, one_member(&program)), &section.id).unwrap();
        assert!(page.markdown.contains("Some free prose."));
    }

    #[test]
    fn unknown_id_yields_no_children_and_no_page() {
        let program = program_of("fn f() {}");
        let env = StubEnv;
        assert!(
            children(
                &DocCtx::new(&env, one_member(&program)),
                &DocId::new("project/99/nope")
            )
            .is_empty()
        );
        assert!(
            page(
                &DocCtx::new(&env, one_member(&program)),
                &DocId::new("guide/whatever")
            )
            .is_none()
        );
        assert!(
            page(
                &DocCtx::new(&env, one_member(&program)),
                &DocId::new("project/0/nope")
            )
            .is_none()
        );
    }
}
