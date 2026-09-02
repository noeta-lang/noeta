//! The Understand pillar's compiler-answers-cheaply half: `type_at` and `symbols`. Both ride only
//! the public salsa graph + parsed AST — the shared IDE engine's resolver backs the navigation
//! tools instead (see [`crate::navigate`]).

use crate::analyze::{self, LineIndex, Prepared, SpanLoc};
use noeta_ast::Program;
use noeta_ide::docs as model;
use rmcp::schemars;
use serde::Serialize;

/// The `type_at` result: the inferred type at a site, addressed by symbol name or position.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct TypeAtOutput {
    /// True when a typed expression was found at the site.
    pub found: bool,
    /// The inferred type in surface syntax (`List<int>`, `?string`, `A | B`), empty when not found.
    pub r#type: String,
    /// The span the type belongs to (the tightest typed expression under the site), when found.
    pub location: Option<SpanLoc>,
    /// The byte offset the request resolved to (from `symbol` or `line`/`column`), for transparency.
    pub resolved_offset: Option<u32>,
    /// When not found, a short note on why (no such symbol / no typed expression there).
    pub note: Option<String>,
    /// The type's storage note when non-default (`@packed — 12 bytes`, `flat packed storage — 12
    /// bytes/element, column-major (SoA)`); absent for ordinary boxed storage. The same wording
    /// LSP hover shows (shared `noeta_ide::layout_note`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
}

/// Answer `type_at`. The site is a `symbol` name (first whole-word occurrence in the entry file) or
/// a 1-based `line`/`column`; the type is the tightest `expr_types` span in the entry file covering
/// it — the same "smallest span at the cursor" rule the LSP hover uses, over the same index.
pub fn type_at(
    p: &Prepared,
    symbol: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> TypeAtOutput {
    let text = p.entry_text();
    let index = LineIndex::new(text);
    let ide = noeta_db::linked_checked_ide(&p.db, p.ws);

    let offset = match (symbol, line, column) {
        (Some(name), _, _) => {
            // Prefer the tightest typed *expression* whose source text is exactly this identifier —
            // a use site with a known type — since `expr_types` keys expressions, not declaration
            // sites (a binding target or a parameter name is not itself an expression). Fall back to
            // the first textual occurrence for a symbol that never appears as a bare expression.
            let named = ide
                .expr_types
                .keys()
                .filter(|s| {
                    analyze::in_entry(**s)
                        && s.end > s.start
                        && text.get(s.start as usize..s.end as usize) == Some(name)
                })
                .min_by_key(|s| s.end - s.start)
                .map(|s| s.start);
            match named.or_else(|| analyze::symbol_offset(text, name)) {
                Some(off) => off,
                None => {
                    return not_found(format!("no identifier `{name}` in the entry file"), None);
                }
            }
        }
        (None, Some(l), Some(c)) => index.offset(l, c),
        (None, _, _) => {
            return not_found(
                "provide `symbol` (a name) or both `line` and `column`".to_string(),
                None,
            );
        }
    };

    let tightest = ide
        .expr_types
        .iter()
        .filter(|(span, _)| {
            analyze::in_entry(**span)
                && span.end > span.start
                && span.start <= offset
                && offset <= span.end
        })
        .min_by_key(|(span, _)| span.end - span.start);

    match tightest {
        Some((span, repr)) => TypeAtOutput {
            found: true,
            r#type: repr.to_string(),
            location: Some(index.span_loc(*span)),
            resolved_offset: Some(offset),
            note: None,
            layout: noeta_ide::layout_note(repr, &ide.packed_layouts),
        },
        None => not_found("no typed expression at that site".to_string(), Some(offset)),
    }
}

fn not_found(note: String, offset: Option<u32>) -> TypeAtOutput {
    TypeAtOutput {
        found: false,
        r#type: String::new(),
        location: None,
        resolved_offset: offset,
        note: Some(note),
        layout: None,
    }
}

/// The `symbols` result: the entry file's declaration outline.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SymbolsOutput {
    pub symbols: Vec<SymbolNode>,
}

