//! The shared Noeta IDE engine (MCP arc, slice **M5** — extracted from `noeta-lsp`).
//!
//! Every editor-facing language feature over the compiler's salsa query graph (`noeta-db`), with
//! **no wire protocol**: the [`DocumentStore`] owns a [`LangDatabase`], the open buffers, and one
//! [`Workspace`] per open document — its entry plus the sibling `.noe` modules in its directory
//! (open buffers overlaying disk) plus resolved dependency packages. Every language feature is
//! then a *read* of a memoized query; editing a document calls the salsa `set_text` setter, and
//! salsa recomputes only the queries that edit invalidated. That incremental spine is inherited
//! wholesale from M1, not built here.
//!
//! Features: **live diagnostics** over the whole-workspace `linked_checked` query (an imported
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

pub mod callgraph;
pub mod completion;
pub mod inlay;
pub mod offsets;
pub mod resolve;
pub mod semtokens;
pub mod signature;
pub mod symbols;
pub mod trace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use noeta_ast::reflect::{PackedLayout, TypeRepr};
use noeta_db::{DepModule, LangDatabase, SourceProgram, Workspace};
use noeta_lexer::TokenKind;
use noeta_span::{SourceId, Span};
use salsa::Setter;

pub use offsets::{Encoding, LineIndex, Position, Range};
pub use semtokens::SemanticToken;
pub use symbols::{DocumentSymbol, SymbolKind};

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

/// One open document's workspace: the salsa [`Workspace`] input (its entry plus the sibling `.noe`
/// modules discovered in the entry's directory) and, per [`SourceId`], the module's URI and salsa
/// input. The entry is always [`SourceId::FIRST`] (index 0). Rebuilt when the file *set* changes;
/// otherwise member texts are updated in place so `linked` recomputes incrementally.
#[derive(Debug)]
struct WorkspaceCache {
    workspace: Workspace,
    /// Per `SourceId`: the source's URI (index 0 = the entry). Maps a merged-program span back to
    /// the file it belongs to, for cross-file diagnostics and navigation. Entry + siblings only —
    /// the reuse fast-path compares this against a fresh sibling scan; dependency modules (which
    /// don't change during editing) live in `dep_uris`/`dep_programs`.
    source_uris: Vec<String>,
    /// Per `SourceId`: the salsa input, for in-place text updates.
    programs: Vec<SourceProgram>,
    /// Dependency-package modules (package-manager P2.1c), indexed by `SourceId - programs.len()`
    /// (their ids continue past the siblings). Kept apart from `source_uris`/`programs` so the
    /// per-keystroke reuse check and text-update loop stay over entry+siblings only, while
    /// cross-package navigation still maps a dependency span back to its file.
    dep_uris: Vec<String>,
    dep_programs: Vec<SourceProgram>,
}

impl WorkspaceCache {
    /// The entry source's salsa input ([`SourceId::FIRST`]) — what the single-file hover and
    /// within-file navigation queries read.
    fn entry(&self) -> SourceProgram {
        self.programs[0]
    }
}

/// The resolved dependency modules for a workspace (package-manager P2.1c): the salsa [`DepModule`]
/// inputs the `Workspace` links, plus — parallel-indexed — each module's URI and salsa input so a
/// cross-package definition span maps back to its file.
#[derive(Debug, Default)]
struct ResolvedDeps {
    modules: Vec<DepModule>,
    uris: Vec<String>,
    programs: Vec<SourceProgram>,
}

