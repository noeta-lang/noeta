//! The Noeta language server (`noeta lsp`).
//!
//! A thin JSON-RPC adapter over the compiler's salsa query graph (`noeta-db`). The server owns a
//! [`LangDatabase`], the open editor buffers, and one [`Workspace`] per open document — its entry
//! plus the sibling `.noe` modules in its directory (with open buffers overlaying disk). Every
//! language feature is then a *read* of a memoized query; editing a document calls the salsa
//! `set_text` setter, and salsa recomputes only the queries that edit invalidated. That incremental
//! spine is what makes the server responsive; it is inherited wholesale from M1, not built here.
//!
//! Slice **L0** stood up the skeleton and document lifecycle. Slice **L1** added **live
//! diagnostics**, now computed over the whole-workspace `linked_checked` query (slice **W1**) so a
//! name imported from a sibling module resolves — each maps to an LSP `Diagnostic` (severity,
//! `E0xxx` code, range, related labels), filtered to the entry file's own spans and published. Slice **L2**
//! adds **hover types**: the cursor position becomes a byte offset, the tightest enclosing span in
//! the workspace `expr_types` index gives the inferred type, rendered back to surface syntax (see
//! [`hover`]). Slice **L3** adds **go-to-definition**: the reference under the cursor resolves to its
//! declaration — a scope-aware value index handles locals, parameters, and functions
//! (shadowing-correct); member accesses `x.member` resolve via the receiver's type; and a top-level
//! name table backs type references (see [`resolve`]). Hover and go-to-definition run over the merged
//! workspace program (slice **W2**), so an imported name resolves and go-to-definition can land in a
//! *different file*. Slice **L4** adds **document symbols**: the entry document's declarations become
//! the hierarchical outline the editor renders (see [`symbols`]). **Find-references** lists every use
//! of the value symbol under the cursor (across modules), reusing the def/use index; **rename** turns
//! those into a `WorkspaceEdit`. **Signature help** shows the called function's signature and active
//! argument while typing a call (see [`signature`]). Slice **L5** adds **completion**:
//! on a member access `receiver.member` the cursor offers the receiver type's fields, variants, and
//! methods; in a type-annotation position it offers the type names; otherwise it offers the language
//! keywords, the top-level declarations, and the value bindings in scope there (see [`completion`]).
//! Positions are converted encoding-aware (see [`offsets`]).

mod completion;
mod hover;
mod offsets;
mod resolve;
mod signature;
mod symbols;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use noeta_ast::reflect::TypeRepr;
use noeta_db::{LangDatabase, SourceProgram, Workspace};
use noeta_lexer::TokenKind;
use noeta_span::{SourceId, Span};
use offsets::{Encoding, LineIndex};
use salsa::Setter;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

/// One open document's workspace: the salsa [`Workspace`] input (its entry plus the sibling `.noe`
/// modules discovered in the entry's directory) and, per [`SourceId`], the module's URI and salsa
/// input. The entry is always [`SourceId::FIRST`] (index 0). Rebuilt when the file *set* changes;
/// otherwise member texts are updated in place so `linked` recomputes incrementally.
#[derive(Debug)]
struct WorkspaceCache {
    workspace: Workspace,
    /// Per `SourceId`: the source's URI (index 0 = the entry). Maps a merged-program span back to
    /// the file it belongs to, for cross-file diagnostics and navigation.
    source_uris: Vec<String>,
    /// Per `SourceId`: the salsa input, for in-place text updates.
    programs: Vec<SourceProgram>,
}

impl WorkspaceCache {
    /// The entry source's salsa input ([`SourceId::FIRST`]) — what the single-file hover and
    /// within-file navigation queries read.
    fn entry(&self) -> SourceProgram {
        self.programs[0]
    }
}

/// The server's document state: the salsa database, the open editor buffers, and one cached
/// [`WorkspaceCache`] per open document (treated as its own workspace entry). Kept behind a
/// [`Mutex`] on the [`Backend`]; the request handlers lock it, do their (synchronous, fast) salsa
/// work, and release it before awaiting any client I/O.
///
/// Split out from [`Backend`] so it can be unit-tested without a live [`Client`].
#[derive(Default)]
struct DocumentStore {
    db: LangDatabase,
    /// Open documents: URI → current buffer text (the authoritative content, possibly unsaved).
    buffers: HashMap<String, String>,
    /// One workspace per open document, keyed by the document's URI (its entry).
    workspaces: HashMap<String, WorkspaceCache>,
}

