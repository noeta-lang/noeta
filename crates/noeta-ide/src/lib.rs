//! The shared Noeta IDE engine (MCP arc, slice **M5** — extracted from `noeta-lsp`).
//!
//! Every editor-facing language feature over the compiler's salsa query graph (`noeta-db`), with
//! **no wire protocol**: the [`DocumentStore`] owns a [`LangDatabase`], the open buffers, and one
//! [`Workspace`] per **directory** with an open document — the directory's `.noe` members (open
//! buffers overlaying disk) plus resolved dependency packages, SHARED by every open document in
//! it. Each document reads its own merged program through the entry-parametric
//! [`linked_from`](noeta_db::linked_from) query family (memoized per `(workspace, document)`),
//! so the per-file lex/parse work memoizes once per file no matter how many documents are open
//! (audit-4 finding 6). Every language feature is then a *read* of a memoized query; editing a
//! document calls the salsa `set_text` setter on its ONE input, and salsa recomputes only the
//! queries that edit invalidated. That incremental spine is inherited wholesale from M1, not
//! built here.
//!
//! Features: **live diagnostics** over the whole-workspace `linked_checked_ide` query — the same
//! single checker run every other feature reads, so one edit checks once (an imported
//! name resolves across siblings); **hover types** (the tightest enclosing `expr_types` span,
//! rendered to surface syntax); **go-to-definition** — a scope-aware value index for locals,
//! parameters, and functions (shadowing-correct), member accesses `x.member` via the receiver's
//! type, and top-level name tables for type references, cross-file over the merged workspace
//! program (see [`resolve`]); **find-references** and **rename** over the same occurrence index;
//! **document symbols** (see [`symbols`]); **signature help** (see [`signature`]); **semantic
//! tokens** (see [`semtokens`]); **completion** — member, bare-dot, type-position, and identifier
//! forms (see [`completion`]); **inlay type hints** (see [`inlay`]); **formatting** over the
//! shared `noeta fmt` engine; and the **call hierarchy** (see [`callgraph`]; ide-ui U0) — cursor →
//! function, incoming/outgoing call groups with `@role` bindings on every item, the same
//! graph+reflection join the MCP `trace` tool serves. Positions convert encoding-aware (see
//! [`offsets`]).
//!
//! The engine speaks its own positional types ([`Position`], [`Range`], [`TextEdit`], …) —
//! field-compatible with their LSP counterparts but owned here, so `noeta lsp` (JSON-RPC) and
//! `noeta mcp` (MCP tools) are both thin adapters over this one implementation and can never
//! drift.

pub mod api;
pub mod callgraph;
pub mod completion;
pub mod docs;
pub mod guide;
pub mod highlight;
pub mod impact;
pub mod inlay;
pub mod offsets;
pub mod resolve;
pub mod semtokens;
pub mod signature;
pub mod symbols;
pub mod trace;
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use noeta_ast::reflect::{PackedLayout, TypeRepr};
use noeta_db::{LangDatabase, SourceProgram};
use noeta_lexer::TokenKind;
use noeta_span::{SourceId, Span};
use salsa::Setter;

use crate::workspace::{
    WorkspaceCache, disk_noe_uris, edition_of_uri, uri_to_path, workspace_key,
};

pub use offsets::{Encoding, LineIndex, Position, Range};
pub use semtokens::SemanticToken;
pub use symbols::{DocumentSymbol, SymbolKind};

/// Run one read over a [`DocumentStore::snapshot`], absorbing salsa **cancellation**: `None` when
/// a concurrent input write unwound the read mid-query (`salsa::Cancelled` — the result would have
/// been stale anyway), `Some` otherwise. Any *other* panic propagates unchanged — cancellation is
/// control flow, a checker bug is still a bug. Owned here so adapters (`noeta-lsp`) need no salsa
/// dependency to speak the contract.
///
/// `AssertUnwindSafe` is sound: on unwind the closure's captures (the snapshot) are dropped, and
/// the shared salsa storage is designed to be left consistent by a `Cancelled` unwind — that is
/// the mechanism's whole point.
pub fn catch_cancelled<T>(f: impl FnOnce() -> T) -> Option<T> {
    salsa::Cancelled::catch(std::panic::AssertUnwindSafe(f)).ok()
}

/// One replacement edit: substitute `new_text` for the text at `range`. Field-compatible with the
/// LSP wire `TextEdit`, owned here so the engine stays wire-protocol-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// The name of the synthetic caller representing the program's **top-level statements** — Noeta's
/// entry point (there is no `main`). Adapters treat an item with this name as an anchor to jump
/// to, never a node to expand (its selection range sits on a call site, not a declaration).
pub const TOP_LEVEL: &str = "(top level)";

/// A function in the call hierarchy (ide-ui U0): its name (`Type.method` for methods), the `@role`
/// bindings it bears (`Enum.Variant`, from the merged program's reflection index — the item
/// detail), and where it lives. Field-compatible with the LSP `CallHierarchyItem` essentials.
#[derive(Debug, Clone, PartialEq)]
pub struct HierarchyItem {
    pub name: String,
    /// [`SymbolKind::Method`] for a method, else [`SymbolKind::Function`] (including the synthetic
    /// `(top level)` caller — Noeta's entry is the top-level statement sequence).
    pub kind: SymbolKind,
    pub roles: Vec<String>,
    pub uri: String,
    /// The whole declaration.
    pub range: Range,
    /// The declared name (where the editor puts the cursor on jump).
    pub selection_range: Range,
}

/// One call-hierarchy answer: the function at the other end, plus every use site — each flagged
/// syntactic call (`true`) vs reference (`false`: the function passed as a value — a callback or
/// handler registration, still part of the flow). Site ranges are in the **caller's** document,
/// whichever direction was asked (LSP `fromRanges` semantics).
#[derive(Debug, Clone)]
pub struct HierarchyCall {
    pub item: HierarchyItem,
    pub sites: Vec<(Range, bool)>,
}

/// One `@role` binding located in a document (ide-ui U2) — the data behind a role CodeLens.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleLens {
    /// The annotated declaration's *name* range (where the lens hangs).
    pub range: Range,
    /// The role, `Enum.Variant`.
    pub role: String,
    /// The bearer's declaration name (`handle`, `Counter.bump`, a type name).
    pub target: String,
    /// True when the bearer is a function in the call graph — a trace can start there. A role on
    /// a non-function declaration (a role-annotated struct/class) is informational only.
    pub traceable: bool,
}

/// One node of the Architecture view (ide-ui U3): a role bearer, or a callee reached from one —
/// located when it lives in source, honestly labeled when it doesn't.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchNode {
    pub name: String,
    /// The `Enum.Variant` roles this declaration bears.
    pub roles: Vec<String>,
    /// The declaration's location; absent for external/dynamic leaves.
    pub uri: Option<String>,
    pub range: Option<Range>,
    /// Reached as a passed value rather than a syntactic call.
    pub reference: bool,
    pub external: bool,
    pub dynamic: bool,
    pub cycle: bool,
    /// Has further outgoing calls — the tree can expand it (lazily, one level per request).
    pub expandable: bool,
}

/// One Architecture-view group: a role and every declaration bearing it, in declaration order.
#[derive(Debug, Clone)]
pub struct ArchRole {
    pub role: String,
    pub bearers: Vec<ArchNode>,
}

/// One `@test` fn discovered in a document (ide-ui U3): what the editor's test explorer lists and
/// the gutter run-arrows anchor to. `name` is the fn to pass to `noeta test --name`; `display` is
/// the `#[Name("…")]` label when present.
#[derive(Debug, Clone, PartialEq)]
pub struct TestItem {
    pub name: String,
    pub display: Option<String>,
    pub group: Option<String>,
    pub skipped: bool,
    /// The test fn's declaration range in the document.
    pub range: Range,
}

/// The server's document state: the salsa database, the open editor buffers, and one cached
/// [`WorkspaceCache`] per **directory** with an open document (every open document in a directory
/// shares it). Kept behind a [`Mutex`] on the [`Backend`]; the request handlers lock it, do their
/// (synchronous, fast) salsa work, and release it before awaiting any client I/O.
///
/// Split out from [`Backend`] so it can be unit-tested without a live [`Client`].
#[derive(Default)]
pub struct DocumentStore {
    db: LangDatabase,
    /// Open documents: URI → current buffer text (the authoritative content, possibly unsaved).
    buffers: HashMap<String, String>,
    /// One workspace per directory with an open document, keyed by [`workspace_key`].
    workspaces: HashMap<String, WorkspaceCache>,
    /// Bumped by every document mutation ([`open`](Self::open) / [`change`](Self::change) /
    /// [`close`](Self::close)) — the supersede check for reads that ran off the lock on a
    /// [`snapshot`](Self::snapshot): a result computed at revision *r* is stale (and must not be
    /// delivered) unless the store still reads *r* afterwards (audit-4 finding 9).
    revision: u64,
}

