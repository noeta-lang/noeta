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

/// The host seam the pure assembly resolves through: map a declaration [`Span`] to an editor
/// [`DocLoc`], and name a project source (returning `None` for a source that should not appear in
/// the project corpus — e.g. a dependency module, excluded in this slice). The
/// [`DocumentStore`](crate::DocumentStore) implements this over its salsa database; tests stub it.
pub trait DocEnv {
    fn locate(&self, span: Span) -> Option<DocLoc>;
    /// The display name of a project source (a file basename), or `None` if the source is not part
    /// of the project corpus (excluded from the tree).
    fn source_name(&self, source: SourceId) -> Option<String>;
}

/// The context a doc request resolves in: the workspace's [`DocEnv`] and linked [`Program`] when a
/// workspace is open, or `None` for both when nothing is open. The **project** corpus needs both
/// (it yields nothing without them); the **language guide** corpus is workspace-independent and is
/// served in either case — so the guide is always browsable, even with no `.noe` file open.
pub struct DocCtx<'a> {
    pub env: Option<&'a dyn DocEnv>,
    pub program: Option<&'a Program>,
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
    /// A context backed by an open workspace.
    pub fn new(env: &'a dyn DocEnv, program: &'a Program) -> Self {
        DocCtx {
            env: Some(env),
            program: Some(program),
        }
    }

    /// A workspace-less context — only the guide corpus resolves.
    pub fn empty() -> Self {
        DocCtx {
            env: None,
            program: None,
        }
    }

    /// The `(env, program)` pair when a workspace is open, for the project corpus arms.
    fn workspace(&self) -> Option<(&'a dyn DocEnv, &'a Program)> {
        Some((self.env?, self.program?))
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
    if id.root() != PROJECT_ROOT {
        return Vec::new();
    }
    let Some((env, program)) = ctx.workspace() else {
        return Vec::new(); // the project corpus needs an open workspace
    };
    let tree = ProjectTree::build(env, program);
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
    if id.root() != PROJECT_ROOT {
        return None;
    }
    let (env, program) = ctx.workspace()?; // the project corpus needs an open workspace
    let tree = ProjectTree::build(env, program);
    let seg = id.segments();
    // A section node's id is `project/{source}/~{index}` — dispatch it before the decl arm, which
    // it would otherwise shadow (both are three segments).
    if seg.last().is_some_and(|last| last.starts_with('~')) {
        return section_page(&tree, &seg);
    }
    let (node, signature, markdown) = match seg.as_slice() {
        [_root, s] => {
            let m = tree.module(s)?;
            (&m.node, None, m.prose.clone())
        }
        [_root, s, decl] => {
            let d = tree.module(s)?.decl(decl)?;
            (&d.node, d.node.detail.clone(), d.prose.clone())
        }
        [_root, s, decl, member] => {
            let m = tree.module(s)?.decl(decl)?.member(member)?;
            (&m.node, m.node.detail.clone(), m.prose.clone())
        }
        _ => return None,
    };
    Some(DocPage {
        id: node.id.clone(),
        title: node.title.clone(),
        kind: node.kind,
        signature,
        markdown,
        location: node.location.clone(),
        xrefs: Vec::new(),
    })
}

fn section_page(tree: &ProjectTree, seg: &[&str]) -> Option<DocPage> {
    let [_root, s, _sec] = seg else {
        return None;
    };
    let module = tree.module(s)?;
    let section = module
        .sections
        .iter()
        .find(|x| x.node.id.segments() == *seg)?;
    Some(DocPage {
        id: section.node.id.clone(),
        title: section.node.title.clone(),
        kind: DocKind::Section,
        signature: None,
        markdown: section.prose.clone(),
        location: section.node.location.clone(),
        xrefs: Vec::new(),
    })
}