impl std::fmt::Debug for DocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentStore")
            .field("open", &self.buffers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl DocumentStore {
    /// Register or replace an open document's buffer, then refresh every open document's workspace
    /// (an edit to one file can change what its importers see).
    fn open(&mut self, uri: &str, text: String) {
        self.buffers.insert(uri.to_string(), text);
        self.refresh_all();
    }

    /// Apply a full-document change: replace the buffer and push the new text into the salsa inputs.
    /// Returns the entry input of the changed document's workspace (for callers/tests that re-query
    /// it).
    ///
    /// The hot path (every keystroke). A buffer edit cannot change the file *set* or any sibling's
    /// on-disk content, so — unlike open/close — there is nothing to re-discover: the new text is
    /// pushed straight into the changed document's input wherever it appears (its own workspace and
    /// any importer's), with **no directory scan and no disk reads**. Only a change to a document with
    /// no workspace yet (an editor that skipped `didOpen`) falls back to a full build.
    fn change(&mut self, uri: &str, text: String) -> SourceProgram {
        self.buffers.insert(uri.to_string(), text);
        if self.workspaces.contains_key(uri) {
            self.propagate(uri);
        } else {
            self.refresh_all();
        }
        self.workspaces[uri].entry()
    }

    /// Push the changed document's current buffer text into every salsa input that represents it —
    /// its own workspace entry and the sibling slot of any other open document that imports it —
    /// without re-reading the directory or any file. Salsa backdates the untouched inputs, so only
    /// queries that actually depend on the edited text recompute.
    fn propagate(&mut self, changed_uri: &str) {
        let Some(text) = self.buffers.get(changed_uri).cloned() else {
            return;
        };
        // Collect the input handles first (`SourceProgram` is `Copy`), then set their text — the two
        // steps borrow `self.workspaces` and `self.db` respectively.
        let targets: Vec<SourceProgram> = self
            .workspaces
            .values()
            .flat_map(|cache| cache.source_uris.iter().zip(&cache.programs))
            .filter(|(source_uri, _)| source_uri.as_str() == changed_uri)
            .map(|(_, program)| *program)
            .collect();
        for program in targets {
            program.set_text(&mut self.db).to(text.clone());
        }
    }

    /// Drop a closed document (its buffer and workspace), then refresh the rest.
    fn close(&mut self, uri: &str) {
        self.buffers.remove(uri);
        self.workspaces.remove(uri);
        self.refresh_all();
    }

    /// The URIs of the open documents.
    fn open_uris(&self) -> Vec<String> {
        self.buffers.keys().cloned().collect()
    }

    /// Rebuild or update the workspace of every open document. Each open document is the entry of
    /// its own workspace; its modules are the sibling `.noe` files in the entry's directory, with
    /// open files taking their (unsaved) buffer content over disk.
    fn refresh_all(&mut self) {
        for uri in self.open_uris() {
            self.refresh_workspace(&uri);
        }
    }

    /// (Re)build the workspace for entry `uri`. Discovers the entry's sibling `.noe` files, overlays
    /// open buffers, and either updates the cached inputs' text in place (file set unchanged) or
    /// builds a fresh workspace (file set changed).
    fn refresh_workspace(&mut self, uri: &str) {
        let sources = self.discover_sources(uri);
        let uris: Vec<String> = sources.iter().map(|(u, _)| u.clone()).collect();

        // File set unchanged → update each member's text in place (salsa backdates unchanged ones).
        let reuse = self
            .workspaces
            .get(uri)
            .filter(|cache| cache.source_uris == uris)
            .map(|cache| cache.programs.clone());
        if let Some(programs) = reuse {
            for (program, (_, text)) in programs.iter().zip(&sources) {
                program.set_text(&mut self.db).to(text.clone());
            }
            return;
        }

        // File set changed (or first build) → fresh inputs and a fresh workspace.
        let programs: Vec<SourceProgram> = sources
            .iter()
            .enumerate()
            .map(|(id, (u, text))| SourceProgram::new(&self.db, id as u32, u.clone(), text.clone()))
            .collect();
        let workspace = Workspace::new(&self.db, programs[0], programs[1..].to_vec());
        self.workspaces.insert(
            uri.to_string(),
            WorkspaceCache {
                workspace,
                source_uris: uris,
                programs,
            },
        );
    }

    /// The ordered `(uri, text)` sources of entry `uri`'s workspace: the entry first, then its
    /// sibling `.noe` files sorted by path (matching the loader's `SourceId` convention). A sibling
    /// that is open uses its editor buffer; otherwise its on-disk content. A non-`file:` entry (or
    /// one with no directory) is a lone workspace.
    fn discover_sources(&self, uri: &str) -> Vec<(String, String)> {
        let entry_text = self.buffers.get(uri).cloned().unwrap_or_default();
        let mut sources = vec![(uri.to_string(), entry_text)];

        if let Some(entry_path) = uri_to_path(uri)
            && let Some(dir) = entry_path.parent()
            && let Ok(read_dir) = std::fs::read_dir(dir)
        {
            let mut siblings: Vec<PathBuf> = read_dir
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "noe"))
                .filter(|p| *p != entry_path)
                .collect();
            siblings.sort();
            for path in siblings {
                let sib_uri = path_to_uri(&path);
                let text = self
                    .buffers
                    .get(&sib_uri)
                    .cloned()
                    .or_else(|| std::fs::read_to_string(&path).ok())
                    .unwrap_or_default();
                sources.push((sib_uri, text));
            }
        }
        sources
    }

    /// The `uri`'s own diagnostics (cross-module resolution, but only the entry file's own
    /// diagnostics — each open module reports its own through its own workspace) together with the
    /// entry text for position mapping. `None` if the document is not open.
    ///
    /// Runs over the whole-workspace [`linked_checked`](noeta_db::linked_checked) query, so a name
    /// imported from a sibling module resolves and no longer reports a false "unknown name". A load
    /// or parse failure carries its diagnostics through the same query.
    fn diagnostics(&self, uri: &str) -> Option<(Vec<noeta_diagnostics::Diagnostic>, String)> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let diags = noeta_db::linked_checked(db, cache.workspace)
            .diagnostics
            .iter()
            .filter(|d| d.span.source == SourceId::FIRST)
            .cloned()
            .collect();
        Some((diags, cache.entry().text(db).clone()))
    }

    /// The type at `position` for hover: the **smallest** `expr_types` span in the entry file that
    /// contains the cursor (the most specific expression under it), rendered plus its LSP range.
    /// Runs over the whole-workspace type index so an expression's type is known even when it depends
    /// on an imported declaration; the `source` filter keeps the lookup to the entry file the cursor
    /// is in. `None` if the document is unknown or no typed expression covers the position.
    fn hover_type(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(TypeRepr, Range)> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let index = LineIndex::new(cache.entry().text(db));
        let offset = index.offset(position, encoding);
        let (span, repr) = noeta_db::linked_checked_ide(db, cache.workspace)
            .expr_types
            .iter()
            // Non-empty spans in the entry file that cover the cursor; pick the tightest.
            .filter(|(span, _)| {
                span.source == SourceId::FIRST
                    && span.end > span.start
                    && span.start <= offset
                    && offset <= span.end
            })
            .min_by_key(|(span, _)| span.end - span.start)?;
        Some((repr.clone(), index.range(*span, encoding)))
    }

    /// Resolve the definition of the reference at `position` for go-to-definition, as a `(URI,
    /// range)` — the target may be a **different file** (a cross-module reference). Runs over the
    /// merged workspace program, so an imported name resolves to its declaration in the sibling that
    /// declares it. Three layers, in order: the scope-aware value index (locals, parameters,
    /// functions — shadowing-correct); a member access `x.member` resolved via the receiver's type
    /// and the type's member table; and the identifier token under the cursor resolved by name
    /// against the top-level definitions (type references, constructors).
    fn definition(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(String, Range)> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let entry = cache.entry();
        let entry_text = entry.text(db);
        let entry_index = LineIndex::new(entry_text);
        let offset = entry_index.offset(position, encoding);
        let cursor = SourceId::FIRST;

        // The merged program when the link succeeded, else the entry's own AST (so within-file
        // navigation still works while a sibling is broken).
        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, entry);
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
            && let Some(receiver_ty) = noeta_db::linked_checked_ide(db, cache.workspace)
                .expr_types
                .get(&receiver_span)
            && let Some(type_name) = nominal_name(receiver_ty)
            && let Some(def) = resolve::MemberTable::collect(program).lookup(type_name, member)
        {
            return self.locate(cache, def, encoding);
        }

        // 3. Fallback: the identifier token under the cursor (from the entry's tokens) resolved by
        //    name against the top-level definitions. Covers type references and constructors.
        let token = noeta_db::tokens(db, entry).0.tokens.iter().find(|token| {
            token.kind == TokenKind::Ident && token.span.start <= offset && offset <= token.span.end
        })?;
        let name = &entry_text[token.span.range()];
        let def_span = resolve::Definitions::collect(program).resolve(name)?;
        self.locate(cache, def_span, encoding)
    }

    /// All references to the value symbol (local, parameter, or function) at `position` — every use,
    /// plus the declaration when `include_declaration` is set — as `(URI, range)` pairs. Runs the
    /// scope-aware def/use index over the merged workspace program, so references to a function are
    /// found **across modules**. The cursor may be on a use or on the declaration itself.
    ///
    /// Scoped to value symbols (what the def/use index resolves); type and member references are a
    /// follow-up. `None` if the document is not open or the cursor is on no such symbol.
    fn references(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
        include_declaration: bool,
    ) -> Option<Vec<(String, Range)>> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let entry = cache.entry();
        let offset = LineIndex::new(entry.text(db)).offset(position, encoding);
        let cursor = SourceId::FIRST;

        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        let def_use = resolve::DefUse::build(program);
        let def = def_use.symbol_at(offset, cursor)?;
        let mut spans = def_use.references_to(def);
        if include_declaration {
            spans.push(def);
        }

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

    /// The edits that rename the value symbol at `position` to `new_name` — every use and the
    /// declaration — grouped by URI. Reuses [`references`](Self::references) (declaration included), so
    /// a rename of a function propagates **across modules**. `None` if the cursor is on no renameable
    /// symbol or `new_name` is not a valid identifier (an invalid rename must not silently corrupt the
    /// source).
    fn rename_edits(
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

    /// The range of the renameable value symbol under the cursor — for `prepareRename`, so the editor
    /// can validate before showing its input box and pre-select the old name. Returns the span of the
    /// identifier occurrence at the cursor when it resolves to a local, parameter, or function; `None`
    /// when the cursor is not on such a symbol (the editor then refuses the rename).
    fn prepare_rename(&self, uri: &str, position: Position, encoding: Encoding) -> Option<Range> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let entry = cache.entry();
        let entry_text = entry.text(db);
        let index = LineIndex::new(entry_text);
        let offset = index.offset(position, encoding);

        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        // Only offer a rename where the def/use index resolves a value symbol.
        resolve::DefUse::build(program).symbol_at(offset, SourceId::FIRST)?;
        // Return the range of the identifier occurrence the cursor is actually on.
        let token = noeta_db::tokens(db, entry).0.tokens.iter().find(|token| {
            token.kind == TokenKind::Ident && token.span.start <= offset && offset <= token.span.end
        })?;
        Some(index.range(token.span, encoding))
    }

    /// Signature help for the call the cursor at `position` is inside: the called function's
    /// signature and the active argument. Token-based (so a half-typed call with an unbalanced paren
    /// still resolves); the callee is looked up among the merged program's top-level functions, so an
    /// imported function's signature is shown. `None` if the cursor is not in a known function call.
    fn signature_help(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<signature::SignatureData> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let entry = cache.entry();
        let text = entry.text(db);
        let offset = LineIndex::new(text).offset(position, encoding);
        let tokens = &noeta_db::tokens(db, entry).0.tokens;

        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        signature::signature_at(tokens, text, program, offset)
    }

    /// The completion candidates at `position` in `uri`. When the cursor is on a member access
    /// `receiver.member`, the receiver's type is resolved and the completions are that type's fields,
    /// variants, and methods (**member completion**, C2) — nothing else, since an identifier after a
    /// `.` would be noise. A *partial* member (`c.ge`) is read straight from the workspace type index;
    /// a *bare* dot (`c.`, the `.`-trigger case) does not parse, so a lightly-munged buffer is
    /// re-checked off the salsa graph to recover the receiver type (C2.1). Otherwise the completions
    /// are the language keywords, the top-level declarations, and the value bindings in scope at the
    /// cursor (**identifier completion**, C1).
    ///
    /// A best-effort read of the mid-edit document: it relies on the recovering parser and the
    /// client's prefix filtering. `None` if the document is not open.
    fn completions(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<Vec<completion::Candidate>> {
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let entry = cache.entry();
        let entry_text = entry.text(db);
        let index = LineIndex::new(entry_text);
        let offset = index.offset(position, encoding);
        let cursor = SourceId::FIRST;

        // Prefer the merged workspace program (so an imported type's members resolve); fall back to
        // the entry's own AST while a sibling is unparseable.
        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };

        // Member completion, partial form: the parser produced a `receiver.member` access under the
        // cursor. Resolve the receiver's type from the workspace type index and list its members.
        let def_use = resolve::DefUse::build(program);
        if let Some((receiver_span, _member)) = def_use.member_at(offset, cursor) {
            let members = noeta_db::linked_checked_ide(db, cache.workspace)
                .expr_types
                .get(&receiver_span)
                .and_then(nominal_name)
                .map(|type_name| completion::members_of(program, type_name))
                .unwrap_or_default();
            return Some(members);
        }

        // Member completion, bare-dot form (`c.` with no member name yet — the `.`-trigger case). The
        // dangling dot makes the statement fail to parse, so the receiver never gets a type from the
        // cached check. Re-check a copy of the buffer with a synthetic member name inserted, off the
        // salsa graph, to recover it. Single-file: the receiver's type must be declared in this file.
        if is_bare_dot(entry_text, offset) {
            return Some(bare_dot_members(entry_text, offset, program).unwrap_or_default());
        }

        // Type-annotation position (`x: |`, `fn f(): |`, `List<|>`): offer type names only.
        if let Some(types) = type_position_completion(entry_text, offset, program) {
            return Some(types);
        }

        Some(completion::complete(program, offset, cursor))
    }

    /// The document outline for `uri`: the hierarchical symbol tree (top-level functions and type
    /// declarations, with fields/variants and methods nested) mapped to LSP `DocumentSymbol`s. A
    /// single-file feature — it reads the entry document's own AST, not the merged workspace — so an
    /// unparseable document yields whatever the recovering parser produced. `None` if the document is
    /// not open.
    fn document_symbols(&self, uri: &str, encoding: Encoding) -> Option<Vec<DocumentSymbol>> {
        let cache = self.workspaces.get(uri)?;
        let entry = cache.entry();
        let index = LineIndex::new(entry.text(&self.db));
        let program = &noeta_db::ast(&self.db, entry).0.program;
        Some(
            symbols::outline(program)
                .iter()
                .map(|node| to_document_symbol(&index, node, encoding))
                .collect(),
        )
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
        let uri = cache.source_uris.get(idx)?.clone();
        let program = *cache.programs.get(idx)?;
        let index = LineIndex::new(program.text(&self.db));
        Some((uri, index.range(span, encoding)))
    }
}