impl std::fmt::Debug for DocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentStore")
            .field("open", &self.buffers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl DocumentStore {
    /// Register or replace an open document's buffer, then refresh its directory's shared
    /// workspace (creating it if this is the directory's first open document). Other directories'
    /// workspaces are untouched — their inputs cannot have changed.
    pub fn open(&mut self, uri: &str, text: String) {
        self.revision += 1;
        self.buffers.insert(uri.to_string(), text);
        self.refresh_workspace(&workspace_key(uri));
    }

    /// Apply a full-document change: replace the buffer and push the new text into the salsa
    /// input. Returns the changed document's input (for callers/tests that re-query it).
    ///
    /// The hot path (every keystroke). A buffer edit cannot change the file *set* or any sibling's
    /// on-disk content, so — unlike open/close — there is nothing to re-discover: the new text is
    /// pushed straight into the document's ONE shared input (every open document in the directory
    /// reads it), with **no directory scan and no disk reads**. Only a change to a document with no
    /// workspace yet (an editor that skipped `didOpen`) falls back to a full build.
    pub fn change(&mut self, uri: &str, text: String) -> SourceProgram {
        self.revision += 1;
        self.buffers.insert(uri.to_string(), text);
        let known = self
            .workspaces
            .get(&workspace_key(uri))
            .is_some_and(|cache| cache.source_uris.iter().any(|u| u == uri));
        if known {
            self.propagate(uri);
        } else {
            self.refresh_workspace(&workspace_key(uri));
        }
        self.doc_cache(uri)
            .expect("refresh registered the changed document")
            .1
    }

    /// Push the changed document's current buffer text into the salsa input that represents it —
    /// there is exactly one, in its directory's shared workspace — without re-reading the
    /// directory or any file. Every open document of the directory (importers included) reads the
    /// same input, so the edit is visible to all of them by construction.
    fn propagate(&mut self, changed_uri: &str) {
        let Some(text) = self.buffers.get(changed_uri).cloned() else {
            return;
        };
        let target = self.doc_cache(changed_uri).map(|(_, program, _)| program);
        if let Some(program) = target {
            program.set_text(&mut self.db).to(text);
        }
    }

    /// Drop a closed document's buffer. While other documents in its directory stay open, the
    /// shared workspace is refreshed instead of dropped — the closed member reverts to its on-disk
    /// content (or leaves the member set if it was never saved). The last close drops the
    /// directory's workspace.
    pub fn close(&mut self, uri: &str) {
        self.revision += 1;
        self.buffers.remove(uri);
        let key = workspace_key(uri);
        if self.buffers.keys().any(|open| workspace_key(open) == key) {
            self.refresh_workspace(&key);
        } else {
            self.workspaces.remove(&key);
        }
    }

    /// The URIs of the open documents.
    pub fn open_uris(&self) -> Vec<String> {
        self.buffers.keys().cloned().collect()
    }

    /// The store's current mutation revision — see the field docs. Capture it together with a
    /// [`snapshot`](Self::snapshot); compare after the off-lock read to detect a superseded result.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// A read view for running one expensive feature **off the store's lock**, so a newer edit can
    /// cancel it instead of queueing behind it (audit-4 finding 9). The snapshot shares the salsa
    /// storage — its queries read (and memoize into) the same graph — plus copies of the
    /// buffer/workspace tables, so every [`DocumentStore`] read method works on it unchanged.
    ///
    /// The cancellation contract (salsa 0.27): while any snapshot is alive, an input write on the
    /// primary store (`open`/`change`/`close`) first flags cancellation — the snapshot's in-flight
    /// queries unwind with `salsa::Cancelled` (run reads under [`catch_cancelled`]) — and then
    /// **blocks until every snapshot handle is dropped**. So a holder must:
    /// - drop the snapshot promptly after one read (never park it),
    /// - never wait for the primary store's lock while still holding a snapshot (deadlock: the
    ///   writer holds the lock and waits for the snapshot), and
    /// - never take a snapshot and mutate the primary store from the same thread concurrently.
    ///
    /// This is deliberately **not** a frozen version: the snapshot reads whatever revision the
    /// storage is at. A result is only revision-consistent if [`revision`](Self::revision) is
    /// unchanged across the read — otherwise discard it (the write that bumped it re-publishes).
    pub fn snapshot(&self) -> DocumentStore {
        DocumentStore {
            db: self.db.clone(),
            buffers: self.buffers.clone(),
            workspaces: self.workspaces.clone(),
            revision: self.revision,
        }
    }

    /// The document's workspace-wide text-tier set (text-tiers arc) — what keeps a `@<name> { … }`
    /// body verbatim under formatting when the tier is declared in a sibling or dependency. A
    /// document with no workspace falls back to the default set (same-file declarations are
    /// discovered by the lexer itself).
    fn text_tiers_of(&self, uri: &str) -> noeta_lexer::TextTiers {
        match self.workspaces.get(&workspace_key(uri)) {
            Some(cache) => noeta_lexer::TextTiers::with(
                noeta_db::workspace_text_tiers(&self.db, cache.workspace)
                    .iter()
                    .cloned(),
            ),
            None => noeta_lexer::TextTiers::default(),
        }
    }

    /// Format the whole document at `uri` into the canonical style, returning a single
    /// full-document replacement edit — or `None` if the document is not open, or an empty edit list
    /// if it is already canonical. Style is the nearest `noeta.toml` `[fmt]` config (defaults if
    /// none); a source that does not parse (or an internal safety abort) yields no edits, leaving the
    /// buffer untouched. The same `noeta_fmt` engine as `noeta fmt`, so editor and CLI agree.
    pub fn format_document(&self, uri: &str, encoding: Encoding) -> Option<Vec<TextEdit>> {
        let text = self.buffers.get(uri)?;
        let config = uri_to_path(uri)
            .map(|p| noeta_pm::manifest::resolve_fmt_config_lenient(&p))
            .unwrap_or_default();
        let tiers = self.text_tiers_of(uri);
        let edition = edition_of_uri(uri);
        let formatted = noeta_fmt::format_source_in(uri, text, &config, edition, &tiers).ok()?;
        if formatted == *text {
            return Some(Vec::new()); // already canonical — no edit, no churn
        }
        let end = LineIndex::new(text).position(text.len() as u32, encoding);
        Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), end),
            new_text: formatted,
        }])
    }

    /// On-type formatting: reformat just the top-level statement containing `position` (e.g. the
    /// block the user just closed with `}`). Returns `None` unless the whole document parses and the
    /// statement is not already canonical — so it is quiet while code is mid-typed and never edits
    /// unsafely (the same AST-preserving guarantee as [`Self::format_document`]).
    pub fn format_on_type(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<Vec<TextEdit>> {
        let text = self.buffers.get(uri)?;
        let config = uri_to_path(uri)
            .map(|p| noeta_pm::manifest::resolve_fmt_config_lenient(&p))
            .unwrap_or_default();
        let index = LineIndex::new(text);
        let offset = index.offset(position, encoding);
        let tiers = self.text_tiers_of(uri);
        let edition = edition_of_uri(uri);
        let (start, end, new_text) =
            noeta_fmt::format_stmt_at_in(uri, text, offset, &config, edition, &tiers)?;
        Some(vec![TextEdit {
            range: Range::new(
                index.position(start, encoding),
                index.position(end, encoding),
            ),
            new_text,
        }])
    }

    /// Range ("Format Selection") formatting: reformat the top-level statements overlapping `range`,
    /// each expanded to a whole statement. `None` unless the whole document parses and something in
    /// the selection would change — same AST-preserving safety as the other formatting entry points.
    pub fn format_range(
        &self,
        uri: &str,
        range: Range,
        encoding: Encoding,
    ) -> Option<Vec<TextEdit>> {
        let text = self.buffers.get(uri)?;
        let config = uri_to_path(uri)
            .map(|p| noeta_pm::manifest::resolve_fmt_config_lenient(&p))
            .unwrap_or_default();
        let index = LineIndex::new(text);
        let start = index.offset(range.start, encoding);
        let end = index.offset(range.end, encoding);
        let tiers = self.text_tiers_of(uri);
        let edition = edition_of_uri(uri);
        let edits = noeta_fmt::format_range_in(uri, text, start, end, &config, edition, &tiers)?;
        Some(
            edits
                .into_iter()
                .map(|(s, e, new_text)| TextEdit {
                    range: Range::new(index.position(s, encoding), index.position(e, encoding)),
                    new_text,
                })
                .collect(),
        )
    }

    /// (Re)build or update the shared workspace keyed `key` (see [`workspace_key`]). Discovers the
    /// directory's `.noe` members, overlays open buffers, and hands the list to the shared
    /// construction core ([`workspace::sync`]) — which updates the cached inputs' text in place
    /// (file set unchanged) or **reuses the inputs by URI** across the new file set (finding 9),
    /// re-resolving dependencies only on a set change.
    fn refresh_workspace(&mut self, key: &str) {
        let sources = self.discover_sources(key);
        let existing = self.workspaces.remove(key);
        if let Some(cache) = workspace::sync(&mut self.db, existing, sources) {
            self.workspaces.insert(key.to_string(), cache);
        }
    }

    /// The ordered `(uri, text)` members of the workspace keyed `key`: every on-disk `.noe` file
    /// in the directory plus every open buffer that lives there (a new unsaved file is a member
    /// before it ever hits disk), sorted by URI — the stable [`SourceId`] order every open
    /// document in the directory shares (path order, matching the loader's convention). An open
    /// member uses its editor buffer; the rest read disk. A directory-less key (`lone:`) is a
    /// single-member workspace of just that document's buffer.
    fn discover_sources(&self, key: &str) -> Vec<(String, String)> {
        let mut uris: Vec<String> = Vec::new();
        if let Some(dir) = key.strip_prefix("dir:") {
            uris.extend(disk_noe_uris(Path::new(dir)));
            // Open buffers in this directory that are not (yet) on disk are members too.
            uris.extend(
                self.buffers
                    .keys()
                    .filter(|uri| workspace_key(uri) == key)
                    .cloned(),
            );
        } else if let Some(uri) = key.strip_prefix("lone:") {
            uris.push(uri.to_string());
        }
        uris.sort();
        uris.dedup();
        uris.into_iter()
            .map(|uri| {
                let text = self
                    .buffers
                    .get(&uri)
                    .cloned()
                    .or_else(|| {
                        uri_to_path(&uri).and_then(|path| std::fs::read_to_string(path).ok())
                    })
                    .unwrap_or_default();
                (uri, text)
            })
            .collect()
    }

    /// The workspace serving the document `uri` as a **member** — open, or discovered on disk in
    /// an open directory — together with its salsa input and the [`SourceId`] it carries in the
    /// shared per-directory workspace. What every document-addressed feature resolves through.
    fn doc_cache(&self, uri: &str) -> Option<(&WorkspaceCache, SourceProgram, SourceId)> {
        let cache = self.workspaces.get(&workspace_key(uri))?;
        let idx = cache.source_uris.iter().position(|u| u == uri)?;
        Some((cache, cache.programs[idx], SourceId(idx as u32)))
    }

    /// The `uri`'s own diagnostics (cross-module resolution, but only this file's own diagnostics
    /// — each open module reports its own through its own merged program) together with the
    /// document text for position mapping. `None` if the document is in no open workspace.
    ///
    /// Runs over the whole-workspace [`linked_checked_ide_from`](noeta_db::linked_checked_ide_from)
    /// query — the SAME query hover/inlay/completions read — so one edit runs the checker **once**
    /// per document version (diagnostics are identical to `linked_checked_from`'s by construction;
    /// the ide flavor only additionally records `expr_types`, which the other features need
    /// anyway). A name imported from a sibling module resolves and no longer reports a false
    /// "unknown name". A load or parse failure carries its diagnostics through the same query.
    pub fn diagnostics(&self, uri: &str) -> Option<(Vec<noeta_diagnostics::Diagnostic>, String)> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let mut diags: Vec<noeta_diagnostics::Diagnostic> =
            noeta_db::linked_checked_ide_from(db, cache.workspace, doc)
                .diagnostics
                .iter()
                .filter(|d| d.span.source == source)
                .cloned()
                .collect();
        // A hard dependency-resolution failure (audit-5 #7): surface the real cause — a trust
        // refusal, a version conflict, a broken manifest — at the top of the file instead of
        // leaving only the spurious unknown-import errors it causes downstream. Reported under
        // E0019 (unresolved import): the imports genuinely cannot resolve, and this names why.
        if let Some(err) = &cache.dep_error {
            diags.insert(
                0,
                noeta_diagnostics::Diagnostic::error(
                    noeta_diagnostics::DiagnosticCode::UnresolvedImport,
                    noeta_span::Span::new_in(source, 0, 0),
                    format!("dependency resolution failed: {err}"),
                ),
            );
        }
        Some((diags, doc.text(db).clone()))
    }

    /// Inlay **type hints** for the visible `range` of `uri`: the inferred type of every
    /// un-annotated binding *declaration*, positioned right after the binding's name — rendered
    /// with the same `expr_types` spelling hover shows, so the inline text and the hover can never
    /// disagree. `None` if the document is unknown; an unlinkable workspace degrades to the entry's
    /// own AST (hints for within-file bindings keep working while a sibling is broken).
    pub fn inlay_hints(
        &self,
        uri: &str,
        range: Range,
        encoding: Encoding,
    ) -> Option<Vec<(Position, String, inlay::HintKind)>> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let index = LineIndex::new(doc.text(db));
        let start = index.offset(range.start, encoding);
        let end = index.offset(range.end, encoding);

        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let ide = noeta_db::linked_checked_ide_from(db, cache.workspace, doc);
        Some(
            inlay::type_hints(program, &ide.expr_types, &ide.packed_layouts, source)
                .into_iter()
                .filter(|hint| start <= hint.offset && hint.offset <= end)
                .map(|hint| (index.position(hint.offset, encoding), hint.label, hint.kind))
                .collect(),
        )
    }

    /// Whether `uri`'s entry document currently lexes and parses without errors. The LSP uses this
    /// to tell an *authoritative* empty result apart from a *transient* one: mid-keystroke the buffer
    /// is often momentarily unparseable (typing `p.` before the member name exists), which collapses
    /// `expr_types` and empties the inlay hints — the client would then clear every inline type and
    /// re-show it one keystroke later (a flicker). When this returns `false`, the caller keeps the
    /// last good hints instead of clearing them; when `true`, an empty result is real (a fully
    /// annotated file has no hints) and supersedes the cache. `false` for an unknown document.
    pub fn entry_parses_cleanly(&self, uri: &str) -> bool {
        let Some((_, doc, _)) = self.doc_cache(uri) else {
            return false;
        };
        let db = &self.db;
        noeta_db::tokens(db, doc).0.diagnostics.is_empty()
            && noeta_db::ast(db, doc).0.diagnostics.is_empty()
    }

    /// The type at `position` for hover: the **smallest** `expr_types` span in the entry file that
    /// contains the cursor (the most specific expression under it), rendered plus its LSP range,
    /// plus the type's [`layout_note`] when its storage is non-default (`@packed` / flat list).
    /// Runs over the whole-workspace type index so an expression's type is known even when it depends
    /// on an imported declaration; the `source` filter keeps the lookup to the entry file the cursor
    /// is in. `None` if the document is unknown or no typed expression covers the position.
    pub fn hover_type(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(TypeRepr, Option<String>, Range)> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let index = LineIndex::new(doc.text(db));
        let offset = index.offset(position, encoding);
        let checked = noeta_db::linked_checked_ide_from(db, cache.workspace, doc);
        let (span, repr) = checked
            .expr_types
            .iter()
            // Non-empty spans in this file that cover the cursor; pick the tightest.
            .filter(|(span, _)| {
                span.source == source
                    && span.end > span.start
                    && span.start <= offset
                    && offset <= span.end
            })
            .min_by_key(|(span, _)| span.end - span.start)?;
        let note = layout_note(repr, &checked.packed_layouts);
        Some((repr.clone(), note, index.range(*span, encoding)))
    }

    /// A **signature hover** for the callable name under the cursor: the declaration of the free
    /// function, method, or associated function it refers to, rendered in surface syntax
    /// (`fn manhattan(): int`) plus the name's LSP range. This is what a reader expects when hovering
    /// a call — [`hover_type`] alone reports only the call *expression's* type (its return type,
    /// e.g. `int`), never the parameters. The server shows this ahead of the type hover when it
    /// resolves.
    ///
    /// Resolves three positions: a bare function name (`classify` — a call or a top-level `fn`
    /// declaration name), a member method name (`p.manhattan` — the receiver's value type supplies
    /// the owning type), and an associated-function name (`Point.origin` — the receiver is the type
    /// itself). `None` when the cursor is not on a name that resolves to a function declaration in
    /// this workspace (built-ins have no `FnDecl` to show).
    ///
    /// [`hover_type`]: Self::hover_type
    pub fn hover_signature(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let text = doc.text(db);
        let index = LineIndex::new(text);
        let offset = index.offset(position, encoding);

        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        // The identifier token under the cursor — the callable's name, and the range to underline.
        let token = noeta_db::tokens(db, doc).0.tokens.iter().find(|t| {
            t.kind == TokenKind::Ident
                && t.span.source == source
                && t.span.start <= offset
                && offset <= t.span.end
        })?;

        let def_use = resolve::DefUse::build(program);
        let decl = if let Some((receiver_span, member)) = def_use.member_at(offset, source) {
            // `recv.m` or `Type.f`: name the owning type, then find the method/associated fn on it.
            let type_name =
                self.receiver_type_name(program, cache.workspace, doc, text, receiver_span);
            type_method(program, type_name.as_deref()?, member)?
        } else {
            // A bare name: a free function (call site or its own declaration name), an **imported**
            // function (the entry's `use` binds the local name — alias included — to the qualified
            // identity the linker rewrote the merged declaration to), else — if the cursor is on a
            // method's declaration name — that method.
            let name = &text[token.span.range()];
            match top_level_fn(program, name).or_else(|| {
                let qualified = resolve::import_targets(&entry_ast.0.program).remove(name)?;
                top_level_fn(program, &qualified)
            }) {
                Some(decl) => decl,
                None => method_decl_at(program, offset, source)?,
            }
        };
        Some((render_fn_signature(decl), index.range(token.span, encoding)))
    }

    /// The nominal type name a member-access receiver refers to, for [`hover_signature`]. A receiver
    /// that is itself a declared type name (`Point.origin`) names that type directly — an associated
    /// function call; any other receiver is a value whose nominal type comes from the workspace type
    /// index (`p.manhattan` where `p: Point`). `None` if the receiver has no nominal type.
    fn receiver_type_name(
        &self,
        program: &noeta_ast::Program,
        workspace: noeta_db::Workspace,
        doc: SourceProgram,
        text: &str,
        receiver_span: Span,
    ) -> Option<String> {
        let recv = &text[receiver_span.range()];
        if is_declared_type(program, recv) {
            return Some(recv.to_string());
        }
        let checked = noeta_db::linked_checked_ide_from(&self.db, workspace, doc);
        checked
            .expr_types
            .get(&receiver_span)
            .and_then(nominal_name)
            .map(str::to_string)
    }

    /// A **type-definition hover** for the `struct`/`class`/`enum` name under the cursor: the type's
    /// declaration rendered in surface syntax — its fields (with types and defaults) or variants, and
    /// its method signatures — plus the name's LSP range. Where [`hover_type`] reports only the
    /// nominal name (`Point`) for a type reference, this shows what the type *is*.
    ///
    /// Fires whenever the identifier under the cursor names a type declared in the workspace: at a
    /// construction (`Point {}`), a type annotation (`x: Point`), an associated-call receiver
    /// (`Point.origin`), or the declaration name itself. `None` when the cursor is not on a declared
    /// type name (a value binding keeps its ordinary type hover). Field defaults are rendered from
    /// source when the declaration is in the file under the cursor; a type imported from another file
    /// elides them (its text is not the entry text).
    ///
    /// [`hover_type`]: Self::hover_type
    pub fn hover_type_definition(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let text = doc.text(db);
        let index = LineIndex::new(text);
        let offset = index.offset(position, encoding);

        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        let token = noeta_db::tokens(db, doc).0.tokens.iter().find(|t| {
            t.kind == TokenKind::Ident
                && t.span.source == source
                && t.span.start <= offset
                && offset <= t.span.end
        })?;
        let name = &text[token.span.range()];
        // An imported type's merged declaration is linker-qualified (`geometry.vec.Vec2`), so a
        // bare reference (`Vec2 {}`) resolves through the entry's `use` bindings when the direct
        // lookup misses.
        let rendered = render_type_definition(program, name, text, source).or_else(|| {
            let qualified = resolve::import_targets(&entry_ast.0.program).remove(name)?;
            render_type_definition(program, &qualified, text, source)
        })?;
        Some((rendered, index.range(token.span, encoding)))
    }

    /// A hover for a **namespace-group** binding (`http` from `use std.http`, module-namespaces):
    /// the group's qualified prefix and its members. A group is not a typed value, so [`hover_type`]
    /// returns nothing for it — this fills that gap. `None` unless the cursor's identifier is a group
    /// bound in this file.
    ///
    /// [`hover_type`]: Self::hover_type
    pub fn hover_namespace(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (_cache, doc, _source) = self.doc_cache(uri)?;
        let db = &self.db;
        let entry_text = doc.text(db);
        let index = LineIndex::new(entry_text);
        let offset = index.offset(position, encoding);
        let token = noeta_db::tokens(db, doc).0.tokens.iter().find(|t| {
            t.kind == TokenKind::Ident && t.span.start <= offset && offset <= t.span.end
        })?;
        let name = &entry_text[token.span.range()];
        let entry_ast = noeta_db::ast(db, doc);
        let prefix = completion::namespace_bindings(&entry_ast.0.program).remove(name)?;
        let members = noeta_stdlib::registry::single_registry_process().namespace_children(&prefix);
        let members = if members.is_empty() {
            String::new()
        } else {
            let list = members
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("\n\nMembers: {list}")
        };
        let value = format!("namespace group `{name}` → `{prefix}`{members}");
        Some((value, index.range(token.span, encoding)))
    }

    /// The `@doc` prose of a **dependency** declaration named by a `use` (`path` is the use path,
    /// `leaf` the imported name): the linker merges a dep's declarations without their doc blocks,
    /// so the prose comes from the dep module's own AST — filtered to the dependency whose import
    /// key is the path's first segment, where the decl still carries its bare (pre-qualification)
    /// name. The same source the Dependencies docs corpus reads.
    fn dep_import_prose(
        &self,
        cache: &WorkspaceCache,
        path: &[String],
        leaf: &str,
    ) -> Option<String> {
        let key = path.first()?;
        let db = &self.db;
        for (i, sp) in cache.dep_programs.iter().enumerate() {
            if cache.dep_modules.get(i)?.key(db) != key {
                continue;
            }
            let ast = noeta_db::ast(db, *sp);
            let program = &ast.0.program;
            let span = program.stmts.iter().find_map(|s| match s {
                noeta_ast::Stmt::Fn(d) if d.name == leaf => Some(d.name_span),
                noeta_ast::Stmt::Struct(d) if d.name == leaf => Some(d.name_span),
                noeta_ast::Stmt::Class(d) if d.name == leaf => Some(d.name_span),
                noeta_ast::Stmt::Enum(d) if d.name == leaf => Some(d.name_span),
                _ => None,
            });
            if let Some(prose) = span.and_then(|span| doc_prose_at(program, span)) {
                return Some(prose);
            }
        }
        None
    }

    /// The `@doc` prose attached to the declaration the cursor's identifier resolves to, if any —
    /// what hover appends under the type. Attachment is adjacency-resolved from the merged
    /// workspace program's bare parse (`noeta_check::resolve_docs`), so it works regardless of
    /// which tiers a build would activate. Resolution mirrors [`Self::definition`]'s first and
    /// third layers (the scope-aware value index, then top-level definitions by name), and the
    /// match is by the declaration's **name-span** — the key `resolve_docs` reports.
    /// Resolve the cursor to the **name span** of the definition it refers to: the scope-aware
    /// value index first (a call site or the declaration itself), then the top-level definitions
    /// by the identifier token's text (type references, constructors). Shared by [`Self::hover_doc`]
    /// and [`Self::doc_for_symbol`] so hover prose and the docs-browser jump agree on "what symbol
    /// is under the cursor". `doc`/`cursor` are the document's input and [`SourceId`] within
    /// `cache` (see [`Self::doc_cache`]).
    fn definition_name_span(
        &self,
        cache: &WorkspaceCache,
        doc: SourceProgram,
        cursor: SourceId,
        position: Position,
        encoding: Encoding,
    ) -> Option<Span> {
        let db = &self.db;
        let entry_text = doc.text(db);
        let offset = LineIndex::new(entry_text).offset(position, encoding);

        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        resolve::DefUse::build(program)
            .definition_at(offset, cursor)
            .or_else(|| {
                let token = noeta_db::tokens(db, doc).0.tokens.iter().find(|token| {
                    token.kind == TokenKind::Ident
                        && token.span.start <= offset
                        && offset <= token.span.end
                })?;
                resolve::Definitions::collect(program).resolve(&entry_text[token.span.range()])
            })
            // A method's declaration name (`fn manhattan`) — not a top-level definition, so resolve
            // it directly to the method's own name-span (the key a method's `@doc` is stored under).
            .or_else(|| method_decl_at(program, offset, cursor).map(|m| m.name_span))
            // A method/associated call name (`p.manhattan`, `Point.origin`) — resolve the receiver's
            // type and look the method up, so hovering a call surfaces the method's `@doc` too.
            .or_else(|| {
                let def_use = resolve::DefUse::build(program);
                let (receiver_span, member) = def_use.member_at(offset, cursor)?;
                let ty = self.receiver_type_name(
                    program,
                    cache.workspace,
                    doc,
                    entry_text,
                    receiver_span,
                )?;
                type_method(program, &ty, member).map(|m| m.name_span)
            })
    }

    pub fn hover_doc(&self, uri: &str, position: Position, encoding: Encoding) -> Option<String> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let def_span = self.definition_name_span(cache, doc, source, position, encoding)?;

        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        doc_prose_at(program, def_span)
    }

    /// A hover for any element of a **`use` statement** — none of which is a typed expression, so
    /// every other hover has nothing to say there. An imported *item* (`Vec2`, `add` — or its
    /// alias) hovers as the declaration it binds: a source function's signature, a source type's
    /// definition, or a native item's registry signature, each with its doc prose; a grouped module
    /// import (`use std.{math, json}`) and the *path segments* (`geometry`, `vec`) hover as the
    /// module they name, with its members.
    pub fn hover_use(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let text = doc.text(db);
        let index = LineIndex::new(text);
        let offset = index.offset(position, encoding);

        // The `use` statement under the cursor — from the entry parse (its `use`s live here).
        let entry_ast = noeta_db::ast(db, doc);
        let (path, names) = entry_ast.0.program.stmts.iter().find_map(|s| match s {
            noeta_ast::Stmt::Use { path, names, span }
                if span.source == source && span.start <= offset && offset <= span.end =>
            {
                Some((path, names))
            }
            _ => None,
        })?;
        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let prefix = path.join(".");

        // On an imported name (or the alias it binds): the item hover.
        if let Some(n) = names
            .iter()
            .find(|n| n.span.start <= offset && offset <= n.span.end)
        {
            let qualified = format!("{prefix}.{}", n.name);
            let mut value = if let Some(decl) = top_level_fn(program, &qualified) {
                // A source function merged from the project or a dependency (linker-qualified).
                // Prose: the merged program carries a project decl's `@doc`; a dependency's doc
                // blocks are NOT merged (only its decls are), so fall back to the dep module's own
                // AST — the same source the Dependencies docs corpus reads.
                let mut v = format!("```noeta\n{}\n```", render_fn_signature(decl));
                if let Some(prose) = doc_prose_at(program, decl.name_span)
                    .or_else(|| self.dep_import_prose(cache, path, &n.name))
                {
                    v.push_str("\n\n---\n\n");
                    v.push_str(&prose);
                }
                v
            } else if let Some(def) = render_type_definition(program, &qualified, text, source) {
                // A source type: its full definition, plus its `@doc` prose when present.
                let mut v = format!("```noeta\n{def}\n```");
                let merged_prose = resolve::Definitions::collect(program)
                    .resolve(&qualified)
                    .and_then(|span| doc_prose_at(program, span));
                if let Some(prose) =
                    merged_prose.or_else(|| self.dep_import_prose(cache, path, &n.name))
                {
                    v.push_str("\n\n---\n\n");
                    v.push_str(&prose);
                }
                v
            } else if let Some(f) = api::function(&prefix, &n.name) {
                // A native function (`use std.math.abs`): the registry's signature and prose.
                let mut v = format!("```noeta\n{}\n```", f.signature);
                if !f.doc.is_empty() {
                    v.push_str("\n\n---\n\n");
                    v.push_str(&f.doc);
                }
                v
            } else if let Some(t) = api::type_(&qualified) {
                // A native extern type (`use std.id.Uuid`): name it and list its methods.
                let methods = t
                    .methods
                    .iter()
                    .map(|m| format!("`{}`", m.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                if methods.is_empty() {
                    format!("extern type `{qualified}`")
                } else {
                    format!("extern type `{qualified}`\n\nMethods: {methods}")
                }
            } else {
                // A grouped module import (`use std.{math, json}`) or an unresolved name: describe
                // the module when the registry or the program knows it, else just name the import.
                let members = module_members(program, &qualified);
                if members.is_empty() {
                    format!("import `{qualified}`")
                } else {
                    format!("module `{qualified}`\n\nMembers: {}", members.join(", "))
                }
            };
            if let Some(alias) = &n.alias {
                value.push_str(&format!("\n\nimported as `{alias}`"));
            }
            return Some((value, index.range(n.span, encoding)));
        }

        // On a path segment (`geometry`, `vec`, `std`, …): the module prefix up to that segment.
        // Segments carry no individual spans, so find the identifier token and match its text.
        let token = noeta_db::tokens(db, doc).0.tokens.iter().find(|t| {
            t.kind == TokenKind::Ident
                && t.span.source == source
                && t.span.start <= offset
                && offset <= t.span.end
        })?;
        let word = &text[token.span.range()];
        let idx = path.iter().position(|s| s == word)?;
        let module = path[..=idx].join(".");
        let members = module_members(program, &module);
        let value = if members.is_empty() {
            format!("module `{module}`")
        } else {
            format!("module `{module}`\n\nMembers: {}", members.join(", "))
        };
        Some((value, index.range(token.span, encoding)))
    }

    /// The **tier-body descriptor** for an embedded-language block under the cursor (text-tiers /
    /// expr-tiers arcs): when the cursor is on a `@<name> { … }` block's tier name, report what its
    /// body is — the declared body language (`text: "sql"` / `doc` → markdown) and, for an
    /// expression tier, the value type its blocks evaluate to (`expr: T`). Read from the
    /// [`noeta_check::tiers::TierRegistry`], which unions the program's own `@tier` declarations
    /// with any an installed extension contributes — so a program-declared tier and a native
    /// package's tier surface identically. Returns the descriptor line plus the tier-name range.
    pub fn hover_tier(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let index = LineIndex::new(doc.text(db));
        let offset = index.offset(position, encoding);

        // The tier at the cursor: this file's parse, scanned for a `@<name> { … }` whose tier
        // name covers the offset. Uses the **workspace-aware** parse (`ast_in`), so a native
        // tier's `@json { … }` body is captured verbatim (the ext-tier lexer seed) and shows up as
        // a `TierExpr` rather than being mis-lexed as code.
        let ast = noeta_db::ast_in(db, cache.workspace, doc);
        let (tier, tier_span) = tier_name_at(&ast.0.program, offset, source)?;

        // The tier's declaration, from the workspace-merged program's registry (imports + this
        // file). A built-in `doc` has no declaration but a known language.
        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let registry = noeta_check::tiers::TierRegistry::collect(program);

        let lang = registry.text_lang(&tier);
        let value = registry.expr_type(&tier);
        // The built-in documentation tier gets tailored prose, like the code tiers below — the
        // generic "text tier — markdown body" line undersells what `@doc` actually does.
        if tier == "doc" {
            let descriptor = "documentation tier `@doc` — markdown prose that attaches to the \
                              declaration it precedes (a fn, method, or type), or documents the \
                              module when none follows. Surfaces in hover, the docs browser, and \
                              `noeta doc`"
                .to_string();
            return Some((descriptor, index.range(tier_span, encoding)));
        }
        let descriptor = match (value, lang) {
            (Some(ty), Some(lang)) => {
                format!("expression tier `@{tier}` — `{lang}` body, evaluates to `{ty}`")
            }
            (Some(ty), None) => format!("expression tier `@{tier}` — evaluates to `{ty}`"),
            (None, Some(lang)) => format!("text tier `@{tier}` — `{lang}` body"),
            // A **code tier**: no embedded-language body, but the directive itself deserves a
            // hover — the built-ins get tailored prose, a `@tier(...)`-declared one a generic
            // descriptor (with its knob type when it has one). Unknown names stay silent (they
            // are an E0036 anyway).
            (None, None) => match tier.as_str() {
                "test" => format!(
                    "dev tier `@{tier}` — compiled and run only under `noeta test`; the annotated \
                     fn (or each fn in the block) is a test root"
                ),
                "bench" => format!(
                    "dev tier `@{tier}` — compiled and run only under `noeta bench`; the annotated \
                     fn is a benchmark root"
                ),
                "debug" => format!(
                    "dev tier `@{tier}` — compiled only when the debug tier is active; stripped \
                     otherwise"
                ),
                _ if registry.is_known(&tier) => {
                    let knobs = registry
                        .declared(&tier)
                        .and_then(|d| d.config.as_deref())
                        .map(|c| format!(" — knobs: `{c}`"))
                        .unwrap_or_default();
                    format!("dev tier `@{tier}`{knobs}")
                }
                _ => return None,
            },
        };
        Some((descriptor, index.range(tier_span, encoding)))
    }

    /// A hover for the built-in **decorator directives** — the closed set that prefixes a *type*
    /// declaration (`@derive`, `@attribute`, `@role`, `@semantic`, `@packed`; the tier directives
    /// are [`Self::hover_tier`]'s). Token-level: fires when the cursor is on the `@` or the name it
    /// introduces, wherever the directive sits. The set and its meanings are core language
    /// (mirrors the parser's `is_decorator_directive`).
    pub fn hover_directive(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (_cache, doc, source) = self.doc_cache(uri)?;
        let db = &self.db;
        let text = doc.text(db);
        let index = LineIndex::new(text);
        let offset = index.offset(position, encoding);
        let toks = noeta_db::tokens(db, doc);
        let tokens = &toks.0.tokens;
        let i = tokens.iter().position(|t| {
            t.span.source == source && t.span.start <= offset && offset <= t.span.end
        })?;
        let (at, name_tok) = match tokens[i].kind {
            TokenKind::Ident if i > 0 && tokens[i - 1].kind == TokenKind::At => {
                (&tokens[i - 1], &tokens[i])
            }
            TokenKind::At if tokens.get(i + 1).map(|t| t.kind) == Some(TokenKind::Ident) => {
                (&tokens[i], &tokens[i + 1])
            }
            _ => return None,
        };
        let descriptor = match &text[name_tok.span.range()] {
            "derive" => {
                "codegen directive `@derive(Trait, …)` — generates built-in trait \
                 implementations (`Equatable`, `Comparable`, `Printable`, `Serialize<…>`, …) for \
                 this type"
            }
            "attribute" => {
                "declares this struct as a **metadata attribute**: instances attach to \
                 declarations as `#[Name(args)]` and are read back with `attributes_of::<Name>()`. \
                 An optional site argument (`@attribute(Function)`) restricts what it may annotate"
            }
            "role" => {
                "architectural-role directive: every declaration this attribute annotates is \
                 bound to the named role (`@role(Enum.Variant)` — a variant of a `@semantic` \
                 enum). The compile-time role index powers `roles_of()`, the Architecture view, \
                 and `noeta trace`"
            }
            "semantic" => {
                "marks this enum as **role-eligible**: its variants can be conferred on \
                 declarations as architectural roles, via `@role(ThisEnum.Variant)` on an \
                 attribute"
            }
            "packed" => {
                "storage directive: a **packed value struct** — fields lay out flat (no boxing), \
                 and a `List` of a packed struct is one contiguous buffer"
            }
            _ => return None,
        };
        let span = Span {
            start: at.span.start,
            end: name_tok.span.end,
            source,
        };
        Some((descriptor.to_string(), index.range(span, encoding)))
    }

    /// Resolve the definition of the reference at `position` for go-to-definition, as a `(URI,
    /// range)` — the target may be a **different file** (a cross-module reference). Runs over the
    /// merged workspace program, so an imported name resolves to its declaration in the sibling that
    /// declares it. Three layers, in order: the scope-aware value index (locals, parameters,
    /// functions — shadowing-correct); a member access `x.member` resolved via the receiver's type
    /// and the type's member table; and the identifier token under the cursor resolved by name
    /// against the top-level definitions (type references, constructors).
    pub fn definition(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let (cache, doc, cursor) = self.doc_cache(uri)?;
        let db = &self.db;
        let entry_text = doc.text(db);
        let entry_index = LineIndex::new(entry_text);
        let offset = entry_index.offset(position, encoding);

        // The merged program when the link succeeded, else the document's own AST (so within-file
        // navigation still works while a sibling is broken).
        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let def_use = resolve::DefUse::build(program);

        // 1. Scope-aware value resolution — a local, parameter, or function reference resolves to
        //    the precise binding in scope at the cursor (shadowing-correct).
        if let Some(def) = def_use.definition_at(offset, cursor) {
            return self.locate(cache, def, encoding);
        }

        // 2. Member access `receiver.member`: resolve the receiver's type from the workspace type
        //    index, then look the member up among that type's declared fields/variants/methods.
        if let Some((receiver_span, member)) = def_use.member_at(offset, cursor)
            && let Some(receiver_ty) = noeta_db::linked_checked_ide_from(db, cache.workspace, doc)
                .expr_types
                .get(&receiver_span)
            && let Some(type_name) = nominal_name(receiver_ty)
            && let Some(def) = resolve::MemberTable::collect(program).lookup(type_name, member)
        {
            return self.locate(cache, def, encoding);
        }

        // 3. Fallback: the identifier token under the cursor (from the document's tokens) resolved
        //    by name against the top-level definitions. Covers type references and constructors.
        let token = noeta_db::tokens(db, doc).0.tokens.iter().find(|token| {
            token.kind == TokenKind::Ident && token.span.start <= offset && offset <= token.span.end
        })?;
        let name = &entry_text[token.span.range()];
        let defs = resolve::Definitions::collect(program);
        // An aliased or plain import resolves through the entry's own `use` list to the qualified
        // identity the linker merged in (arc Phase B); `resolve` itself falls back to leaf matching for
        // a bare reference to a namespaced declaration. The entry AST (not the linked program, whose
        // resolved `use`s are dropped) carries the imports.
        let def_span = resolve::import_targets(&entry_ast.0.program)
            .get(name)
            .and_then(|qualified| defs.resolve(qualified))
            .or_else(|| defs.resolve(name))?;
        self.locate(cache, def_span, encoding)
    }

    /// All references to the symbol at `position` — every use, plus the declaration when
    /// `include_declaration` is set — as `(URI, range)` pairs. Handles a **value** symbol (local,
    /// parameter, function — via the scope-aware def/use index) or a **member** symbol (field,
    /// variant, method — matched by the receiver's type so a same-named member on another type is not
    /// swept in). Runs over the merged workspace program, so references are found **across modules**.
    /// The cursor may be on a use or on the declaration itself. `None` if the document is not open or
    /// the cursor is on no resolvable symbol.
    pub fn references(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
        include_declaration: bool,
    ) -> Option<Vec<(String, Range)>> {
        let (cache, doc, cursor) = self.doc_cache(uri)?;
        let offset = LineIndex::new(doc.text(&self.db)).offset(position, encoding);
        let spans = self.symbol_occurrences(cache, doc, cursor, offset, include_declaration)?;

        let mut locations: Vec<(String, Range)> = spans
            .into_iter()
            .filter_map(|span| self.locate(cache, span, encoding))
            .collect();
        // Stable order and no duplicates (e.g. a declaration that also matched a use span).
        locations.sort_by(|a, b| {
            (&a.0, a.1.start.line, a.1.start.character).cmp(&(
                &b.0,
                b.1.start.line,
                b.1.start.character,
            ))
        });
        locations.dedup();
        Some(locations)
    }

    /// Every occurrence span of the symbol at `offset` (uses, and the declaration when
    /// `include_declaration`), whether it is a value symbol or a member symbol. The returned spans
    /// carry their own [`SourceId`], so they may span multiple files. `None` if the cursor is on no
    /// resolvable symbol. The shared core of find-references, rename, and prepare-rename.
    fn symbol_occurrences(
        &self,
        cache: &WorkspaceCache,
        doc: SourceProgram,
        cursor: SourceId,
        offset: u32,
        include_declaration: bool,
    ) -> Option<Vec<Span>> {
        let db = &self.db;
        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let def_use = resolve::DefUse::build(program);

        // 1. Value symbol (local, parameter, function).
        if let Some(def) = def_use.symbol_at(offset, cursor) {
            let mut spans = def_use.references_to(def);
            if include_declaration {
                spans.push(def);
            }
            return Some(spans);
        }

        // 2. Member symbol (field, variant, method), keyed by `(type, member)`. The target is read
        //    from the `.member` access under the cursor (receiver type from the workspace index) or
        //    from the member declaration the cursor is on.
        let members = resolve::MemberTable::collect(program);
        let ide = noeta_db::linked_checked_ide_from(db, cache.workspace, doc);
        let expr_types = &ide.expr_types;
        let (type_name, member_name) = match def_use.member_at(offset, cursor) {
            Some((receiver_span, name)) => {
                let ty = expr_types.get(&receiver_span).and_then(nominal_name)?;
                (ty.to_string(), name.to_string())
            }
            None => {
                let (ty, member) = members.declaration_at(offset, cursor)?;
                (ty.to_string(), member.to_string())
            }
        };

        // Every `.member` access whose name matches and whose receiver has the target type.
        let mut spans: Vec<Span> = def_use
            .member_occurrences()
            .filter(|(name, _, receiver_span)| {
                *name == member_name
                    && expr_types.get(receiver_span).and_then(nominal_name)
                        == Some(type_name.as_str())
            })
            .map(|(_, name_span, _)| name_span)
            .collect();
        if include_declaration && let Some(decl) = members.lookup(&type_name, &member_name) {
            spans.push(decl);
        }
        Some(spans)
    }

    /// The edits that rename the symbol at `position` to `new_name` — every use and the declaration —
    /// grouped by URI. Reuses [`references`](Self::references) (declaration included), so it renames a
    /// value or member symbol and propagates **across modules**. `None` if the cursor is on no
    /// renameable symbol or `new_name` is not a valid identifier (an invalid rename must not silently
    /// corrupt the source).
    pub fn rename_edits(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
        new_name: &str,
    ) -> Option<HashMap<String, Vec<Range>>> {
        if !is_identifier(new_name) {
            return None;
        }
        let locations = self.references(uri, position, encoding, true)?;
        let mut by_uri: HashMap<String, Vec<Range>> = HashMap::new();
        for (target_uri, range) in locations {
            by_uri.entry(target_uri).or_default().push(range);
        }
        Some(by_uri)
    }

    /// The range of the renameable symbol under the cursor — for `prepareRename`, so the editor can
    /// validate before showing its input box and pre-select the old name. Returns the occurrence at
    /// the cursor (in the entry file) when it resolves to a value or member symbol; `None` otherwise
    /// (the editor then refuses the rename).
    pub fn prepare_rename(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<Range> {
        let (cache, doc, cursor) = self.doc_cache(uri)?;
        let index = LineIndex::new(doc.text(&self.db));
        let offset = index.offset(position, encoding);
        // The occurrences include the one under the cursor (a use or the declaration); return it.
        let here = self
            .symbol_occurrences(cache, doc, cursor, offset, true)?
            .into_iter()
            .find(|span| span.source == cursor && span.start <= offset && offset <= span.end)?;
        Some(index.range(here, encoding))
    }

    /// Signature help for the call the cursor at `position` is inside: the called function's or
    /// method's signature and the active argument. Token-based (so a half-typed call with an
    /// unbalanced paren still resolves). A plain call `f(` resolves the callee among the merged
    /// program's top-level functions (imported functions included); a method call `recv.m(` resolves
    /// the receiver's type — by closing the call in a munged copy and re-checking off the salsa graph
    /// — and finds `m` among that type's methods. `None` if the cursor is not in a resolvable call.
    pub fn signature_help(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<signature::SignatureData> {
        let (cache, doc, _source) = self.doc_cache(uri)?;
        let db = &self.db;
        let text = doc.text(db);
        let offset = LineIndex::new(text).offset(position, encoding);
        let tokens = &noeta_db::tokens(db, doc).0.tokens;

        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        // A directive's argument list (`@derive(|`, `@bench(|` — C5) gets a synthetic signature
        // naming the directive's vocabulary; a half-typed directive never resolves as a call, so
        // this comes first.
        if let Some(args) = completion::directive_arg_context(text, offset) {
            return signature::directive_signature(&args, program);
        }

        let call = signature::enclosing_call(tokens, text, offset)?;
        match call.receiver {
            // Plain function call: the callee is a top-level function.
            None => {
                let decl = top_level_fn(program, &call.callee)?;
                Some(signature::from_decl(decl, call.active))
            }
            // Method call `recv.m(`: resolve `recv`'s type (closing the call so it type-checks), then
            // find `m` among that type's methods.
            Some(receiver_span) => {
                let type_name = receiver_type_at(text, offset, receiver_span)?;
                let decl = type_method(program, &type_name, &call.callee)?;
                Some(signature::from_decl(decl, call.active))
            }
        }
    }

    /// The completion candidates at `position` in `uri`. When the cursor is on a member access
    /// `receiver.member`, the receiver's type is resolved and the completions are that type's fields,
    /// variants, and methods (**member completion**, C2) — nothing else, since an identifier after a
    /// `.` would be noise. A *partial* member (`c.ge`) is read straight from the workspace type index;
    /// a *bare* dot (`c.`, the `.`-trigger case) does not parse, so a lightly-munged buffer is
    /// re-checked off the salsa graph to recover the receiver type (C2.1). Otherwise the completions
    /// are the language keywords, the top-level declarations, and the value bindings in scope at the
    /// cursor (**identifier completion**, C1). An `@` prefix instead offers the decorator directives
    /// and the tier name-space (**directive completion**, C4).
    ///
    /// A best-effort read of the mid-edit document: it relies on the recovering parser and the
    /// client's prefix filtering. `None` if the document is not open.
    pub fn completions(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<Vec<completion::Candidate>> {
        let (cache, doc, cursor) = self.doc_cache(uri)?;
        let db = &self.db;
        let entry_text = doc.text(db);
        let index = LineIndex::new(entry_text);
        let offset = index.offset(position, encoding);

        // Prefer the merged workspace program (so an imported type's members resolve); fall back to
        // the document's own AST while a sibling is unparseable.
        let linked = noeta_db::linked_from(db, cache.workspace, doc);
        let entry_ast = noeta_db::ast(db, doc);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        // Directive completion (`@|`, `@te|` — the `@`-trigger case, C4): the decorator directives
        // and the tier name-space. Detected textually (a dangling `@` never parses), before the
        // member branches — an `@` prefix is never a member access.
        if completion::is_directive_position(entry_text, offset) {
            return Some(completion::directives(program));
        }

        // Directive-argument completion (`@derive(|`, `@role(Semantic.|`, `@bench(|` — C5): the
        // directive's own vocabulary. Also before the member branches — `@packed(Layout.|` must
        // complete the layout variants, not munge a member access out of `Layout.`. An empty
        // vocabulary still returns (suppressing identifier completion, which is noise inside a
        // directive's parens).
        if let Some(args) = completion::directive_arg_context(entry_text, offset) {
            return Some(completion::directive_arg_candidates(&args, program));
        }

        // Member completion, partial form: the parser produced a `receiver.member` access under the
        // cursor. A namespace-group receiver (`http.cl`) has no value type, so it is resolved first
        // against the group bindings and offers the group's submodules/types (module-namespaces).
        let def_use = resolve::DefUse::build(program);
        let namespaces = completion::namespace_bindings(program);
        if let Some((receiver_span, _member)) = def_use.member_at(offset, cursor) {
            if let Some(prefix) = namespaces.get(&entry_text[receiver_span.range()]) {
                return Some(completion::namespace_members(prefix));
            }
            let checked = noeta_db::linked_checked_ide_from(db, cache.workspace, doc);
            let mut members = checked
                .expr_types
                .get(&receiver_span)
                .and_then(nominal_name)
                .map(|type_name| completion::members_of(program, type_name))
                .unwrap_or_default();
            // Bundle-contributed methods (kernel-methods K4): a bound `@packed` type offers its
            // bundles' Element methods, a `List<T>` of one their Bulk methods.
            if let Some(repr) = checked.expr_types.get(&receiver_span) {
                members.extend(bundle_members_for(repr, &checked.bundle_bindings));
            }
            return Some(members);
        }

        // Member completion, bare-dot form (`c.` with no member name yet — the `.`-trigger case). The
        // dangling dot makes the statement fail to parse, so the receiver never gets a type from the
        // cached check. Re-check a copy of the buffer with a synthetic member name inserted, off the
        // salsa graph, to recover it. Single-file: the receiver's type must be declared in this file.
        if is_bare_dot(entry_text, offset) {
            return Some(
                bare_dot_members(entry_text, offset, program, &namespaces).unwrap_or_default(),
            );
        }

        // Type-annotation position (`x: |`, `fn f(): |`, `List<|>`): offer type names only.
        if let Some(types) = type_position_completion(entry_text, offset, program) {
            return Some(types);
        }

        Some(completion::complete(program, offset, cursor))
    }

    /// The semantic tokens for `uri`: the compiler-classified identifiers (function/variable/type/
    /// property), delta-encoded per the LSP wire format against the [`semtokens::LEGEND`]. A
    /// single-file overlay over the entry document's own AST — the client keeps its static grammar for
    /// everything else. `None` if the document is not open.
    pub fn semantic_tokens(&self, uri: &str, encoding: Encoding) -> Option<Vec<SemanticToken>> {
        let (_cache, doc, _source) = self.doc_cache(uri)?;
        let index = LineIndex::new(doc.text(&self.db));
        let program = &noeta_db::ast(&self.db, doc).0.program;

        let mut data = Vec::new();
        let (mut prev_line, mut prev_char) = (0u32, 0u32);
        for (span, kind) in semtokens::highlights(program) {
            let range = index.range(span, encoding);
            // Identifiers do not span lines, so the token length is the width on the start line.
            let length = range.end.character - range.start.character;
            let delta_line = range.start.line - prev_line;
            let delta_start = if delta_line == 0 {
                range.start.character - prev_char
            } else {
                range.start.character
            };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type: kind as u32,
                token_modifiers_bitset: 0,
            });
            (prev_line, prev_char) = (range.start.line, range.start.character);
        }
        Some(data)
    }

    /// The document outline for `uri`: the hierarchical symbol tree (top-level functions and type
    /// declarations, with fields/variants and methods nested) mapped to LSP `DocumentSymbol`s. A
    /// single-file feature — it reads the entry document's own AST, not the merged workspace — so an
    /// unparseable document yields whatever the recovering parser produced. `None` if the document is
    /// not open.
    pub fn document_symbols(&self, uri: &str, encoding: Encoding) -> Option<Vec<DocumentSymbol>> {
        let (_cache, doc, _source) = self.doc_cache(uri)?;
        let index = LineIndex::new(doc.text(&self.db));
        let program = &noeta_db::ast(&self.db, doc).0.program;
        Some(
            symbols::outline(program)
                .iter()
                .map(|node| to_document_symbol(&index, node, encoding))
                .collect(),
        )
    }

    /// The workspace serving `uri`, the **entry** whose merged program answers for it, and the
    /// [`SourceId`] `uri` carries within the workspace. An open document is its own entry; an
    /// unopened member or dependency module is answered through the workspace's first open
    /// document (the importer's view — there is always one while the cache exists). This is what
    /// lets call-hierarchy expansion continue from an item in a file the user never opened — the
    /// hierarchy requests address items by `(uri, selection range)`, not by the entry document.
    fn workspace_of(&self, uri: &str) -> Option<(&WorkspaceCache, SourceProgram, SourceId)> {
        if let Some((cache, program, source)) = self.doc_cache(uri) {
            let entry = if self.buffers.contains_key(uri) {
                program
            } else {
                self.entry_for(cache, program)
            };
            return Some((cache, entry, source));
        }
        // A dependency module discovered by some open workspace (ids continue past the members).
        self.workspaces.values().find_map(|cache| {
            let idx = cache.dep_uris.iter().position(|u| u == uri)?;
            let entry = self.entry_for(cache, cache.programs[0]);
            Some((cache, entry, SourceId((idx + cache.programs.len()) as u32)))
        })
    }

    /// The link-driving entry for requests about a file that is not itself open: the first open
    /// member of `cache` (in sorted member order, so the choice is deterministic), or `fallback`
    /// if none is — unreachable in practice, since a cache exists only while a member is open.
    fn entry_for(&self, cache: &WorkspaceCache, fallback: SourceProgram) -> SourceProgram {
        cache
            .source_uris
            .iter()
            .zip(&cache.programs)
            .find(|(u, _)| self.buffers.contains_key(*u))
            .map(|(_, program)| *program)
            .unwrap_or(fallback)
    }

    /// The source text of `source` within `cache` (entry + siblings, then dependency modules —
    /// the same id layout [`Self::locate`] maps).
    fn source_text(&self, cache: &WorkspaceCache, source: SourceId) -> Option<&String> {
        let idx = source.0 as usize;
        let program = if idx < cache.programs.len() {
            cache.programs.get(idx)?
        } else {
            cache.dep_programs.get(idx - cache.programs.len())?
        };
        Some(program.text(&self.db))
    }

    /// The function the cursor addresses, as a call-hierarchy item (ide-ui U0): its declared name,
    /// the `@role` bindings it bears, and its location. The cursor may be on the function's name,
    /// on a call/reference site (resolving to the callee), or anywhere inside the declaration.
    /// `uri` may be any file of an open workspace, not just an open document (see
    /// [`Self::workspace_of`]). `None` if no workspace covers the file or the cursor addresses no
    /// function.
    pub fn function_at(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<HierarchyItem> {
        let (cache, entry, source) = self.workspace_of(uri)?;
        let offset = LineIndex::new(self.source_text(cache, source)?).offset(position, encoding);
        let (graph, info) = self.call_graph(cache, entry);
        let roles = trace::roles_by_target(&info);
        let idx = graph.function_at(offset, source)?;
        self.hierarchy_item(cache, &graph, &roles, idx, encoding)
    }

    /// The calls **out of** the function at `position`, grouped by callee, each group carrying its
    /// sites in the caller's document. Callees the static graph cannot place in source — external
    /// module calls, dynamic closure calls — are omitted here (a hierarchy item needs a real
    /// location); the trace document renders them as labeled leaves instead (ide-ui U2).
    pub fn outgoing_calls(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<Vec<HierarchyCall>> {
        let (cache, entry, source) = self.workspace_of(uri)?;
        let offset = LineIndex::new(self.source_text(cache, source)?).offset(position, encoding);
        let (graph, info) = self.call_graph(cache, entry);
        let roles = trace::roles_by_target(&info);
        let caller = graph.function_at(offset, source)?;

        // Group the caller's edges by callee, first-site order (edges are already source-ordered).
        let mut groups: Vec<(usize, Vec<(Range, bool)>)> = Vec::new();
        for edge in graph.edges_from(Some(caller)) {
            let callgraph::Callee::Function(target) = edge.callee else {
                continue;
            };
            let Some((_, site)) = self.locate(cache, edge.site, encoding) else {
                continue;
            };
            match groups.iter_mut().find(|(t, _)| *t == target) {
                Some((_, sites)) => sites.push((site, edge.call)),
                None => groups.push((target, vec![(site, edge.call)])),
            }
        }
        Some(
            groups
                .into_iter()
                .filter_map(|(target, sites)| {
                    let item = self.hierarchy_item(cache, &graph, &roles, target, encoding)?;
                    Some(HierarchyCall { item, sites })
                })
                .collect(),
        )
    }

    /// The calls **into** the function at `position`, grouped by caller, each group's sites in that
    /// caller's document. A use from the program's top-level statements (Noeta's entry — there is
    /// no `main`) is a real caller: it appears as a synthetic `(top level)` item located at its
    /// first site, so the editor can still jump to it.
    pub fn incoming_calls(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<Vec<HierarchyCall>> {
        let (cache, entry, source) = self.workspace_of(uri)?;
        let offset = LineIndex::new(self.source_text(cache, source)?).offset(position, encoding);
        let (graph, info) = self.call_graph(cache, entry);
        let roles = trace::roles_by_target(&info);
        let target = graph.function_at(offset, source)?;

        // One group per caller: `(caller, flagged sites, the sites' URI — the caller's document)`.
        type CallerGroup = (Option<usize>, Vec<(Range, bool)>, String);
        let mut groups: Vec<CallerGroup> = Vec::new();
        for edge in &graph.edges {
            if edge.callee != callgraph::Callee::Function(target) {
                continue;
            }
            let Some((site_uri, site)) = self.locate(cache, edge.site, encoding) else {
                continue;
            };
            match groups.iter_mut().find(|(c, _, _)| *c == edge.caller) {
                Some((_, sites, _)) => sites.push((site, edge.call)),
                None => groups.push((edge.caller, vec![(site, edge.call)], site_uri)),
            }
        }
        Some(
            groups
                .into_iter()
                .filter_map(|(caller, sites, site_uri)| {
                    let item = match caller {
                        Some(idx) => self.hierarchy_item(cache, &graph, &roles, idx, encoding)?,
                        None => HierarchyItem {
                            name: TOP_LEVEL.to_string(),
                            kind: SymbolKind::Function,
                            roles: Vec::new(),
                            uri: site_uri,
                            range: sites[0].0,
                            selection_range: sites[0].0,
                        },
                    };
                    Some(HierarchyCall { item, sites })
                })
                .collect(),
        )
    }

    /// The `@role` bindings declared in `uri` (ide-ui U2), in source order — each locating the
    /// annotated declaration's name. The data behind the editor's role CodeLenses. Roles are
    /// indexed over the **merged** program (the `@attribute` struct conferring a role may live in
    /// a sibling), then filtered to bindings whose target is declared in this file.
    pub fn role_lenses(&self, uri: &str, encoding: Encoding) -> Option<Vec<RoleLens>> {
        let (cache, entry, source) = self.workspace_of(uri)?;
        let (graph, info) = self.call_graph(cache, entry);
        let index = LineIndex::new(self.source_text(cache, source)?);
        let mut lenses: Vec<RoleLens> = info
            .roles
            .iter()
            .filter(|r| r.target_span.source == source)
            .map(|r| RoleLens {
                range: index.range(r.target_span, encoding),
                role: format!("{}.{}", r.enum_name, r.variant),
                target: r.target.clone(),
                traceable: graph.function_named(&r.target).is_some(),
            })
            .collect();
        lenses.sort_by(|a, b| {
            (a.range.start.line, a.range.start.character, &a.role).cmp(&(
                b.range.start.line,
                b.range.start.character,
                &b.role,
            ))
        });
        Some(lenses)
    }

    /// Render the role-aware static trace from `from` (a function name or a role spec — every
    /// bearer; `None` = every role-bearing function) as a plain-text document: the answer to the
    /// editor's `noeta/trace` custom request, opened read-only as a `noeta-trace:` document. The
    /// walk is [`trace`] — the same engine the MCP `trace` tool serves. Locations render as
    /// `path:line` (1-based), relative to the workspace entry's directory when inside it. `None`
    /// only when no open workspace covers `uri`; an unmatched `from` renders an explanatory
    /// document (the user clicked something — answer in the document, not with silence).
    pub fn trace_document(&self, uri: &str, from: Option<&str>) -> Option<String> {
        let (cache, entry, _) = self.workspace_of(uri)?;
        let (graph, info) = self.call_graph(cache, entry);
        let roots = match trace::resolve_roots(&graph, &info, from) {
            trace::Roots::Functions(roots) => roots,
            trace::Roots::AllRoleBearers(all) if !all.is_empty() => all,
            trace::Roots::AllRoleBearers(_) => {
                return Some(
                    "noeta trace\n\nno `@role` bindings on any function — nothing to trace\n"
                        .to_string(),
                );
            }
            trace::Roots::NotFound => {
                return Some(format!(
                    "noeta trace\n\n`{}` matches no role binding and no function\n",
                    from.unwrap_or_default()
                ));
            }
        };
        let walked = trace::walk(
            &graph,
            &trace::roles_by_target(&info),
            &roots,
            trace::DEFAULT_MAX_DEPTH,
            trace::NODE_BUDGET,
        );
        Some(self.render_trace(cache, from, &walked))
    }

    /// Render a finished walk as the trace document: a header, the `boundaries reached` summary,
    /// then one indented tree per root with `path:line` locations on every locatable node.
    fn render_trace(
        &self,
        cache: &WorkspaceCache,
        from: Option<&str>,
        walked: &trace::Trace,
    ) -> String {
        let mut out = String::new();
        match from {
            Some(spec) => out.push_str(&format!("noeta trace — from {spec}\n")),
            None => out.push_str("noeta trace — from every role-bearing function\n"),
        }

        if !walked.boundaries.is_empty() {
            out.push_str("\nboundaries reached\n");
            for b in &walked.boundaries {
                let loc = b
                    .decl_span
                    .and_then(|span| self.trace_loc(cache, span))
                    .map(|loc| format!(" ({loc})"))
                    .unwrap_or_default();
                out.push_str(&format!("  ⚑ {} — {}{}\n", b.role, b.target, loc));
            }
        }

        for root in &walked.roots {
            out.push('\n');
            out.push_str(&self.trace_node_line(cache, root));
            out.push('\n');
            self.render_children(cache, root, "", &mut out);
        }
        if walked.truncated {
            out.push_str("\n(truncated — node budget reached)\n");
        }
        out
    }

    fn render_children(
        &self,
        cache: &WorkspaceCache,
        node: &trace::TraceNode,
        prefix: &str,
        out: &mut String,
    ) {
        let last = node.children.len().saturating_sub(1);
        for (i, child) in node.children.iter().enumerate() {
            let (branch, cont) = if i == last {
                ("└── ", "    ")
            } else {
                ("├── ", "│   ")
            };
            out.push_str(prefix);
            out.push_str(branch);
            out.push_str(&self.trace_node_line(cache, child));
            out.push('\n');
            self.render_children(cache, child, &format!("{prefix}{cont}"), out);
        }
    }

    /// One node's line: name, role flags, location, and the honesty markers (reference /
    /// external / dynamic / cycle / truncation).
    fn trace_node_line(&self, cache: &WorkspaceCache, node: &trace::TraceNode) -> String {
        let mut line = node.name.clone();
        if !node.roles.is_empty() {
            line.push_str(&format!("  ⚑ {}", node.roles.join(", ")));
        }
        if let Some(loc) = node.decl_span.and_then(|span| self.trace_loc(cache, span)) {
            line.push_str(&format!("  {loc}"));
        }
        if node.kind == trace::TraceKind::Reference {
            line.push_str("  (reference — passed as value)");
        }
        if node.external {
            line.push_str("  [external]");
        }
        if node.dynamic {
            line.push_str("  [dynamic]");
        }
        if node.cycle {
            line.push_str("  (cycle)");
        }
        if node.truncated {
            line.push_str("  …");
        }
        line
    }

    /// A span as `path:line` (1-based), path relative to the workspace entry's directory when it
    /// lives inside it — the shape the extension's link provider turns into a clickable location.
    fn trace_loc(&self, cache: &WorkspaceCache, span: Span) -> Option<String> {
        let (uri, range) = self.locate(cache, span, Encoding::Utf8)?;
        let line = range.start.line + 1;
        let entry_dir = cache
            .source_uris
            .first()
            .and_then(|entry| uri_to_path(entry))
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let path = match uri_to_path(&uri) {
            Some(p) => match entry_dir
                .as_deref()
                .and_then(|dir| p.strip_prefix(dir).ok())
            {
                Some(rel) => rel.display().to_string(),
                None => p.display().to_string(),
            },
            None => uri,
        };
        Some(format!("{path}:{line}"))
    }

    /// The workspace's architectural surface (ide-ui U3): every `@role`, with its bearers, in
    /// declaration order — the Architecture view's top level. A bearer that is a graph function
    /// is expandable (its outgoing calls unfold lazily via
    /// [`architecture_children`](Self::architecture_children)); a role on a non-function
    /// declaration is a located, non-expandable entry.
    pub fn architecture(&self, uri: &str, encoding: Encoding) -> Option<Vec<ArchRole>> {
        let (cache, entry, _) = self.workspace_of(uri)?;
        let (graph, info) = self.call_graph(cache, entry);
        let roles_map = trace::roles_by_target(&info);
        let mut groups: Vec<ArchRole> = Vec::new();
        for r in &info.roles {
            let role = format!("{}.{}", r.enum_name, r.variant);
            let (uri, range) = match self.locate(cache, r.target_span, encoding) {
                Some((uri, range)) => (Some(uri), Some(range)),
                None => (None, None),
            };
            let expandable = graph
                .function_named(&r.target)
                .is_some_and(|idx| graph.edges_from(Some(idx)).next().is_some());
            let node = ArchNode {
                name: r.target.clone(),
                roles: roles_map.get(&r.target).cloned().unwrap_or_default(),
                uri,
                range,
                reference: false,
                external: false,
                dynamic: false,
                cycle: false,
                expandable,
            };
            match groups.iter_mut().find(|g| g.role == role) {
                Some(group) => {
                    if !group.bearers.contains(&node) {
                        group.bearers.push(node);
                    }
                }
                None => groups.push(ArchRole {
                    role,
                    bearers: vec![node],
                }),
            }
        }
        Some(groups)
    }

    /// One lazily-unfolded level of the Architecture view: what `function` (a graph name —
    /// `handle`, `Counter.bump`) calls or references, external/dynamic callees as labeled leaves.
    /// An unknown function yields no children (the view shows a leaf), not an error.
    pub fn architecture_children(
        &self,
        uri: &str,
        function: &str,
        encoding: Encoding,
    ) -> Option<Vec<ArchNode>> {
        let (cache, entry, _) = self.workspace_of(uri)?;
        let (graph, info) = self.call_graph(cache, entry);
        let Some(idx) = graph.function_named(function) else {
            return Some(Vec::new());
        };
        let walked = trace::walk(
            &graph,
            &trace::roles_by_target(&info),
            &[idx],
            1,
            trace::NODE_BUDGET,
        );
        let Some(root) = walked.roots.first() else {
            return Some(Vec::new());
        };
        Some(
            root.children
                .iter()
                .map(|child| {
                    let (uri, range) = match child
                        .decl_span
                        .and_then(|s| self.locate(cache, s, encoding))
                    {
                        Some((uri, range)) => (Some(uri), Some(range)),
                        None => (None, None),
                    };
                    ArchNode {
                        name: child.name.clone(),
                        roles: child.roles.clone(),
                        uri,
                        range,
                        reference: child.kind == trace::TraceKind::Reference,
                        external: child.external,
                        dynamic: child.dynamic,
                        cycle: child.cycle,
                        // At depth 1 a function child with further calls is exactly the one the
                        // walk marked `truncated` (children cut by the depth limit).
                        expandable: child.truncated && !child.cycle,
                    }
                })
                .collect(),
        )
    }

    // ---- The unified documentation model (docs-browser arc, slice 0). ----------------------
    //
    // These four methods are the single interface every tool goes through: `noeta lsp` (the
    // editor's docs browser) and `noeta mcp` (the agent's docs tool) are both thin adapters over
    // them, so the human and the agent browse the identical tree (see [`docs`]). Each reads the
    // linked workspace program (falling back to the entry file's bare parse when a sibling fails
    // to link, so docs still work on WIP code) and resolves through [`StoreDocEnv`].

    /// Resolve `uri` to a [`docs::DocCtx`] and run `f` against it. A workspace-backed context when
    /// `uri` names an open workspace (the project corpus resolves), otherwise an **empty** context
    /// (only the workspace-independent language guide resolves). Centralizes the borrow dance —
    /// `linked`/`entry_ast`/`env` live for the closure's duration — so the four doc entry points
    /// stay one-liners.
    fn with_doc_ctx<R>(
        &self,
        uri: &str,
        encoding: Encoding,
        f: impl FnOnce(&docs::DocCtx) -> R,
    ) -> R {
        match self.workspace_of(uri) {
            Some((cache, _entry, _)) => {
                let db = &self.db;
                let env = StoreDocEnv {
                    store: self,
                    cache,
                    encoding,
                };
                // Every workspace member's own program (workspace-aware parse, so cross-file text
                // tiers hold): the project corpus documents the WHOLE directory — a sibling the
                // current file never imports is still a source file worth documenting — never
                // just one entry's import closure (which hid `hotpath.noe` next to `main.noe`).
                let member_asts: Vec<_> = cache
                    .programs
                    .iter()
                    .map(|sp| (*sp, noeta_db::ast_in(db, cache.workspace, *sp)))
                    .collect();
                let members: Vec<docs::MemberDoc> = member_asts
                    .iter()
                    .map(|(sp, ast)| docs::MemberDoc {
                        source: SourceId(sp.id(db)),
                        program: &ast.0.program,
                    })
                    .collect();
                // Thread each direct **source** dependency module's own program in — a dependency
                // module is a separate salsa input, so the deps corpus must walk its AST
                // directly. Filtered to direct manifest keys (never shadow deps).
                let direct = env.direct_source_dep_keys();
                let dep_asts: Vec<_> = cache
                    .dep_programs
                    .iter()
                    .map(|sp| noeta_db::ast(db, *sp))
                    .collect();
                let mut deps = Vec::new();
                for (i, ast) in dep_asts.iter().enumerate() {
                    // A dep module's `key` is its consumer-facing import root (what the manifest
                    // names); `root` is the package's own namespace segment.
                    let root = cache.dep_modules[i].key(db).clone();
                    if !direct.contains(&root) {
                        continue;
                    }
                    deps.push(docs::DepDoc {
                        root,
                        module_name: basename(&cache.dep_uris[i]),
                        source: SourceId((cache.programs.len() + i) as u32),
                        program: &ast.0.program,
                    });
                }
                f(&docs::DocCtx::with_deps(&env, members, deps))
            }
            None => f(&docs::DocCtx::empty()),
        }
    }

    /// The documentation corpus roots — the top level of the docs browser (`Project` and `Language
    /// Guide`). Always available: the guide browses even with no `.noe` file open.
    pub fn doc_index(&self, _uri: &str) -> Vec<docs::DocNode> {
        docs::roots()
    }

    /// One lazily-unfolded level of the docs tree under `id` (root → modules → declarations →
    /// members), mirroring the Architecture view's lazy unfolding. The language-guide subtree
    /// resolves even when `uri` names no open workspace.
    pub fn doc_children(&self, uri: &str, id: &str, encoding: Encoding) -> Vec<docs::DocNode> {
        self.with_doc_ctx(uri, encoding, |ctx| {
            docs::children(ctx, &docs::DocId(id.to_string()))
        })
    }

    /// The rendered page (signature + `@doc` prose + source location) for the node `id`, or `None`
    /// if the id names nothing in the current corpus.
    pub fn doc_page(&self, uri: &str, id: &str, encoding: Encoding) -> Option<docs::DocPage> {
        self.with_doc_ctx(uri, encoding, |ctx| {
            docs::page(ctx, &docs::DocId(id.to_string()))
        })
    }

    /// Rank the doc corpus against `query`, best-first (see [`docs::search`]). Spans the guide even
    /// with no open workspace; adds the project corpus when there is one.
    pub fn doc_search(&self, uri: &str, query: &str, encoding: Encoding) -> Vec<docs::DocHit> {
        self.with_doc_ctx(uri, encoding, |ctx| docs::search(ctx, query))
    }

    /// The doc node documenting the symbol under the cursor at `position` in `uri` — powers the
    /// editor's "show docs for symbol" command. Resolves the cursor exactly like hover
    /// ([`Self::definition_name_span`]), then maps the definition's name span to its doc id.
    pub fn doc_for_symbol(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<docs::DocId> {
        let (cache, doc, source) = self.doc_cache(uri)?;
        let def_span = self.definition_name_span(cache, doc, source, position, encoding)?;
        let db = &self.db;
        let env = StoreDocEnv {
            store: self,
            cache,
            encoding,
        };
        // The same per-member corpus `with_doc_ctx` serves: the definition's name span carries
        // its member's SourceId, and the member's own AST preserves those spans, so the lookup
        // lands on the same node the tree shows.
        let member_asts: Vec<_> = cache
            .programs
            .iter()
            .map(|sp| (*sp, noeta_db::ast_in(db, cache.workspace, *sp)))
            .collect();
        let members: Vec<docs::MemberDoc> = member_asts
            .iter()
            .map(|(sp, ast)| docs::MemberDoc {
                source: SourceId(sp.id(db)),
                program: &ast.0.program,
            })
            .collect();
        docs::id_for_name_span(&env, &members, def_span)
    }

    /// The `@test` fns declared in `uri` (ide-ui U3), in source order — what the editor's test
    /// explorer lists and its gutter run-arrows anchor to. Discovery is the runner's own
    /// [`activate_tiers`](noeta_check::activate_tiers) walk over the merged program, filtered to
    /// this file, so the explorer and `noeta test` can never disagree about what a test is.
    pub fn tests(&self, uri: &str, encoding: Encoding) -> Option<Vec<TestItem>> {
        let (cache, entry, source) = self.workspace_of(uri)?;
        let db = &self.db;
        let linked = noeta_db::linked_from(db, cache.workspace, entry);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let activated = noeta_check::activate_tiers(program, &["test"]);
        let index = LineIndex::new(self.source_text(cache, source)?);
        Some(
            activated
                .tests
                .iter()
                .filter(|t| t.span.source == source)
                .map(|t| TestItem {
                    name: t.name.clone(),
                    display: attr_str(&t.attrs, "Name"),
                    group: attr_str(&t.attrs, "Group"),
                    skipped: t.attrs.iter().any(|a| a.name == "Skip"),
                    range: index.range(t.span, encoding),
                })
                .collect(),
        )
    }

    /// The call-graph context of `uri`'s workspace: the static graph over the merged program plus
    /// the reflection index (`@role`/attribute bindings with their target spans) — the same
    /// [`callgraph`]+[`reflect`](noeta_ast::reflect) join the MCP `trace` tool serves, so editor
    /// and agent read one engine. An unlinkable workspace degrades to the entry's own AST (the
    /// within-file graph keeps working while a sibling is broken).
    fn call_graph(
        &self,
        cache: &WorkspaceCache,
        entry: SourceProgram,
    ) -> (callgraph::CallGraph, noeta_ast::reflect::ReflectionInfo) {
        let db = &self.db;
        let linked = noeta_db::linked_from(db, cache.workspace, entry);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let ide = noeta_db::linked_checked_ide_from(db, cache.workspace, entry);
        // Texts by SourceId index: members, then dependency modules (ids continue past).
        let texts: Vec<&str> = cache
            .programs
            .iter()
            .chain(&cache.dep_programs)
            .map(|p| p.text(db).as_str())
            .collect();
        let graph = callgraph::build(program, &ide.expr_types, &texts);
        (graph, noeta_ast::reflect::build(program))
    }

    /// Resolve graph function `idx` to a located [`HierarchyItem`] (roles joined by declaration
    /// name). `None` if its spans map to no known file.
    fn hierarchy_item(
        &self,
        cache: &WorkspaceCache,
        graph: &callgraph::CallGraph,
        roles: &HashMap<String, Vec<String>>,
        idx: usize,
        encoding: Encoding,
    ) -> Option<HierarchyItem> {
        let f = &graph.functions[idx];
        let (uri, selection_range) = self.locate(cache, f.name_span, encoding)?;
        let (_, range) = self.locate(cache, f.decl_span, encoding)?;
        Some(HierarchyItem {
            name: f.name.clone(),
            kind: if f.method {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            },
            roles: roles.get(&f.name).cloned().unwrap_or_default(),
            uri,
            range,
            selection_range,
        })
    }

    /// Map a definition `span` (whose [`SourceId`] names the file it belongs to) to the `(URI,
    /// range)` the editor jumps to, resolving the range against that file's own text.
    fn locate(
        &self,
        cache: &WorkspaceCache,
        span: Span,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let idx = span.source.0 as usize;
        // Entry + siblings are indexed directly; a dependency module's SourceId continues past them
        // (see `resolve_dep_modules`), so it maps into the dep arrays (package-manager P2.1c).
        let (uri, program) = if idx < cache.programs.len() {
            (
                cache.source_uris.get(idx)?.clone(),
                *cache.programs.get(idx)?,
            )
        } else {
            let di = idx - cache.programs.len();
            (
                cache.dep_uris.get(di)?.clone(),
                *cache.dep_programs.get(di)?,
            )
        };
        let index = LineIndex::new(program.text(&self.db));
        Some((uri, index.range(span, encoding)))
    }
}

/// Map an outline [`SymbolNode`](symbols::SymbolNode) to its resolved [`DocumentSymbol`], resolving
/// the declaration span to `range` and the name span to `selection_range`, and recursing into
/// children.
fn to_document_symbol(
    index: &LineIndex,
    node: &symbols::SymbolNode,
    encoding: Encoding,
) -> DocumentSymbol {
    DocumentSymbol {
        name: node.name.clone(),
        detail: node.detail.clone(),
        kind: node.kind,
        range: index.range(node.full_span, encoding),
        selection_range: index.range(node.name_span, encoding),
        children: node
            .children
            .iter()
            .map(|child| to_document_symbol(index, child, encoding))
            .collect(),
    }
}

/// The first string argument of the attribute named `name` (`#[Name("…")]`, `#[Group("…")]`), if
/// the declaration carries it.
fn attr_str(attrs: &[noeta_ast::Attribute], name: &str) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == name)?
        .args
        .iter()
        .find_map(|arg| match &arg.value {
            noeta_ast::AttrValue::Str(s) => Some(s.clone()),
            _ => None,
        })
}

/// The declared type name a reflected [`TypeRepr`] refers to, for member resolution — the nominal
/// variants (`struct` / `class` / `enum` / unknown-kind named). Scalars, containers, functions, and
/// unions have no user declaration to jump into, so they yield `None`.
/// Bundle-contributed members for a receiver type (kernel-methods K4): a bound `@packed` type
/// `T` gets its bundles' `Element` methods, a `List<T>` their `Bulk` methods; anything else none.
fn bundle_members_for(
    repr: &TypeRepr,
    bindings: &std::collections::HashMap<String, Vec<(String, String)>>,
) -> Vec<completion::Candidate> {
    use noeta_stdlib::BundleReceiver;
    let (name, kind) = match repr {
        TypeRepr::List(elem) => match nominal_name(elem) {
            Some(n) => (n, BundleReceiver::Bulk),
            None => return Vec::new(),
        },
        other => match nominal_name(other) {
            Some(n) => (n, BundleReceiver::Element),
            None => return Vec::new(),
        },
    };
    match bindings.get(name) {
        Some(b) => completion::bundle_members(b, kind),
        None => Vec::new(),
    }
}

fn nominal_name(repr: &TypeRepr) -> Option<&str> {
    match repr {
        TypeRepr::Struct(name, _)
        | TypeRepr::Class(name, _)
        | TypeRepr::Enum(name, _)
        | TypeRepr::Named(name, _) => Some(name),
        _ => None,
    }
}

/// The human-readable storage note for a hovered type, when its storage is non-default: a `@packed`
/// nominal (with its flat byte size) or a `List` of one (stored as a single contiguous buffer, row-
/// or column-major per the `@packed(layout: …)` declaration). `None` for everything else — types
/// with ordinary boxed storage say nothing. Shared by LSP hover and the MCP `type_at` tool so both
/// surfaces describe layout identically. Only a directly-packed list element specializes (matching
/// the runtime: flatness is derived from "element type is packed"; Set/Map never specialize today).
pub fn layout_note(repr: &TypeRepr, layouts: &HashMap<String, PackedLayout>) -> Option<String> {
    match repr {
        TypeRepr::List(elem) => {
            let layout = layouts.get(nominal_name(elem)?)?;
            let order = if layout.column {
                "column-major (SoA)"
            } else {
                "row-major"
            };
            Some(format!(
                "flat packed storage — {} bytes/element, {order}",
                layout.byte_size()
            ))
        }
        other => {
            let layout = layouts.get(nominal_name(other)?)?;
            let column = if layout.column {
                ", column-major lists (SoA)"
            } else {
                ""
            };
            Some(format!("@packed — {} bytes{column}", layout.byte_size()))
        }
    }
}

/// The [`docs::DocEnv`] the store supplies to the unified doc model: resolve a declaration span to
/// an editor location via [`DocumentStore::locate`], and name a project source (excluding
/// dependency modules, whose ids sit past the entry+siblings range). Ephemeral per request.
struct StoreDocEnv<'a> {
    store: &'a DocumentStore,
    cache: &'a WorkspaceCache,
    encoding: Encoding,
}

impl docs::DocEnv for StoreDocEnv<'_> {
    fn locate(&self, span: Span) -> Option<docs::DocLoc> {
        self.store
            .locate(self.cache, span, self.encoding)
            .map(|(uri, range)| docs::DocLoc { uri, range })
    }

    fn source_name(&self, source: SourceId) -> Option<String> {
        let idx = source.0 as usize;
        // Project sources are entry + siblings (indices below `programs.len()`); a dependency
        // module's id continues past them and is excluded from the project corpus.
        if idx >= self.cache.programs.len() {
            return None;
        }
        self.cache.source_uris.get(idx).map(|uri| basename(uri))
    }

    fn dependencies(&self) -> Vec<docs::DepInfo> {
        let Some(manifest) = self.load_manifest() else {
            return Vec::new();
        };
        let source = self.source_dep_roots();
        let keys: Vec<String> = manifest.dependencies().keys().cloned().collect();
        // Native classification needs the resolved graph; if it can't resolve (network/IO), those
        // deps simply read as unresolved rather than failing the whole listing.
        let native = self.native_dep_keys(&keys);
        manifest
            .dependencies()
            .iter()
            .map(|(root, dep)| {
                let base = describe_dep(dep);
                // A key with linked `.noe` modules is browsable source; else a resolved native
                // package is a placeholder; else it isn't on disk yet.
                let (detail, kind) = if source.iter().any(|r| r == root) {
                    (base, docs::DepKind::Source)
                } else if native.iter().any(|r| r == root) {
                    (format!("{base} · native"), docs::DepKind::Native)
                } else {
                    (format!("{base} · not fetched"), docs::DepKind::Unresolved)
                };
                docs::DepInfo {
                    root: root.clone(),
                    detail,
                    kind,
                }
            })
            .collect()
    }
}

impl StoreDocEnv<'_> {
    /// The workspace's entry file path (the first member), for manifest discovery and dependency
    /// resolution.
    fn entry_path(&self) -> Option<PathBuf> {
        self.cache
            .source_uris
            .first()
            .and_then(|uri| uri_to_path(uri))
    }

    /// Load the workspace's `noeta.toml`, if one exists at or above the entry directory.
    fn load_manifest(&self) -> Option<noeta_pm::manifest::Manifest> {
        let entry = self.entry_path()?;
        let dir = entry.parent()?;
        let manifest_path = noeta_pm::manifest::find(dir)?;
        noeta_pm::manifest::load(&manifest_path).ok()
    }

    /// The **direct** dependency import roots that resolved to `.noe` **source** modules — the
    /// manifest's own keys (never transitive/shadow deps) intersected with the roots that have
    /// linked modules. Scopes the dependencies corpus and its search to direct source packages.
    fn direct_source_dep_keys(&self) -> Vec<String> {
        let Some(manifest) = self.load_manifest() else {
            return Vec::new();
        };
        let source = self.source_dep_roots();
        manifest
            .dependencies()
            .keys()
            .filter(|k| source.iter().any(|r| r == *k))
            .cloned()
            .collect()
    }

    /// The distinct import **keys** of every dependency module linked into the program (direct and
    /// transitive) — the query-free "has source" set. A [`DepModule`]'s `key` is the consumer-facing
    /// import root (the dependency-table key it was re-rooted to), which is what the manifest names;
    /// its `root` is the package's *own* namespace segment (e.g. `vec`), which the manifest does not.
    /// Callers intersect with the manifest's direct keys to keep the dependencies corpus to direct
    /// deps only (never shadow deps).
    fn source_dep_roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .cache
            .dep_modules
            .iter()
            .map(|d| d.key(&self.store.db).clone())
            .collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// Which of `keys` (the manifest's direct import roots) name a **native** package, from the
    /// side-effect-free resolve graph's native entry crates. A pure-native package ships no `.noe`
    /// modules, so it never appears in the linked program — only here. A native crate's identity is
    /// `company/package`; a consumer keys it by the shared **scope** (`para` ⊃ `para/p2p`, so the
    /// key equals the company) or, for a single package, by its **root** segment (the `package`
    /// half), so we match a key against either. Empty on any resolution failure — such deps then
    /// read as unresolved rather than failing the whole listing.
    fn native_dep_keys(&self, keys: &[String]) -> Vec<String> {
        let Some(entry) = self.entry_path() else {
            return Vec::new();
        };
        let Ok(graph) = noeta_pm::graph::resolve_graph_query(&entry) else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::new();
        for nc in &graph.native_crates {
            let (company, package) = nc.identity.split_once('/').unwrap_or(("", &nc.identity));
            for key in keys {
                if (key == company || key == package) && !out.contains(key) {
                    out.push(key.clone());
                }
            }
        }
        out
    }
}

/// A dim, human detail for a dependency row — its source and (for a registry dep) version.
fn describe_dep(dep: &noeta_pm::manifest::Dependency) -> String {
    use noeta_pm::manifest::Dependency;
    match dep {
        Dependency::Path { path } => format!("path {}", path.display()),
        Dependency::Git { url, git_ref } => {
            let name = url
                .rsplit('/')
                .next()
                .unwrap_or(url)
                .trim_end_matches(".git");
            format!("git {name}@{}", git_ref.describe())
        }
        Dependency::Registry { package, req } => match package {
            Some(p) => format!("{}/{} {req}", p.company, p.package),
            None => req.to_string(),
        },
        Dependency::Scope(members) => format!("scope · {} packages", members.len()),
    }
}

/// The trailing path segment of a document uri — its file name — for a module's display title.
fn basename(uri: &str) -> String {
    uri.rsplit(['/', '\\']).next().unwrap_or(uri).to_string()
}

/// The top-level function declaration named `name`, for signature help on a plain call.
fn top_level_fn<'a>(program: &'a noeta_ast::Program, name: &str) -> Option<&'a noeta_ast::FnDecl> {
    program.stmts.iter().find_map(|stmt| match stmt {
        noeta_ast::Stmt::Fn(decl) if decl.name == name => Some(decl),
        _ => None,
    })
}

