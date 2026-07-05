//! The Noeta language server (`noeta lsp`).
//!
//! A thin JSON-RPC adapter over the compiler's salsa query graph (`noeta-db`). The server owns
//! exactly two pieces of state: one [`LangDatabase`] and a map from each open document's URI to its
//! salsa [`SourceProgram`] input. Every language feature is then a *read* of a memoized query —
//! editing a document calls the salsa-generated `set_text` setter, and salsa recomputes only the
//! queries that the edit invalidated. That incremental spine is what makes the server responsive;
//! it is inherited wholesale from M1, not built here.
//!
//! Slice **L0** stood up the skeleton and document lifecycle. Slice **L1** added **live
//! diagnostics**: on open/change the server runs `tokens` → `ast` → `checked_ide` (each
//! salsa-memoized, so an edit recomputes only what it touched), takes the earliest failing stage's
//! diagnostics — mirroring the compiler's own lex→parse→check gating — maps each to an LSP
//! `Diagnostic` (severity, `E0xxx` code, range, related labels), and publishes them. Slice **L2**
//! adds **hover types**: the cursor position becomes a byte offset, the tightest enclosing span in
//! the checker's `expr_types` index gives the inferred type, and it is rendered back to surface
//! syntax (see [`hover`]). Both features read the one `checked_ide` query, so a document version is
//! checked once. Positions are converted encoding-aware (see [`offsets`]).

mod hover;
mod offsets;

use std::collections::HashMap;
use std::sync::Mutex;

use noeta_ast::reflect::TypeRepr;
use noeta_db::{LangDatabase, SourceProgram};
use offsets::{Encoding, LineIndex};
use salsa::Setter;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

/// The server's document state: the salsa database plus the live set of open documents, each
/// mapped to its salsa input. Kept behind a [`Mutex`] on the [`Backend`]; the LSP request handlers
/// lock it, do their (synchronous, fast) salsa work, and release it before awaiting any client I/O.
///
/// Split out from [`Backend`] so it can be unit-tested without a live [`Client`]: the document
/// bookkeeping is pure and does not touch the transport.
#[derive(Default)]
struct DocumentStore {
    db: LangDatabase,
    /// Keyed by the document URI's string form (`Uri` is not a convenient map key across
    /// lsp-types versions; its text is stable and unique per open document).
    open: HashMap<String, SourceProgram>,
}

impl std::fmt::Debug for DocumentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentStore")
            .field("open", &self.open.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl DocumentStore {
    /// Register a freshly opened document. Creates a new [`SourceProgram`] input (id
    /// [`SourceId::FIRST`](noeta_span::SourceId::FIRST) — each open document is checked as its own
    /// single-file entry program until cross-file features arrive in L3) and stores it under `uri`.
    /// Re-opening a URI replaces its input.
    fn open(&mut self, uri: &str, text: String) {
        let program = SourceProgram::new(&self.db, 0, uri.to_string(), text);
        self.open.insert(uri.to_string(), program);
    }

    /// Apply a full-document change. Mutates the existing input's text in place — the salsa setter
    /// invalidates exactly the queries that read it — or, if the document is somehow unknown (no
    /// prior `didOpen`), registers it. Returns the input so callers can immediately re-query it.
    fn change(&mut self, uri: &str, text: String) -> SourceProgram {
        match self.open.get(uri) {
            Some(&program) => {
                program.set_text(&mut self.db).to(text);
                program
            }
            None => {
                self.open(uri, text);
                self.open[uri]
            }
        }
    }

    /// Drop a closed document and its input handle.
    fn close(&mut self, uri: &str) {
        self.open.remove(uri);
    }

    /// Collect the diagnostics for `uri` together with its current text (needed for position
    /// mapping). `None` if the document is not open.
    ///
    /// Follows the compiler's own **lex → parse → check gating**: the earliest stage that reports
    /// anything wins, because each later stage runs on the earlier stage's (broken) output and its
    /// diagnostics would be noise. All three queries are salsa-memoized, so an edit re-runs only the
    /// stages it actually invalidated.
    fn diagnostics(&self, uri: &str) -> Option<(Vec<noeta_diagnostics::Diagnostic>, String)> {
        let &program = self.open.get(uri)?;
        let db = &self.db;
        let lexed = noeta_db::tokens(db, program);
        let diags = if !lexed.0.diagnostics.is_empty() {
            lexed.0.diagnostics.clone()
        } else {
            let parsed = noeta_db::ast(db, program);
            if !parsed.0.diagnostics.is_empty() {
                parsed.0.diagnostics.clone()
            } else {
                // `checked_ide` (not `checked`) so hover reads the same memoized run — one checker
                // pass per document version serves both diagnostics and the type index.
                noeta_db::checked_ide(db, program).diagnostics.clone()
            }
        };
        Some((diags, program.text(db).clone()))
    }

    /// The type at `position` for hover: the **smallest** `expr_types` span that contains the cursor
    /// (the most specific expression under it), rendered plus its LSP range. `None` if the document
    /// is unknown or no typed expression covers the position. The whole lookup runs under one salsa
    /// borrow — the type index is never cloned.
    fn hover_type(
        &self,
        uri: &str,
        position: Position,
        encoding: Encoding,
    ) -> Option<(TypeRepr, Range)> {
        let &program = self.open.get(uri)?;
        let db = &self.db;
        let index = LineIndex::new(program.text(db));
        let offset = index.offset(position, encoding);
        let (span, repr) = noeta_db::checked_ide(db, program)
            .expr_types
            .iter()
            // Non-empty spans that cover the cursor; pick the tightest (innermost) one.
            .filter(|(span, _)| span.end > span.start && span.start <= offset && offset <= span.end)
            .min_by_key(|(span, _)| span.end - span.start)?;
        Some((repr.clone(), index.range(*span, encoding)))
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = doc.uri;
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.open(uri.as_str(), doc.text);
        }
        self.publish(uri).await;
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
        self.publish(uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.close(uri.as_str());
        }
        // Clear any diagnostics the client is still showing for the now-closed document.
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
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
        assert_eq!(store.open.len(), 1);
        let program = store.open["file:///a.noe"];
        assert_eq!(program.text(&store.db), "let x = 1");
    }

    #[test]
    fn change_mutates_the_same_input_in_place() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "old".to_string());
        let before = store.open["file:///a.noe"];

        let after = store.change("file:///a.noe", "new".to_string());

        // Same salsa input handle (edited in place, not replaced) with the updated text — this is
        // what lets salsa recompute only the affected downstream queries.
        assert_eq!(before, after);
        assert_eq!(after.text(&store.db), "new");
        assert_eq!(store.open.len(), 1);
    }

    #[test]
    fn change_on_unknown_document_registers_it() {
        let mut store = DocumentStore::default();
        let program = store.change("file:///ghost.noe", "hi".to_string());
        assert_eq!(program.text(&store.db), "hi");
        assert_eq!(store.open.len(), 1);
    }

    #[test]
    fn close_drops_the_document() {
        let mut store = DocumentStore::default();
        store.open("file:///a.noe", "x".to_string());
        store.close("file:///a.noe");
        assert!(store.open.is_empty());
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
    fn editing_rechecks_and_clears_the_error() {
        let mut store = DocumentStore::default();
        store.open("file:///f.noe", "count: int = \"lots\"".to_string());
        assert_eq!(store.diagnostics("file:///f.noe").unwrap().0.len(), 1);
        // Fix it — salsa re-runs the check on the mutated input and the error is gone.
        store.change("file:///f.noe", "count: int = 7".to_string());
        assert!(store.diagnostics("file:///f.noe").unwrap().0.is_empty());
    }
}