/// Map a compiler [`Diagnostic`](noeta_diagnostics::Diagnostic) to its LSP wire form: the primary
/// span becomes the range, the stable `E0xxx` code and `noeta` source are attached, secondary
/// labels become related information, and any help line is appended to the message (LSP has no
/// dedicated help field).
fn to_lsp_diagnostic(
    index: &LineIndex,
    uri: &Uri,
    diag: &noeta_diagnostics::Diagnostic,
    encoding: Encoding,
) -> Diagnostic {
    use noeta_diagnostics::Severity;
    let severity = Some(match diag.severity {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Note => DiagnosticSeverity::INFORMATION,
    });
    let related_information = (!diag.labels.is_empty()).then(|| {
        diag.labels
            .iter()
            .map(|label| DiagnosticRelatedInformation {
                location: Location {
                    uri: uri.clone(),
                    range: index.range(label.span, encoding),
                },
                message: label.message.clone(),
            })
            .collect()
    });
    let message = match &diag.help {
        Some(help) => format!("{}\nhelp: {help}", diag.message),
        None => diag.message.clone(),
    };
    Diagnostic {
        range: index.range(diag.span, encoding),
        severity,
        code: Some(NumberOrString::String(diag.code.code().to_string())),
        source: Some("noeta".to_string()),
        message,
        related_information,
        ..Default::default()
    }
}