/// The `@doc` prose attached to the declaration whose name span is `name_span`, dedented — the
/// body every doc-bearing hover appends. Adjacency-resolved from the merged program (see
/// [`noeta_check::resolve_docs`]).
fn doc_prose_at(program: &noeta_ast::Program, name_span: Span) -> Option<String> {
    noeta_check::resolve_docs(program)
        .into_iter()
        .find_map(|doc| match doc.target {
            noeta_check::DocTarget::Decl {
                name_span: span, ..
            } if span == name_span => Some(noeta_check::dedent_doc(&doc.text).trim().to_string()),
            _ => None,
        })
}

/// The members of module `prefix`, for a `use`-path hover: the registry's namespace children
/// (native modules and types) unioned with the merged program's own declarations exactly one
/// segment below the prefix (a source dependency's fns/types). Sorted, deduped, backticked.
fn module_members(program: &noeta_ast::Program, prefix: &str) -> Vec<String> {
    let mut members: Vec<String> =
        noeta_stdlib::registry::single_registry_process().namespace_children(prefix);
    // A native module's functions (`std.math` → sqrt, pow, …): namespace_children lists only
    // submodules and extern types, but for a leaf module the functions ARE the members.
    if let Some(module) = api::module(prefix) {
        members.extend(module.functions.iter().map(|f| f.name.clone()));
    }
    let dotted = format!("{prefix}.");
    let mut push_leaf = |name: &str| {
        if let Some(rest) = name.strip_prefix(&dotted)
            && !rest.is_empty()
            && !rest.contains('.')
        {
            members.push(rest.to_string());
        }
    };
    for stmt in &program.stmts {
        match stmt {
            noeta_ast::Stmt::Fn(decl) => push_leaf(&decl.name),
            noeta_ast::Stmt::Struct(decl) => push_leaf(&decl.name),
            noeta_ast::Stmt::Class(decl) => push_leaf(&decl.name),
            noeta_ast::Stmt::Enum(decl) => push_leaf(&decl.name),
            _ => {}
        }
    }
    members.sort();
    members.dedup();
    members.into_iter().map(|m| format!("`{m}`")).collect()
}