/// Rank every project node by how well it matches `query` (case-insensitive): a title hit scores
/// higher than a body/detail hit. Returns hits sorted best-first, capped at [`SEARCH_LIMIT`].
pub fn search(ctx: &DocCtx, query: &str) -> Vec<DocHit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<DocHit> = Vec::new();
    // The project corpus contributes only when a workspace is open.
    if let Some((env, program)) = ctx.workspace() {
        let tree = ProjectTree::build(env, program);
        tree.for_each(&mut |node: &DocNode, prose: &str| {
            let title_l = node.title.to_lowercase();
            let detail_l = node.detail.as_deref().unwrap_or("").to_lowercase();
            let prose_l = prose.to_lowercase();
            let mut score = 0;
            if title_l == needle {
                score += 100;
            } else if title_l.contains(&needle) {
                score += 40;
            }
            if detail_l.contains(&needle) {
                score += 8;
            }
            if prose_l.contains(&needle) {
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
    // Merge in the language-guide and API-reference corpora so one search spans all three. Scores
    // are on each ranker's own scale; close enough for a combined best-first order at this size.
    hits.extend(guide_search(query));
    hits.extend(api_search(&needle));
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    hits.truncate(SEARCH_LIMIT);
    hits
}

/// The [`DocId`] documenting the declaration whose name span is `name_span`, if it is a project
/// node — the bridge for "show docs for the symbol under the cursor" (the store resolves the
/// cursor to a name span; this maps that span to its doc node).
pub fn id_for_name_span(env: &impl DocEnv, program: &Program, name_span: Span) -> Option<DocId> {
    let tree = ProjectTree::build(env, program);
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
/// leading `fn ` (`sqrt(float): float`).
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
            nodes
        }
        // A qualified name is either a module (→ functions) or an extern type (→ methods).
        [_root, qualified] => {
            if let Some(m) = api::module(qualified) {
                m.functions
                    .iter()
                    .map(|f| api_fn_node(qualified, f, DocKind::Function))
                    .collect()
            } else if let Some(t) = api::type_(qualified) {
                t.methods
                    .iter()
                    .map(|f| api_fn_node(qualified, f, DocKind::Method))
                    .collect()
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
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
    let (kind, members): (DocKind, Vec<api::ApiFn>) = if let Some(m) = api::module(qualified) {
        (DocKind::Module, m.functions)
    } else if let Some(t) = api::type_(qualified) {
        (DocKind::Struct, t.methods)
    } else {
        return None;
    };
    let noun = if kind == DocKind::Module {
        "Functions"
    } else {
        "Methods"
    };
    let mut markdown = String::new();
    if members.is_empty() {
        markdown.push_str(&format!("_No {}._", noun.to_lowercase()));
    } else {
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
    let (f, kind) = match api::function(qualified, name) {
        Some(f) => (f, DocKind::Function),
        None => (api::method(qualified, name)?, DocKind::Method),
    };
    Some(DocPage {
        id: id.clone(),
        title: format!("{qualified}.{name}"),
        kind,
        signature: Some(f.signature),
        markdown: f.doc,
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
    let short = qualified.rsplit('.').next().unwrap_or(qualified).to_lowercase();
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

impl DeclEntry {
    fn member(&self, name: &str) -> Option<&MemberEntry> {
        self.members
            .iter()
            .find(|m| last_segment(&m.node.id) == name)
    }
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

    fn build(env: &dyn DocEnv, program: &Program) -> ProjectTree {
        let docs = ResolvedDocs::collect(program);
        let mut modules: Vec<ModuleEntry> = Vec::new();

        for sym in outline(program) {
            let source = sym.name_span.source;
            let Some(module_name) = env.source_name(source) else {
                continue; // not a project source (e.g. a dependency module)
            };
            let key = source.0.to_string();
            let mod_idx = match modules.iter().position(|m| m.key == key) {
                Some(i) => i,
                None => {
                    modules.push(ModuleEntry {
                        key: key.clone(),
                        node: DocNode {
                            id: DocId::new(format!("{PROJECT_ROOT}/{key}")),
                            title: module_name,
                            kind: DocKind::Module,
                            detail: None,
                            has_page: false,
                            expandable: false,
                            location: None,
                        },
                        prose: docs.module.get(&source).cloned().unwrap_or_default(),
                        decls: Vec::new(),
                        sections: Vec::new(),
                    });
                    modules.len() - 1
                }
            };
            let decl = decl_entry(env, &key, &sym, &docs);
            modules[mod_idx].decls.push(decl);
        }

        // Attach each source's section prose and finalize module page/expandable flags.
        for m in &mut modules {
            let source = SourceId(m.key.parse().unwrap_or(0));
            if let Some(sections) = docs.sections.get(&source) {
                for (i, (span, text)) in sections.iter().enumerate() {
                    m.sections.push(SectionEntry {
                        node: DocNode {
                            id: DocId::new(format!("{}/~{i}", m.node.id.as_str())),
                            title: section_title(text),
                            kind: DocKind::Section,
                            detail: None,
                            has_page: true,
                            expandable: false,
                            location: env.locate(*span),
                        },
                        prose: text.clone(),
                    });
                }
            }
            m.node.expandable = !m.decls.is_empty() || !m.sections.is_empty();
            m.node.has_page = !m.prose.trim().is_empty();
        }

        ProjectTree { modules }
    }
}

fn decl_entry(env: &dyn DocEnv, mod_key: &str, sym: &SymbolNode, docs: &ResolvedDocs) -> DeclEntry {
    let id = DocId::new(format!("{PROJECT_ROOT}/{mod_key}/{}", sym.name));
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
                location: env.locate(child.name_span),
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
            location: env.locate(sym.name_span),
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

    #[test]
    fn the_roots_are_project_guide_api() {
        let roots = roots();
        assert_eq!(roots.len(), 3);
        assert_eq!(roots[0].id.as_str(), "project");
        assert_eq!(roots[1].id.as_str(), "guide");
        assert_eq!(roots[2].id.as_str(), "api");
        assert!(
            roots
                .iter()
                .all(|r| r.kind == DocKind::Root && r.expandable)
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
        assert!(overview.markdown.contains("sqrt"), "overview lists functions");

        let fns = children(&ctx, &math.id);
        let sqrt = fns.iter().find(|f| f.title == "sqrt").expect("math.sqrt");
        assert_eq!(sqrt.kind, DocKind::Function);
        assert!(sqrt.has_page && !sqrt.expandable);
        assert_eq!(sqrt.detail.as_deref(), Some("sqrt(float): float"));

        let rendered = page(&ctx, &sqrt.id).expect("the function page renders");
        assert_eq!(rendered.signature.as_deref(), Some("fn sqrt(float): float"));
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
    fn the_guide_root_lists_pages_that_render_full_markdown() {
        let program = program_of("fn f() {}");
        let env = StubEnv;
        // The guide corpus is workspace-independent — env/program are ignored for guide ids.
        let pages = children(&DocCtx::new(&env, &program), &DocId::new("guide"));
        assert!(pages.len() > 5, "guide root lists the wiki pages");
        assert!(
            pages
                .iter()
                .all(|p| p.kind == DocKind::Guide && p.has_page && !p.expandable)
        );
        let page_node = &pages[0];
        let page = page(&DocCtx::new(&env, &program), &page_node.id).expect("a guide page renders");
        assert_eq!(page.kind, DocKind::Guide);
        assert!(!page.markdown.is_empty());
        // A guide page is a leaf.
        assert!(children(&DocCtx::new(&env, &program), &page_node.id).is_empty());
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
        let project_hit = search(&DocCtx::new(&env, &program), "Widget");
        assert!(
            project_hit
                .iter()
                .any(|h| h.kind == DocKind::Struct && h.title == "Widget")
        );
        let guide_hits = search(&DocCtx::new(&env, &program), "standard library");
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
        let modules = children(&DocCtx::new(&env, &program), &DocId::new("project"));
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].kind, DocKind::Module);
        assert_eq!(modules[0].title, "t.noe");
        assert!(modules[0].expandable);

        // Level 2: the module's declarations.
        let decls = children(&DocCtx::new(&env, &program), &modules[0].id);
        let names: Vec<&str> = decls.iter().map(|d| d.title.as_str()).collect();
        assert_eq!(names, vec!["greet", "Point"]);
        let point = decls.iter().find(|d| d.title == "Point").unwrap();
        assert_eq!(point.kind, DocKind::Struct);
        assert!(point.expandable);

        // Level 3: the struct's fields.
        let fields = children(&DocCtx::new(&env, &program), &point.id);
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
        let modules = children(&DocCtx::new(&env, &program), &DocId::new("project"));
        let decls = children(&DocCtx::new(&env, &program), &modules[0].id);
        let greet = &decls[0];

        let page = page(&DocCtx::new(&env, &program), &greet.id).unwrap();
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
        let modules = children(&DocCtx::new(&env, &program), &DocId::new("project"));
        assert!(modules[0].has_page);
        let page = page(&DocCtx::new(&env, &program), &modules[0].id).unwrap();
        assert_eq!(page.markdown, "The geometry module.");
        assert_eq!(page.kind, DocKind::Module);
    }

    #[test]
    fn search_ranks_a_name_hit_above_a_prose_hit() {
        let program =
            program_of("@doc {\n  A helper about widgets.\n}\nfn helper() {}\nfn widget() {}");
        let env = StubEnv;
        let hits = search(&DocCtx::new(&env, &program), "widget");
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
        let id = id_for_name_span(&env, &program, greet_span).unwrap();
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
        let modules = children(&DocCtx::new(&env, &program), &DocId::new("project"));
        let kids = children(&DocCtx::new(&env, &program), &modules[0].id);
        let section = kids.iter().find(|k| k.kind == DocKind::Section).unwrap();
        assert_eq!(section.title, "Notes");
        let page = page(&DocCtx::new(&env, &program), &section.id).unwrap();
        assert!(page.markdown.contains("Some free prose."));
    }

    #[test]
    fn unknown_id_yields_no_children_and_no_page() {
        let program = program_of("fn f() {}");
        let env = StubEnv;
        assert!(children(&DocCtx::new(&env, &program), &DocId::new("project/99/nope")).is_empty());
        assert!(page(&DocCtx::new(&env, &program), &DocId::new("guide/whatever")).is_none());
        assert!(page(&DocCtx::new(&env, &program), &DocId::new("project/0/nope")).is_none());
    }
}