/// Map a completion [`Candidate`](completion::Candidate) to its LSP `CompletionItem`: the label, an
/// icon kind, and any detail. The label is also the inserted and filter text (client default).
fn to_completion_item(candidate: &completion::Candidate) -> CompletionItem {
    use completion::CandidateKind;
    let kind = match candidate.kind {
        CandidateKind::Keyword => CompletionItemKind::KEYWORD,
        CandidateKind::Function => CompletionItemKind::FUNCTION,
        CandidateKind::Struct => CompletionItemKind::STRUCT,
        CandidateKind::Class => CompletionItemKind::CLASS,
        CandidateKind::Enum => CompletionItemKind::ENUM,
        CandidateKind::Variable => CompletionItemKind::VARIABLE,
        CandidateKind::Field => CompletionItemKind::FIELD,
        CandidateKind::Method => CompletionItemKind::METHOD,
        CandidateKind::EnumMember => CompletionItemKind::ENUM_MEMBER,
        CandidateKind::Type => CompletionItemKind::INTERFACE,
    };
    CompletionItem {
        label: candidate.label.clone(),
        kind: Some(kind),
        detail: candidate.detail.clone(),
        ..Default::default()
    }
}

/// Map an outline [`SymbolNode`](symbols::SymbolNode) to its LSP `DocumentSymbol`, resolving the
/// declaration span to `range` and the name span to `selection_range`, and recursing into children.
fn to_document_symbol(
    index: &LineIndex,
    node: &symbols::SymbolNode,
    encoding: Encoding,
) -> DocumentSymbol {
    #[allow(deprecated)]
    // `deprecated` is a required struct field, not a value we set meaningfully.
    DocumentSymbol {
        name: node.name.clone(),
        detail: node.detail.clone(),
        kind: node.kind,
        tags: None,
        deprecated: None,
        range: index.range(node.full_span, encoding),
        selection_range: index.range(node.name_span, encoding),
        children: (!node.children.is_empty()).then(|| {
            node.children
                .iter()
                .map(|child| to_document_symbol(index, child, encoding))
                .collect()
        }),
    }
}