/// Render a function/method declaration in Noeta surface syntax for a signature hover:
/// `fn manhattan(): int`, `fn bump(by: int): void`, or `fn origin()` when the return type is
/// unannotated. Parameters use the same `name: T` spelling as signature help and symbol detail.
fn render_fn_signature(decl: &noeta_ast::FnDecl) -> String {
    let params: Vec<String> = decl.params.iter().map(symbols::param_detail).collect();
    let head = format!("fn {}({})", decl.name, params.join(", "));
    match &decl.ret {
        Some(ret) => format!("{head}: {}", symbols::render_type_ref(ret)),
        None => head,
    }
}

/// Render generic type parameters as `<A, B>`, or `""` when there are none — for a type-definition
/// hover header (`struct Pair<A, B>`).
fn render_type_params(params: &[noeta_ast::TypeParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
    format!("<{}>", names.join(", "))
}

/// One `struct`/`class` field rendered as a body line: `[pub ][mut ]name: Type[ = default]`. The
/// default value is sliced from `text` when it lives in the file under the cursor (`source`); a field
/// whose declaration was imported from another file shows `= …` since its source text is not `text`.
fn render_field(field: &noeta_ast::FieldDecl, text: &str, source: SourceId) -> String {
    let mut line = String::new();
    if field.is_public {
        line.push_str("pub ");
    }
    if field.mut_field {
        line.push_str("mut ");
    }
    line.push_str(&field.name);
    if let Some(ty) = &field.ty {
        line.push_str(": ");
        line.push_str(&symbols::render_type_ref(ty));
    }
    if let Some(default) = &field.default {
        let span = default.span();
        if span.source == source {
            line.push_str(" = ");
            line.push_str(&text[span.range()]);
        } else {
            line.push_str(" = …");
        }
    }
    line
}

/// One enum variant rendered as a body line: `Name`, `Name(A, B)` for an algebraic variant, or
/// `Name = <backed>` for a backed enum, with the backed value sliced from `text` when in-file.
fn render_variant(variant: &noeta_ast::VariantDecl, text: &str, source: SourceId) -> String {
    let mut line = variant.name.clone();
    if !variant.fields.is_empty() {
        let fields: Vec<String> = variant.fields.iter().map(symbols::param_detail).collect();
        line.push_str(&format!("({})", fields.join(", ")));
    }
    if let Some(backed) = &variant.backed_value {
        let span = backed.span();
        if span.source == source {
            line.push_str(&format!(" = {}", &text[span.range()]));
        }
    }
    line
}

/// Render the declaration of the `struct`/`class`/`enum` named `name` in surface syntax for a
/// type-definition hover: the header (`struct Point`, with generic params), the fields or variants,
/// and each method's signature — one member per line, four-space indented. `None` when no such type
/// is declared in `program`.
fn render_type_definition(
    program: &noeta_ast::Program,
    name: &str,
    text: &str,
    source: SourceId,
) -> Option<String> {
    // Header keyword, member lines, and method signatures for the matched declaration.
    let (keyword, generics, mut members, methods): (
        &str,
        String,
        Vec<String>,
        &[noeta_ast::FnDecl],
    ) = program.stmts.iter().find_map(|stmt| match stmt {
        noeta_ast::Stmt::Struct(d) if d.name == name => Some((
            "struct",
            render_type_params(&d.type_params),
            d.fields
                .iter()
                .map(|f| render_field(f, text, source))
                .collect(),
            d.methods.as_slice(),
        )),
        noeta_ast::Stmt::Class(d) if d.name == name => Some((
            "class",
            render_type_params(&d.type_params),
            d.fields
                .iter()
                .map(|f| render_field(f, text, source))
                .collect(),
            d.methods.as_slice(),
        )),
        noeta_ast::Stmt::Enum(d) if d.name == name => Some((
            "enum",
            render_type_params(&d.type_params),
            d.variants
                .iter()
                .map(|v| render_variant(v, text, source))
                .collect(),
            d.methods.as_slice(),
        )),
        _ => None,
    })?;
    members.extend(methods.iter().map(render_fn_signature));
    if members.is_empty() {
        return Some(format!("{keyword} {name}{generics} {{}}"));
    }
    let body = members
        .iter()
        .map(|m| format!("    {m}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{keyword} {name}{generics} {{\n{body}\n}}"))
}

/// Whether `name` is a type declared in this program (a `struct`/`class`/`enum`) — the signal that a
/// member-access receiver like `Point` in `Point.origin` is the type itself (an associated-function
/// call) rather than a value.
fn is_declared_type(program: &noeta_ast::Program, name: &str) -> bool {
    program.stmts.iter().any(|stmt| {
        matches!(stmt,
            noeta_ast::Stmt::Struct(d) if d.name == name)
            || matches!(stmt, noeta_ast::Stmt::Class(d) if d.name == name)
            || matches!(stmt, noeta_ast::Stmt::Enum(d) if d.name == name)
    })
}

/// The method/associated-function declaration whose **own name** covers `offset` in file `source`,
/// scanning every type's methods — so a signature hover works on a method's declaration name
/// (`fn manhattan` in `struct Point`), not only at call sites. `None` if no method name is under the
/// cursor.
fn method_decl_at(
    program: &noeta_ast::Program,
    offset: u32,
    source: SourceId,
) -> Option<&noeta_ast::FnDecl> {
    let covers = |decl: &noeta_ast::FnDecl| {
        decl.name_span.source == source
            && decl.name_span.start <= offset
            && offset <= decl.name_span.end
    };
    program.stmts.iter().find_map(|stmt| {
        let methods = match stmt {
            noeta_ast::Stmt::Struct(d) => &d.methods,
            noeta_ast::Stmt::Class(d) => &d.methods,
            noeta_ast::Stmt::Enum(d) => &d.methods,
            _ => return None,
        };
        methods.iter().find(|m| covers(m))
    })
}

/// The `(name, tier-name span)` of the `@<name> { … }` tier block/expression whose tier name
/// covers `offset` in file `source`, if any (expr-tiers/text-tiers hover). Walks statement bodies
/// and the expression positions a `TierExpr` can occupy — a block is usually a binding value,
/// `echo`, `return`, or call argument.
fn tier_name_at(
    program: &noeta_ast::Program,
    offset: u32,
    source: SourceId,
) -> Option<(String, Span)> {
    fn covers(span: Span, offset: u32, source: SourceId) -> bool {
        span.source == source && span.start <= offset && offset <= span.end
    }
    fn in_stmts(
        stmts: &[noeta_ast::Stmt],
        offset: u32,
        source: SourceId,
    ) -> Option<(String, Span)> {
        stmts.iter().find_map(|s| in_stmt(s, offset, source))
    }
    fn in_fn(f: &noeta_ast::FnDecl, offset: u32, source: SourceId) -> Option<(String, Span)> {
        f.directives
            .iter()
            .find(|dir| covers(dir.name_span, offset, source))
            .map(|dir| (dir.name.clone(), dir.name_span))
            .or_else(|| in_stmts(&f.body, offset, source))
    }
    fn in_stmt(stmt: &noeta_ast::Stmt, offset: u32, source: SourceId) -> Option<(String, Span)> {
        use noeta_ast::Stmt;
        match stmt {
            Stmt::TierBlock {
                tier,
                tier_span,
                items,
                ..
            } => covers(*tier_span, offset, source)
                .then(|| (tier.clone(), *tier_span))
                .or_else(|| in_stmts(items, offset, source)),
            Stmt::Echo { value, .. }
            | Stmt::Binding { value, .. }
            | Stmt::Destructure { value, .. }
            | Stmt::Yield { value, .. }
            | Stmt::Expr { expr: value, .. } => in_expr(value, offset, source),
            Stmt::Return { value, .. } => value.as_ref().and_then(|v| in_expr(v, offset, source)),
            Stmt::Fn(f) => in_fn(f, offset, source),
            // A type declaration: its methods may carry leading `@<tier>` directives
            // (directive-sites arc) — the tier name there hovers like any other tier position.
            Stmt::Struct(d) => d.methods.iter().find_map(|m| in_fn(m, offset, source)),
            Stmt::Class(d) => d.methods.iter().find_map(|m| in_fn(m, offset, source)),
            Stmt::Enum(d) => d.methods.iter().find_map(|m| in_fn(m, offset, source)),
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => in_expr(cond, offset, source)
                .or_else(|| in_stmts(then_body, offset, source))
                .or_else(|| else_body.as_ref().and_then(|b| in_stmts(b, offset, source))),
            Stmt::For { iterable, body, .. } => {
                in_expr(iterable, offset, source).or_else(|| in_stmts(body, offset, source))
            }
            Stmt::While { cond, body, .. } => {
                in_expr(cond, offset, source).or_else(|| in_stmts(body, offset, source))
            }
            Stmt::Concurrent { body, .. } => in_stmts(body, offset, source),
            _ => None,
        }
    }
    fn in_expr(expr: &noeta_ast::Expr, offset: u32, source: SourceId) -> Option<(String, Span)> {
        use noeta_ast::{ClosureBody, Expr};
        // Only recurse into an expression whose extent covers the cursor — the fast reject.
        if !covers(expr.span(), offset, source) {
            return None;
        }
        match expr {
            Expr::TierExpr {
                tier,
                tier_span,
                holes,
                ..
            } => covers(*tier_span, offset, source)
                .then(|| (tier.clone(), *tier_span))
                .or_else(|| holes.iter().find_map(|h| in_expr(h, offset, source))),
            Expr::Call { callee, args, .. } => in_expr(callee, offset, source)
                .or_else(|| args.iter().find_map(|a| in_expr(a, offset, source))),
            Expr::Binary { lhs, rhs, .. }
            | Expr::Pipeline {
                left: lhs,
                right: rhs,
                ..
            }
            | Expr::Coalesce {
                value: lhs,
                fallback: rhs,
                ..
            }
            | Expr::Index {
                receiver: lhs,
                index: rhs,
                ..
            }
            | Expr::Range {
                start: lhs,
                end: rhs,
                ..
            } => in_expr(lhs, offset, source).or_else(|| in_expr(rhs, offset, source)),
            Expr::Unary { operand, .. } => in_expr(operand, offset, source),
            Expr::Member { receiver, .. } | Expr::TupleIndex { receiver, .. } => {
                in_expr(receiver, offset, source)
            }
            Expr::List { items, .. } | Expr::Tuple { items, .. } => {
                items.iter().find_map(|i| in_expr(i, offset, source))
            }
            Expr::Map { entries, .. } => entries.iter().find_map(|(k, v)| {
                in_expr(k, offset, source).or_else(|| in_expr(v, offset, source))
            }),
            Expr::Closure { body, .. } => match body {
                ClosureBody::Expr(e) => in_expr(e, offset, source),
                ClosureBody::Block(stmts) => in_stmts(stmts, offset, source),
            },
            Expr::Match {
                scrutinee, arms, ..
            } => in_expr(scrutinee, offset, source).or_else(|| {
                arms.iter().find_map(|a| match &a.body {
                    noeta_ast::ClosureBody::Expr(e) => in_expr(e, offset, source),
                    noeta_ast::ClosureBody::Block(stmts) => in_stmts(stmts, offset, source),
                })
            }),
            Expr::Object(lit) => lit
                .fields
                .iter()
                .find_map(|f| in_expr(&f.value, offset, source))
                .or_else(|| lit.spread.as_ref().and_then(|s| in_expr(s, offset, source))),
            Expr::Try { expr, .. }
            | Expr::Await { expr, .. }
            | Expr::Spawn { future: expr, .. }
            | Expr::As { expr, .. }
            | Expr::TypeTest { expr, .. }
            | Expr::TypeOf { value: expr, .. }
            | Expr::FieldsOf { value: expr, .. }
            | Expr::FromBytes { blob: expr, .. } => in_expr(expr, offset, source),
            _ => None,
        }
    }
    in_stmts(&program.stmts, offset, source)
}

/// The method named `method` on the type named `type_name`, for signature help on a method call.
fn type_method<'a>(
    program: &'a noeta_ast::Program,
    type_name: &str,
    method: &str,
) -> Option<&'a noeta_ast::FnDecl> {
    program.stmts.iter().find_map(|stmt| {
        let methods = match stmt {
            noeta_ast::Stmt::Struct(decl) if decl.name == type_name => &decl.methods,
            noeta_ast::Stmt::Class(decl) if decl.name == type_name => &decl.methods,
            noeta_ast::Stmt::Enum(decl) if decl.name == type_name => &decl.methods,
            _ => return None,
        };
        methods.iter().find(|m| m.name == method)
    })
}

