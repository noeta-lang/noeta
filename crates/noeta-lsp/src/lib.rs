//! The Noeta language server (`noeta lsp`).
//!
//! A thin JSON-RPC adapter over the compiler's salsa query graph (`noeta-db`). The server owns
//! exactly two pieces of state: one [`LangDatabase`] and a map from each open document's URI to its
//! salsa [`SourceProgram`] input. Every language feature is then a *read* of a memoized query —
//! editing a document calls the salsa-generated `set_text` setter, and salsa recomputes only the
//! queries that the edit invalidated. That incremental spine is what makes the server responsive;
//! it is inherited wholesale from M1, not built here.
//!
//! This is milestone slice **L0**: the server skeleton and document lifecycle. It stands up the
//! stdio transport, the `initialize`/`initialized`/`shutdown` handshake, capability advertisement,
//! and `didOpen`/`didChange`/`didClose` text synchronization (full-document sync), maintaining the
//! URI→`SourceProgram` map. No language features are wired yet — live diagnostics land in L1.

use std::collections::HashMap;
use std::sync::Mutex;

use noeta_db::{LangDatabase, SourceProgram};
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
}

/// The language server backend: the LSP transport handle plus the shared [`DocumentStore`].
#[derive(Debug)]
struct Backend {
    client: Client,
    store: Mutex<DocumentStore>,
}

impl Backend {
    fn new(client: Client) -> Backend {
        Backend {
            client,
            store: Mutex::new(DocumentStore::default()),
        }
    }
}

impl LanguageServer for Backend {
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "noeta-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // Full-document sync for L0: `didChange` ships the whole buffer. Salsa still only
                // recomputes what the edit touched, so this is not a bottleneck; incremental
                // (range) sync is a later refinement.
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
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

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let doc = params.text_document;
        let uri = doc.uri.as_str().to_string();
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.open(&uri, doc.text);
        }
        self.client
            .log_message(MessageType::INFO, format!("opened {uri}"))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.as_str().to_string();
        // Full sync: the last content change carries the entire new document text.
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.change(&uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.as_str().to_string();
        {
            let mut store = self.store.lock().expect("document store poisoned");
            store.close(&uri);
        }
        self.client
            .log_message(MessageType::INFO, format!("closed {uri}"))
            .await;
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
}