/// Convert a `file:` document URI to a filesystem path. Returns `None` for any other scheme (e.g.
/// `untitled:`), which the caller treats as a lone, directory-less document. A minimal decoder: the
/// path component after `file://`, with `%`-escapes not yet decoded (paths with escaped bytes are
/// rare and degrade to a lone workspace, never a wrong file).
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs` → `/abs`; a leading host (`file://host/p`) is not expected for local files.
    Some(PathBuf::from(rest))
}

/// The `file:` URI for a filesystem path — the inverse of [`uri_to_path`] for the paths it produces.
fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// The declared type name a reflected [`TypeRepr`] refers to, for member resolution — the nominal
/// variants (`struct` / `class` / `enum` / unknown-kind named). Scalars, containers, functions, and
/// unions have no user declaration to jump into, so they yield `None`.
fn nominal_name(repr: &TypeRepr) -> Option<&str> {
    match repr {
        TypeRepr::Struct(name, _)
        | TypeRepr::Class(name, _)
        | TypeRepr::Enum(name, _)
        | TypeRepr::Named(name, _) => Some(name),
        _ => None,
    }
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
    let type_name = nominal_name(checked.expr_types.get(&receiver_span)?)?;
    Some(completion::members_of(program, type_name))
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

/// Pick the position encoding: prefer UTF-8 when the client advertises support for it (LSP 3.17),
/// so the server can use the compiler's native byte offsets directly; otherwise fall back to the
/// protocol default, UTF-16.
fn negotiate_encoding(params: &InitializeParams) -> Encoding {
    let supports_utf8 = params
        .capabilities
        .general
        .as_ref()
        .and_then(|general| general.position_encodings.as_ref())
        .is_some_and(|encodings| encodings.contains(&PositionEncodingKind::UTF8));
    if supports_utf8 {
        Encoding::Utf8
    } else {
        Encoding::Utf16
    }
}

/// The language server backend: the LSP transport handle, the shared [`DocumentStore`], and the
/// position encoding negotiated at `initialize`.
#[derive(Debug)]
struct Backend {
    client: Client,
    store: Mutex<DocumentStore>,
    encoding: Mutex<Encoding>,
}

impl Backend {
    fn new(client: Client) -> Backend {
        Backend {
            client,
            store: Mutex::new(DocumentStore::default()),
            // Overwritten during `initialize`; UTF-16 is the protocol default until then.
            encoding: Mutex::new(Encoding::Utf16),
        }
    }

    /// Type-check `uri`'s current text and push its diagnostics to the client. A no-op for a URI
    /// that is not open. Runs the salsa work under the store lock, then releases it before the
    /// awaited client I/O.
    async fn publish(&self, uri: Uri) {
        let collected = {
            let store = self.store.lock().expect("document store poisoned");
            store.diagnostics(uri.as_str())
        };
        let Some((diags, text)) = collected else {
            return;
        };
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let index = LineIndex::new(&text);
        let lsp_diags: Vec<Diagnostic> = diags
            .iter()
            .map(|diag| to_lsp_diagnostic(&index, &uri, diag, encoding))
            .collect();
        self.client.publish_diagnostics(uri, lsp_diags, None).await;
    }

    /// Republish diagnostics for every open document. Editing one file can change what a sibling
    /// that imports it sees, so a change re-publishes the whole open set (bounded by the number of
    /// open documents).
    async fn publish_all(&self) {
        let uris = {
            let store = self.store.lock().expect("document store poisoned");
            store.open_uris()
        };
        for uri in uris {
            if let Ok(uri) = uri.parse::<Uri>() {
                self.publish(uri).await;
            }
        }
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let encoding = negotiate_encoding(&params);
        *self.encoding.lock().expect("encoding lock poisoned") = encoding;
        let position_encoding = Some(match encoding {
            Encoding::Utf8 => PositionEncodingKind::UTF8,
            Encoding::Utf16 => PositionEncodingKind::UTF16,
        });
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "noeta-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                position_encoding,
                // Full-document sync for L0: `didChange` ships the whole buffer. Salsa still only
                // recomputes what the edit touched, so this is not a bottleneck; incremental
                // (range) sync is a later refinement.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    // `(` opens a call, `,` moves to the next argument.
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                // `prepareProvider` lets the editor validate the cursor is on a renameable symbol
                // and pre-select the old name before showing its rename box.
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                // Completion: invoked explicitly, as the user types a name, or on `.` — the trigger
                // that fires member completion at a bare receiver dot (`c.`).
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "noeta language server initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let found = {
            let store = self.store.lock().expect("document store poisoned");
            store.hover_type(uri.as_str(), position_params.position, encoding)
        };
        Ok(found.map(|(repr, range)| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```noeta\n{}\n```", hover::render_type(&repr)),
            }),
            range: Some(range),
        }))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let target = {
            let store = self.store.lock().expect("document store poisoned");
            store.definition(uri.as_str(), position_params.position, encoding)
        };
        // The target may be a different file; parse its URI back for the `Location`.
        Ok(target.and_then(|(target_uri, range)| {
            target_uri
                .parse::<Uri>()
                .ok()
                .map(|uri| GotoDefinitionResponse::Scalar(Location { uri, range }))
        }))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let found = {
            let store = self.store.lock().expect("document store poisoned");
            store.references(
                uri.as_str(),
                position_params.position,
                encoding,
                params.context.include_declaration,
            )
        };
        Ok(found.map(|locations| {
            locations
                .into_iter()
                .filter_map(|(target_uri, range)| {
                    target_uri
                        .parse::<Uri>()
                        .ok()
                        .map(|uri| Location { uri, range })
                })
                .collect()
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let new_name = params.new_name;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let edits = {
            let store = self.store.lock().expect("document store poisoned");
            store.rename_edits(uri.as_str(), position_params.position, encoding, &new_name)
        };
        Ok(edits.map(|by_uri| {
            let changes = by_uri
                .into_iter()
                .filter_map(|(target_uri, ranges)| {
                    let uri = target_uri.parse::<Uri>().ok()?;
                    let text_edits = ranges
                        .into_iter()
                        .map(|range| TextEdit {
                            range,
                            new_text: new_name.clone(),
                        })
                        .collect();
                    Some((uri, text_edits))
                })
                .collect();
            WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }
        }))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let data = {
            let store = self.store.lock().expect("document store poisoned");
            store.signature_help(uri.as_str(), position_params.position, encoding)
        };
        Ok(data.map(|data| {
            let active = data.active_param as u32;
            let parameters = data
                .parameters
                .into_iter()
                .map(|label| ParameterInformation {
                    label: ParameterLabel::Simple(label),
                    documentation: None,
                })
                .collect();
            SignatureHelp {
                signatures: vec![SignatureInformation {
                    label: data.label,
                    documentation: None,
                    parameters: Some(parameters),
                    active_parameter: Some(active),
                }],
                active_signature: Some(0),
                active_parameter: Some(active),
            }
        }))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let range = {
            let store = self.store.lock().expect("document store poisoned");
            store.prepare_rename(uri.as_str(), params.position, encoding)
        };
        Ok(range.map(PrepareRenameResponse::Range))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let symbols = {
            let store = self.store.lock().expect("document store poisoned");
            store.document_symbols(uri.as_str(), encoding)
        };
        Ok(symbols.map(DocumentSymbolResponse::Nested))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let encoding = *self.encoding.lock().expect("encoding lock poisoned");
        let candidates = {
            let store = self.store.lock().expect("document store poisoned");
            store.completions(uri.as_str(), position_params.position, encoding)
        };
        Ok(candidates.map(|candidates| {
            CompletionResponse::Array(candidates.iter().map(to_completion_item).collect())
        }))
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.open(doc.uri.as_str(), doc.text);
        }
        // Republish every open document: the newly opened file may be a module another open file
        // imports, so its arrival can resolve names that were previously unknown.
        self.publish_all().await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        // Full sync: the last content change carries the entire new document text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.change(uri.as_str(), change.text);
        }
        self.publish_all().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.close(uri.as_str());
        }
        // Clear any diagnostics the client is still showing for the now-closed document, then
        // refresh the rest (they may now be missing a module they imported).
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
        self.publish_all().await;
    }
}