/// The declared type name of the receiver at `receiver_span`, for method signature help. The call the
/// cursor is inside is unclosed (`recv.m(…|`), so the receiver does not type-check as written; the
/// call is closed in a copy that is re-checked off the salsa graph. At an argument boundary (right
/// after `(` or `,`) a synthetic argument is inserted before the `)` so no dangling comma trips the
/// parser (which would leave the receiver untyped); mid-argument, a bare `)` suffices. The receiver
/// precedes the insertion, so its span is unchanged. The munged copy is parsed under the
/// receiver span's own [`SourceId`] (the document's id in its shared directory workspace), so the
/// standalone check's `expr_types` keys match the span being looked up. `None` if its type is not
/// a nominal.
fn receiver_type_at(text: &str, offset: u32, receiver_span: Span) -> Option<String> {
    let o = offset as usize;
    let at_arg_boundary = {
        let before = text[..o].trim_end();
        before.ends_with('(') || before.ends_with(',')
    };
    let closer = if at_arg_boundary { "x)" } else { ")" };
    let munged = format!("{}{closer}{}", &text[..o], &text[o..]);
    let source = noeta_span::Source::new(receiver_span.source, "<signature>", &munged);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    let checked = noeta_check::check_all_with_types(&parsed.program);
    let type_name = nominal_name(checked.expr_types.get(&receiver_span)?)?;
    Some(type_name.to_string())
}

/// Whether `name` is a syntactically valid identifier — a non-empty run starting with a letter or
/// `_`, then letters, digits, or `_`. Guards rename from writing a new name that would not lex as an
/// identifier (an operator, a number, whitespace), which would corrupt the source.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Whether byte `offset` sits immediately after a lone `.` — the bare member-access position a `.`
/// trigger fires in (`c.|`). Excludes a preceding `..` (range/spread) so `a..|b` is not mistaken for
/// a member access.
fn is_bare_dot(text: &str, offset: u32) -> bool {
    let o = offset as usize;
    let bytes = text.as_bytes();
    o >= 1 && o <= bytes.len() && bytes[o - 1] == b'.' && (o < 2 || bytes[o - 2] != b'.')
}

/// Member candidates for a bare dot (`receiver.|`): the statement does not parse with a dangling dot,
/// so a synthetic member name is spliced in at `offset`, and the copy is re-lexed/parsed/checked off
/// the salsa graph to recover the receiver's type. The members themselves are listed from `program`
/// (the live merged program). Single-file: the receiver type must be declared in the edited file, so
/// its type resolves in the standalone check. `None` if there is no member access or the receiver's
/// type is not a known nominal.
fn bare_dot_members(
    text: &str,
    offset: u32,
    program: &noeta_ast::Program,
    namespaces: &std::collections::HashMap<String, String>,
) -> Option<Vec<completion::Candidate>> {
    let o = offset as usize;
    // Splice a synthetic identifier after the dot so `c.` becomes `c.a` — now a real member access
    // whose receiver (unshifted, before the insertion) gets a type.
    let munged = format!("{}a{}", &text[..o], &text[o..]);
    let source = noeta_span::Source::new(SourceId::FIRST, "<completion>", &munged);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    let checked = noeta_check::check_all_with_types(&parsed.program);
    let def_use = resolve::DefUse::build(&parsed.program);
    let (receiver_span, _member) = def_use.member_at(offset, SourceId::FIRST)?;
    // A namespace-group receiver (`http.`) has no value type — offer the group's members directly.
    if let Some(prefix) = namespaces.get(&munged[receiver_span.range()]) {
        return Some(completion::namespace_members(prefix));
    }
    let repr = checked.expr_types.get(&receiver_span)?;
    let mut members = nominal_name(repr)
        .map(|type_name| completion::members_of(program, type_name))
        .unwrap_or_default();
    // Bundle-contributed methods (kernel-methods K4), as in the parsed-member path above.
    members.extend(bundle_members_for(repr, &checked.bundle_bindings));
    Some(members)
}

/// Type-name candidates when `offset` is in a type-annotation position, else `None`. A synthetic
/// type name is spliced in at the cursor and the copy re-parsed, so an empty annotation (`x: |`) is
/// recognised (the synthetic name becomes a `TypeRef` under the cursor) while a value position — a
/// map-literal value, an initializer — is not (it parses as an expression). The names themselves come
/// from `program` (the live merged program), so imported types are offered.
///
/// Two splice forms are tried: a bare `T` (covers a parameter, field, return, or an annotation whose
/// binding already has `= value`), and `T = 0` (completes a value-less binding annotation `total: |`,
/// which does not parse without a right-hand side). Either recognising a type position is enough.
fn type_position_completion(
    text: &str,
    offset: u32,
    program: &noeta_ast::Program,
) -> Option<Vec<completion::Candidate>> {
    // A capitalized synthetic name so it parses as a type reference (`x: T`, `List<T>`).
    (splices_a_type("T", text, offset) || splices_a_type("T = 0", text, offset))
        .then(|| completion::type_names(program))
}