/// One outline node: a declaration with its kind, a short detail, its span, and its members.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SymbolNode {
    pub name: String,
    /// `function` / `struct` / `class` / `enum` / `impl` / `field` / `variant` / `method`.
    pub kind: String,
    /// A short signature-ish detail (a function's parameter names, an `impl`'s trait+target), when
    /// useful. Parameter *types* are omitted here — call `type_at` or `ast` for precise types.
    pub detail: Option<String>,
    pub location: SpanLoc,
    /// The architectural roles this declaration bears (`Enum.Variant`, from `@role`-tagged
    /// attributes it carries) — the same index `reflect` serves, placed on the map itself.
    pub roles: Vec<String>,
    pub children: Vec<SymbolNode>,
}

/// One `@doc { … }` block of the project, adjacency-resolved: what it
/// documents, where it lives, and its dedented Markdown body.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProjectDoc {
    /// What the block documents: `"module"`, `"decl"`, or `"section"`.
    pub scope: String,
    /// The documented declaration's name (`scope == "decl"` only).
    pub target: Option<String>,
    /// The source file the block lives in.
    pub file: String,
    /// 1-based line of the block.
    pub line: usize,
    /// The dedented Markdown body.
    pub text: String,
}

/// The `project_docs` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ProjectDocsOutput {
    /// Every `@doc` block, in source order (entry first, then linked modules).
    pub docs: Vec<ProjectDoc>,
}

/// Collect the project's own `@doc` documentation — every block across the linked workspace
/// (entry + siblings + imported dependency declarations), adjacency-resolved to what it
/// documents. Works from a bare parse: no type-checking, so docs extract from work-in-progress
/// code (falling back to the entry file alone when a sibling fails to link).
pub fn project_docs(p: &Prepared) -> ProjectDocsOutput {
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry_ast = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = match &linked.program {
        Ok(prog) => prog,
        Err(_) => &entry_ast.0.program,
    };
    let docs = noeta_check::resolve_docs(program)
        .into_iter()
        .map(|doc| {
            let source = p
                .sources
                .iter()
                .find(|s| s.id() == doc.span.source)
                .unwrap_or(&p.sources[0]);
            let (scope, target) = match doc.target {
                noeta_check::DocTarget::Module => ("module".to_string(), None),
                noeta_check::DocTarget::Section => ("section".to_string(), None),
                noeta_check::DocTarget::Decl { name, .. } => ("decl".to_string(), Some(name)),
            };
            ProjectDoc {
                scope,
                target,
                file: source.name().to_string(),
                line: source.line_col(doc.span.start).line as usize,
                text: noeta_check::dedent_doc(&doc.text).trim().to_string(),
            }
        })
        .collect();
    ProjectDocsOutput { docs }
}

// ---- The docs browser over MCP. -------------------------------------------------------------
//
// The agent browses the *same* unified doc model (`noeta_ide::docs`) the LSP docs browser serves —
// both go through it, so the human's tree and the agent's tree can never drift. This is the MCP
// adapter: a `DocEnv` over the prepared workspace plus two tools (`doc_browse`, `doc_page`).

/// The [`model::DocEnv`] over an MCP [`Prepared`]: resolve a declaration span to a workspace
/// location, and name a project source (excluding any dependency source beyond the prepared set).
struct PreparedDocEnv<'a> {
    p: &'a Prepared,
}

impl model::DocEnv for PreparedDocEnv<'_> {
    fn locate(&self, span: noeta_span::Span) -> Option<model::DocLoc> {
        let (uri, sl) = analyze::locate_span(self.p, span)?;
        Some(model::DocLoc {
            uri,
            range: noeta_ide::Range {
                start: noeta_ide::Position {
                    line: sl.start.line.saturating_sub(1),
                    character: sl.start.column.saturating_sub(1),
                },
                end: noeta_ide::Position {
                    line: sl.end.line.saturating_sub(1),
                    character: sl.end.column.saturating_sub(1),
                },
            },
        })
    }

    fn source_name(&self, source: noeta_span::SourceId) -> Option<String> {
        self.p
            .sources
            .get(source.0 as usize)
            .map(|s| basename(s.name()))
    }
}