/// Run the language server over stdio, blocking until the client disconnects. Called by the
/// `noeta lsp` CLI subcommand. Builds a dedicated multi-threaded tokio runtime (the CLI's `main`
/// is synchronous) and serves JSON-RPC on stdin/stdout.
pub fn run_stdio() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the language-server tokio runtime");
    runtime.block_on(serve());
}

async fn serve() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_registers_a_document() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "let x = 1".to_string());
        assert_eq!(store.buffers.len(), 1);
        let program = store.workspaces["file:///a.noe"].entry();
        assert_eq!(program.text(&store.db), "let x = 1");
    }

    #[test]
    fn change_mutates_the_same_input_in_place() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "old".to_string());
        let before = store.workspaces["file:///a.noe"].entry();

        let after = store.change("file:///a.noe", "new".to_string());

        // Same salsa input handle (edited in place, not replaced — the file set is unchanged) with
        // the updated text; this is what lets salsa recompute only the affected downstream queries.
        assert_eq!(before, after);
        assert_eq!(after.text(&store.db), "new");
        assert_eq!(store.buffers.len(), 1);
    }

    #[test]
    fn change_on_unknown_document_registers_it() {
        let mut store = DocumentStore::default();
        let program = store.change("file:///ghost.noe", "hi".to_string());
        assert_eq!(program.text(&store.db), "hi");
        assert_eq!(store.buffers.len(), 1);
    }

    #[test]
    fn close_drops_the_document() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "x".to_string());
        store.close("file:///a.noe");
        assert!(store.buffers.is_empty());
        assert!(store.workspaces.is_empty());
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
        let mut store = DocumentStore::default();
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
    fn change_reuses_the_workspace_without_rebuilding() {
        let dir = temp_workspace(
            "incremental",
            &[(
                "models.noe",
                "namespace App.Models;\npub struct User { id: int }\n",
            )],
        );
        let entry_uri = path_to_uri(&dir.join("main.noe"));
        let mut store = DocumentStore::default();
        store.open(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 1 }".to_string(),
        );
        // Capture the input handles, then edit — the fast path must set text in place, not rebuild.
        let before = store.workspaces[&entry_uri].programs.clone();

        store.change(
            &entry_uri,
            "use App.Models.User;\nu = User { id: 2 }".to_string(),
        );

        let after = &store.workspaces[&entry_uri].programs;
        assert_eq!(
            &before, after,
            "change must reuse the same salsa inputs (no rebuild/rescan)"
        );
        assert_eq!(
            store.workspaces[&entry_uri]
                .entry()
                .text(&store.db)
                .lines()
                .count(),
            2
        );
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
        store.open(&entry_uri, "count: int = \"lots\"".to_string());
        let (diags, _text) = store.diagnostics(&entry_uri).unwrap();
        assert!(
            diags.iter().any(|d| d.code.code() == "E0007"),
            "the entry's own type error must still report; got {diags:?}"
        );
    }

    #[test]
    fn clean_program_has_no_diagnostics() {
        let mut store = DocumentStore::default();
        store.open("file:///ok.noe", "echo 1".to_string());
        let (diags, _text) = store.diagnostics("file:///ok.noe").unwrap();
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn type_error_is_reported_and_maps_to_lsp() {
        let mut store = DocumentStore::default();
        // A binding whose value violates its annotation — a check-stage mismatch (E0007).
        store.open("file:///bad.noe", "count: int = \"lots\"".to_string());
        let (diags, text) = store.diagnostics("file:///bad.noe").unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.code(), "E0007");

        let uri: Uri = "file:///bad.noe".parse().unwrap();
        let index = LineIndex::new(&text);
        let mapped = to_lsp_diagnostic(&index, &uri, &diags[0], Encoding::Utf8);
        assert_eq!(mapped.range.start.line, 0);
        assert_eq!(mapped.severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(
            mapped.code,
            Some(NumberOrString::String("E0007".to_string()))
        );
        assert_eq!(mapped.source.as_deref(), Some("noeta"));
    }

    #[test]
    fn diagnostics_for_unknown_document_is_none() {
        let store = DocumentStore::default();
        assert!(store.diagnostics("file:///nope.noe").is_none());
    }

    #[test]
    fn hover_reports_expression_types() {
        let mut store = DocumentStore::default();
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
                .map(|(repr, _range)| hover::render_type(&repr))
        };
        assert_eq!(at(7).as_deref(), Some("List<int>")); // the `[1, 2, 3]` literal
        assert_eq!(at(8).as_deref(), Some("int")); // the `1` element
    }

    #[test]
    fn hover_off_any_expression_is_none() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
    fn goto_definition_off_a_known_name_is_none() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        assert_eq!(point.kind, SymbolKind::STRUCT);
        // The selection range is the name on line 0 (`struct Point` → `Point` at column 7).
        assert_eq!(point.selection_range.start.line, 0);
        assert_eq!(point.selection_range.start.character, 7);
        // Nested field then method.
        let kids = point.children.as_ref().expect("struct has members");
        assert_eq!(kids.len(), 2);
        assert_eq!(
            (kids[0].name.as_str(), kids[0].kind),
            ("x", SymbolKind::FIELD)
        );
        assert_eq!(
            (kids[1].name.as_str(), kids[1].kind),
            ("norm", SymbolKind::METHOD)
        );

        assert_eq!(syms[1].name, "main");
        assert_eq!(syms[1].kind, SymbolKind::FUNCTION);
        assert!(syms[1].children.is_none()); // a leaf carries no `children`, not an empty list
    }

    #[test]
    fn completions_offer_keywords_decls_and_scoped_locals() {
        let mut store = DocumentStore::default();
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
            .expect("open document offers completions")
            .iter()
            .map(to_completion_item)
            .collect::<Vec<_>>();
        let has = |label: &str, kind: CompletionItemKind| {
            items
                .iter()
                .any(|i| i.label == label && i.kind == Some(kind))
        };
        assert!(has("helper", CompletionItemKind::FUNCTION));
        assert!(has("total", CompletionItemKind::VARIABLE));
        assert!(has("return", CompletionItemKind::KEYWORD));
    }

    #[test]
    fn references_finds_all_uses_of_a_local() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
    fn signature_help_outside_a_call_is_none() {
        let mut store = DocumentStore::default();
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
    fn references_on_nothing_is_none() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
            .expect("open document offers completions")
            .iter()
            .map(to_completion_item)
            .collect::<Vec<_>>();
        // Members present with member kinds…
        assert!(
            items
                .iter()
                .any(|i| i.label == "get" && i.kind == Some(CompletionItemKind::METHOD)),
            "method `get` offered; got {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|i| i.label == "n" && i.kind == Some(CompletionItemKind::FIELD))
        );
        // …and nothing else (no keywords/locals leaking in after the dot).
        assert!(
            !items
                .iter()
                .any(|i| i.kind == Some(CompletionItemKind::KEYWORD)),
            "keywords must not appear in member completion; got {items:?}"
        );
    }

    #[test]
    fn completions_after_a_bare_dot_offer_the_receiver_type_members() {
        let mut store = DocumentStore::default();
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
            .expect("open document offers completions")
            .iter()
            .map(to_completion_item)
            .collect::<Vec<_>>();
        assert!(
            items
                .iter()
                .any(|i| i.label == "get" && i.kind == Some(CompletionItemKind::METHOD)),
            "bare-dot member completion offers `get`; got {items:?}"
        );
        assert!(items.iter().any(|i| i.label == "n"));
        assert!(
            !items
                .iter()
                .any(|i| i.kind == Some(CompletionItemKind::KEYWORD)),
            "no keywords after a bare dot; got {items:?}"
        );
    }

    #[test]
    fn range_operator_is_not_mistaken_for_a_bare_dot() {
        // `0..` ends in a dot, but the preceding `.` marks a range — identifier completion, not
        // member completion (so keywords are present).
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
            .expect("offers completions")
            .iter()
            .map(to_completion_item)
            .collect::<Vec<_>>();
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
                .any(|i| i.kind == Some(CompletionItemKind::KEYWORD)),
            "no keywords in a type position; got {items:?}"
        );
    }

    #[test]
    fn completions_in_a_value_position_are_not_type_names() {
        let mut store = DocumentStore::default();
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
        let store = DocumentStore::default();
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
        let store = DocumentStore::default();
        assert!(
            store
                .document_symbols("file:///nope.noe", Encoding::Utf8)
                .is_none()
        );
    }

    #[test]
    fn editing_rechecks_and_clears_the_error() {
        let mut store = DocumentStore::default();
        store.open("file:///f.noe", "count: int = \"lots\"".to_string());
        assert_eq!(store.diagnostics("file:///f.noe").unwrap().0.len(), 1);
        // Fix it — salsa re-runs the check on the mutated input and the error is gone.
        store.change("file:///f.noe", "count: int = 7".to_string());
        assert!(store.diagnostics("file:///f.noe").unwrap().0.is_empty());
    }
}