/// The server's document state: the salsa database, the open editor buffers, and one cached
/// [`WorkspaceCache`] per open document (treated as its own workspace entry). Kept behind a
/// [`Mutex`] on the [`Backend`]; the request handlers lock it, do their (synchronous, fast) salsa
/// work, and release it before awaiting any client I/O.
///
/// Split out from [`Backend`] so it can be unit-tested without a live [`Client`].
#[derive(Default)]
pub struct DocumentStore {
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
    pub fn open(&mut self, uri: &str, text: String) {
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
    pub fn change(&mut self, uri: &str, text: String) -> SourceProgram {
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
    pub fn close(&mut self, uri: &str) {
        self.buffers.remove(uri);
        self.workspaces.remove(uri);
        self.refresh_all();
    }

    /// The URIs of the open documents.
    pub fn open_uris(&self) -> Vec<String> {
        self.buffers.keys().cloned().collect()
    }

    /// Format the whole document at `uri` into the canonical style, returning a single
    /// full-document replacement edit — or `None` if the document is not open, or an empty edit list
    /// if it is already canonical. Style is the nearest `noeta.toml` `[fmt]` config (defaults if
    /// none); a source that does not parse (or an internal safety abort) yields no edits, leaving the
    /// buffer untouched. The same `noeta_fmt` engine as `noeta fmt`, so editor and CLI agree.
    pub fn format_document(&self, uri: &str, encoding: Encoding) -> Option<Vec<TextEdit>> {
        let text = self.buffers.get(uri)?;
        let config = uri_to_path(uri)
            .and_then(|p| p.parent().map(noeta_fmt::FmtConfig::discover))
            .unwrap_or_default();
        let formatted = noeta_fmt::format_source(uri, text, &config).ok()?;
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
            .and_then(|p| p.parent().map(noeta_fmt::FmtConfig::discover))
            .unwrap_or_default();
        let index = LineIndex::new(text);
        let offset = index.offset(position, encoding);
        let (start, end, new_text) = noeta_fmt::format_stmt_at(uri, text, offset, &config)?;
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
            .and_then(|p| p.parent().map(noeta_fmt::FmtConfig::discover))
            .unwrap_or_default();
        let index = LineIndex::new(text);
        let start = index.offset(range.start, encoding);
        let end = index.offset(range.end, encoding);
        let edits = noeta_fmt::format_range(uri, text, start, end, &config)?;
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
        // Dependency packages (package-manager P2.1c): resolve the entry's deps and add each dep
        // module as a `DepModule` input (SourceIds continue past the siblings), so cross-package
        // `use <dep-key>.…` resolves in hover/goto/completion exactly as the CLI resolves it.
        let deps = self.resolve_dep_modules(uri, programs.len() as u32);
        let workspace = Workspace::new(&self.db, programs[0], programs[1..].to_vec(), deps.modules);
        self.workspaces.insert(
            uri.to_string(),
            WorkspaceCache {
                workspace,
                source_uris: uris,
                programs,
                dep_uris: deps.uris,
                dep_programs: deps.programs,
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

    /// Resolve the entry `uri`'s dependency packages into salsa [`DepModule`] inputs (package-manager
    /// P2.1c), each source given a [`SourceId`] continuing from `first_id` (past entry + siblings) so
    /// its spans stay distinct and map back to its file for cross-package navigation. Resolution
    /// reuses the CLI's `noeta-pm` walk — path deps read locally, git deps served from the package
    /// store (materialized by a prior CLI run) — so the editor sees the same cross-package program.
    /// A resolution failure (a registry dep, an unfetched git dep) degrades to no dependencies rather
    /// than breaking the workspace; the user still gets the entry's own analysis.
    fn resolve_dep_modules(&self, uri: &str, first_id: u32) -> ResolvedDeps {
        let mut deps = ResolvedDeps::default();
        let Some(entry_path) = uri_to_path(uri) else {
            return deps;
        };
        let Ok(packages) = noeta_pm::manifest::dependency_packages(&entry_path) else {
            return deps;
        };
        let mut next_id = first_id;
        for package in &packages {
            let renames: Vec<String> = package
                .dep_renames
                .iter()
                .flat_map(|(local, global)| [local.clone(), global.clone()])
                .collect();
            for module in &package.modules {
                let src =
                    SourceProgram::new(&self.db, next_id, module.name.clone(), module.text.clone());
                next_id += 1;
                deps.modules.push(DepModule::new(
                    &self.db,
                    src,
                    package.root.clone(),
                    package.key.clone(),
                    renames.clone(),
                ));
                deps.uris.push(path_to_uri(Path::new(&module.name)));
                deps.programs.push(src);
            }
        }
        deps
    }

    /// The `uri`'s own diagnostics (cross-module resolution, but only the entry file's own
    /// diagnostics — each open module reports its own through its own workspace) together with the
    /// entry text for position mapping. `None` if the document is not open.
    ///
    /// Runs over the whole-workspace [`linked_checked`](noeta_db::linked_checked) query, so a name
    /// imported from a sibling module resolves and no longer reports a false "unknown name". A load
    /// or parse failure carries its diagnostics through the same query.
    pub fn diagnostics(&self, uri: &str) -> Option<(Vec<noeta_diagnostics::Diagnostic>, String)> {
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
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let entry = cache.entry();
        let index = LineIndex::new(entry.text(db));
        let start = index.offset(range.start, encoding);
        let end = index.offset(range.end, encoding);

        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, entry);
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let ide = noeta_db::linked_checked_ide(db, cache.workspace);
        Some(
            inlay::type_hints(program, &ide.expr_types, &ide.packed_layouts, SourceId::FIRST)
                .into_iter()
                .filter(|hint| start <= hint.offset && hint.offset <= end)
                .map(|hint| (index.position(hint.offset, encoding), hint.label, hint.kind))
                .collect(),
        )
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
        let cache = self.workspaces.get(uri)?;
        let db = &self.db;
        let index = LineIndex::new(cache.entry().text(db));
        let offset = index.offset(position, encoding);
        let checked = noeta_db::linked_checked_ide(db, cache.workspace);
        let (span, repr) = checked
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
        let note = layout_note(repr, &checked.packed_layouts);
        Some((repr.clone(), note, index.range(*span, encoding)))
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
        let cache = self.workspaces.get(uri)?;
        let offset = LineIndex::new(cache.entry().text(&self.db)).offset(position, encoding);
        let spans = self.symbol_occurrences(cache, offset, include_declaration)?;

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
        offset: u32,
        include_declaration: bool,
    ) -> Option<Vec<Span>> {
        let db = &self.db;
        let cursor = SourceId::FIRST;
        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, cache.entry());
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
        let ide = noeta_db::linked_checked_ide(db, cache.workspace);
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
        let cache = self.workspaces.get(uri)?;
        let index = LineIndex::new(cache.entry().text(&self.db));
        let offset = index.offset(position, encoding);
        // The occurrences include the one under the cursor (a use or the declaration); return it.
        let here = self
            .symbol_occurrences(cache, offset, true)?
            .into_iter()
            .find(|span| {
                span.source == SourceId::FIRST && span.start <= offset && offset <= span.end
            })?;
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
    /// cursor (**identifier completion**, C1).
    ///
    /// A best-effort read of the mid-edit document: it relies on the recovering parser and the
    /// client's prefix filtering. `None` if the document is not open.
    pub fn completions(
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
            let checked = noeta_db::linked_checked_ide(db, cache.workspace);
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
            return Some(bare_dot_members(entry_text, offset, program).unwrap_or_default());
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
        let cache = self.workspaces.get(uri)?;
        let entry = cache.entry();
        let index = LineIndex::new(entry.text(&self.db));
        let program = &noeta_db::ast(&self.db, entry).0.program;

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

    /// The workspace serving `uri` and the [`SourceId`] `uri` carries within it: an open document
    /// is its own workspace's entry; otherwise any open workspace that discovered `uri` as a
    /// sibling or dependency module answers for it (same merged program either way). This is what
    /// lets call-hierarchy expansion continue from an item in a file the user never opened — the
    /// hierarchy requests address items by `(uri, selection range)`, not by the entry document.
    fn workspace_of(&self, uri: &str) -> Option<(&WorkspaceCache, SourceId)> {
        if let Some(cache) = self.workspaces.get(uri) {
            return Some((cache, SourceId::FIRST));
        }
        self.workspaces.values().find_map(|cache| {
            let idx = cache
                .source_uris
                .iter()
                .position(|u| u == uri)
                .or_else(|| {
                    cache
                        .dep_uris
                        .iter()
                        .position(|u| u == uri)
                        .map(|i| i + cache.programs.len())
                })?;
            Some((cache, SourceId(idx as u32)))
        })
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
        let (cache, source) = self.workspace_of(uri)?;
        let offset = LineIndex::new(self.source_text(cache, source)?).offset(position, encoding);
        let (graph, info) = self.call_graph(cache);
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
        let (cache, source) = self.workspace_of(uri)?;
        let offset = LineIndex::new(self.source_text(cache, source)?).offset(position, encoding);
        let (graph, info) = self.call_graph(cache);
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
        let (cache, source) = self.workspace_of(uri)?;
        let offset = LineIndex::new(self.source_text(cache, source)?).offset(position, encoding);
        let (graph, info) = self.call_graph(cache);
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
        let (cache, source) = self.workspace_of(uri)?;
        let (graph, info) = self.call_graph(cache);
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
        let (cache, _) = self.workspace_of(uri)?;
        let (graph, info) = self.call_graph(cache);
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

    /// The call-graph context of `uri`'s workspace: the static graph over the merged program plus
    /// the reflection index (`@role`/attribute bindings with their target spans) — the same
    /// [`callgraph`]+[`reflect`](noeta_ast::reflect) join the MCP `trace` tool serves, so editor
    /// and agent read one engine. An unlinkable workspace degrades to the entry's own AST (the
    /// within-file graph keeps working while a sibling is broken).
    fn call_graph(
        &self,
        cache: &WorkspaceCache,
    ) -> (callgraph::CallGraph, noeta_ast::reflect::ReflectionInfo) {
        let db = &self.db;
        let linked = noeta_db::linked(db, cache.workspace);
        let entry_ast = noeta_db::ast(db, cache.entry());
        let program = match &linked.0 {
            Ok(program) => program,
            Err(_) => &entry_ast.0.program,
        };
        let ide = noeta_db::linked_checked_ide(db, cache.workspace);
        // Texts by SourceId index: entry + siblings, then dependency modules (ids continue past).
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

/// The top-level function declaration named `name`, for signature help on a plain call.
fn top_level_fn<'a>(program: &'a noeta_ast::Program, name: &str) -> Option<&'a noeta_ast::FnDecl> {
    program.stmts.iter().find_map(|stmt| match stmt {
        noeta_ast::Stmt::Fn(decl) if decl.name == name => Some(decl),
        _ => None,
    })
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
/// precedes the insertion, so its span is unchanged. `None` if its type is not a nominal.
fn receiver_type_at(text: &str, offset: u32, receiver_span: Span) -> Option<String> {
    let o = offset as usize;
    let at_arg_boundary = {
        let before = text[..o].trim_end();
        before.ends_with('(') || before.ends_with(',')
    };
    let closer = if at_arg_boundary { "x)" } else { ")" };
    let munged = format!("{}{closer}{}", &text[..o], &text[o..]);
    let source = noeta_span::Source::new(SourceId::FIRST, "<signature>", &munged);
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

    #[test]
    fn open_registers_a_document() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "let x = 1".to_string());
        assert_eq!(store.buffers.len(), 1);
        let program = store.workspaces["file:///a.noe"].entry();
        assert_eq!(program.text(&store.db), "let x = 1");
    }

    #[test]
    fn format_document_returns_a_full_replacement_edit() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "echo 1\n".to_string());
        let edits = store
            .format_document("file:///a.noe", Encoding::Utf16)
            .expect("open document");
        assert!(edits.is_empty(), "already-formatted source needs no edit");
    }

    #[test]
    fn format_document_declines_unparseable_source() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "fn (".to_string());
        // Broken source yields no edits (the LSP returns `None`), leaving the buffer untouched.
        let edits = store.format_document("file:///a.noe", Encoding::Utf16);
        assert!(edits.unwrap_or_default().is_empty());
    }

    #[test]
    fn format_on_type_reformats_the_closed_block() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "fn f() {\n".to_string()); // unbalanced, mid-type
        assert!(
            store
                .format_on_type("file:///a.noe", Position::new(1, 0), Encoding::Utf16)
                .is_none()
        );
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
        let mut store = DocumentStore::default();
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
    fn inlay_hints_mark_packed_storage_compactly() {
        let mut store = DocumentStore::default();
        store.open(
            "file:///packed.noe",
            "@packed struct Vec3 { x: f32; y: f32; z: f32 }\n\
             @packed(layout: column) struct Cell { n: int; on: bool }\n\
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
    fn type_error_is_reported() {
        let mut store = DocumentStore::default();
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
                .map(|(repr, _note, _range)| repr.to_string())
        };
        assert_eq!(at(7).as_deref(), Some("List<int>")); // the `[1, 2, 3]` literal
        assert_eq!(at(8).as_deref(), Some("int")); // the `1` element
    }

    #[test]
    fn hover_notes_packed_storage() {
        let mut store = DocumentStore::default();
        store.open(
            "file:///p.noe",
            "@packed(layout: column) struct Vec3 { x: f32; y: f32; z: f32 }\n\
             v = Vec3 { x: 1.0f32, y: 2.0f32, z: 3.0f32 }\n\
             vs = [v]\n"
                .to_string(),
        );
        let at = |line, character| {
            store
                .hover_type("file:///p.noe", Position { line, character }, Encoding::Utf8)
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
        let mut store = DocumentStore::default();
        store.open(
            "file:///q.noe",
            "struct P { x: int; y: int }\np = P { x: 1, y: 2 }\nps = [p]\n".to_string(),
        );
        let note_at = |line, character| {
            store
                .hover_type("file:///q.noe", Position { line, character }, Encoding::Utf8)
                .and_then(|(_repr, note, _range)| note)
        };
        assert_eq!(note_at(1, 4), None); // the `P { … }` literal
        assert_eq!(note_at(2, 5), None); // the `[p]` list
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
            .expect("open document offers completions");
        let has = |label: &str, kind: completion::CandidateKind| {
            items.iter().any(|i| i.label == label && i.kind == kind)
        };
        assert!(has("helper", completion::CandidateKind::Function));
        assert!(has("total", completion::CandidateKind::Variable));
        assert!(has("return", completion::CandidateKind::Keyword));
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
    fn signature_help_resolves_a_method_call() {
        let mut store = DocumentStore::default();
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
    fn references_find_a_field_across_uses_not_a_namesake() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
    fn call_hierarchy_for_unknown_document_is_none() {
        let store = DocumentStore::default();
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