fn basename(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).to_string()
}

/// Every workspace member's own program — the project corpus documents the whole workspace
/// (a sibling the entry never imports included), each module from its own parse. Bare parses,
/// so the docs browser works on work-in-progress code, exactly as [`project_docs`] does.
fn member_programs(p: &Prepared) -> Vec<(noeta_span::SourceId, Program)> {
    p.ws.members(&p.db)
        .iter()
        .map(|&sp| {
            (
                noeta_span::SourceId(sp.id(&p.db)),
                noeta_db::ast_in(&p.db, p.ws, sp).0.program.clone(),
            )
        })
        .collect()
}

/// Borrow `member_programs`' output as the [`model::MemberDoc`] list a [`model::DocCtx`] takes.
fn member_docs(owned: &[(noeta_span::SourceId, Program)]) -> Vec<model::MemberDoc<'_>> {
    owned
        .iter()
        .map(|(source, program)| model::MemberDoc {
            source: *source,
            program,
        })
        .collect()
}

/// One node of the doc tree for the agent: a navigation row plus its 1-based source line.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocNodeOut {
    /// The node's stable id — pass it back to `doc_browse` (to expand) or `doc_page` (to read).
    pub id: String,
    pub title: String,
    /// `root` / `module` / `function` / `struct` / `class` / `enum` / `variant` / `field` /
    /// `method` / `interface` / `section`.
    pub kind: String,
    /// A short signature-like detail, when useful.
    pub detail: Option<String>,
    /// Whether `doc_page` yields a body worth reading for this node.
    pub has_page: bool,
    /// Whether `doc_browse` on this id yields further children.
    pub expandable: bool,
    /// The declaring source file, when the node has a location.
    pub file: Option<String>,
    /// The declaration's 1-based line, when located.
    pub line: Option<u32>,
}

impl From<model::DocNode> for DocNodeOut {
    fn from(n: model::DocNode) -> DocNodeOut {
        let loc = n.location;
        DocNodeOut {
            id: n.id.0,
            title: n.title,
            kind: n.kind.as_str().to_string(),
            detail: n.detail,
            has_page: n.has_page,
            expandable: n.expandable,
            file: loc.as_ref().map(|l| l.uri.clone()),
            line: loc.as_ref().map(|l| l.range.start.line + 1),
        }
    }
}

/// The `doc_browse` result: the children (or roots) at the requested level.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocBrowseOutput {
    pub nodes: Vec<DocNodeOut>,
}

/// A cross-reference to a related doc node (e.g. an API symbol's "see also" guide pages).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocXrefOut {
    /// The related node's id — pass it to `doc_page`/`doc_browse`.
    pub id: String,
    pub title: String,
}

/// A rendered doc page for the agent.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocPageOutput {
    pub found: bool,
    pub id: String,
    pub title: String,
    pub kind: String,
    pub signature: Option<String>,
    pub markdown: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// Related nodes — for an API symbol, the guide pages that mention it.
    pub xrefs: Vec<DocXrefOut>,
}

/// Browse one level of the project's doc tree: the corpus roots when `id` is omitted, else the
/// children of `id` (root → modules → declarations → members).
pub fn doc_browse(p: &Prepared, id: Option<&str>) -> DocBrowseOutput {
    let owned = member_programs(p);
    let env = PreparedDocEnv { p };
    let ctx = model::DocCtx::new(&env, member_docs(&owned));
    let nodes = match id {
        None => model::roots(),
        Some(id) => model::children(&ctx, &model::DocId(id.to_string())),
    };
    DocBrowseOutput {
        nodes: nodes.into_iter().map(DocNodeOut::from).collect(),
    }
}

