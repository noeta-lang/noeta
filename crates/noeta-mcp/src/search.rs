//! `code_search` — the workspace-wide lexical search over the project's own declarations, and the
//! first move for an agent holding a sentence rather than a name.
//!
//! Every other graph tool needs an identity to start from: `symbols` outlines a file, `definition`
//! and `references` take a name, `trace` takes a role or a function. `docs_search` searches the
//! language guide, not the project. So "where does an order get persisted?" had nowhere to begin.
//! This ranks every declaration of the linked program against that sentence, over its name, its
//! qualified path, its `@doc` prose, its signature, its `@role` bindings and attributes, its file
//! path, and the identifiers its body mentions.
//!
//! The ranking is [`noeta_ide::search`]'s BM25F — the same core behind `docs_search`, so one
//! implementation of tokenization, saturation and length normalization serves both. Every result
//! carries the shared [`NodeId`], so a hit is an address the rest of the graph tools accept.

use noeta_ide::search::{CodeIndex, DeclKind, IndexedSource, SearchFilter};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::analyze::{self, LinkStatus, NodeId, NodeKind, Prepared};
use crate::graph::DeclIndex;

/// How many results a request gets when it does not say.
pub const DEFAULT_LIMIT: usize = 10;

/// The most results one request can ask for. A ranked list is for picking a seed from, and a
/// longer one costs the agent context without telling it more.
pub const MAX_LIMIT: usize = 50;

/// Arguments to `code_search`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CodeSearchArgs {
    /// Inline Noeta source to search. Provide this or `file`.
    #[serde(default)]
    pub source: Option<String>,
    /// Path to a `.noe` file in the project to search. Its sibling modules and its `noeta.toml`
    /// dependency packages are linked in, so the search covers the whole program.
    #[serde(default)]
    pub file: Option<String>,
    /// What to look for: a name (`place_order`), a qualified path (`orders.place_order`), or a
    /// plain sentence (`where does an order get written to the database`).
    pub query: String,
    /// Keep only declarations of this kind.
    #[serde(default)]
    pub kind: Option<SearchKind>,
    /// Keep only declarations bearing one of these `@role` bindings — a bare variant
    /// (`EntryPoint`) or a qualified one (`Semantic.EntryPoint`), case-insensitive.
    #[serde(default)]
    pub roles: Option<Vec<String>>,
    /// How many results to return (default 10, max 50).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The declaration kinds `code_search` filters on — the searchable subset of [`NodeKind`], which
/// also names callees no declaration in the program answers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Function,
    Method,
    Struct,
    Class,
    Enum,
    Variant,
    Field,
    Trait,
    Impl,
}

impl From<SearchKind> for DeclKind {
    fn from(kind: SearchKind) -> DeclKind {
        match kind {
            SearchKind::Function => DeclKind::Function,
            SearchKind::Method => DeclKind::Method,
            SearchKind::Struct => DeclKind::Struct,
            SearchKind::Class => DeclKind::Class,
            SearchKind::Enum => DeclKind::Enum,
            SearchKind::Variant => DeclKind::Variant,
            SearchKind::Field => DeclKind::Field,
            SearchKind::Trait => DeclKind::Trait,
            SearchKind::Impl => DeclKind::Impl,
        }
    }
}

/// The graph vocabulary's name for an indexed declaration's kind.
fn node_kind(kind: DeclKind) -> NodeKind {
    match kind {
        DeclKind::Function => NodeKind::Function,
        DeclKind::Method => NodeKind::Method,
        DeclKind::Struct => NodeKind::Struct,
        DeclKind::Class => NodeKind::Class,
        DeclKind::Enum => NodeKind::Enum,
        DeclKind::Variant => NodeKind::Variant,
        DeclKind::Field => NodeKind::Field,
        DeclKind::Trait => NodeKind::Trait,
        DeclKind::Impl => NodeKind::Impl,
    }
}

/// One ranked declaration.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CodeSearchHit {
    /// The declaration's stable identity — the same `id` `symbols`, `trace`, `impact` and
    /// `callers` emit, so a seed found here is one they accept.
    pub id: NodeId,
    pub kind: NodeKind,
    /// The `Enum.Variant` roles it bears.
    pub roles: Vec<String>,
    /// Its rendered signature.
    pub signature: String,
    /// The relevance score. Meaningful **relative to the other hits of the same query** only: a
    /// rare term's match is worth more than a common one's, so scores do not compare across
    /// queries.
    pub score: f32,
    /// Which indexed fields held a query term: `name`, `qualified`, `kind`, `roles`, `doc`,
    /// `signature`, `body`, `path`.
    pub matched_fields: Vec<String>,
    /// A line of evidence: the `@doc` prose that matched, else the signature.
    pub snippet: String,
    /// The `@tier` block it was declared in (`test`, `bench`), when it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// The `code_search` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CodeSearchOutput {
    /// The ranked declarations, best first.
    pub results: Vec<CodeSearchHit>,
    /// How many declarations were searched — the size of the index this answer came from.
    pub indexed: usize,
    /// Whether the search ran over the merged workspace program.
    pub linked: bool,
    /// What stopped the link, in `check`'s diagnostic shape. Empty when `linked`.
    pub link_diagnostics: Vec<noeta_diagnostics::JsonDiagnostic>,
    pub note: Option<String>,
}

