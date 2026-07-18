//! In-browser language smarts (P-WASM W2.3, the engine half): hover, completion,
//! go-to-definition, and signature help over [`noeta_ide::DocumentStore`] — the same
//! wire-protocol-free engine `noeta lsp` adapts over, so the playground's answers are the LSP's
//! answers. There is no JSON-RPC here (a browser tab has no stdio peer); the editor calls these
//! through the wasm ABI and gets plain JSON.
//!
//! Positions are **zero-based `(line, character)` in UTF-16 code units** — the LSP default and
//! what a JS editor (whose string indices *are* UTF-16 units) speaks natively. Ranges come back
//! in the same convention.
//!
//! One [`DocumentStore`] persists for the instance's lifetime (thread-local — a wasm instance is
//! single-threaded), holding the single `playground.noe` document. Every request pushes the
//! current buffer through [`DocumentStore::change`], the LSP's own keystroke path, so salsa
//! recomputes only what the edit invalidated — the playground gets the IDE's incrementality for
//! free. In the browser the store's sibling-module discovery degrades gracefully to single-file
//! (`std::fs` errors on wasm32-unknown-unknown, and the store treats an unreadable directory as
//! "no siblings").

use std::cell::RefCell;

use noeta_ide::completion::CandidateKind;
use noeta_ide::{DocumentStore, Encoding, Position, Range};
use serde_json::json;

/// The playground buffer's URI in the store — matching [`crate::SOURCE_NAME`], so hover types,
/// diagnostics, and tracebacks all name the same document.
const URI: &str = "playground.noe";

thread_local! {
    static STORE: RefCell<DocumentStore> = RefCell::new(DocumentStore::default());
}

/// Push `text` into the persistent store (the keystroke path) and answer `f` against it.
fn with_document<T>(text: &str, f: impl FnOnce(&DocumentStore) -> T) -> T {
    STORE.with(|store| {
        let mut store = store.borrow_mut();
        store.change(URI, text.to_string());
        f(&store)
    })
}

fn position(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn range_json(range: Range) -> serde_json::Value {
    json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character },
    })
}

/// The type under the cursor: `{"found": true, "type": …, "note": …, "range": …}` — the type in
/// its Noeta surface spelling (the same rendering the LSP hover and the debugger's Variables view
/// use), `note` the optional storage fact (`@packed` / flat list layout).
pub fn hover_source(text: &str, line: u32, character: u32) -> String {
    with_document(text, |store| {
        match store.hover_type(URI, position(line, character), Encoding::Utf16) {
            Some((repr, note, range)) => json!({
                "found": true,
                "type": repr.to_string(),
                "note": note,
                "range": range_json(range),
            })
            .to_string(),
            None => json!({ "found": false }).to_string(),
        }
    })
}

/// The definition of the reference under the cursor: `{"found": true, "range": …}`. The
/// playground is single-file, so the target is always in the same buffer (the engine can resolve
/// cross-file, but there are no siblings to point at).
pub fn definition_source(text: &str, line: u32, character: u32) -> String {
    with_document(text, |store| {
        match store.definition(URI, position(line, character), Encoding::Utf16) {
            Some((_, range)) => json!({ "found": true, "range": range_json(range) }).to_string(),
            None => json!({ "found": false }).to_string(),
        }
    })
}

/// Completion candidates at the cursor: `{"items": [{"label", "kind", "detail"}…]}`. Member
/// completion after `.` (including the bare-dot mid-edit form), type names in annotation
/// position, otherwise keywords + declarations + in-scope bindings — the LSP's exact behavior.
pub fn complete_source(text: &str, line: u32, character: u32) -> String {
    with_document(text, |store| {
        let items: Vec<_> = store
            .completions(URI, position(line, character), Encoding::Utf16)
            .unwrap_or_default()
            .into_iter()
            .map(|candidate| {
                json!({
                    "label": candidate.label,
                    "kind": kind_word(candidate.kind),
                    "detail": candidate.detail,
                })
            })
            .collect();
        json!({ "items": items }).to_string()
    })
}

/// Signature help inside a call: `{"found": true, "label", "parameters": […], "active": n}` with
/// `active` the 0-based index of the argument under the cursor.
pub fn signature_source(text: &str, line: u32, character: u32) -> String {
    with_document(text, |store| {
        match store.signature_help(URI, position(line, character), Encoding::Utf16) {
            Some(data) => json!({
                "found": true,
                "label": data.label,
                "parameters": data.parameters,
                "active": data.active_param,
            })
            .to_string(),
            None => json!({ "found": false }).to_string(),
        }
    })
}

/// The lowercase kind word an editor maps onto its completion icons.
fn kind_word(kind: CandidateKind) -> &'static str {
    match kind {
        CandidateKind::Keyword => "keyword",
        CandidateKind::Function => "function",
        CandidateKind::Struct => "struct",
        CandidateKind::Class => "class",
        CandidateKind::Enum => "enum",
        CandidateKind::Variable => "variable",
        CandidateKind::Field => "field",
        CandidateKind::Method => "method",
        CandidateKind::EnumMember => "enum-member",
        CandidateKind::Type => "type",
        CandidateKind::Trait => "trait",
        CandidateKind::Module => "module",
        CandidateKind::Directive => "directive",
    }
}