/// The rendered page (signature + `@doc` prose + location) for a node `id`.
pub fn doc_page(p: &Prepared, id: &str) -> DocPageOutput {
    let owned = member_programs(p);
    let env = PreparedDocEnv { p };
    let ctx = model::DocCtx::new(&env, member_docs(&owned));
    match model::page(&ctx, &model::DocId(id.to_string())) {
        Some(page) => DocPageOutput {
            found: true,
            id: page.id.0,
            title: page.title,
            kind: page.kind.as_str().to_string(),
            signature: page.signature,
            markdown: page.markdown,
            file: page.location.as_ref().map(|l| l.uri.clone()),
            line: page.location.as_ref().map(|l| l.range.start.line + 1),
            xrefs: page
                .xrefs
                .into_iter()
                .map(|x| DocXrefOut {
                    id: x.id.0,
                    title: x.title,
                })
                .collect(),
        },
        None => DocPageOutput {
            found: false,
            id: id.to_string(),
            title: String::new(),
            kind: String::new(),
            signature: None,
            markdown: String::new(),
            file: None,
            line: None,
            xrefs: Vec::new(),
        },
    }
}

/// Build the entry file's outline: top-level `fn`/`struct`/`class`/`enum`/`impl`, with fields,
/// variants, and methods as one level of children, in source order. Walks the parsed AST (available
/// even when the program has type errors), not the merged workspace, so it describes *this file*.
///
/// The walk itself is the shared [`noeta_ide::symbols::outline`] — the same engine the LSP's
/// document-symbols serves — so the agent's outline and the editor's outline can never drift
/// (audit-4 finding 8). This adapter only reshapes onto the MCP wire: kind strings, the
/// types-omitted `fn name(a, b)` detail convention, 1-based locations, and the `@role` post-pass.
pub fn symbols(p: &Prepared) -> SymbolsOutput {
    let parsed = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program: &Program = &parsed.0.program;
    let index = LineIndex::new(p.entry_text());
    let mut symbols: Vec<SymbolNode> = noeta_ide::symbols::outline(program)
        .iter()
        .map(|node| from_outline(node, &index))
        .collect();

    // Annotate the outline with the `@role` index (over the merged program, so a role conferred by
    // an attribute declared in a sibling module still lands; entry-file targets only, since the
    // outline is entry-file only). Nested members key as `Type.member`, matching the index.
    let linked = noeta_db::linked(&p.db, p.ws);
    let role_program: &Program = match &linked.program {
        Ok(prog) => prog,
        Err(_) => program,
    };
    let native_roles = noeta_stdlib::registry::single_registry_process().native_roles();
    let info = noeta_ast::reflect::build(role_program, &native_roles, &Default::default());
    let mut role_map: std::collections::HashMap<&str, Vec<String>> =
        std::collections::HashMap::new();
    for r in &info.roles {
        if analyze::in_entry(r.target_span) {
            role_map
                .entry(r.target.as_str())
                .or_default()
                .push(format!("{}.{}", r.enum_name, r.variant));
        }
    }
    annotate_roles(&mut symbols, None, &role_map);
    SymbolsOutput { symbols }
}

/// Attach roles to each node: a top-level node keys by its name, a member by `Parent.name` (the
/// index's `Type.member` convention). `impl` nodes have no single name and never match.
fn annotate_roles(
    nodes: &mut [SymbolNode],
    parent: Option<&str>,
    role_map: &std::collections::HashMap<&str, Vec<String>>,
) {
    for node in nodes {
        let key = match parent {
            Some(parent) => format!("{parent}.{}", node.name),
            None => node.name.clone(),
        };
        if let Some(roles) = role_map.get(key.as_str()) {
            node.roles = roles.clone();
        }
        let name = node.name.clone();
        annotate_roles(&mut node.children, Some(&name), role_map);
    }
}