/// Answer `code_search`: index the linked program's declarations, rank them against `query`, and
/// return the best `limit` of them.
///
/// The index is built per call from the prepared workspace, so it describes the program as it is
/// on disk right now rather than as it was when some cache was filled.
pub fn code_search(p: &Prepared, args: &CodeSearchArgs) -> CodeSearchOutput {
    let status = LinkStatus::of(p);
    let linked = noeta_db::linked(&p.db, p.ws);
    let entry = noeta_db::ast(&p.db, analyze::entry_program(p));
    let program = match &linked.program {
        Ok(program) => program,
        Err(_) => &entry.0.program,
    };

    // File names as every other tool reports them: relative to the project root.
    let names: Vec<String> = (0..p.sources.len())
        .map(|i| p.file_name(i).unwrap_or_default())
        .collect();
    let sources: Vec<IndexedSource<'_>> = p
        .sources
        .iter()
        .enumerate()
        .map(|(i, s)| IndexedSource {
            name: names[i].as_str(),
            text: s.text(),
        })
        .collect();
    // The linked program first, so its qualified names win; then every workspace member's own
    // parse, because a link reaches only the modules the entry imports and a sibling nothing
    // imports is part of the project all the same.
    let members: Vec<noeta_ast::Program> =
        p.ws.members(&p.db)
            .iter()
            .map(|m| noeta_db::ast_in(&p.db, p.ws, *m).0.program.clone())
            .collect();
    let mut programs: Vec<&noeta_ast::Program> = vec![program];
    programs.extend(members.iter());
    let index = CodeIndex::build_over(&programs, &sources);
    let decls = DeclIndex::build(program);

    let filter = SearchFilter {
        kind: args.kind.map(DeclKind::from),
        roles: args.roles.clone().unwrap_or_default(),
    };
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let hits = index.search(&args.query, &filter, limit);

    let results: Vec<CodeSearchHit> = hits
        .into_iter()
        .map(|hit| {
            let decl = &index.decls()[hit.decl];
            let kind = node_kind(decl.kind);
            // The identity comes from the shared declaration inventory wherever it names the same
            // declaration, so a hit's id is byte-identical to the one `symbols` reports.
            let id = match decls.at_name_span(decl.name_span) {
                Some(found) => found.id(p),
                None => p.node_id(&decl.name, kind, decl.name_span),
            };
            CodeSearchHit {
                id,
                kind,
                roles: decl.roles.clone(),
                signature: decl.signature.clone(),
                score: hit.score,
                matched_fields: hit.matched.iter().map(|f| f.as_str().to_string()).collect(),
                snippet: hit.snippet,
                tier: decl.tier.clone(),
            }
        })
        .collect();

    let note = if args.query.trim().is_empty() {
        Some("`query` is empty — pass a name, a qualified path, or a sentence".to_string())
    } else if results.is_empty() {
        Some(format!(
            "nothing in this workspace's {} declarations matches `{}` — try fewer words, or \
             `symbols` for the outline",
            index.decls().len(),
            args.query.trim()
        ))
    } else {
        status.note()
    };

    CodeSearchOutput {
        results,
        indexed: index.decls().len(),
        linked: status.linked,
        link_diagnostics: status.link_diagnostics,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    const SRC: &str = "\
@doc { Persists a completed purchase to the durable ledger on disk. }
fn place_order(id: int): int { return id }

@doc { One purchase, while it is still being assembled. }
struct Order {
    id: int
    fn total(): int { return self.id }
}
";

    fn search(query: &str, kind: Option<SearchKind>) -> CodeSearchOutput {
        let p = prepare(&Some(SRC.to_string()), &None).expect("prepare");
        code_search(
            &p,
            &CodeSearchArgs {
                source: Some(SRC.to_string()),
                file: None,
                query: query.to_string(),
                kind,
                roles: None,
                limit: None,
            },
        )
    }

    /// A sentence with no identifier in it finds the declaration whose prose answers it, and the
    /// hit carries the shared node identity.
    #[test]
    fn a_prose_query_ranks_the_documented_declaration_and_carries_its_id() {
        let out = search(
            "which code writes a completed purchase to the durable ledger",
            None,
        );
        let top = out.results.first().expect("a hit");
        assert_eq!(top.id.name, "place_order");
        assert_eq!(top.kind, NodeKind::Function);
        assert!(top.id.span.is_some(), "the id locates the declaration");
        assert!(
            top.matched_fields.iter().any(|f| f == "doc"),
            "{:?}",
            top.matched_fields
        );
        assert!(out.indexed >= 4, "indexed {}", out.indexed);
        assert!(out.linked);
    }

    /// `kind` narrows the answer to one shape of declaration.
    #[test]
    fn a_kind_filter_keeps_only_that_kind() {
        let out = search("purchase", Some(SearchKind::Struct));
        assert!(!out.results.is_empty());
        assert!(
            out.results.iter().all(|h| h.kind == NodeKind::Struct),
            "{:?}",
            out.results
                .iter()
                .map(|h| (h.id.name.clone(), h.kind))
                .collect::<Vec<_>>()
        );
    }

    /// A query nothing answers comes back empty with a note that says where to go instead.
    #[test]
    fn an_unmatched_query_says_so() {
        let out = search("kubernetes sidecar", None);
        assert!(out.results.is_empty());
        assert!(out.note.is_some_and(|n| n.contains("symbols")));
    }
}