/// Whether splicing `insert` in at `offset` and re-parsing puts a `TypeRef` under the cursor.
fn splices_a_type(insert: &str, text: &str, offset: u32) -> bool {
    let o = offset as usize;
    let munged = format!("{}{insert}{}", &text[..o], &text[o..]);
    let source = noeta_span::Source::new(SourceId::FIRST, "<completion>", &munged);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    completion::is_type_position(&parsed.program, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::path_to_uri;

    /// A fresh store with the process-global std registry seeded first. Every IDE feature that runs
    /// the checker resolves stdlib names (`abs`, `math.sqrt`, …) through the extension registry, so
    /// it must be installed before the first front-end lookup or the checker panics. Production seeds
    /// it (a possibly *composed*, per-session registry) in the assembling binary before building the
    /// store — hence this is test-only and not folded into `DocumentStore::default`. `default_seeded`
    /// is idempotent, so calling it per test (across parallel threads) is safe.
    fn test_store() -> DocumentStore {
        noeta_stdlib::registry::default_seeded();
        DocumentStore::default()
    }

    /// The salsa input serving `uri` in its directory's shared workspace — the test-side view of
    /// [`DocumentStore::doc_cache`].
    fn doc_program(store: &DocumentStore, uri: &str) -> SourceProgram {
        store
            .doc_cache(uri)
            .expect("document is a member of an open workspace")
            .1
    }

    #[test]
    fn open_registers_a_document() {
        let mut store = test_store();
        store.open("file:///a.noe", "let x = 1".to_string());
        assert_eq!(store.buffers.len(), 1);
        let program = doc_program(&store, "file:///a.noe");
        assert_eq!(program.text(&store.db), "let x = 1");
    }

    #[test]
    fn unresolved_use_is_published_with_a_suggestion() {
        // The editor surfaces a mistyped `use` target (module-namespaces): a std module typo is an
        // E0019 the checker produces, flowing through the whole-workspace `linked_checked` query to
        // the client, and carries a "did you mean" hint.
        let mut store = test_store();
        store.open("file:///a.noe", "use std.htpt\n".to_string());
        let (diags, _) = store
            .diagnostics("file:///a.noe")
            .expect("diagnostics available");
        let unresolved = diags
            .iter()
            .find(|d| d.code == noeta_diagnostics::DiagnosticCode::UnresolvedImport)
            .expect("an E0019 for the unresolved import");
        assert_eq!(unresolved.help.as_deref(), Some("did you mean `http`?"));
    }

    #[test]
    fn hovering_directives_describes_them() {
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "@semantic\nenum L { A; }\n@attribute\n@role(L.A)\nstruct M { x: int }\n@test\nfn t(): void { assert(true, \"ok\") }\necho 1\n"
                .to_string(),
        );
        let enc = Encoding::Utf16;
        let dir = |line, ch| {
            store
                .hover_directive("file:///a.noe", Position::new(line, ch), enc)
                .map(|(v, _)| v)
        };
        assert!(
            dir(0, 3).is_some_and(|v| v.contains("role-eligible")),
            "@semantic"
        );
        assert!(
            dir(2, 3).is_some_and(|v| v.contains("metadata attribute")),
            "@attribute"
        );
        assert!(
            dir(3, 2).is_some_and(|v| v.contains("architectural-role")),
            "@role"
        );
        // `@test` is a tier, not a decorator — the tier hover describes it (code-tier arm).
        assert!(dir(5, 2).is_none());
        let (tier, _) = store
            .hover_tier("file:///a.noe", Position::new(5, 2), enc)
            .expect("@test hovers as a dev tier");
        assert!(tier.contains("noeta test"), "got: {tier}");
    }

    #[test]
    fn hovering_use_elements_describes_what_they_name() {
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "use std.math.sqrt\necho sqrt(4.0)\n".to_string(),
        );
        let enc = Encoding::Utf16;
        // The imported native function: its registry signature (and prose follows after a rule).
        let (item, _) = store
            .hover_use("file:///a.noe", Position::new(0, 14), enc)
            .expect("hover on the imported name");
        assert!(item.contains("fn sqrt(float): float"), "got: {item}");
        // A path segment: the module it names, with members.
        let (module, _) = store
            .hover_use("file:///a.noe", Position::new(0, 9), enc)
            .expect("hover on the `math` segment");
        assert!(module.starts_with("module `std.math`"), "got: {module}");
        assert!(module.contains("`sqrt`"), "members listed: {module}");
        // Outside a use statement, this hover stays silent (the others take over).
        assert!(
            store
                .hover_use("file:///a.noe", Position::new(1, 6), enc)
                .is_none()
        );
    }

    #[test]
    fn hover_doc_surfaces_the_attached_doc_block() {
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "@doc { Adds two ints. }\n\
             fn add(a: int, b: int): int { return a + b }\n\
             echo add(1, 2)\n"
                .to_string(),
        );
        // On the declaration's own name (line 1, within `add`).
        let on_decl = store.hover_doc("file:///a.noe", Position::new(1, 4), Encoding::Utf16);
        assert_eq!(on_decl.as_deref(), Some("Adds two ints."));
        // On the call site (line 2, within `add`).
        let on_call = store.hover_doc("file:///a.noe", Position::new(2, 6), Encoding::Utf16);
        assert_eq!(on_call.as_deref(), Some("Adds two ints."));
        // An undocumented position (the literal) has no doc.
        assert_eq!(
            store.hover_doc("file:///a.noe", Position::new(2, 10), Encoding::Utf16),
            None
        );
    }

    #[test]
    fn hover_doc_surfaces_a_methods_directive() {
        let mut store = test_store();
        store.open(
            "file:///m.noe",
            "struct Point {\n    \
             x: int = 0\n    \
             @doc { Distance from origin. }\n    \
             fn manhattan(): int { return self.x }\n\
             }\n\
             p = Point {}\n\
             echo p.manhattan()\n"
                .to_string(),
        );
        // On the method's declaration name (line 3, within `manhattan`).
        assert_eq!(
            store
                .hover_doc("file:///m.noe", Position::new(3, 8), Encoding::Utf16)
                .as_deref(),
            Some("Distance from origin.")
        );
        // On the call site `p.manhattan()` (line 6, within `manhattan`).
        assert_eq!(
            store
                .hover_doc("file:///m.noe", Position::new(6, 9), Encoding::Utf16)
                .as_deref(),
            Some("Distance from origin.")
        );
    }

    #[test]
    fn doc_model_browses_the_workspace_through_the_store() {
        let mut store = test_store();
        store.open(
            "file:///widgets.noe",
            "@doc { Makes a widget. }\n\
             fn make(n: int): int { return n }\n\
             struct Widget { size: int }\n\
             echo make(1)\n"
                .to_string(),
        );
        let uri = "file:///widgets.noe";
        let enc = Encoding::Utf16;

        // The roots are the project corpus, the dependencies, the language guide, and the API ref.
        let roots = store.doc_index(uri);
        assert_eq!(roots.len(), 4);
        assert_eq!(roots[0].id.as_str(), "project");
        assert_eq!(roots[1].id.as_str(), "deps");
        assert_eq!(roots[2].id.as_str(), "guide");
        assert_eq!(roots[3].id.as_str(), "api");
        // With no manifest, the dependencies corpus is empty (no direct deps to list).
        assert!(store.doc_children(uri, "deps", enc).is_empty());

        // Expand: root → module (named by file basename) → declarations.
        let modules = store.doc_children(uri, "project", enc);
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].title, "widgets.noe");
        let decls = store.doc_children(uri, modules[0].id.as_str(), enc);
        let names: Vec<&str> = decls.iter().map(|d| d.title.as_str()).collect();
        assert_eq!(names, vec!["make", "Widget"]);

        // A decl page carries prose, a signature, and — via the real StoreDocEnv — a source
        // location pointing back at the declaration.
        let make = decls.iter().find(|d| d.title == "make").unwrap();
        let page = store.doc_page(uri, make.id.as_str(), enc).unwrap();
        assert_eq!(page.markdown, "Makes a widget.");
        assert_eq!(page.signature.as_deref(), Some("(n: int) -> int"));
        let loc = page.location.expect("decl page has a resolved location");
        assert_eq!(loc.uri, uri);
        assert_eq!(loc.range.start.line, 1); // `fn make` sits on line 1 (0-based)

        // Search finds the documented declaration.
        let hits = store.doc_search(uri, "widget", enc);
        assert!(hits.iter().any(|h| h.title == "Widget"));

        // With no open workspace the guide still browses (the project root is empty).
        let no_ws = store.doc_children("file:///nonexistent.noe", "guide", enc);
        assert!(no_ws.len() > 5, "guide browses without a workspace");
        assert!(
            store
                .doc_children("file:///nonexistent.noe", "project", enc)
                .is_empty()
        );

        // "Docs for the symbol under the cursor" resolves the call site on line 3 to `make`.
        let id = store
            .doc_for_symbol(uri, Position::new(3, 6), enc)
            .expect("cursor on `make(...)` resolves to a doc node");
        assert_eq!(id.as_str(), "project/0/make");
    }

    #[test]
    fn hover_tier_describes_an_expression_block_body() {
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "@tier(sql, text: \"sql\", expr: Query)\n\
             fn q(statics: List<string>, holes: List<() -> int>): Query { return Query {} }\n\
             struct Query {}\n\
             x = @sql { select 1 }\n"
                .to_string(),
        );
        // On the `@sql` tier name (line 4): the descriptor names the language and value type.
        let on_block = store.hover_tier("file:///a.noe", Position::new(3, 5), Encoding::Utf16);
        assert_eq!(
            on_block.as_ref().map(|(d, _)| d.as_str()),
            Some("expression tier `@sql` — `sql` body, evaluates to `Query`")
        );
        // On the `@tier` declaration keyword (line 1) — not a block use — no descriptor.
        assert_eq!(
            store.hover_tier("file:///a.noe", Position::new(0, 2), Encoding::Utf16),
            None
        );
    }

    #[test]
    fn hover_tier_describes_a_doc_text_block() {
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "@doc { Some prose. }\nfn f(): int { return 1 }\n".to_string(),
        );
        // The built-in `doc` tier's body language is markdown.
        let on_doc = store.hover_tier("file:///a.noe", Position::new(0, 1), Encoding::Utf16);
        assert_eq!(
            on_doc.as_ref().map(|(d, _)| d.as_str()),
            Some(concat!(
                "documentation tier `@doc` — markdown prose that attaches to the declaration it precedes (a fn, method, or type), or documents the module when none follows. Surfaces in hover, the docs browser, and `noeta doc`"
            ))
        );
    }

    #[test]
    fn hover_tier_describes_a_native_expression_tier() {
        // std's `@json` is a *native* (extension-declared) expression tier — no program `@tier`
        // declares it. Hover reads it through the same registry surface as a program tier, so it
        // reports the declared `json` body language and the `string` value type.
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "n = \"x\"\ndoc = @json { {\"k\": ${n}} }\necho doc\n".to_string(),
        );
        let on_json = store.hover_tier("file:///a.noe", Position::new(1, 8), Encoding::Utf16);
        assert_eq!(
            on_json.as_ref().map(|(d, _)| d.as_str()),
            Some("expression tier `@json` — `json` body, evaluates to `string`")
        );
    }

    #[test]
    fn format_document_returns_a_full_replacement_edit() {
        let mut store = test_store();
        store.open("file:///a.noe", "fn  f( a ){\n echo a\n}\n".to_string());
        let edits = store
            .format_document("file:///a.noe", Encoding::Utf16)
            .expect("open document");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "fn f(a) {\n    echo a\n}\n");
        // The edit replaces from the document start.
        assert_eq!(edits[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn format_document_of_canonical_source_is_a_no_op() {
        let mut store = test_store();
        store.open("file:///a.noe", "echo 1\n".to_string());
        let edits = store
            .format_document("file:///a.noe", Encoding::Utf16)
            .expect("open document");
        assert!(edits.is_empty(), "already-formatted source needs no edit");
    }

    #[test]
    fn format_document_declines_unparseable_source() {
        let mut store = test_store();
        store.open("file:///a.noe", "fn (".to_string());
        // Broken source yields no edits (the LSP returns `None`), leaving the buffer untouched.
        let edits = store.format_document("file:///a.noe", Encoding::Utf16);
        assert!(edits.unwrap_or_default().is_empty());
    }

    #[test]
    fn format_on_type_reformats_the_closed_block() {
        let mut store = test_store();
        // Cursor at end of line 1 (just after the fn's `}`), UTF-16.
        store.open("file:///a.noe", "fn  f( a ){\n    echo a\n}\n".to_string());
        let edits = store
            .format_on_type("file:///a.noe", Position::new(2, 1), Encoding::Utf16)
            .expect("a parseable doc reformats the enclosing statement");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "fn f(a) {\n    echo a\n}");
        assert_eq!(edits[0].range.start, Position::new(0, 0));
    }

    #[test]
    fn format_range_reformats_the_selected_statement() {
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "fn  a(){\n echo 1\n}\nfn  b(){\n echo 2\n}\n".to_string(),
        );
        // Select all of the first fn (lines 0–2).
        let range = Range::new(Position::new(0, 0), Position::new(2, 1));
        let edits = store
            .format_range("file:///a.noe", range, Encoding::Utf16)
            .expect("reformats the selection");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "fn a() {\n    echo 1\n}");
    }

    #[test]
    fn format_on_type_is_quiet_mid_typing() {
        let mut store = test_store();
        store.open("file:///a.noe", "fn f() {\n".to_string()); // unbalanced, mid-type
        assert!(
            store
                .format_on_type("file:///a.noe", Position::new(1, 0), Encoding::Utf16)
                .is_none()
        );
    }

    #[test]
    fn change_mutates_the_same_input_in_place() {
        let mut store = test_store();
        store.open("file:///a.noe", "old".to_string());
        let before = doc_program(&store, "file:///a.noe");

        let after = store.change("file:///a.noe", "new".to_string());

        // Same salsa input handle (edited in place, not replaced — the file set is unchanged) with
        // the updated text; this is what lets salsa recompute only the affected downstream queries.
        assert_eq!(before, after);
        assert_eq!(after.text(&store.db), "new");
        assert_eq!(store.buffers.len(), 1);
    }

    #[test]
    fn change_on_unknown_document_registers_it() {
        let mut store = test_store();
        let program = store.change("file:///ghost.noe", "hi".to_string());
        assert_eq!(program.text(&store.db), "hi");
        assert_eq!(store.buffers.len(), 1);
    }

    #[test]
    fn close_drops_the_document() {
        let mut store = test_store();
        store.open("file:///a.noe", "x".to_string());
        store.close("file:///a.noe");
        assert!(store.buffers.is_empty());
        assert!(store.workspaces.is_empty());
    }

    /// All inlay hints for a document as `(line, label)` pairs, whole-file range, UTF-16.
    fn hints_of(store: &DocumentStore, uri: &str) -> Vec<(u32, String)> {
        store
            .inlay_hints(
                uri,
                Range::new(Position::new(0, 0), Position::new(9999, 0)),
                Encoding::Utf16,
            )
            .expect("document is open")
            .into_iter()
            .map(|(position, label, _)| (position.line, label))
            .collect()
    }

    #[test]
    fn inlay_hints_show_inferred_types_for_unannotated_declarations_only() {
        let mut store = test_store();
        store.open(
            "file:///hints.noe",
            "mut xs = [1, 2, 3]\n\
             count: int = 3\n\
             mut s = \"hi\"\n\
             xs = [4]\n\
             fn f(n: int): int {\n    \
             mut doubled = n * 2\n    \
             return doubled\n}\n"
                .to_string(),
        );
        let hints = hints_of(&store, "file:///hints.noe");
        // Un-annotated declarations get their inferred type — top level and inside fn bodies.
        assert!(
            hints.contains(&(0, ": List<int>".to_string())),
            "list binding: {hints:?}"
        );
        assert!(
            hints.contains(&(2, ": string".to_string())),
            "string binding: {hints:?}"
        );
        assert!(
            hints.contains(&(5, ": int".to_string())),
            "fn-body binding: {hints:?}"
        );
        // An ANNOTATED binding shows nothing (the type is already on screen), and a REASSIGNMENT
        // is a use, not a declaration.
        assert!(
            !hints.iter().any(|(line, _)| *line == 1),
            "annotated binding must not hint: {hints:?}"
        );
        assert!(
            !hints.iter().any(|(line, _)| *line == 3),
            "reassignment must not hint: {hints:?}"
        );
    }

    #[test]
    fn inlay_hints_skip_the_field_assignment_desugar() {
        let mut store = test_store();
        // `self.n = …` desugars to a `Stmt::Binding` for the receiver `self` carrying an
        // `Expr::FieldSet` — it must NOT be hinted (it would render `self: Counter` inside the body).
        store.open(
            "file:///c.noe",
            "class Counter {\n    \
             mut n: u64 = 0u64\n    \
             fn bump(by: int): void {\n        \
             self.n = self.n + by.to_u64()\n    \
             }\n\
             }\n"
            .to_string(),
        );
        let hints = hints_of(&store, "file:///c.noe");
        // The field-assignment line (line 3) shows no type hint at all.
        assert!(
            !hints.iter().any(|(line, _)| *line == 3),
            "field assignment must not hint: {hints:?}"
        );
        assert!(
            !hints.iter().any(|(_, label)| label.contains("Counter")),
            "no `: Counter` receiver hint anywhere: {hints:?}"
        );
    }

    #[test]
    fn inlay_hints_mark_packed_storage_compactly() {
        let mut store = test_store();
        store.open(
            "file:///packed.noe",
            "@packed struct Vec3 { x: f32; y: f32; z: f32 }\n\
             @packed(Layout.Column) struct Cell { n: int; on: bool }\n\
             v = Vec3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }\n\
             vs = [v]\n\
             cs = [Cell { n: 1, on: true }]\n\
             ns = [1, 2, 3]\n"
                .to_string(),
        );
        let hints = hints_of(&store, "file:///packed.noe");
        // A packed nominal is marked; its lists say flat (row-major) or SoA (column-major);
        // ordinary boxed storage is unmarked.
        assert!(
            hints.contains(&(2, ": Vec3 · packed".to_string())),
            "packed nominal: {hints:?}"
        );
        assert!(
            hints.contains(&(3, ": List<Vec3> · flat".to_string())),
            "row-major flat list: {hints:?}"
        );
        assert!(
            hints.contains(&(4, ": List<Cell> · SoA".to_string())),
            "column-major list: {hints:?}"
        );
        assert!(
            hints.contains(&(5, ": List<int>".to_string())),
            "boxed list stays unmarked: {hints:?}"
        );
    }

    #[test]
    fn inlay_hints_cover_closure_bodies_and_respect_the_range() {
        let mut store = test_store();
        store.open(
            "file:///closure.noe",
            "mut total = 0\n\
             [1, 2, 3].map(fn(x: int) {\n    \
             mut bumped = x + 1;\n    \
             return bumped;\n})\n"
                .to_string(),
        );
        let all = hints_of(&store, "file:///closure.noe");
        assert!(
            all.contains(&(0, ": int".to_string())),
            "top-level binding: {all:?}"
        );
        assert!(
            all.contains(&(2, ": int".to_string())),
            "closure-body binding: {all:?}"
        );
        // The range filter: asking only for line 0 drops the closure-body hint.
        let first_line_only = store
            .inlay_hints(
                "file:///closure.noe",
                Range::new(Position::new(0, 0), Position::new(0, 99)),
                Encoding::Utf16,
            )
            .expect("document is open");
        assert!(
            first_line_only.iter().all(|(p, _, _)| p.line == 0),
            "range filter: {first_line_only:?}"
        );
        assert!(!first_line_only.is_empty());
    }

    #[test]
    fn inlay_hints_type_closure_parameters_and_name_call_arguments() {
        let mut store = test_store();
        store.open(
            "file:///params.noe",
            "fn scale(factor: int, offset: int): int { return factor + offset }\n\
             mut r = scale(2, 3)\n\
             mut offset = 1\n\
             mut s = scale(4, offset)\n\
             fn apply(op: (int) -> int, n: int): int { return op(n) }\n\
             apply(fn(x) => x + 1, 3)\n\
             mut doubled = [10, 20].map(fn(y) => y * 2)\n\
             mut f = fn(x) => x + 1\n"
                .to_string(),
        );
        let hints = hints_of(&store, "file:///params.noe");
        // Call arguments carry the parameter's name...
        assert!(
            hints.contains(&(1, "factor:".to_string())),
            "first arg names its param: {hints:?}"
        );
        assert!(
            hints.contains(&(1, "offset:".to_string())),
            "second arg names its param: {hints:?}"
        );
        // ...except an argument that IS an identifier with the parameter's own name.
        assert!(
            hints
                .iter()
                .filter(|(line, label)| *line == 3 && label == "offset:")
                .count()
                == 0,
            "same-named identifier arg shows nothing: {hints:?}"
        );
        assert!(
            hints.contains(&(3, "factor:".to_string())),
            "the other arg on that line still hints: {hints:?}"
        );
        // A closure argument's parameter shows its inferred type — flowed bidirectionally from a
        // USER function's typed fn parameter (line 5: the only hint is the closure's `x`)...
        assert!(
            hints.contains(&(5, ": int".to_string())),
            "user-fn closure param: {hints:?}"
        );
        // ...and from a BUILTIN method's element type (the dyn-closure gap, fixed): `y: int` AND
        // the refined binding `doubled: List<int>` on line 6.
        assert!(
            hints
                .iter()
                .filter(|(line, label)| *line == 6 && label == ": int")
                .count()
                == 1,
            "builtin-method closure param: {hints:?}"
        );
        assert!(
            hints.contains(&(6, ": List<int>".to_string())),
            "refined map result: {hints:?}"
        );
        // A standalone closure's UNINFERRED parameter shows nothing.
        assert!(
            !hints
                .iter()
                .any(|(line, label)| *line == 7 && label == ": dyn"),
            "uninferred closure param must not hint: {hints:?}"
        );
    }

    /// Create a fresh temp directory with the given `(filename, content)` files on disk, for the
    /// multi-file workspace tests (sibling discovery reads the real directory).
    fn temp_workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("noeta_lsp_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (filename, content) in files {
            std::fs::write(dir.join(filename), content).unwrap();
        }
        dir
    }

    #[test]
    fn workspace_resolves_a_name_imported_from_a_sibling() {
        let dir = temp_workspace(
            "import_ok",
            &[(
                "models.noe",
                "namespace App.Models;\npub struct User { id: int }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        // Without the workspace, `User` would be an unknown name; with the sibling linked in, it
        // resolves and there are no diagnostics.
        store.open(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 1 }\necho u.id".to_string(),
        );
        let (diags, _text) = store.diagnostics(&entry_uri).unwrap();
        assert!(
            diags.is_empty(),
            "imported name should resolve; got {diags:?}"
        );
    }

    #[test]
    fn the_project_docs_corpus_spans_unimported_siblings() {
        // The project corpus documents the WHOLE workspace: a sibling module the entry never
        // imports (a `hotpath.noe` next to `main.noe`) is still a project source file — it must
        // appear as its own module row with its own decls and `@doc` prose. (It used to be built
        // from the entry's linked program, which silently hid every unimported sibling.)
        let dir = temp_workspace(
            "docs_unimported_sibling",
            &[(
                "hotpath.noe",
                "namespace App.Hot;\n@doc {\n  The hot path.\n}\npub fn simulate(n: int): int { return n * 2 }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        // NOTE: main.noe does NOT import App.Hot.
        store.open(
            &entry_uri,
            "fn add(a: int, b: int): int { return a + b }\necho add(1, 2)\n".to_string(),
        );
        let modules = store.doc_children(&entry_uri, "project", Encoding::Utf16);
        let titles: Vec<&str> = modules.iter().map(|m| m.title.as_str()).collect();
        assert!(
            titles.iter().any(|t| t.contains("hotpath")),
            "the unimported sibling must be a project module; got {titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.contains("main")),
            "the entry stays a project module; got {titles:?}"
        );
        // The sibling's own subtree resolves: its decl is listed and its page carries the prose.
        let hot = modules
            .iter()
            .find(|m| m.title.contains("hotpath"))
            .unwrap();
        let decls = store.doc_children(&entry_uri, hot.id.as_str(), Encoding::Utf16);
        let simulate = decls
            .iter()
            .find(|d| d.title == "simulate")
            .expect("the sibling's fn is documented");
        let page = store
            .doc_page(&entry_uri, simulate.id.as_str(), Encoding::Utf16)
            .expect("the sibling fn's page renders");
        assert!(
            page.markdown.contains("hot path") || page.signature.is_some(),
            "the page carries substance: {page:?}"
        );
    }

    #[test]
    fn workspace_captures_a_text_tier_declared_in_a_sibling() {
        // Text-tiers arc: a sibling declares `@tier(spec, text: "xml")`; the open document's
        // `@spec { … }` body — invalid as Noeta tokens (XML quotes) — must still capture verbatim
        // in the editor, i.e. the workspace lex (`tokens_in` via `workspace_text_tiers`) applies
        // the sibling's declaration and the file diagnoses clean.
        let dir = temp_workspace(
            "text_tier_sibling",
            &[(
                "tiers.noe",
                "namespace App.Tiers;\n@tier(spec, text: \"xml\")\npub fn run_specs(roots: List<TierText>): void { return }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Tiers.run_specs\n@spec {\n  <case name=\"quoted\"/>\n}\nfn add(a: int, b: int): int { return a + b }\necho add(1, 2)\n".to_string(),
        );
        let (diags, _text) = store.diagnostics(&entry_uri).unwrap();
        assert!(
            diags.is_empty(),
            "sibling-declared text tier should capture; got {diags:?}"
        );
    }

    #[test]
    fn change_reuses_the_workspace_without_rebuilding() {
        let dir = temp_workspace(
            "incremental",
            &[(
                "models.noe",
                "namespace App.Models;\npub struct User { id: int }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 1 }".to_string(),
        );
        // Capture the input handles, then edit — the fast path must set text in place, not rebuild.
        let key = workspace_key(&entry_uri);
        let before = store.workspaces[&key].programs.clone();

        store.change(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 2 }".to_string(),
        );

        let after = &store.workspaces[&key].programs;
        assert_eq!(
            &before, after,
            "change must reuse the same salsa inputs (no rebuild/rescan)"
        );
        assert_eq!(
            doc_program(&store, &entry_uri)
                .text(&store.db)
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn two_open_documents_in_one_directory_share_one_workspace_and_inputs() {
        // Audit-4 finding 6: two open files in one directory used to hold two independent salsa
        // input copies of every file in it. Now they share ONE per-directory workspace — the same
        // `SourceProgram` input serves a file no matter which document's view reads it — and an
        // edit to one document neither creates inputs nor re-parses the other member.
        let dir = temp_workspace(
            "shared_dir",
            &[
                (
                    "alpha.noe",
                    "namespace App.Alpha;\npub fn one(): int { return 1 }\n",
                ),
                ("beta.noe", "use App.Alpha.one;\necho one()\n"),
            ],
        );
        let alpha_uri = path_to_uri(&dir.join("alpha.noe"));
        let beta_uri = path_to_uri(&dir.join("beta.noe"));
        let mut store = test_store();
        store.open(
            &alpha_uri,
            "namespace App.Alpha;\npub fn one(): int { return 1 }\n".to_string(),
        );
        store.open(&beta_uri, "use App.Alpha.one;\necho one()\n".to_string());

        // ONE workspace for the directory, not one per open document.
        assert_eq!(store.workspaces.len(), 1, "one workspace per directory");
        let (cache_a, alpha, alpha_id) = store.doc_cache(&alpha_uri).expect("alpha resolves");
        let (cache_b, _beta, beta_id) = store.doc_cache(&beta_uri).expect("beta resolves");
        assert!(std::ptr::eq(cache_a, cache_b), "same shared cache");
        assert_ne!(alpha_id, beta_id, "distinct stable SourceIds");
        // The input representing alpha inside beta's view IS alpha's own input (no copy).
        assert_eq!(
            cache_b.programs[alpha_id.0 as usize], alpha,
            "one SourceProgram input per file, shared across documents"
        );

        // Both documents diagnose cleanly over their own merged programs.
        assert!(store.diagnostics(&alpha_uri).unwrap().0.is_empty());
        assert!(store.diagnostics(&beta_uri).unwrap().0.is_empty());

        // Editing beta: no inputs are created or replaced, and alpha's workspace-aware parse is
        // untouched (the memoized value is identical) — the per-file work is shared, not copied.
        let workspace = cache_a.workspace;
        let programs_before = cache_a.programs.clone();
        let alpha_ast_before =
            noeta_db::ast_in(&store.db, workspace, alpha) as *const noeta_db::Ast;
        store.change(
            &beta_uri,
            "use App.Alpha.one;\necho one() + 1\n".to_string(),
        );
        let (cache_after, _, _) = store.doc_cache(&alpha_uri).expect("alpha still resolves");
        assert_eq!(
            cache_after.programs, programs_before,
            "an edit must not create or duplicate inputs"
        );
        assert_eq!(
            alpha_ast_before,
            noeta_db::ast_in(&store.db, workspace, alpha) as *const noeta_db::Ast,
            "editing beta must not recompute alpha's parse"
        );
        // And beta's edit is live in both views (shared input): its own diagnostics still clean.
        assert!(store.diagnostics(&beta_uri).unwrap().0.is_empty());
    }

    #[test]
    fn file_set_change_reuses_inputs_by_uri() {
        // Audit-4 finding 9 (the cheap half): a file-set change used to abandon every
        // `SourceProgram` and build fresh ones — salsa inputs are never collected, so a long
        // session grew the database without bound. Now a rescan reuses the existing inputs by URI
        // (id/text updated in place) and only a genuinely new file gets a new input.
        let dir = temp_workspace("set_change_reuse", &[("main.noe", "echo 1\n")]);
        let main_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        store.open(&main_uri, "echo 1\n".to_string());
        let before = doc_program(&store, &main_uri);
        let workspace_before = store.doc_cache(&main_uri).unwrap().0.workspace;

        // A new sibling appears on disk — sorted BEFORE main.noe, so main's SourceId shifts.
        std::fs::write(
            dir.join("aaa.noe"),
            "namespace App.Aaa;\npub fn a(): int { return 1 }\n",
        )
        .unwrap();
        let aaa_uri = path_to_uri(&dir.join("aaa.noe"));
        store.open(
            &aaa_uri,
            "namespace App.Aaa;\npub fn a(): int { return 1 }\n".to_string(),
        );

        let (cache, after, after_id) = store.doc_cache(&main_uri).expect("main still resolves");
        assert_eq!(
            before, after,
            "the existing member's input must be reused, not abandoned"
        );
        assert_eq!(after_id, SourceId(1), "main re-slots after the new sibling");
        assert_eq!(after.id(&store.db), 1, "the input's id field follows");
        assert_eq!(
            cache.workspace, workspace_before,
            "the Workspace input itself is updated in place, not replaced"
        );
        assert!(store.diagnostics(&main_uri).unwrap().0.is_empty());
        assert!(store.diagnostics(&aaa_uri).unwrap().0.is_empty());
    }

    #[test]
    fn editing_an_open_sibling_propagates_to_its_importer() {
        let dir = temp_workspace(
            "propagate",
            &[(
                "models.noe",
                "namespace App.Models;\npub struct User { id: int }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let models_uri = path_to_uri(&dir.join("models.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 1 }".to_string(),
        );
        store.open(
            &models_uri,
            "namespace App.Models;\npub struct User { id: int }\n".to_string(),
        );
        assert!(
            store.diagnostics(&entry_uri).unwrap().0.is_empty(),
            "imports resolve initially"
        );

        // Edit the open sibling to remove `User` — the importer must see the broken import via the
        // in-place propagation (no rebuild of its workspace).
        store.change(
            &models_uri,
            "namespace App.Models;\npub struct Account { id: int }\n".to_string(),
        );
        let diags = store.diagnostics(&entry_uri).unwrap().0;
        assert!(
            !diags.is_empty(),
            "the importer should now report the missing `User`; got {diags:?}"
        );
    }

    #[test]
    fn workspace_still_reports_the_entrys_own_error() {
        let dir = temp_workspace("import_err", &[]);
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        store.open(&entry_uri, "count: int = \"lots\"".to_string());
        let (diags, _text) = store.diagnostics(&entry_uri).unwrap();
        assert!(
            diags.iter().any(|d| d.code.code() == "E0007"),
            "the entry's own type error must still report; got {diags:?}"
        );
    }

    #[test]
    fn clean_program_has_no_diagnostics() {
        let mut store = test_store();
        store.open("file:///ok.noe", "echo 1".to_string());
        let (diags, _text) = store.diagnostics("file:///ok.noe").unwrap();
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn type_error_is_reported() {
        let mut store = test_store();
        // A binding whose value violates its annotation — a check-stage mismatch (E0007).
        store.open("file:///bad.noe", "count: int = \"lots\"".to_string());
        let (diags, text) = store.diagnostics("file:///bad.noe").unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.code(), "E0007");
        // The primary span resolves onto line 0 of the entry text.
        let range = LineIndex::new(&text).range(diags[0].span, Encoding::Utf8);
        assert_eq!(range.start.line, 0);
        assert_eq!(diags[0].severity, noeta_diagnostics::Severity::Error);
    }

    #[test]
    fn diagnostics_for_unknown_document_is_none() {
        let store = test_store();
        assert!(store.diagnostics("file:///nope.noe").is_none());
    }

    #[test]
    fn hover_reports_expression_types() {
        let mut store = test_store();
        store.open("file:///h.noe", "nums = [1, 2, 3]".to_string());
        let at = |c| {
            store
                .hover_type(
                    "file:///h.noe",
                    Position {
                        line: 0,
                        character: c,
                    },
                    Encoding::Utf8,
                )
                .map(|(repr, _note, _range)| repr.to_string())
        };
        assert_eq!(at(7).as_deref(), Some("List<int>")); // the `[1, 2, 3]` literal
        assert_eq!(at(8).as_deref(), Some("int")); // the `1` element
    }

    #[test]
    fn hover_signature_shows_the_declaration_of_the_callable_under_the_cursor() {
        let mut store = test_store();
        let src = "fn add(a: int, b: int): int { return a + b }\n\
                   struct Point {\n\
                   \x20   x: int = 0\n\
                   \x20   fn manhattan(): int { return self.x }\n\
                   \x20   fn origin(): Point { return Point {} }\n\
                   }\n\
                   r = add(1, 2)\n\
                   p = Point {}\n\
                   m = p.manhattan()\n\
                   o = Point.origin()\n";
        store.open("file:///s.noe", src.to_string());
        let sig = |line, character| {
            store
                .hover_signature(
                    "file:///s.noe",
                    Position { line, character },
                    Encoding::Utf8,
                )
                .map(|(label, _range)| label)
        };
        // A plain call: the free function's full signature, not the call's `int` result type.
        assert_eq!(sig(6, 5).as_deref(), Some("fn add(a: int, b: int): int"));
        // An instance-method call `p.manhattan()`: resolved through the receiver's `Point` type.
        assert_eq!(sig(8, 9).as_deref(), Some("fn manhattan(): int"));
        // An associated-function call `Point.origin()`: the receiver is the type itself.
        assert_eq!(sig(9, 12).as_deref(), Some("fn origin(): Point"));
        // Also works on the declaration names themselves.
        assert_eq!(sig(0, 4).as_deref(), Some("fn add(a: int, b: int): int"));
        assert_eq!(sig(3, 10).as_deref(), Some("fn manhattan(): int"));
        // A non-callable identifier (the binding `r`) has no signature — the type hover handles it.
        assert_eq!(sig(6, 0), None);
    }

    #[test]
    fn hover_type_definition_renders_the_declaration_of_the_type_under_the_cursor() {
        let mut store = test_store();
        let src = "struct Point {\n\
                   \x20   x: int = 0\n\
                   \x20   y: int = 0\n\
                   \x20   fn manhattan(): int { return self.x }\n\
                   }\n\
                   enum Shape {\n\
                   \x20   Circle(float);\n\
                   \x20   Rect(int, int);\n\
                   }\n\
                   p = Point {}\n\
                   s: Shape = Shape.Circle(1.0)\n";
        store.open("file:///t.noe", src.to_string());
        let def = |line, character| {
            store
                .hover_type_definition(
                    "file:///t.noe",
                    Position { line, character },
                    Encoding::Utf8,
                )
                .map(|(label, _range)| label)
        };
        // On the construction `Point {}` (line 9): the whole struct, with defaults and the method.
        assert_eq!(
            def(9, 4).as_deref(),
            Some("struct Point {\n    x: int = 0\n    y: int = 0\n    fn manhattan(): int\n}")
        );
        // On the declaration name itself (line 0).
        assert!(def(0, 8).is_some());
        // An enum: its variants (line 5 header, and the annotation `Shape` on line 10).
        assert_eq!(
            def(10, 3).as_deref(),
            Some("enum Shape {\n    Circle(float)\n    Rect(int, int)\n}")
        );
        // A value binding (`p`) is not a type name — no definition hover (the type hover covers it).
        assert_eq!(def(9, 0), None);
    }

    #[test]
    fn hover_notes_packed_storage() {
        let mut store = test_store();
        store.open(
            "file:///p.noe",
            "@packed(Layout.Column) struct Vec3 { x: f32; y: f32; z: f32 }\n\
             v = Vec3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }\n\
             vs = [v]\n"
                .to_string(),
        );
        let at = |line, character| {
            store
                .hover_type(
                    "file:///p.noe",
                    Position { line, character },
                    Encoding::Utf8,
                )
                .map(|(repr, note, _range)| (repr.to_string(), note))
        };
        // The `Vec3 { … }` literal (line 1) is a packed nominal.
        let (repr, note) = at(1, 6).expect("literal hovers");
        assert_eq!(repr, "Vec3");
        assert_eq!(
            note.as_deref(),
            Some("@packed — 12 bytes, column-major lists (SoA)")
        );
        // The `[v]` list (line 2, col 6 = inside the brackets… col 5 is `[`) stores flat.
        let (repr, note) = at(2, 5).expect("list hovers");
        assert_eq!(repr, "List<Vec3>");
        assert_eq!(
            note.as_deref(),
            Some("flat packed storage — 12 bytes/element, column-major (SoA)")
        );
    }

    #[test]
    fn hover_on_ordinary_types_has_no_layout_note() {
        let mut store = test_store();
        store.open(
            "file:///q.noe",
            "struct P { x: int; y: int }\np = P { x: 1, y: 2 }\nps = [p]\n".to_string(),
        );
        let note_at = |line, character| {
            store
                .hover_type(
                    "file:///q.noe",
                    Position { line, character },
                    Encoding::Utf8,
                )
                .and_then(|(_repr, note, _range)| note)
        };
        assert_eq!(note_at(1, 4), None); // the `P { … }` literal
        assert_eq!(note_at(2, 5), None); // the `[p]` list
    }

    #[test]
    fn hover_off_any_expression_is_none() {
        let mut store = test_store();
        store.open("file:///h.noe", "nums = [1, 2, 3]".to_string());
        // Column 5 is the ` = ` gap — the binding's LHS name and the operator are not expressions,
        // so no `expr_types` span covers the cursor.
        let hit = store.hover_type(
            "file:///h.noe",
            Position {
                line: 0,
                character: 5,
            },
            Encoding::Utf8,
        );
        assert!(hit.is_none());
    }

    #[test]
    fn goto_definition_jumps_from_a_call_to_the_fn() {
        let mut store = test_store();
        store.open(
            "file:///d.noe",
            "fn greet(): int { return 1 }\ntotal = greet()".to_string(),
        );
        // Cursor on `greet` inside the call on line 1 (byte 37 = "…total = gr|eet()").
        let (_uri, range) = store
            .definition(
                "file:///d.noe",
                Position {
                    line: 1,
                    character: 10,
                },
                Encoding::Utf8,
            )
            .expect("call resolves to the fn");
        // Jumps to the declared name on line 0 at column 3 (`fn greet` — name starts after "fn ").
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 3);
        assert_eq!(range.end.character, 8);
    }

    #[test]
    fn goto_definition_jumps_from_a_use_to_a_local_binding() {
        let mut store = test_store();
        store.open("file:///d.noe", "total = 1 + 2\necho total".to_string());
        // Cursor on `total` in `echo total` (line 1) → jumps to the binding on line 0, column 0.
        let (_uri, range) = store
            .definition(
                "file:///d.noe",
                Position {
                    line: 1,
                    character: 7,
                },
                Encoding::Utf8,
            )
            .expect("use resolves to the local binding");
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 0);
    }

    #[test]
    fn goto_definition_resolves_a_field_access() {
        let mut store = test_store();
        store.open(
            "file:///m.noe",
            "struct Point { x: int }\no = Point { x: 1 }\nd = o.x".to_string(),
        );
        // Cursor on `.x` in `o.x` (line 2, `d = o.x` → the `x` is column 6).
        let (_uri, range) = store
            .definition(
                "file:///m.noe",
                Position {
                    line: 2,
                    character: 6,
                },
                Encoding::Utf8,
            )
            .expect("field access resolves to the field declaration");
        // The field `x` is declared on line 0 at column 15 (`struct Point { x: int }`).
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 15);
    }

    #[test]
    fn goto_definition_resolves_a_method_call() {
        let mut store = test_store();
        store.open(
            "file:///m.noe",
            "class Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.get()".to_string(),
        );
        // Cursor on `.get` in `c.get()` (last line, `v = c.get()` → `get` starts at column 6).
        let (_uri, range) = store
            .definition(
                "file:///m.noe",
                Position {
                    line: 4,
                    character: 6,
                },
                Encoding::Utf8,
            )
            .expect("method call resolves to the method declaration");
        // `fn get` is declared on line 1 (`  fn get(...)` → `get` at column 5).
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 5);
    }

    #[test]
    fn goto_definition_jumps_across_modules() {
        let dir = temp_workspace(
            "goto_xmod",
            &[(
                "models.noe",
                "namespace App.Models;\npub struct User { id: int }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let models_uri = path_to_uri(&dir.join("models.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 1 }".to_string(),
        );
        // Cursor on `User` in the constructor (line 1, column 4) jumps to the sibling that declares
        // it — a different file.
        let (target_uri, range) = store
            .definition(
                &entry_uri,
                Position {
                    line: 1,
                    character: 4,
                },
                Encoding::Utf8,
            )
            .expect("imported type resolves across modules");
        assert_eq!(target_uri, models_uri);
        // `pub struct User` is on line 1 of models.noe (line 0 is the `namespace`).
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 11); // after "pub struct "
    }

    #[test]
    fn goto_definition_disambiguates_aliased_same_named_imports() {
        // Arc Phase B: two sibling modules each declare `Amount`; the entry imports both under
        // distinct aliases. Go-to-definition on each alias resolves through the entry's imports to the
        // *right* qualified declaration — `Money` to money.noe, `Distance` to geo.noe — never
        // conflating the two same-short-named types.
        let dir = temp_workspace(
            "goto_alias",
            &[
                (
                    "money.noe",
                    "namespace App.Money;\npub struct Amount { cents: int }\n",
                ),
                (
                    "geo.noe",
                    "namespace App.Geo;\npub struct Amount { meters: int }\n",
                ),
            ],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let money_uri = path_to_uri(&dir.join("money.noe"));
        let geo_uri = path_to_uri(&dir.join("geo.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Money.Amount as Money;\nuse App.Geo.Amount as Distance;\nm = Money { cents: 1 };\nd = Distance { meters: 2 }".to_string(),
        );

        // Cursor on `Money` (line 2, column 4) → money.noe's `Amount`.
        let (money_target, _) = store
            .definition(
                &entry_uri,
                Position {
                    line: 2,
                    character: 4,
                },
                Encoding::Utf8,
            )
            .expect("aliased `Money` resolves");
        assert_eq!(money_target, money_uri);

        // Cursor on `Distance` (line 3, column 4) → geo.noe's `Amount` (a different file).
        let (distance_target, _) = store
            .definition(
                &entry_uri,
                Position {
                    line: 3,
                    character: 4,
                },
                Encoding::Utf8,
            )
            .expect("aliased `Distance` resolves");
        assert_eq!(distance_target, geo_uri);
    }

    #[test]
    fn goto_definition_jumps_into_a_dependency_package() {
        // package-manager P2.1c: with dependency resolution wired into the salsa workspace, a
        // cross-package `use hi.hello.greeting` resolves in-editor — goto-definition on the call
        // jumps into the dependency package's own source file.
        let base = std::env::temp_dir().join("noeta_lsp_crosspkg");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        let lib = base.join("greetlib");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(
            app.join("noeta.toml"),
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nhi = { path = \"../greetlib\" }\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("noeta.toml"),
            "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            lib.join("hello.noe"),
            "namespace greet.hello;\npub struct Greeter { n: int }\n",
        )
        .unwrap();

        let entry_uri = path_to_uri(&app.join("main.noe"));
        // The dependency's modules are addressed by their canonical path (the walk canonicalizes a
        // path dep's directory), so build the expected URI the same way.
        let hello_uri = path_to_uri(&lib.join("hello.noe").canonicalize().unwrap());
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use hi.hello.Greeter;\ng = Greeter { n: 1 }\n".to_string(),
        );

        // Cursor on `Greeter` in the constructor (line 1, char 4) jumps into the dependency source.
        let (target_uri, _range) = store
            .definition(
                &entry_uri,
                Position {
                    line: 1,
                    character: 4,
                },
                Encoding::Utf8,
            )
            .expect("cross-package type resolves to its dependency source");
        assert_eq!(target_uri, hello_uri);
    }

    #[test]
    fn a_hard_dependency_resolution_failure_is_surfaced_as_a_diagnostic() {
        // audit-5 #7: a broken manifest (or a trust refusal / version conflict) used to degrade
        // silently to "no dependencies", leaving the user with inexplicable unknown-import
        // errors. The typed pm error now reaches diagnostics with the real cause.
        let base = std::env::temp_dir().join("noeta_lsp_dep_error");
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        // `dep = 5` is not a valid dependency source — a Manifest-kind resolution failure.
        std::fs::write(app.join("noeta.toml"), "[dependencies]\ndep = 5\n").unwrap();

        let entry_uri = path_to_uri(&app.join("main.noe"));
        let mut store = test_store();
        store.open(&entry_uri, "use dep.thing;\n".to_string());

        let (diags, _text) = store.diagnostics(&entry_uri).unwrap();
        let surfaced = diags
            .iter()
            .find(|d| d.message.starts_with("dependency resolution failed:"))
            .expect("the pm failure is surfaced, not swallowed");
        assert_eq!(
            surfaced.code,
            noeta_diagnostics::DiagnosticCode::UnresolvedImport
        );
        assert!(
            surfaced.message.contains("dep"),
            "names the offending entry: {}",
            surfaced.message
        );
        // It is reported at the top of the entry document (span 0..0 of this source).
        assert_eq!(surfaced.span.start, 0);
        assert_eq!(surfaced.span.end, 0);

        // A workspace that is simply NOT a project (no manifest) stays quiet — the routine
        // degrade is unchanged.
        let bare = base.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        let bare_uri = path_to_uri(&bare.join("lone.noe"));
        store.open(&bare_uri, "x = 1\n".to_string());
        let (diags, _) = store.diagnostics(&bare_uri).unwrap();
        assert!(
            diags
                .iter()
                .all(|d| !d.message.starts_with("dependency resolution failed:")),
            "no manifest is not an error: {diags:?}"
        );
    }

    #[test]
    fn goto_definition_off_a_known_name_is_none() {
        let mut store = test_store();
        store.open("file:///d.noe", "total = 1 + 2".to_string());
        // `total` is a local binding, not a top-level definition — no jump (never a wrong one).
        assert!(
            store
                .definition(
                    "file:///d.noe",
                    Position {
                        line: 0,
                        character: 2,
                    },
                    Encoding::Utf8,
                )
                .is_none()
        );
    }

    #[test]
    fn document_symbols_builds_the_outline_with_ranges() {
        let mut store = test_store();
        store.open(
            "file:///o.noe",
            "struct Point {\n  x: int\n  fn norm(): int { return self.x }\n}\nfn main() {}"
                .to_string(),
        );
        let syms = store
            .document_symbols("file:///o.noe", Encoding::Utf8)
            .expect("open document has an outline");
        assert_eq!(syms.len(), 2);

        let point = &syms[0];
        assert_eq!(point.name, "Point");
        assert_eq!(point.kind, SymbolKind::Struct);
        // The selection range is the name on line 0 (`struct Point` → `Point` at column 7).
        assert_eq!(point.selection_range.start.line, 0);
        assert_eq!(point.selection_range.start.character, 7);
        // Nested field then method.
        let kids = &point.children;
        assert_eq!(kids.len(), 2);
        assert_eq!(
            (kids[0].name.as_str(), kids[0].kind),
            ("x", SymbolKind::Field)
        );
        assert_eq!(
            (kids[1].name.as_str(), kids[1].kind),
            ("norm", SymbolKind::Method)
        );

        assert_eq!(syms[1].name, "main");
        assert_eq!(syms[1].kind, SymbolKind::Function);
        assert!(syms[1].children.is_empty()); // a leaf has no members
    }

    #[test]
    fn completions_offer_keywords_decls_and_scoped_locals() {
        let mut store = test_store();
        store.open(
            "file:///c.noe",
            "fn helper(): int { return 1 }\nfn main() {\n  total = 1\n  return total\n}"
                .to_string(),
        );
        // Cursor on the `return total` line — helper (top-level fn), total (local), and keywords
        // should all be offered; `main`'s scope does not leak elsewhere.
        let text = &store.buffers["file:///c.noe"];
        let line = text
            .lines()
            .position(|l| l.contains("return total"))
            .unwrap() as u32;
        let items = store
            .completions(
                "file:///c.noe",
                Position { line, character: 2 },
                Encoding::Utf8,
            )
            .expect("open document offers completions");
        let has = |label: &str, kind: completion::CandidateKind| {
            items.iter().any(|i| i.label == label && i.kind == kind)
        };
        assert!(has("helper", completion::CandidateKind::Function));
        assert!(has("total", completion::CandidateKind::Variable));
        assert!(has("return", completion::CandidateKind::Keyword));
    }

    #[test]
    fn completions_after_an_at_offer_directives() {
        let mut store = test_store();
        // A dangling `@te` mid-edit — nothing after the `@` parses, yet the directive names come.
        store.open("file:///d.noe", "fn f() {}\n@te".to_string());
        let items = store
            .completions(
                "file:///d.noe",
                Position {
                    line: 1,
                    character: 3,
                },
                Encoding::Utf8,
            )
            .expect("open document offers completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"test"), "got {labels:?}");
        assert!(labels.contains(&"doc"), "got {labels:?}");
        assert!(labels.contains(&"derive"), "got {labels:?}");
        assert!(
            items
                .iter()
                .all(|i| i.kind == completion::CandidateKind::Directive),
            "an @ prefix offers directives only"
        );
    }

    #[test]
    fn references_finds_all_uses_of_a_local() {
        let mut store = test_store();
        store.open(
            "file:///r.noe",
            "total = 1\necho total\necho total".to_string(),
        );
        // Cursor on the binding `total` (line 0) — without the declaration, two uses.
        let uses = store
            .references(
                "file:///r.noe",
                Position {
                    line: 0,
                    character: 2,
                },
                Encoding::Utf8,
                false,
            )
            .expect("resolves the symbol");
        assert_eq!(uses.len(), 2, "two uses of total; got {uses:?}");
        assert!(
            uses.iter()
                .all(|(_, r)| r.start.line == 1 || r.start.line == 2)
        );

        // With the declaration included, three locations.
        let with_decl = store
            .references(
                "file:///r.noe",
                Position {
                    line: 1,
                    character: 5,
                },
                Encoding::Utf8,
                true,
            )
            .expect("resolves the symbol from a use too");
        assert_eq!(
            with_decl.len(),
            3,
            "two uses + the declaration; got {with_decl:?}"
        );
    }

    #[test]
    fn references_span_modules_for_a_function() {
        let dir = temp_workspace(
            "refs_xmod",
            &[(
                "util.noe",
                "namespace App.Util;\npub fn helper(): int { return 1 }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let util_uri = path_to_uri(&dir.join("util.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Util.helper;\na = helper()\nb = helper()".to_string(),
        );
        // Cursor on a call to the imported `helper` — references include the two call sites here plus
        // the declaration in the sibling.
        let refs = store
            .references(
                &entry_uri,
                Position {
                    line: 1,
                    character: 4,
                },
                Encoding::Utf8,
                true,
            )
            .expect("resolves the imported function");
        assert!(
            refs.iter().any(|(u, _)| *u == util_uri),
            "the declaration in util.noe is a reference; got {refs:?}"
        );
        assert!(
            refs.iter().filter(|(u, _)| *u == entry_uri).count() >= 2,
            "both call sites in main.noe; got {refs:?}"
        );
    }

    #[test]
    fn rename_edits_cover_every_occurrence() {
        let mut store = test_store();
        store.open(
            "file:///r.noe",
            "total = 1\necho total\necho total".to_string(),
        );
        // Rename `total` from a use — the declaration and both uses get an edit.
        let by_uri = store
            .rename_edits(
                "file:///r.noe",
                Position {
                    line: 1,
                    character: 5,
                },
                Encoding::Utf8,
                "sum",
            )
            .expect("renameable symbol");
        let ranges = &by_uri["file:///r.noe"];
        assert_eq!(ranges.len(), 3, "declaration + two uses; got {ranges:?}");
        // Every edit spans exactly the old name (5 chars, `total`).
        assert!(
            ranges
                .iter()
                .all(|r| r.end.character - r.start.character == 5)
        );
    }

    #[test]
    fn rename_spans_modules() {
        let dir = temp_workspace(
            "rename_xmod",
            &[(
                "util.noe",
                "namespace App.Util;\npub fn helper(): int { return 1 }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let util_uri = path_to_uri(&dir.join("util.noe"));
        let mut store = test_store();
        store.open(&entry_uri, "use App.Util.helper;\na = helper()".to_string());
        let by_uri = store
            .rename_edits(
                &entry_uri,
                Position {
                    line: 1,
                    character: 4,
                },
                Encoding::Utf8,
                "run",
            )
            .expect("renameable imported function");
        assert!(
            by_uri.contains_key(&util_uri),
            "declaration file edited; got {by_uri:?}"
        );
        assert!(by_uri.contains_key(&entry_uri), "call-site file edited");
    }

    #[test]
    fn prepare_rename_returns_the_symbol_range() {
        let mut store = test_store();
        store.open("file:///r.noe", "total = 1\necho total".to_string());
        // On a use of `total` (line 1) → the range of that occurrence.
        let range = store
            .prepare_rename(
                "file:///r.noe",
                Position {
                    line: 1,
                    character: 7,
                },
                Encoding::Utf8,
            )
            .expect("renameable");
        assert_eq!(range.start.line, 1);
        assert_eq!(range.end.character - range.start.character, 5); // `total`
        // Not on a symbol → None.
        assert!(
            store
                .prepare_rename(
                    "file:///r.noe",
                    Position {
                        line: 0,
                        character: 6,
                    },
                    Encoding::Utf8,
                )
                .is_none()
        );
    }

    #[test]
    fn rename_to_an_invalid_identifier_is_rejected() {
        let mut store = test_store();
        store.open("file:///r.noe", "total = 1\necho total".to_string());
        for bad in ["1sum", "a b", "x+y", ""] {
            assert!(
                store
                    .rename_edits(
                        "file:///r.noe",
                        Position {
                            line: 0,
                            character: 2,
                        },
                        Encoding::Utf8,
                        bad,
                    )
                    .is_none(),
                "invalid name {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn signature_help_shows_the_active_parameter() {
        let mut store = test_store();
        store.open(
            "file:///s.noe",
            "fn add(a: int, b: int): int { return a + b }\nx = add(1, ".to_string(),
        );
        let text = &store.buffers["file:///s.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32; // after `add(1, `
        let sig = store
            .signature_help(
                "file:///s.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("inside the call");
        assert_eq!(sig.label, "add(a: int, b: int) -> int");
        assert_eq!(sig.active_param, 1);
    }

    #[test]
    fn signature_help_resolves_a_method_call() {
        let mut store = test_store();
        store.open(
            "file:///s.noe",
            "class Calc { n: int\n  fn add(a: int, b: int): int { return a + b }\n}\nc = Calc { n: 0 }\nv = c.add(1, ".to_string(),
        );
        let text = &store.buffers["file:///s.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32; // after `c.add(1, `
        let sig = store
            .signature_help(
                "file:///s.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("inside the method call");
        assert_eq!(sig.label, "add(a: int, b: int) -> int");
        assert_eq!(sig.active_param, 1);
    }

    #[test]
    fn signature_help_inside_directive_args_names_the_vocabulary() {
        let mut store = test_store();
        store.open("file:///d.noe", "@derive(".to_string());
        let sig = store
            .signature_help(
                "file:///d.noe",
                Position {
                    line: 0,
                    character: 8,
                },
                Encoding::Utf8,
            )
            .expect("inside the directive args");
        assert!(sig.label.contains("Comparable"), "got {}", sig.label);
        assert!(sig.label.contains("Serialize<Format>"), "got {}", sig.label);

        // A tier annotation's signature is its config attribute's field list.
        store.open("file:///b.noe", "@bench(".to_string());
        let sig = store
            .signature_help(
                "file:///b.noe",
                Position {
                    line: 0,
                    character: 7,
                },
                Encoding::Utf8,
            )
            .expect("inside the tier annotation args");
        assert!(sig.label.contains("iterations: int"), "got {}", sig.label);
    }

    #[test]
    fn completions_inside_directive_args_offer_the_vocabulary() {
        let mut store = test_store();
        store.open(
            "file:///da.noe",
            "@derive(Com\nstruct P { x: int }".to_string(),
        );
        let items = store
            .completions(
                "file:///da.noe",
                Position {
                    line: 0,
                    character: 11,
                },
                Encoding::Utf8,
            )
            .expect("open document offers completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Comparable"), "got {labels:?}");
        assert!(
            !labels.contains(&"fn"),
            "keywords are noise inside directive args; got {labels:?}"
        );
        // `@packed(Layout.` completes the variants, not a munged member access.
        store.open(
            "file:///pl.noe",
            "@packed(Layout.\nstruct V { x: f32 }".to_string(),
        );
        let items = store
            .completions(
                "file:///pl.noe",
                Position {
                    line: 0,
                    character: 15,
                },
                Encoding::Utf8,
            )
            .expect("open document offers completions");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["Row", "Column"]);
    }

    #[test]
    fn signature_help_outside_a_call_is_none() {
        let mut store = test_store();
        store.open(
            "file:///s.noe",
            "fn f(a: int): int { return 1 }\ny = 2".to_string(),
        );
        assert!(
            store
                .signature_help(
                    "file:///s.noe",
                    Position {
                        line: 1,
                        character: 5,
                    },
                    Encoding::Utf8,
                )
                .is_none()
        );
    }

    #[test]
    fn references_find_a_field_across_uses_not_a_namesake() {
        let mut store = test_store();
        // Two types with a same-named field `x`; a reference search on `Point`'s `x` must not sweep in
        // `Other`'s `x`.
        store.open(
            "file:///m.noe",
            "struct Point { x: int }\nstruct Other { x: int }\np = Point { x: 1 }\no = Other { x: 2 }\na = p.x\nb = o.x\nc = p.x".to_string(),
        );
        // Cursor on `.x` in `a = p.x` (line 4, `a = p.x` → `x` at col 6).
        let refs = store
            .references(
                "file:///m.noe",
                Position {
                    line: 4,
                    character: 6,
                },
                Encoding::Utf8,
                true,
            )
            .expect("field symbol resolves");
        // Declaration (line 0) + the two `p.x` accesses (lines 4 and 6) — never `o.x` (line 5).
        let lines: Vec<u32> = refs.iter().map(|(_, r)| r.start.line).collect();
        assert!(lines.contains(&0), "field declaration; got {lines:?}");
        assert!(
            lines.contains(&4) && lines.contains(&6),
            "both p.x accesses; got {lines:?}"
        );
        assert!(
            !lines.contains(&5),
            "o.x is a different type's field; got {lines:?}"
        );
    }

    #[test]
    fn rename_a_method_updates_declaration_and_calls() {
        let mut store = test_store();
        store.open(
            "file:///m.noe",
            "class Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.get()".to_string(),
        );
        // Cursor on `.get` in `c.get()` (line 4, col 6).
        let by_uri = store
            .rename_edits(
                "file:///m.noe",
                Position {
                    line: 4,
                    character: 6,
                },
                Encoding::Utf8,
                "value",
            )
            .expect("method symbol renameable");
        let ranges = &by_uri["file:///m.noe"];
        // The method declaration (line 1) and the call site (line 4).
        let lines: Vec<u32> = ranges.iter().map(|r| r.start.line).collect();
        assert!(
            lines.contains(&1) && lines.contains(&4),
            "decl + call; got {lines:?}"
        );
        assert!(
            ranges
                .iter()
                .all(|r| r.end.character - r.start.character == 3)
        ); // `get`
    }

    #[test]
    fn prepare_rename_accepts_a_field() {
        let mut store = test_store();
        store.open(
            "file:///m.noe",
            "struct Point { x: int }\np = Point { x: 1 }\nd = p.x".to_string(),
        );
        // On `.x` in `d = p.x` (line 2, col 6).
        let range = store
            .prepare_rename(
                "file:///m.noe",
                Position {
                    line: 2,
                    character: 6,
                },
                Encoding::Utf8,
            )
            .expect("field is renameable");
        assert_eq!(range.start.line, 2);
        assert_eq!(range.end.character - range.start.character, 1); // `x`
    }

    #[test]
    fn references_on_nothing_is_none() {
        let mut store = test_store();
        store.open("file:///r.noe", "echo 1".to_string());
        assert!(
            store
                .references(
                    "file:///r.noe",
                    Position {
                        line: 0,
                        character: 5,
                    },
                    Encoding::Utf8,
                    true,
                )
                .is_none()
        );
    }

    #[test]
    fn completions_after_a_dot_offer_the_receiver_type_members() {
        let mut store = test_store();
        store.open(
            "file:///m.noe",
            "class Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.ge".to_string(),
        );
        // Cursor at the end of the partial member `c.ge` (last line) — offer Counter's members only.
        let text = &store.buffers["file:///m.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32;
        let items = store
            .completions(
                "file:///m.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("open document offers completions");
        // Members present with member kinds…
        assert!(
            items
                .iter()
                .any(|i| i.label == "get" && i.kind == completion::CandidateKind::Method),
            "method `get` offered; got {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|i| i.label == "n" && i.kind == completion::CandidateKind::Field)
        );
        // …and nothing else (no keywords/locals leaking in after the dot).
        assert!(
            !items
                .iter()
                .any(|i| i.kind == completion::CandidateKind::Keyword),
            "keywords must not appear in member completion; got {items:?}"
        );
    }

    #[test]
    fn completions_after_a_bare_dot_offer_the_receiver_type_members() {
        let mut store = test_store();
        // Trailing bare dot `c.` — does not parse into a statement; the munged re-check recovers
        // Counter as the receiver type.
        store.open(
            "file:///b.noe",
            "class Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.".to_string(),
        );
        let text = &store.buffers["file:///b.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32; // just after the dot
        let items = store
            .completions(
                "file:///b.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("open document offers completions");
        assert!(
            items
                .iter()
                .any(|i| i.label == "get" && i.kind == completion::CandidateKind::Method),
            "bare-dot member completion offers `get`; got {items:?}"
        );
        assert!(items.iter().any(|i| i.label == "n"));
        assert!(
            !items
                .iter()
                .any(|i| i.kind == completion::CandidateKind::Keyword),
            "no keywords after a bare dot; got {items:?}"
        );
    }

    #[test]
    fn completions_after_a_namespace_group_dot_offer_its_members() {
        // `http.` — a namespace-group receiver (module-namespaces) has no value type, so completion
        // offers the group's submodules and types (`client`, `server`, `Response`), not a keyword or
        // scope list. Exercises both the group binding and the member-kinds.
        let mut store = test_store();
        store.open("file:///a.noe", "use std.http\nx = http.".to_string());
        // Just after the trailing dot of `x = http.` (line 1, col 9).
        let items = store
            .completions("file:///a.noe", Position::new(1, 9), Encoding::Utf8)
            .expect("open document offers completions");
        assert!(
            items
                .iter()
                .any(|i| i.label == "client" && i.kind == completion::CandidateKind::Module),
            "offers the `client` submodule; got {items:?}"
        );
        assert!(items.iter().any(|i| i.label == "server"));
        assert!(
            items
                .iter()
                .any(|i| i.label == "Response" && i.kind == completion::CandidateKind::Type),
            "offers the `Response` type; got {items:?}"
        );
        assert!(
            !items
                .iter()
                .any(|i| i.kind == completion::CandidateKind::Keyword),
            "no keywords after a group dot; got {items:?}"
        );
    }

    #[test]
    fn hover_describes_a_namespace_group() {
        // Hovering the group handle `http` (which has no value type) describes the group and lists
        // its members (module-namespaces).
        let mut store = test_store();
        store.open(
            "file:///a.noe",
            "use std.http\nx = http.client\n".to_string(),
        );
        // On `http` in the `use` line (line 0, col 8 — within `std.http`'s `http`).
        let (desc, _) = store
            .hover_namespace("file:///a.noe", Position::new(1, 5), Encoding::Utf8)
            .expect("a namespace-group hover on `http`");
        assert!(
            desc.contains("namespace group `http`") && desc.contains("`std.http`"),
            "describes the group and prefix; got {desc:?}"
        );
        assert!(
            desc.contains("`client`") && desc.contains("`Response`"),
            "lists the members; got {desc:?}"
        );
    }

    #[test]
    fn range_operator_is_not_mistaken_for_a_bare_dot() {
        // `0..` ends in a dot, but the preceding `.` marks a range — identifier completion, not
        // member completion (so keywords are present).
        let mut store = test_store();
        store.open("file:///r.noe", "xs = [1, 2]\nfor i in 0..".to_string());
        let text = &store.buffers["file:///r.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32;
        let items = store
            .completions(
                "file:///r.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("offers completions");
        assert!(
            items
                .iter()
                .any(|c| matches!(c.kind, completion::CandidateKind::Keyword)),
            "a range `..` must fall through to identifier completion"
        );
    }

    #[test]
    fn completions_in_a_type_annotation_offer_type_names_only() {
        let mut store = test_store();
        // A binding annotation with an empty type after the colon — the `.`-trigger case's cousin.
        store.open(
            "file:///t.noe",
            "struct Point { x: int }\ntotal: ".to_string(),
        );
        let text = &store.buffers["file:///t.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32; // after `total: `
        let items = store
            .completions(
                "file:///t.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("offers completions");
        assert!(
            items.iter().any(|i| i.label == "Point"),
            "user type offered in annotation; got {items:?}"
        );
        assert!(
            items.iter().any(|i| i.label == "int" || i.label == "List"),
            "built-in types offered"
        );
        assert!(
            !items
                .iter()
                .any(|i| i.kind == completion::CandidateKind::Keyword),
            "no keywords in a type position; got {items:?}"
        );
    }

    #[test]
    fn completions_in_a_value_position_are_not_type_names() {
        let mut store = test_store();
        // A binding *initializer* (right of `=`) is a value position — keywords/locals, not types.
        store.open(
            "file:///v.noe",
            "struct Point { x: int }\ntotal = ".to_string(),
        );
        let text = &store.buffers["file:///v.noe"];
        let last = text.lines().count() as u32 - 1;
        let col = text.lines().next_back().unwrap().chars().count() as u32; // after `total = `
        let cands = store
            .completions(
                "file:///v.noe",
                Position {
                    line: last,
                    character: col,
                },
                Encoding::Utf8,
            )
            .expect("offers completions");
        // Identifier completion here includes keywords; a type-position result would not.
        assert!(
            cands
                .iter()
                .any(|c| matches!(c.kind, completion::CandidateKind::Keyword)),
            "a value position must fall through to identifier completion; got {cands:?}"
        );
    }

    #[test]
    fn completions_for_unknown_document_is_none() {
        let store = test_store();
        assert!(
            store
                .completions(
                    "file:///nope.noe",
                    Position {
                        line: 0,
                        character: 0,
                    },
                    Encoding::Utf8,
                )
                .is_none()
        );
    }

    #[test]
    fn document_symbols_for_unknown_document_is_none() {
        let store = test_store();
        assert!(
            store
                .document_symbols("file:///nope.noe", Encoding::Utf8)
                .is_none()
        );
    }

    #[test]
    fn editing_rechecks_and_clears_the_error() {
        let mut store = test_store();
        store.open("file:///f.noe", "count: int = \"lots\"".to_string());
        assert_eq!(store.diagnostics("file:///f.noe").unwrap().0.len(), 1);
        // Fix it — salsa re-runs the check on the mutated input and the error is gone.
        store.change("file:///f.noe", "count: int = 7".to_string());
        assert!(store.diagnostics("file:///f.noe").unwrap().0.is_empty());
    }

    /// An entry point calling through a helper into a persistence boundary, plus an external
    /// module call — the same shape the MCP `trace` fixtures use (ide-ui U0).
    const HIER_SRC: &str = "\
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

echo handle(1)
";

    fn hier_store() -> DocumentStore {
        let mut store = test_store();
        store.open("file:///hier.noe", HIER_SRC.to_string());
        store
    }

    #[test]
    fn function_at_resolves_name_body_and_call_site() {
        let store = hier_store();
        let uri = "file:///hier.noe";
        // On the declared name `handle` (line 11: `fn handle(n: int): int {`).
        let on_name = store
            .function_at(uri, Position::new(11, 5), Encoding::Utf16)
            .expect("cursor on a fn name");
        assert_eq!(on_name.name, "handle");
        assert_eq!(on_name.roles, vec!["Semantic.EntryPoint"]);
        assert_eq!(on_name.selection_range.start.line, 11);
        assert!(on_name.range.end.line >= 14, "range covers the whole decl");
        // Inside a body (line 18: `echo s` in validate) → the enclosing function.
        let in_body = store
            .function_at(uri, Position::new(18, 3), Encoding::Utf16)
            .expect("cursor inside a body");
        assert_eq!(in_body.name, "validate");
        assert!(in_body.roles.is_empty());
        // On a call site (line 12: `v = validate(n)`) → the callee.
        let on_site = store
            .function_at(uri, Position::new(12, 8), Encoding::Utf16)
            .expect("cursor on a call site");
        assert_eq!(on_site.name, "validate");
    }

    #[test]
    fn outgoing_calls_group_by_callee_and_omit_externals() {
        let store = hier_store();
        let calls = store
            .outgoing_calls("file:///hier.noe", Position::new(11, 5), Encoding::Utf16)
            .expect("handle resolves");
        let names: Vec<&str> = calls.iter().map(|c| c.item.name.as_str()).collect();
        assert_eq!(names, vec!["validate", "save"], "source order, grouped");
        // Both are syntactic calls, sites in handle's body.
        let save = &calls[1];
        assert_eq!(save.item.roles, vec!["Semantic.Persistence"]);
        assert_eq!(save.sites.len(), 1);
        assert!(save.sites[0].1, "syntactic call");
        assert_eq!(save.sites[0].0.start.line, 13);
        // validate's own outgoing has only the external math call → no located callee at all.
        let from_validate = store
            .outgoing_calls("file:///hier.noe", Position::new(16, 5), Encoding::Utf16)
            .expect("validate resolves");
        assert!(
            from_validate.is_empty(),
            "external module calls are omitted from the hierarchy: {from_validate:?}"
        );
    }

    #[test]
    fn incoming_calls_include_the_top_level_entry() {
        let store = hier_store();
        // validate is called by handle.
        let into_validate = store
            .incoming_calls("file:///hier.noe", Position::new(16, 5), Encoding::Utf16)
            .expect("validate resolves");
        assert_eq!(into_validate.len(), 1);
        assert_eq!(into_validate[0].item.name, "handle");
        assert_eq!(into_validate[0].item.roles, vec!["Semantic.EntryPoint"]);
        assert_eq!(into_validate[0].sites[0].0.start.line, 12);
        // handle is called from the program's top-level statements (Noeta's entry, no `main`).
        let into_handle = store
            .incoming_calls("file:///hier.noe", Position::new(11, 5), Encoding::Utf16)
            .expect("handle resolves");
        assert_eq!(into_handle.len(), 1);
        assert_eq!(into_handle[0].item.name, "(top level)");
        assert_eq!(into_handle[0].sites[0].0.start.line, 28); // `echo handle(1)`
    }

    #[test]
    fn passing_a_function_shows_as_a_reference_site() {
        let mut store = test_store();
        store.open(
            "file:///cb.noe",
            "fn cb(n: int): int { return n }\nfn run(f: (int) -> int): int { return f(1) }\necho run(cb)\n"
                .to_string(),
        );
        let into_cb = store
            .incoming_calls("file:///cb.noe", Position::new(0, 4), Encoding::Utf16)
            .expect("cb resolves");
        assert_eq!(into_cb.len(), 1);
        assert_eq!(into_cb[0].item.name, "(top level)");
        assert!(!into_cb[0].sites[0].1, "passed as a value, not called");
    }

    #[test]
    fn call_hierarchy_crosses_module_boundaries() {
        let dir = temp_workspace(
            "hierarchy_cross",
            &[(
                "util.noe",
                "namespace App.Util;\npub fn helper(): int { return 1 }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Util.helper;\nfn work(): int { return helper() }\necho work()\n".to_string(),
        );
        let calls = store
            .outgoing_calls(&entry_uri, Position::new(1, 4), Encoding::Utf16)
            .expect("work resolves");
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].item.name, "App.Util.helper");
        // The callee item locates in the sibling file; the site stays in the entry.
        assert!(
            calls[0].item.uri.ends_with("util.noe"),
            "{}",
            calls[0].item.uri
        );
        assert_eq!(calls[0].sites[0].0.start.line, 1);
    }

    #[test]
    fn hierarchy_answers_for_an_unopened_sibling_file() {
        // Expanding a cross-file node hands back the *sibling's* URI — which the user never
        // opened. The query must resolve through the open workspace that discovered it.
        let dir = temp_workspace(
            "hierarchy_unopened",
            &[(
                "util.noe",
                "namespace App.Util;\npub fn helper(): int { return 1 }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let util_uri = path_to_uri(&dir.join("util.noe"));
        let mut store = test_store();
        store.open(
            &entry_uri,
            "use App.Util.helper;\nfn work(): int { return helper() }\necho work()\n".to_string(),
        );
        let into_helper = store
            .incoming_calls(&util_uri, Position::new(1, 8), Encoding::Utf16)
            .expect("a sibling file answers through the open workspace");
        assert_eq!(into_helper.len(), 1, "calls: {into_helper:?}");
        assert_eq!(into_helper[0].item.name, "work");
        assert!(into_helper[0].item.uri.ends_with("main.noe"));
        assert_eq!(into_helper[0].sites[0].0.start.line, 1);
    }

    #[test]
    fn role_lenses_locate_bindings_and_flag_traceability() {
        let store = hier_store();
        let lenses = store
            .role_lenses("file:///hier.noe", Encoding::Utf16)
            .expect("open document");
        // Roles ride on the *annotated* declarations (the attribute structs Route/Store confer
        // them, they don't bear them): handle and save, in source order, both trace roots.
        assert_eq!(lenses.len(), 2, "{lenses:?}");
        assert_eq!(lenses[0].target, "handle");
        assert_eq!(lenses[0].role, "Semantic.EntryPoint");
        assert!(lenses[0].traceable);
        assert_eq!(lenses[0].range.start.line, 11, "lens hangs on the fn name");
        assert_eq!(lenses[1].target, "save");
        assert_eq!(lenses[1].role, "Semantic.Persistence");
    }

    #[test]
    fn trace_document_renders_the_flow_with_boundaries_and_markers() {
        let store = hier_store();
        let doc = store
            .trace_document("file:///hier.noe", Some("EntryPoint"))
            .expect("open document");
        assert!(doc.contains("noeta trace — from EntryPoint"), "{doc}");
        assert!(doc.contains("boundaries reached"), "{doc}");
        assert!(doc.contains("⚑ Semantic.Persistence — save"), "{doc}");
        // The tree: handle's children with locations, the external leaf labeled.
        assert!(doc.contains("├── validate"), "{doc}");
        assert!(doc.contains("└── save"), "{doc}");
        assert!(doc.contains("math.sqrt"), "{doc}");
        assert!(doc.contains("[external]"), "{doc}");
        // 1-based path:line for handle (declared on 0-based line 11).
        assert!(doc.contains(":12"), "{doc}");
    }

    #[test]
    fn trace_document_answers_an_unmatched_spec_in_the_document() {
        let store = hier_store();
        let doc = store
            .trace_document("file:///hier.noe", Some("nope"))
            .expect("open document still answers");
        assert!(
            doc.contains("matches no role binding and no function"),
            "{doc}"
        );
    }

    #[test]
    fn architecture_groups_roles_and_marks_expandability() {
        let store = hier_store();
        let arch = store
            .architecture("file:///hier.noe", Encoding::Utf16)
            .expect("open document");
        let roles: Vec<&str> = arch.iter().map(|g| g.role.as_str()).collect();
        assert_eq!(roles, vec!["Semantic.EntryPoint", "Semantic.Persistence"]);
        let handle = &arch[0].bearers[0];
        assert_eq!(handle.name, "handle");
        assert!(handle.expandable, "handle has outgoing calls");
        assert_eq!(handle.range.as_ref().map(|r| r.start.line), Some(11));
        let save = &arch[1].bearers[0];
        assert_eq!(save.name, "save");
        assert!(!save.expandable, "save calls nothing");
    }

    #[test]
    fn architecture_children_unfold_one_level_with_honest_leaves() {
        let store = hier_store();
        let children = store
            .architecture_children("file:///hier.noe", "handle", Encoding::Utf16)
            .expect("open document");
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["validate", "save"]);
        assert!(
            children[0].expandable,
            "validate has an external call to unfold"
        );
        assert!(!children[1].expandable);
        // The next level shows the external leaf, located nowhere, not expandable.
        let leaves = store
            .architecture_children("file:///hier.noe", "validate", Encoding::Utf16)
            .expect("open document");
        let math = leaves
            .iter()
            .find(|c| c.name == "math.sqrt")
            .expect("external leaf");
        assert!(math.external && math.uri.is_none() && !math.expandable);
        // Unknown function → empty, not an error.
        assert!(
            store
                .architecture_children("file:///hier.noe", "nope", Encoding::Utf16)
                .expect("open document")
                .is_empty()
        );
    }

    #[test]
    fn tests_discovers_test_fns_with_their_metadata() {
        let mut store = test_store();
        store.open(
            "file:///t.noe",
            "fn add(a: int, b: int): int { return a + b }\n\n@test {\n  fn adds(): void {\n    assert(add(1, 2) == 3)\n  }\n  #[Skip]\n  #[Name(\"slow one\")]\n  fn slow(): void {\n    assert(true)\n  }\n}\n"
                .to_string(),
        );
        let tests = store
            .tests("file:///t.noe", Encoding::Utf16)
            .expect("open document");
        assert_eq!(tests.len(), 2, "{tests:?}");
        assert_eq!(tests[0].name, "adds");
        assert!(!tests[0].skipped);
        assert_eq!(tests[0].range.start.line, 3);
        assert_eq!(tests[1].name, "slow");
        assert!(tests[1].skipped);
        assert_eq!(tests[1].display.as_deref(), Some("slow one"));
    }

    // ----- cancellation seam (audit-4 finding 9) -----

    #[test]
    fn revision_bumps_on_every_document_mutation() {
        let mut store = test_store();
        let r0 = store.revision();
        store.open("file:///r.noe", "echo 1\n".to_string());
        let r1 = store.revision();
        assert!(r1 > r0, "open must bump");
        store.change("file:///r.noe", "echo 2\n".to_string());
        let r2 = store.revision();
        assert!(r2 > r1, "change must bump");
        store.close("file:///r.noe");
        assert!(store.revision() > r2, "close must bump");
    }

    #[test]
    fn catch_cancelled_passes_values_and_absorbs_cancellation_only() {
        assert_eq!(catch_cancelled(|| 7), Some(7));
        // A salsa cancellation unwind (the exact payload `Cancelled::throw` resumes with) maps to
        // `None` — the read was superseded, not broken.
        let cancelled = catch_cancelled(|| -> i32 {
            std::panic::resume_unwind(Box::new(salsa::Cancelled::PendingWrite))
        });
        assert_eq!(cancelled, None);
    }

    #[test]
    #[should_panic(expected = "a real bug")]
    fn catch_cancelled_propagates_genuine_panics() {
        // Cancellation is control flow; any other panic must stay a panic (a checker bug should
        // crash loudly, exactly as before the seam existed).
        let _ = catch_cancelled(|| -> i32 { panic!("a real bug") });
    }

    #[test]
    fn snapshot_serves_the_same_reads_as_the_primary() {
        let mut store = test_store();
        store.open("file:///s.noe", "count: int = \"lots\"\n".to_string());
        let snap = store.snapshot();
        let (primary, _) = store.diagnostics("file:///s.noe").unwrap();
        let (snapped, _) = snap.diagnostics("file:///s.noe").unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(
            primary[0].code, snapped[0].code,
            "shared storage, same answer"
        );
        assert_eq!(snap.revision(), store.revision());
    }

    /// The liveness half of the cancellation contract: a writer issuing edits while a reader loops
    /// expensive whole-workspace reads over snapshots must never deadlock — each write cancels (or
    /// waits out) the in-flight read because the reader drops its snapshot after every read. This
    /// is the exact shape the LSP uses (mutate under a lock; read off it on another thread).
    #[test]
    fn concurrent_edits_and_snapshot_reads_make_progress() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let mut store = test_store();
        store.open("file:///c.noe", "x = 0\necho x\n".to_string());
        let store = Arc::new(Mutex::new(store));
        let done = Arc::new(AtomicBool::new(false));

        let reader_store = Arc::clone(&store);
        let reader_done = Arc::clone(&done);
        let reader = std::thread::spawn(move || {
            let mut served = 0usize;
            while !reader_done.load(Ordering::Relaxed) {
                // Snapshot under the lock, read off it — and drop the snapshot before looping
                // back to the lock (holding it across the lock wait would deadlock the writer).
                let snap = reader_store.lock().unwrap().snapshot();
                if let Some(Some((diags, _))) =
                    catch_cancelled(|| snap.diagnostics("file:///c.noe"))
                {
                    // Every intermediate text is well-typed, so any *delivered* answer is clean.
                    assert!(diags.is_empty(), "{diags:?}");
                    served += 1;
                }
            }
            served
        });

        for i in 1..=50 {
            let mut guard = store.lock().unwrap();
            // This set_text cancels any in-flight snapshot read and blocks until the reader's
            // snapshot drops — the property under test is that it always does (no deadlock).
            guard.change("file:///c.noe", format!("x = {i}\necho x\n"));
        }
        done.store(true, Ordering::Relaxed);
        let served = reader.join().unwrap();
        // Liveness: the writer finished all 50 edits and the reader kept the loop turning. The
        // reader may have been cancelled any number of times; `served` counts delivered reads.
        assert!(store.lock().unwrap().revision() >= 51, "all edits applied");
        let _ = served;
    }

    #[test]
    fn call_hierarchy_for_unknown_document_is_none() {
        let store = test_store();
        let pos = Position::new(0, 0);
        assert!(
            store
                .function_at("file:///nope.noe", pos, Encoding::Utf8)
                .is_none()
        );
        assert!(
            store
                .outgoing_calls("file:///nope.noe", pos, Encoding::Utf8)
                .is_none()
        );
        assert!(
            store
                .incoming_calls("file:///nope.noe", pos, Encoding::Utf8)
                .is_none()
        );
    }
}
