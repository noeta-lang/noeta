//! In-browser language smarts (the engine half): hover, completion,
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
///
/// Seeds the process-default extension registry first, for the same reason
/// [`crate::front_end`] does: nothing seeds it in this wasm module, and the store resolves `std`
/// names against it. Without this an IDE request that arrived **before** the first `check`/`run`
/// — a hover racing the page's first diagnostics pass — hit an unseeded registry and aborted the
/// whole instance on a wasm `unreachable`. `check`/`run`/`debug` all seed it via `front_end`, so
/// the fault only ever showed on a cold engine, which is exactly when a visitor's first hover
/// lands. Idempotent, so the repeat cost on every keystroke is a resolved-once check.
fn with_document<T>(text: &str, f: impl FnOnce(&DocumentStore) -> T) -> T {
    noeta_stdlib::registry::default_seeded();
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

/// The composed hover under the cursor: `{"found": true, "value": <markdown>, "range": …}` — the
/// exact rich Markdown the VS Code hover shows, assembled by
/// [`noeta_ide::DocumentStore::hover_markdown`] so the playground and the LSP cannot drift. A
/// callable name yields its full `fn add(a: int, b: int): int` signature (plus doc), a type name
/// its declaration, a plain sub-expression the bare type — not just the return type.
///
/// The doc-only fallback (a declaration's own name, no expression span) has no range: it comes
/// back `{"found": true, "value": …}` with the `range` key **omitted**. `{"found": false}` when
/// nothing hovers.
pub fn hover_source(text: &str, line: u32, character: u32) -> String {
    with_document(text, |store| {
        match store.hover_markdown(URI, position(line, character), Encoding::Utf16) {
            Some((value, Some(range))) => json!({
                "found": true,
                "value": value,
                "range": range_json(range),
            })
            .to_string(),
            Some((value, None)) => json!({
                "found": true,
                "value": value,
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

/// Completion candidates at the cursor: `{"items": [{"label", "kind", "detail", "insertText"}…]}`.
/// Member completion after `.` (including the bare-dot mid-edit form), type names in annotation
/// position, otherwise keywords + reflection primitives + declarations + in-scope bindings — the
/// LSP's exact behavior.
///
/// `insertText` is `null` for all but the turbofish-only reflection primitives, where it carries the
/// `::<` the bare word cannot be written without; a client that ignores it inserts the label, which
/// is the same default the LSP's own clients apply.
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
                    "insertText": candidate.insert_text,
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