/// Reshape one shared-outline node onto the MCP wire. The location is the whole declaration's
/// span (the shared walk's `full_span`); the detail keeps this tool's convention — `fn name(p0,
/// p1)` for callables (parameter names only; precise types come from `type_at` / `ast`), nothing
/// for the other kinds (whose LSP-facing detail carries types the tool deliberately omits).
fn from_outline(node: &noeta_ide::symbols::SymbolNode, index: &LineIndex) -> SymbolNode {
    use noeta_ide::symbols::SymbolKind as K;
    let kind = match node.kind {
        K::Function => "function",
        K::Struct => "struct",
        K::Class => "class",
        K::Enum => "enum",
        K::EnumMember => "variant",
        K::Field => "field",
        K::Method => "method",
        K::Interface => "impl",
        K::Trait => "trait",
    };
    let detail = matches!(node.kind, K::Function | K::Method)
        .then(|| format!("fn {}({})", node.name, node.param_names.join(", ")));
    SymbolNode {
        name: node.name.clone(),
        kind: kind.to_string(),
        detail,
        location: index.span_loc(node.full_span),
        roles: Vec::new(),
        children: node
            .children
            .iter()
            .map(|child| from_outline(child, index))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    #[test]
    fn project_docs_resolves_scopes_and_targets() {
        let src = "@doc { The module. }\n\
                   use std.math.sqrt\n\
                   @doc { Adds two ints. }\n\
                   fn add(a: int, b: int): int { return a + b }\n";
        let p = prepare(&Some(src.to_string()), &None).expect("prepare");
        let out = project_docs(&p);
        assert_eq!(out.docs.len(), 2);
        assert_eq!(out.docs[0].scope, "module");
        assert_eq!(out.docs[0].text, "The module.");
        assert_eq!(out.docs[1].scope, "decl");
        assert_eq!(out.docs[1].target.as_deref(), Some("add"));
        assert!(out.docs[1].line >= 3);
    }

    /// The MCP docs adapter and the LSP `DocumentStore` browse the *same* unified model, so the two
    /// trees must be structurally identical over identical source — the "cannot drift" guarantee.
    #[test]
    fn doc_browse_matches_the_lsp_document_store_tree() {
        let src = "@doc { Adds two ints. }\n\
                   fn add(a: int, b: int): int { return a + b }\n\
                   struct Point { x: int }\n";

        // The MCP path: prepare + doc_browse/doc_page.
        let p = prepare(&Some(src.to_string()), &None).expect("prepare");
        let mcp_mods = doc_browse(&p, Some("project")).nodes;
        let mcp_decls = doc_browse(&p, Some(mcp_mods[0].id.as_str())).nodes;

        // The LSP path: a DocumentStore over the same source.
        let mut store = noeta_ide::DocumentStore::default();
        store.open("file:///t.noe", src.to_string());
        let enc = noeta_ide::Encoding::Utf8;
        let lsp_mods = store.doc_children("file:///t.noe", "project", enc);
        let lsp_decls = store.doc_children("file:///t.noe", lsp_mods[0].id.as_str(), enc);

        // Same declarations, same ids/kinds/details/flags (locations may differ by encoding).
        let mcp_shape: Vec<_> = mcp_decls
            .iter()
            .map(|n| {
                (
                    n.id.as_str(),
                    n.title.as_str(),
                    n.kind.as_str(),
                    n.has_page,
                    n.expandable,
                )
            })
            .collect();
        let lsp_shape: Vec<_> = lsp_decls
            .iter()
            .map(|n| {
                (
                    n.id.as_str(),
                    n.title.as_str(),
                    n.kind.as_str(),
                    n.has_page,
                    n.expandable,
                )
            })
            .collect();
        assert_eq!(mcp_shape, lsp_shape);
        assert_eq!(mcp_shape[0].0, "project/0/add");

        // And the page bodies agree.
        let mcp_page = doc_page(&p, "project/0/add");
        let lsp_page = store
            .doc_page("file:///t.noe", "project/0/add", enc)
            .unwrap();
        assert_eq!(mcp_page.markdown, lsp_page.markdown);
        assert_eq!(mcp_page.markdown, "Adds two ints.");
        assert_eq!(mcp_page.signature, lsp_page.signature);
    }

    const SRC: &str = "\
fn handle(n: int): int {
  xs = [1, 2, 3];
  return n + xs.len();
}

struct Point { x: int; y: int }

enum Color { Red; Green }
";

    fn prep() -> Prepared {
        prepare(&Some(SRC.to_string()), &None).unwrap()
    }

    #[test]
    fn type_at_a_symbol_reports_the_inferred_type() {
        let p = prep();
        // `xs` is a `List<int>` binding; `n` is an `int` parameter — both recovered from a use site.
        let xs = type_at(&p, Some("xs"), None, None);
        assert!(xs.found, "note: {:?}", xs.note);
        assert_eq!(xs.r#type, "List<int>");
        assert!(xs.location.is_some());

        let n = type_at(&p, Some("n"), None, None);
        assert!(n.found);
        assert_eq!(n.r#type, "int");
    }

    #[test]
    fn type_at_notes_packed_storage() {
        let p = prepare(
            &Some(
                "@packed(Layout.Column) struct Vec3 { x: f32; y: f32; z: f32 }\n\
                 v = Vec3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }\n\
                 vs = [v]\n\
                 echo vs.len()\n"
                    .to_string(),
            ),
            &None,
        )
        .unwrap();
        let v = type_at(&p, Some("v"), None, None);
        assert_eq!(v.r#type, "Vec3");
        assert_eq!(
            v.layout.as_deref(),
            Some("@packed — 12 bytes, column-major lists (SoA)")
        );
        let vs = type_at(&p, Some("vs"), None, None);
        assert_eq!(vs.r#type, "List<Vec3>");
        assert_eq!(
            vs.layout.as_deref(),
            Some("flat packed storage — 12 bytes/element, column-major (SoA)")
        );
        // Ordinary boxed storage says nothing.
        let xs = type_at(&prep(), Some("xs"), None, None);
        assert_eq!(xs.layout, None);
    }

    #[test]
    fn type_at_reports_missing_and_unknown_sites() {
        let p = prep();
        assert!(!type_at(&p, Some("nope"), None, None).found);
        // Neither symbol nor position: an explanatory miss, not a panic.
        let none = type_at(&p, None, None, None);
        assert!(!none.found);
        assert!(none.note.unwrap().contains("symbol"));
    }

    /// Pins the `symbols` tool's exact wire JSON over every node kind — names, kind strings,
    /// detail conventions (param-names-only fn details, `null` elsewhere), 1-based locations, and
    /// nesting — so rebasing the walk onto the shared `noeta_ide::symbols::outline` engine
    /// (audit-4 finding 8) is provably behavior-preserving for agents.
    #[test]
    fn symbols_wire_json_is_pinned_across_all_node_kinds() {
        let src = "\
fn add(a: int, b: int): int { return a + b }

struct Point {
  x: int
  fn norm(): int { return self.x }
}

enum Shape {
  Dot
  Circle(radius: int)
  fn area(): int { return 0 }
}

impl Show for Point {
  fn show(): int { return 1 }
}
";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = serde_json::to_value(symbols(&p)).unwrap();
        let loc = |sl: u32, sc: u32, so: u32, el: u32, ec: u32, eo: u32| {
            serde_json::json!({
                "start": {"line": sl, "column": sc, "offset": so},
                "end": {"line": el, "column": ec, "offset": eo},
            })
        };
        let expected = serde_json::json!({
            "symbols": [
                {
                    "name": "add", "kind": "function", "detail": "fn add(a, b)",
                    "location": loc(1, 1, 0, 1, 45, 44), "roles": [], "children": [],
                },
                {
                    "name": "Point", "kind": "struct", "detail": null,
                    "location": loc(3, 1, 46, 6, 2, 106), "roles": [],
                    "children": [
                        {
                            "name": "x", "kind": "field", "detail": null,
                            "location": loc(4, 3, 63, 4, 9, 69), "roles": [], "children": [],
                        },
                        {
                            "name": "norm", "kind": "method", "detail": "fn norm()",
                            "location": loc(5, 3, 72, 5, 35, 104), "roles": [], "children": [],
                        },
                    ],
                },
                {
                    "name": "Shape", "kind": "enum", "detail": null,
                    "location": loc(8, 1, 108, 12, 2, 180), "roles": [],
                    "children": [
                        {
                            "name": "Dot", "kind": "variant", "detail": null,
                            "location": loc(9, 3, 123, 9, 6, 126), "roles": [], "children": [],
                        },
                        {
                            "name": "Circle", "kind": "variant", "detail": null,
                            "location": loc(10, 3, 129, 10, 22, 148), "roles": [], "children": [],
                        },
                        {
                            "name": "area", "kind": "method", "detail": "fn area()",
                            "location": loc(11, 3, 151, 11, 30, 178), "roles": [], "children": [],
                        },
                    ],
                },
                {
                    "name": "Show for Point", "kind": "impl", "detail": null,
                    "location": loc(14, 1, 182, 16, 2, 235), "roles": [],
                    "children": [
                        {
                            "name": "show", "kind": "method", "detail": "fn show()",
                            "location": loc(15, 3, 206, 15, 30, 233), "roles": [], "children": [],
                        },
                    ],
                },
            ],
        });
        assert_eq!(out, expected);
    }

    /// Pins `type_at`'s exact wire locations (1-based line, 1-based UTF-8 byte column, byte
    /// offset) ahead of the finding-8 LineIndex rebase — the span math must survive verbatim.
    #[test]
    fn type_at_wire_locations_are_pinned() {
        let p = prep();
        // The symbol-named lookup resolves `xs` to its typed *use site* — the `xs.len()` receiver
        // on line 3 (byte offset 56) — since declaration targets are not expressions.
        let xs = type_at(&p, Some("xs"), None, None);
        let out = serde_json::to_value(&xs).unwrap();
        assert_eq!(out["found"], serde_json::json!(true));
        assert_eq!(out["type"], serde_json::json!("List<int>"));
        assert_eq!(
            out["location"],
            serde_json::json!({
                "start": {"line": 3, "column": 14, "offset": 56},
                "end": {"line": 3, "column": 16, "offset": 58},
            })
        );
        assert_eq!(out["resolved_offset"], serde_json::json!(56));

        // Position-addressed: line 3 column 10 sits on `n` inside `return n + xs.len()`.
        let at = type_at(&p, None, Some(3), Some(10));
        assert!(at.found);
        assert_eq!(at.r#type, "int");
        assert_eq!(at.resolved_offset, Some(52));
    }

    #[test]
    fn symbols_outlines_declarations_and_members() {
        let p = prep();
        let out = symbols(&p);
        let kinds: Vec<(&str, &str)> = out
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect();
        assert!(kinds.contains(&("handle", "function")));
        assert!(kinds.contains(&("Point", "struct")));
        assert!(kinds.contains(&("Color", "enum")));
        // The struct carries its fields as children.
        let point = out.symbols.iter().find(|s| s.name == "Point").unwrap();
        let fields: Vec<&str> = point.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(fields, vec!["x", "y"]);
        // The function detail lists parameter names.
        let handle = out.symbols.iter().find(|s| s.name == "handle").unwrap();
        assert_eq!(handle.detail.as_deref(), Some("fn handle(n)"));
    }

    #[test]
    fn symbols_carries_architectural_roles() {
        // `@role(Semantic.EntryPoint)` rides the `Route` attribute; `handle` bears `#[Route]`, so
        // the outline node for `handle` shows the role — the architecture on the map itself.
        let src = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

#[Route(\"/x\")]
fn handle(n: int): int { return n }

fn helper(): int { return 1 }
";
        let p = prepare(&Some(src.to_string()), &None).unwrap();
        let out = symbols(&p);
        let handle = out.symbols.iter().find(|s| s.name == "handle").unwrap();
        assert_eq!(handle.roles, vec!["Semantic.EntryPoint"]);
        let helper = out.symbols.iter().find(|s| s.name == "helper").unwrap();
        assert!(helper.roles.is_empty(), "unannotated fn carries no role");
    }
}
