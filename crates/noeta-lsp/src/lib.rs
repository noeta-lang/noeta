//! The Noeta language server (`noeta lsp`).
//!
//! A thin JSON-RPC **wire adapter** over the shared IDE engine
//! ([`noeta_ide::DocumentStore`]) — since MCP slice M5, every language feature (diagnostics,
//! hover, go-to-definition, references, rename, symbols, signature help, semantic tokens,
//! completion, inlay hints, formatting) lives in `noeta-ide`, where `noeta mcp` reads the same
//! implementation. This crate owns only what is LSP: the tower-lsp transport and lifecycle, the
//! position-encoding negotiation, and the mechanical conversions between the engine's positional
//! types and their `ls_types` wire counterparts (field-compatible by construction).
//!
//! The [`Backend`] holds the store behind a `Mutex`; most request handlers lock it, do their
//! (synchronous, fast) salsa work, and release it before awaiting any client I/O. The three
//! *expensive* paths — the diagnostics publish, semantic tokens, completion — instead run over a
//! [`DocumentStore::snapshot`] on a blocking thread ([`Backend::read_latest`]), so a newer edit
//! **cancels** an in-flight run (salsa unwinds it) rather than queueing behind it, and a
//! superseded result is never delivered (audit-4 finding 9).

use std::collections::HashMap;
use std::sync::Mutex;

use noeta_ide::{DocumentStore, Encoding, LineIndex, TOP_LEVEL, completion, inlay, semtokens};
use tower_lsp_server::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp_server::ls_types::*;
use tower_lsp_server::{Client, LanguageServer, LspService, Server};

/// An engine position to its LSP wire form (same fields; distinct types keep the engine
/// wire-protocol-free).
fn wire_position(p: noeta_ide::Position) -> Position {
    Position {
        line: p.line,
        character: p.character,
    }
}

/// An LSP wire position to its engine form.
fn ide_position(p: Position) -> noeta_ide::Position {
    noeta_ide::Position {
        line: p.line,
        character: p.character,
    }
}

fn wire_range(r: noeta_ide::Range) -> Range {
    Range {
        start: wire_position(r.start),
        end: wire_position(r.end),
    }
}

fn ide_range(r: Range) -> noeta_ide::Range {
    noeta_ide::Range {
        start: ide_position(r.start),
        end: ide_position(r.end),
    }
}

fn wire_text_edit(edit: noeta_ide::TextEdit) -> TextEdit {
    TextEdit {
        range: wire_range(edit.range),
        new_text: edit.new_text,
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
                    range: wire_range(index.range(label.span, encoding)),
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
        range: wire_range(index.range(diag.span, encoding)),
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
        CandidateKind::Trait => CompletionItemKind::INTERFACE,
        CandidateKind::Module => CompletionItemKind::MODULE,
        // A decorator/tier directive completed after `@` — KEYWORD for the language-surface icon.
        CandidateKind::Directive => CompletionItemKind::KEYWORD,
    };
    CompletionItem {
        label: candidate.label.clone(),
        kind: Some(kind),
        detail: candidate.detail.clone(),
        ..Default::default()
    }
}

/// Map an engine [`DocumentSymbol`](noeta_ide::DocumentSymbol) (spans already resolved to ranges)
/// to its LSP wire form, recursing into children.
fn to_document_symbol(symbol: noeta_ide::DocumentSymbol) -> DocumentSymbol {
    let kind = match symbol.kind {
        noeta_ide::SymbolKind::Function => SymbolKind::FUNCTION,
        noeta_ide::SymbolKind::Struct => SymbolKind::STRUCT,
        noeta_ide::SymbolKind::Class => SymbolKind::CLASS,
        noeta_ide::SymbolKind::Enum => SymbolKind::ENUM,
        noeta_ide::SymbolKind::EnumMember => SymbolKind::ENUM_MEMBER,
        noeta_ide::SymbolKind::Field => SymbolKind::FIELD,
        noeta_ide::SymbolKind::Method => SymbolKind::METHOD,
        noeta_ide::SymbolKind::Interface => SymbolKind::INTERFACE,
        noeta_ide::SymbolKind::Trait => SymbolKind::INTERFACE,
    };
    #[allow(deprecated)]
    // `deprecated` is a required struct field, not a value we set meaningfully.
    DocumentSymbol {
        name: symbol.name,
        detail: symbol.detail,
        kind,
        tags: None,
        deprecated: None,
        range: wire_range(symbol.range),
        selection_range: wire_range(symbol.selection_range),
        children: (!symbol.children.is_empty()).then(|| {
            symbol
                .children
                .into_iter()
                .map(to_document_symbol)
                .collect()
        }),
    }
}

/// The call-hierarchy item detail line: the function's `@role` bindings, plus a marker when the
/// group reaches it only as a **passed value** (never a syntactic call — a callback or handler
/// registration). This is where the engine's static-analysis honesty surfaces in the native UI.
fn hierarchy_detail(roles: &[String], reference_only: bool) -> Option<String> {
    let mut parts: Vec<&str> = roles.iter().map(String::as_str).collect();
    if reference_only {
        parts.push("reference (passed as value)");
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Map an engine [`HierarchyItem`](noeta_ide::HierarchyItem) (spans already resolved) to the LSP
/// wire item. `None` if its URI does not parse (the item would not be navigable).
fn to_call_hierarchy_item(
    item: noeta_ide::HierarchyItem,
    detail: Option<String>,
) -> Option<CallHierarchyItem> {
    let kind = match item.kind {
        noeta_ide::SymbolKind::Method => SymbolKind::METHOD,
        _ => SymbolKind::FUNCTION,
    };
    Some(CallHierarchyItem {
        name: item.name,
        kind,
        tags: None,
        detail,
        uri: item.uri.parse().ok()?,
        range: wire_range(item.range),
        selection_range: wire_range(item.selection_range),
        data: None,
    })
}

/// Map one engine [`HierarchyCall`](noeta_ide::HierarchyCall) group to `(wire item, fromRanges)` —
/// the shared shape of incoming and outgoing answers (both directions' `fromRanges` live in the
/// caller's document, which is how the engine returns them).
fn to_hierarchy_call(call: noeta_ide::HierarchyCall) -> Option<(CallHierarchyItem, Vec<Range>)> {
    let reference_only = call.sites.iter().all(|(_, is_call)| !is_call);
    let detail = hierarchy_detail(&call.item.roles, reference_only);
    let item = to_call_hierarchy_item(call.item, detail)?;
    let ranges = call.sites.into_iter().map(|(r, _)| wire_range(r)).collect();
    Some((item, ranges))
}

/// Map an engine [`RoleLens`](noeta_ide::RoleLens) to the wire CodeLens: `⚑ Enum.Variant`, with
/// the client-side `noeta.showTrace (uri, function)` command attached when the bearer is a trace
/// root. A non-traceable lens (a role on a type) gets an empty command — the standard trick for a
/// label-only lens.
fn to_code_lens(uri: &str, lens: noeta_ide::RoleLens) -> CodeLens {
    let command = if lens.traceable {
        Command {
            title: format!("⚑ {} · trace call paths", lens.role),
            command: "noeta.showTrace".to_string(),
            arguments: Some(vec![
                serde_json::Value::String(uri.to_string()),
                serde_json::Value::String(lens.target),
            ]),
        }
    } else {
        Command {
            title: format!("⚑ {}", lens.role),
            command: String::new(),
            arguments: None,
        }
    };
    CodeLens {
        range: wire_range(lens.range),
        command: Some(command),
        data: None,
    }
}

/// `noeta/trace` custom-request params: the document anchoring the workspace and the trace spec
/// (a function name or role; absent = every role-bearing function).
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TraceParams {
    uri: String,
    from: Option<String>,
}

/// A resolved location on the trace wire.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceLocWire {
    uri: String,
    line: u32,
    character: u32,
}

impl From<noeta_ide::trace::TraceLoc> for TraceLocWire {
    fn from(l: noeta_ide::trace::TraceLoc) -> TraceLocWire {
        TraceLocWire {
            uri: l.uri,
            line: l.line,
            character: l.character,
        }
    }
}

/// One node of the structured trace on the wire (`noeta/traceTree`).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceNodeTreeWire {
    name: String,
    /// `root` | `call` | `reference` (passed as a value, never a syntactic call).
    kind: String,
    roles: Vec<String>,
    loc: Option<TraceLocWire>,
    external: bool,
    dynamic: bool,
    cycle: bool,
    truncated: bool,
    children: Vec<TraceNodeTreeWire>,
}

impl From<noeta_ide::trace::LocatedTraceNode> for TraceNodeTreeWire {
    fn from(n: noeta_ide::trace::LocatedTraceNode) -> TraceNodeTreeWire {
        use noeta_ide::trace::TraceKind;
        TraceNodeTreeWire {
            name: n.name,
            kind: match n.kind {
                TraceKind::Root => "root",
                TraceKind::Call => "call",
                TraceKind::Reference => "reference",
            }
            .to_string(),
            roles: n.roles,
            loc: n.loc.map(TraceLocWire::from),
            external: n.external,
            dynamic: n.dynamic,
            cycle: n.cycle,
            truncated: n.truncated,
            children: n
                .children
                .into_iter()
                .map(TraceNodeTreeWire::from)
                .collect(),
        }
    }
}

/// A located boundary on the wire.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceBoundaryWire {
    role: String,
    target: String,
    loc: Option<TraceLocWire>,
}

/// `noeta/traceTree` answer: the structured trace the dedicated view renders — boundaries + per-
/// root call trees with resolved locations, or a `status` explaining why it is empty.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceTreeResult {
    from: Option<String>,
    /// `ok` | `noRoles` | `notFound`; absent workspace ⇒ all fields empty with status `ok`.
    status: String,
    truncated: bool,
    boundaries: Vec<TraceBoundaryWire>,
    roots: Vec<TraceNodeTreeWire>,
}

/// `noeta/trace` answer: the rendered trace document text (the client opens it read-only).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TraceResult {
    content: Option<String>,
}

/// A located node of the Architecture view (`noeta/architecture[Children]`, ide-ui U3). Positions
/// are **UTF-16** line/character regardless of the negotiated encoding — custom requests bypass
/// the client library's position conversion, and the consumer is the VS Code extension (JS string
/// semantics).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchNodeWire {
    name: String,
    roles: Vec<String>,
    uri: Option<String>,
    line: Option<u32>,
    character: Option<u32>,
    reference: bool,
    external: bool,
    dynamic: bool,
    cycle: bool,
    expandable: bool,
}

impl From<noeta_ide::ArchNode> for ArchNodeWire {
    fn from(node: noeta_ide::ArchNode) -> ArchNodeWire {
        ArchNodeWire {
            name: node.name,
            roles: node.roles,
            uri: node.uri,
            line: node.range.map(|r| r.start.line),
            character: node.range.map(|r| r.start.character),
            reference: node.reference,
            external: node.external,
            dynamic: node.dynamic,
            cycle: node.cycle,
            expandable: node.expandable,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchitectureParams {
    uri: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchitectureResult {
    roles: Option<Vec<ArchRoleWire>>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchRoleWire {
    role: String,
    bearers: Vec<ArchNodeWire>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchChildrenParams {
    uri: String,
    function: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchChildrenResult {
    children: Option<Vec<ArchNodeWire>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestsParams {
    uri: String,
}

/// `noeta/tests` answer: the file's `@test` fns (UTF-16 positions, like the architecture wire).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TestsResult {
    tests: Option<Vec<TestItemWire>>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TestItemWire {
    name: String,
    display: Option<String>,
    group: Option<String>,
    skipped: bool,
    line: u32,
    character: u32,
    end_line: u32,
}

// ---- The docs browser (docs-browser arc, slice 1): thin JSON adapters over the unified doc
// model. Positions are UTF-16 like the other custom requests. Every handler delegates to
// `DocumentStore::doc_*`, the same model the MCP `docs` tools serve. ----

/// A doc-tree node on the wire (`noeta/docs`, `noeta/docsChildren`): a navigation row plus its
/// source location when it has one.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocNodeWire {
    id: String,
    title: String,
    kind: String,
    detail: Option<String>,
    has_page: bool,
    expandable: bool,
    uri: Option<String>,
    line: Option<u32>,
    character: Option<u32>,
}

impl From<noeta_ide::docs::DocNode> for DocNodeWire {
    fn from(n: noeta_ide::docs::DocNode) -> DocNodeWire {
        let loc = n.location;
        DocNodeWire {
            id: n.id.0,
            title: n.title,
            kind: n.kind.as_str().to_string(),
            detail: n.detail,
            has_page: n.has_page,
            expandable: n.expandable,
            uri: loc.as_ref().map(|l| l.uri.clone()),
            line: loc.as_ref().map(|l| l.range.start.line),
            character: loc.as_ref().map(|l| l.range.start.character),
        }
    }
}

/// A cross-reference on the wire.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocXrefWire {
    id: String,
    title: String,
}

/// A rendered doc page on the wire (`noeta/docsPage`).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocPageWire {
    id: String,
    title: String,
    kind: String,
    signature: Option<String>,
    markdown: String,
    uri: Option<String>,
    line: Option<u32>,
    character: Option<u32>,
    xrefs: Vec<DocXrefWire>,
}

impl From<noeta_ide::docs::DocPage> for DocPageWire {
    fn from(p: noeta_ide::docs::DocPage) -> DocPageWire {
        let loc = p.location;
        DocPageWire {
            id: p.id.0,
            title: p.title,
            kind: p.kind.as_str().to_string(),
            signature: p.signature,
            markdown: p.markdown,
            uri: loc.as_ref().map(|l| l.uri.clone()),
            line: loc.as_ref().map(|l| l.range.start.line),
            character: loc.as_ref().map(|l| l.range.start.character),
            xrefs: p
                .xrefs
                .into_iter()
                .map(|x| DocXrefWire {
                    id: x.id.0,
                    title: x.title,
                })
                .collect(),
        }
    }
}

/// A search hit on the wire (`noeta/docsSearch`).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocHitWire {
    id: String,
    title: String,
    kind: String,
    snippet: String,
    score: i32,
}

impl From<noeta_ide::docs::DocHit> for DocHitWire {
    fn from(h: noeta_ide::docs::DocHit) -> DocHitWire {
        DocHitWire {
            id: h.id.0,
            title: h.title,
            kind: h.kind.as_str().to_string(),
            snippet: h.snippet,
            score: h.score,
        }
    }
}

/// One highlighted span of a snippet on the wire (`noeta/docsHighlight`), in UTF-16 code units —
/// the doc viewer's webview slices JavaScript strings with these directly.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HlSpanWire {
    start: u32,
    end: u32,
    /// The color class tag (`kw`/`str`/`num`/`com`/`ty`/`fn`/`dec`) — CSS `tok-<tag>`.
    class: String,
}

impl From<noeta_ide::highlight::HlSpan> for HlSpanWire {
    fn from(s: noeta_ide::highlight::HlSpan) -> HlSpanWire {
        HlSpanWire {
            start: s.start,
            end: s.end,
            class: s.class.as_str().to_string(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsHighlightParams {
    /// The code snippets to classify (batched: one page's signature + fences in one request).
    snippets: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocsHighlightResult {
    /// Per input snippet, the highlight spans of its **visible** text (parallel to `snippets`).
    spans: Vec<Vec<HlSpanWire>>,
    /// Per input snippet, its `// sample:start`/`// sample:end` split (see
    /// [`noeta_ide::sample`]). The viewer renders `visible` with `spans` above and reveals `full`
    /// with `fullSpans` behind an expander.
    ///
    /// The split is computed HERE rather than in the viewer on purpose: `spans` are byte offsets
    /// into the text they describe, so a viewer that folded lines itself would paint the spans of
    /// the unfolded code onto the folded code and slide every colour off its token.
    samples: Vec<SampleWire>,
}

/// One code snippet's context-folding split on the wire.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SampleWire {
    /// What to show by default (equals `full` when the snippet carries no markers).
    visible: String,
    /// The whole snippet, markers removed — what actually compiles.
    full: String,
    /// Whether anything was folded, i.e. whether to offer the expander at all.
    has_context: bool,
    /// Highlight spans for `full`, so expanding does not lose colouring.
    full_spans: Vec<HlSpanWire>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsParams {
    uri: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocsResult {
    nodes: Option<Vec<DocNodeWire>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsChildrenParams {
    uri: String,
    id: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsPageParams {
    uri: String,
    id: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocsPageResult {
    page: Option<DocPageWire>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsSearchParams {
    uri: String,
    query: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocsSearchResult {
    hits: Option<Vec<DocHitWire>>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocsForSymbolParams {
    uri: String,
    line: u32,
    character: u32,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocsForSymbolResult {
    id: Option<String>,
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
    /// The last inlay hints computed over a *clean* parse, per document URI. Served back while the
    /// buffer is momentarily unparseable mid-edit so the inline types hold steady instead of
    /// flickering off and on with each keystroke (see [`Backend::inlay_hint`]).
    inlay_cache: Mutex<HashMap<String, Vec<InlayHint>>>,
}

impl Backend {
    fn new(client: Client) -> Backend {
        Backend {
            client,
            store: Mutex::new(DocumentStore::default()),
            // Overwritten during `initialize`; UTF-16 is the protocol default until then.
            encoding: Mutex::new(Encoding::Utf16),
            inlay_cache: Mutex::new(HashMap::new()),
        }
    }

    fn encoding(&self) -> Encoding {
        *self.encoding.lock().expect("encoding lock poisoned")
    }

    /// Run one **expensive** read over a [`DocumentStore::snapshot`] on a blocking thread — off
    /// the message loop and off the store lock — so a newer `didChange` is not queued behind it
    /// but *cancels* it: the edit's salsa input write unwinds the in-flight snapshot read
    /// (audit-4 finding 9; contract on [`DocumentStore::snapshot`]). `None` means the result was
    /// **superseded** — the read was cancelled mid-query, or an edit landed while it ran (the
    /// revision check; a snapshot is not a frozen version, so a spanning result could mix two
    /// document versions and must not be delivered). A request path answers `ContentModified`
    /// (the client re-requests against the new content); the publish path simply stops (the
    /// superseding edit's own publish covers every open document).
    async fn read_latest<T, F>(&self, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce(&DocumentStore) -> T + Send + 'static,
    {
        let (snapshot, revision) = {
            let store = self.store.lock().expect("document store poisoned");
            (store.snapshot(), store.revision())
        };
        let result = tokio::task::spawn_blocking(move || {
            // The snapshot drops inside this closure — strictly before the handler re-locks the
            // store below — so a writer blocked in its input write is always released (holding a
            // snapshot while waiting on the store lock would deadlock the writer).
            noeta_ide::catch_cancelled(move || f(&snapshot))
        })
        .await
        // A genuine engine panic stays a crash, exactly as it was under the lock — cancellation
        // is the only unwind absorbed (inside `catch_cancelled`), never a silently-empty answer.
        .expect("feature computation panicked")?;
        let store = self.store.lock().expect("document store poisoned");
        (store.revision() == revision).then_some(result)
    }

    /// The `noeta/trace` custom request (ide-ui U2): render the role-aware static trace as a
    /// plain-text document for the editor's `noeta-trace:` virtual-document view. The same
    /// [`noeta_ide::trace`] walk the MCP `trace` tool serves. `content: null` when no open
    /// workspace covers the URI.
    async fn noeta_trace(&self, params: TraceParams) -> Result<TraceResult> {
        let content = {
            let store = self.store.lock().expect("document store poisoned");
            store.trace_document(&params.uri, params.from.as_deref())
        };
        Ok(TraceResult { content })
    }

    /// `noeta/traceTree`: the structured, located trace for the dedicated view — the same
    /// role-aware call-graph walk `noeta/trace` renders as text.
    async fn noeta_trace_tree(&self, params: TraceParams) -> Result<TraceTreeResult> {
        let tree = {
            let store = self.store.lock().expect("document store poisoned");
            store.trace_tree(&params.uri, params.from.as_deref(), Encoding::Utf16)
        };
        Ok(match tree {
            Some(t) => TraceTreeResult {
                from: t.from,
                status: t.status.as_str().to_string(),
                truncated: t.truncated,
                boundaries: t
                    .boundaries
                    .into_iter()
                    .map(|b| TraceBoundaryWire {
                        role: b.role,
                        target: b.target,
                        loc: b.loc.map(TraceLocWire::from),
                    })
                    .collect(),
                roots: t.roots.into_iter().map(TraceNodeTreeWire::from).collect(),
            },
            None => TraceTreeResult {
                from: params.from,
                status: "ok".to_string(),
                truncated: false,
                boundaries: Vec::new(),
                roots: Vec::new(),
            },
        })
    }

    /// `noeta/architecture` (ide-ui U3): the workspace's role surface — role groups with their
    /// located bearers — for the Architecture tree view's top level.
    async fn noeta_architecture(&self, params: ArchitectureParams) -> Result<ArchitectureResult> {
        let roles = {
            let store = self.store.lock().expect("document store poisoned");
            store.architecture(&params.uri, Encoding::Utf16)
        };
        Ok(ArchitectureResult {
            roles: roles.map(|groups| {
                groups
                    .into_iter()
                    .map(|g| ArchRoleWire {
                        role: g.role,
                        bearers: g.bearers.into_iter().map(ArchNodeWire::from).collect(),
                    })
                    .collect()
            }),
        })
    }

    /// `noeta/architectureChildren` (ide-ui U3): one lazily-unfolded call level for a tree node.
    async fn noeta_architecture_children(
        &self,
        params: ArchChildrenParams,
    ) -> Result<ArchChildrenResult> {
        let children = {
            let store = self.store.lock().expect("document store poisoned");
            store.architecture_children(&params.uri, &params.function, Encoding::Utf16)
        };
        Ok(ArchChildrenResult {
            children: children.map(|nodes| nodes.into_iter().map(ArchNodeWire::from).collect()),
        })
    }

    /// `noeta/tests` (ide-ui U3): the file's `@test` fns — the runner's own discovery walk, so
    /// the editor's test explorer and `noeta test` can never disagree.
    async fn noeta_tests(&self, params: TestsParams) -> Result<TestsResult> {
        let tests = {
            let store = self.store.lock().expect("document store poisoned");
            store.tests(&params.uri, Encoding::Utf16)
        };
        Ok(TestsResult {
            tests: tests.map(|items| {
                items
                    .into_iter()
                    .map(|t| TestItemWire {
                        name: t.name,
                        display: t.display,
                        group: t.group,
                        skipped: t.skipped,
                        line: t.range.start.line,
                        character: t.range.start.character,
                        end_line: t.range.end.line,
                    })
                    .collect()
            }),
        })
    }

    /// `noeta/docs` (docs-browser slice 1): the documentation corpus roots — the docs browser's
    /// top level. Delegates to the unified model the MCP `docs` tools also serve.
    async fn noeta_docs(&self, params: DocsParams) -> Result<DocsResult> {
        let nodes = {
            let store = self.store.lock().expect("document store poisoned");
            store.doc_index(&params.uri)
        };
        Ok(DocsResult {
            nodes: Some(nodes.into_iter().map(DocNodeWire::from).collect()),
        })
    }

    /// `noeta/docsChildren`: one lazily-unfolded level of the docs tree under a node id.
    async fn noeta_docs_children(&self, params: DocsChildrenParams) -> Result<DocsResult> {
        let nodes = {
            let store = self.store.lock().expect("document store poisoned");
            store.doc_children(&params.uri, &params.id, Encoding::Utf16)
        };
        Ok(DocsResult {
            nodes: Some(nodes.into_iter().map(DocNodeWire::from).collect()),
        })
    }

    /// `noeta/docsPage`: the rendered page (signature + prose + location) for a node id.
    async fn noeta_docs_page(&self, params: DocsPageParams) -> Result<DocsPageResult> {
        let page = {
            let store = self.store.lock().expect("document store poisoned");
            store.doc_page(&params.uri, &params.id, Encoding::Utf16)
        };
        Ok(DocsPageResult {
            page: page.map(DocPageWire::from),
        })
    }

    /// `noeta/docsSearch`: ranked full-text search across the workspace's doc nodes.
    async fn noeta_docs_search(&self, params: DocsSearchParams) -> Result<DocsSearchResult> {
        let hits = {
            let store = self.store.lock().expect("document store poisoned");
            store.doc_search(&params.uri, &params.query, Encoding::Utf16)
        };
        Ok(DocsSearchResult {
            hits: Some(hits.into_iter().map(DocHitWire::from).collect()),
        })
    }

    /// `noeta/docsHighlight`: classify Noeta code snippets (a doc page's signature and fences)
    /// into colorable spans via the compiler's lexer (see [`noeta_ide::highlight`]). Stateless —
    /// pure over the snippet text, no document store involved.
    async fn noeta_docs_highlight(
        &self,
        params: DocsHighlightParams,
    ) -> Result<DocsHighlightResult> {
        let samples: Vec<noeta_ide::sample::Sample> = params
            .snippets
            .iter()
            .map(|code| noeta_ide::sample::split(code))
            .collect();
        let highlight = |code: &str| -> Vec<HlSpanWire> {
            noeta_ide::highlight::highlight_code(code)
                .into_iter()
                .map(HlSpanWire::from)
                .collect()
        };
        Ok(DocsHighlightResult {
            // Spans describe the VISIBLE text, which is what the viewer paints by default.
            spans: samples.iter().map(|s| highlight(&s.visible)).collect(),
            samples: samples
                .iter()
                .map(|s| SampleWire {
                    visible: s.visible.clone(),
                    full: s.full.clone(),
                    has_context: s.has_context,
                    full_spans: highlight(&s.full),
                })
                .collect(),
        })
    }

    /// `noeta/docsForSymbol`: the doc node documenting the symbol under the cursor — powers the
    /// editor's "show docs for symbol" command.
    async fn noeta_docs_for_symbol(
        &self,
        params: DocsForSymbolParams,
    ) -> Result<DocsForSymbolResult> {
        let id = {
            let store = self.store.lock().expect("document store poisoned");
            store.doc_for_symbol(
                &params.uri,
                noeta_ide::Position::new(params.line, params.character),
                Encoding::Utf16,
            )
        };
        Ok(DocsForSymbolResult {
            id: id.map(|d| d.0),
        })
    }

    /// Type-check `uri`'s current text and push its diagnostics to the client. The checker run —
    /// the most expensive query in the graph — goes through [`Self::read_latest`], so a newer
    /// edit cancels it instead of queueing behind it. Returns `false` when the run was superseded
    /// (nothing was published — the superseding edit republishes); `true` otherwise, including
    /// the no-op for a URI that is not open.
    async fn publish(&self, uri: Uri) -> bool {
        let uri_string = uri.as_str().to_string();
        let Some(collected) = self
            .read_latest(move |store| store.diagnostics(&uri_string))
            .await
        else {
            return false;
        };
        let Some((diags, text)) = collected else {
            return true;
        };
        let encoding = self.encoding();
        let index = LineIndex::new(&text);
        let lsp_diags: Vec<Diagnostic> = diags
            .iter()
            .map(|diag| to_lsp_diagnostic(&index, &uri, diag, encoding))
            .collect();
        self.client.publish_diagnostics(uri, lsp_diags, None).await;
        true
    }

    /// Republish diagnostics for every open document. Editing one file can change what a sibling
    /// that imports it sees, so a change re-publishes the whole open set (bounded by the number of
    /// open documents). A sweep superseded mid-way stops: the newer mutation runs its own full
    /// sweep, and finishing this one would interleave two document versions.
    async fn publish_all(&self) {
        let uris = {
            let store = self.store.lock().expect("document store poisoned");
            store.open_uris()
        };
        for uri in uris {
            if let Ok(uri) = uri.parse::<Uri>()
                && !self.publish(uri).await
            {
                return;
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
                // Inlay type hints (rust-analyzer style): inferred types after un-annotated
                // binding names. Labels are complete at production — no resolve round-trip.
                inlay_hint_provider: Some(OneOf::Left(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    // `(` opens a call, `,` moves to the next argument.
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                definition_provider: Some(OneOf::Left(true)),
                // Quick-fixes. The only kind offered today is a misspelled `@`-directive rewritten
                // to the nearest real one, so the client is told exactly that — an editor that
                // filters by kind should not round-trip for anything else.
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
                        work_done_progress_options: WorkDoneProgressOptions::default(),
                        resolve_provider: Some(false),
                    },
                )),
                references_provider: Some(OneOf::Left(true)),
                // `prepareProvider` lets the editor validate the cursor is on a renameable symbol
                // and pre-select the old name before showing its rename box.
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                // Whole-document formatting via the shared `noeta fmt` engine.
                document_formatting_provider: Some(OneOf::Left(true)),
                // Range ("Format Selection") formatting over the same engine.
                document_range_formatting_provider: Some(OneOf::Left(true)),
                // On-type formatting: reformat the just-closed block when the user types `}`.
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: "}".to_string(),
                    more_trigger_character: None,
                }),
                // Compiler-accurate identifier highlighting, overlaid on the client's static grammar.
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions::default(),
                            legend: SemanticTokensLegend {
                                // One source of truth with the classifier's `SemKind` indices.
                                token_types: semtokens::LEGEND
                                    .iter()
                                    .map(|name| SemanticTokenType::new(name))
                                    .collect(),
                                token_modifiers: Vec::new(),
                            },
                            range: None,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                // Completion: invoked explicitly, as the user types a name, or on `.` — the trigger
                // that fires member completion at a bare receiver dot (`c.`).
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), "@".to_string()]),
                    ..Default::default()
                }),
                // Call hierarchy (ide-ui U1) over the shared static call graph — the same
                // engine the MCP `trace` tool reads; items carry `@role` bindings in the detail.
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                // Role CodeLenses (ide-ui U2): one lens per `@role` binding in the file; a
                // traceable one carries the client's `noeta.showTrace` command. Lenses are
                // complete at production — no resolve round-trip.
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
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

    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let encoding = self.encoding();
        // Fetch the fresh hints and the parse-clean signal under one lock, so they describe the same
        // document revision.
        let (raw, clean) = {
            let store = self.store.lock().expect("document store poisoned");
            (
                store.inlay_hints(uri.as_str(), ide_range(params.range), encoding),
                store.entry_parses_cleanly(uri.as_str()),
            )
        };
        let key = uri.as_str();
        let Some(raw) = raw else {
            // Unknown document — no hints, and drop any cache we held for it.
            self.inlay_cache
                .lock()
                .expect("inlay cache poisoned")
                .remove(key);
            return Ok(None);
        };
        let hints: Vec<InlayHint> = raw
            .into_iter()
            .map(|(position, label, kind)| InlayHint {
                position: wire_position(position),
                label: InlayHintLabel::String(label),
                kind: Some(match kind {
                    inlay::HintKind::Type => InlayHintKind::TYPE,
                    inlay::HintKind::Parameter => InlayHintKind::PARAMETER,
                }),
                text_edits: None,
                tooltip: None,
                // A type label starts `: ` glued to the name it follows; a parameter label
                // `n:` precedes its argument. Neither wants leading padding; both want a
                // space on the right.
                padding_left: Some(false),
                padding_right: Some(true),
                data: None,
            })
            .collect();
        let mut cache = self.inlay_cache.lock().expect("inlay cache poisoned");
        if clean {
            // Authoritative: the buffer parsed, so this set is correct even when empty (a fully
            // annotated file genuinely has no hints) — it supersedes any stale entry.
            cache.insert(key.to_string(), hints.clone());
            Ok(Some(hints))
        } else {
            // The buffer is momentarily unparseable (typing `p.` before the member name exists),
            // which collapses the inferred types. Keep showing the last good hints so the inline
            // types don't flicker; fall back to the (empty) fresh set only if we have none cached.
            Ok(Some(cache.get(key).cloned().unwrap_or(hints)))
        }
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let fixes = {
            let store = self.store.lock().expect("document store poisoned");
            store.code_actions(uri.as_str(), ide_range(params.range), self.encoding())
        };
        let Some(fixes) = fixes else {
            return Ok(None);
        };
        let actions: CodeActionResponse = fixes
            .into_iter()
            .map(|(title, range, new_text)| {
                let edit = TextEdit {
                    range: wire_range(range),
                    new_text,
                };
                CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::QUICKFIX),
                    edit: Some(WorkspaceEdit {
                        changes: Some([(uri.clone(), vec![edit])].into_iter().collect()),
                        ..WorkspaceEdit::default()
                    }),
                    ..CodeAction::default()
                })
            })
            .collect();
        Ok((!actions.is_empty()).then_some(actions))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let position = ide_position(position_params.position);
        let (found, doc, tier, directive, use_stmt, namespace, signature, type_def) = {
            let store = self.store.lock().expect("document store poisoned");
            (
                store.hover_type(uri.as_str(), position, self.encoding()),
                store.hover_doc(uri.as_str(), position, self.encoding()),
                store.hover_tier(uri.as_str(), position, self.encoding()),
                store.hover_directive(uri.as_str(), position, self.encoding()),
                store.hover_use(uri.as_str(), position, self.encoding()),
                store.hover_namespace(uri.as_str(), position, self.encoding()),
                store.hover_signature(uri.as_str(), position, self.encoding()),
                store.hover_type_definition(uri.as_str(), position, self.encoding()),
            )
        };
        let markdown = |value: String| {
            HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            })
        };
        // Hovering an embedded-language block's tier name (`@sql { … }`) describes its body — the
        // declared language and, for an expression tier, its value type — read from the tier
        // registry (program + extension declarations alike). Takes precedence over the type hover
        // because the cursor is on the tier name, not a typed sub-expression.
        if let Some((descriptor, range)) = tier {
            return Ok(Some(Hover {
                contents: markdown(descriptor),
                range: Some(wire_range(range)),
            }));
        }
        // A decorator directive (`@attribute`, `@role`, `@semantic`, `@packed`, `@derive`) —
        // described in place; the tier directives are the tier hover's.
        if let Some((value, range)) = directive {
            return Ok(Some(Hover {
                contents: markdown(value),
                range: Some(wire_range(range)),
            }));
        }
        // Any element of a `use` statement — imported items (with their signature/definition and
        // doc prose) and module path segments. No other hover fires inside a `use`.
        if let Some((value, range)) = use_stmt {
            return Ok(Some(Hover {
                contents: markdown(value),
                range: Some(wire_range(range)),
            }));
        }
        // A namespace-group name (`http` from `use std.http`) has no typed expression, so describe
        // the group and its members here (module-namespaces).
        if let Some((descriptor, range)) = namespace {
            return Ok(Some(Hover {
                contents: markdown(descriptor),
                range: Some(wire_range(range)),
            }));
        }
        // Hovering a callable's *name* shows its declaration (`fn manhattan(): int`) plus any doc —
        // ahead of the type hover, whose tightest span at a call is the result type alone (`int`).
        if let Some((sig, range)) = signature {
            let mut value = format!("```noeta\n{sig}\n```");
            if let Some(doc) = doc {
                value.push_str("\n\n---\n\n");
                value.push_str(&doc);
            }
            return Ok(Some(Hover {
                contents: markdown(value),
                range: Some(wire_range(range)),
            }));
        }
        // Hovering a type name (`Point`) shows its declaration — fields/variants and method
        // signatures — ahead of the type hover, which would otherwise report just the nominal name.
        if let Some((def, range)) = type_def {
            let mut value = format!("```noeta\n{def}\n```");
            if let Some(doc) = doc {
                value.push_str("\n\n---\n\n");
                value.push_str(&doc);
            }
            return Ok(Some(Hover {
                contents: markdown(value),
                range: Some(wire_range(range)),
            }));
        }
        Ok(match (found, doc) {
            // `TypeRepr` displays as its Noeta surface spelling (`impl Display` in
            // `noeta_ast::reflect`) — the same rendering the debugger's Variables view uses.
            // A non-default storage fact (`@packed` / flat list) follows as a plain line, and
            // the declaration's attached `@doc` prose (already Markdown) follows after a rule.
            (Some((repr, note, range)), doc) => {
                let mut value = match note {
                    Some(note) => format!("```noeta\n{repr}\n```\n{note}"),
                    None => format!("```noeta\n{repr}\n```"),
                };
                if let Some(doc) = doc {
                    value.push_str("\n\n---\n\n");
                    value.push_str(&doc);
                }
                Some(Hover {
                    contents: markdown(value),
                    range: Some(wire_range(range)),
                })
            }
            // No typed expression under the cursor (e.g. the declaration's own name), but the
            // symbol has attached `@doc` prose — a doc-only hover.
            (None, Some(doc)) => Some(Hover {
                contents: markdown(doc),
                range: None,
            }),
            (None, None) => None,
        })
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let edits = {
            let store = self.store.lock().expect("document store poisoned");
            store.format_document(uri.as_str(), self.encoding())
        };
        Ok(edits.map(|edits| edits.into_iter().map(wire_text_edit).collect()))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let edits = {
            let store = self.store.lock().expect("document store poisoned");
            store.format_range(uri.as_str(), ide_range(params.range), self.encoding())
        };
        Ok(edits.map(|edits| edits.into_iter().map(wire_text_edit).collect()))
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let position = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri;
        let edits = {
            let store = self.store.lock().expect("document store poisoned");
            store.format_on_type(uri.as_str(), ide_position(position), self.encoding())
        };
        Ok(edits.map(|edits| edits.into_iter().map(wire_text_edit).collect()))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let target = {
            let store = self.store.lock().expect("document store poisoned");
            store.definition(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
            )
        };
        // The target may be a different file; parse its URI back for the `Location`.
        Ok(target.and_then(|(target_uri, range)| {
            target_uri.parse::<Uri>().ok().map(|uri| {
                GotoDefinitionResponse::Scalar(Location {
                    uri,
                    range: wire_range(range),
                })
            })
        }))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let found = {
            let store = self.store.lock().expect("document store poisoned");
            store.references(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
                params.context.include_declaration,
            )
        };
        Ok(found.map(|locations| {
            locations
                .into_iter()
                .filter_map(|(target_uri, range)| {
                    target_uri.parse::<Uri>().ok().map(|uri| Location {
                        uri,
                        range: wire_range(range),
                    })
                })
                .collect()
        }))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let new_name = params.new_name;
        let edits = {
            let store = self.store.lock().expect("document store poisoned");
            store.rename_edits(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
                &new_name,
            )
        };
        Ok(edits.map(|by_uri| {
            let changes = by_uri
                .into_iter()
                .filter_map(|(target_uri, ranges)| {
                    let uri = target_uri.parse::<Uri>().ok()?;
                    let text_edits = ranges
                        .into_iter()
                        .map(|range| TextEdit {
                            range: wire_range(range),
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
        let data = {
            let store = self.store.lock().expect("document store poisoned");
            store.signature_help(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
            )
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
        let range = {
            let store = self.store.lock().expect("document store poisoned");
            store.prepare_rename(uri.as_str(), ide_position(params.position), self.encoding())
        };
        Ok(range.map(|range| PrepareRenameResponse::Range(wire_range(range))))
    }

    /// Re-requested by the editor after every edit, so it is a prime stale-work source: computed
    /// off the lock via [`Self::read_latest`]; a superseded run answers `ContentModified` and the
    /// client re-requests against the new content (keeping its previous tokens meanwhile).
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri.as_str().to_string();
        let encoding = self.encoding();
        let data = self
            .read_latest(move |store| store.semantic_tokens(&uri, encoding))
            .await
            .ok_or(Error::new(ErrorCode::ContentModified))?;
        Ok(data.map(|data| {
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: data
                    .into_iter()
                    .map(|token| SemanticToken {
                        delta_line: token.delta_line,
                        delta_start: token.delta_start,
                        length: token.length,
                        token_type: token.token_type,
                        token_modifiers_bitset: token.token_modifiers_bitset,
                    })
                    .collect(),
            })
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let symbols = {
            let store = self.store.lock().expect("document store poisoned");
            store.document_symbols(uri.as_str(), self.encoding())
        };
        Ok(symbols.map(|symbols| {
            DocumentSymbolResponse::Nested(symbols.into_iter().map(to_document_symbol).collect())
        }))
    }

    /// Completion can re-check a munged buffer copy (the bare-dot form), making it expensive
    /// enough to cancel: computed off the lock via [`Self::read_latest`]; a superseded run
    /// answers `ContentModified` (the client's next keystroke re-triggers anyway).
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri.as_str().to_string();
        let position = ide_position(position_params.position);
        let encoding = self.encoding();
        let candidates = self
            .read_latest(move |store| store.completions(&uri, position, encoding))
            .await
            .ok_or(Error::new(ErrorCode::ContentModified))?;
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
            // The salsa input write inside `change` is also the cancellation trigger: any
            // in-flight snapshot read (a previous publish sweep, semantic tokens, completion)
            // unwinds at its next query boundary and this write proceeds.
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
        self.inlay_cache
            .lock()
            .expect("inlay cache poisoned")
            .remove(uri.as_str());
        // Clear any diagnostics the client is still showing for the now-closed document, then
        // refresh the rest (they may now be missing a module they imported).
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
        self.publish_all().await;
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        let lenses = {
            let store = self.store.lock().expect("document store poisoned");
            store.role_lenses(uri.as_str(), self.encoding())
        };
        Ok(lenses.map(|lenses| {
            lenses
                .into_iter()
                .map(|lens| to_code_lens(uri.as_str(), lens))
                .collect()
        }))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let item = {
            let store = self.store.lock().expect("document store poisoned");
            store.function_at(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
            )
        };
        Ok(item.and_then(|item| {
            let detail = hierarchy_detail(&item.roles, false);
            to_call_hierarchy_item(item, detail).map(|item| vec![item])
        }))
    }

    /// Expansion requests address the function by the **item** the client holds (its URI +
    /// selection range) — which may be a workspace file the user never opened; the store resolves
    /// it through the open workspace that discovered it.
    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let item = params.item;
        // The synthetic `(top level)` caller is an anchor, not a traversable function — its
        // selection range sits on a call site, which would re-resolve to the callee.
        if item.name == TOP_LEVEL {
            return Ok(Some(Vec::new()));
        }
        let calls = {
            let store = self.store.lock().expect("document store poisoned");
            store.incoming_calls(
                item.uri.as_str(),
                ide_position(item.selection_range.start),
                self.encoding(),
            )
        };
        Ok(calls.map(|calls| {
            calls
                .into_iter()
                .filter_map(to_hierarchy_call)
                .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
                .collect()
        }))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let item = params.item;
        if item.name == TOP_LEVEL {
            return Ok(Some(Vec::new()));
        }
        let calls = {
            let store = self.store.lock().expect("document store poisoned");
            store.outgoing_calls(
                item.uri.as_str(),
                ide_position(item.selection_range.start),
                self.encoding(),
            )
        };
        Ok(calls.map(|calls| {
            calls
                .into_iter()
                .filter_map(to_hierarchy_call)
                .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
                .collect()
        }))
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
    let (service, socket) = LspService::build(Backend::new)
        // The trace document (ide-ui U2) — a custom request, since LSP has no "render me a
        // read-only report" method; the VS Code extension opens the answer as `noeta-trace:`.
        .custom_method("noeta/trace", Backend::noeta_trace)
        .custom_method("noeta/traceTree", Backend::noeta_trace_tree)
        // The Architecture view + test explorer (ide-ui U3): the role surface, lazy call levels,
        // and `@test` discovery — all custom requests read by the VS Code extension.
        .custom_method("noeta/architecture", Backend::noeta_architecture)
        .custom_method(
            "noeta/architectureChildren",
            Backend::noeta_architecture_children,
        )
        .custom_method("noeta/tests", Backend::noeta_tests)
        // The docs browser (docs-browser slice 1): the doc tree, page bodies, search, and
        // "docs for the symbol under the cursor" — thin adapters over the unified doc model.
        .custom_method("noeta/docs", Backend::noeta_docs)
        .custom_method("noeta/docsChildren", Backend::noeta_docs_children)
        .custom_method("noeta/docsPage", Backend::noeta_docs_page)
        .custom_method("noeta/docsSearch", Backend::noeta_docs_search)
        .custom_method("noeta/docsHighlight", Backend::noeta_docs_highlight)
        .custom_method("noeta/docsForSymbol", Backend::noeta_docs_for_symbol)
        .finish();
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests are their own assembling driver (audit-6 F2): seed the std units before the
    /// document store's first check.
    fn store() -> DocumentStore {
        noeta_stdlib::registry::default_seeded();
        DocumentStore::default()
    }

    #[test]
    fn diagnostic_maps_to_lsp_wire_form() {
        let mut store = store();
        // A binding whose value violates its annotation — a check-stage mismatch (E0007).
        store.open("file:///bad.noe", "count: int = \"lots\"".to_string());
        let (diags, text) = store.diagnostics("file:///bad.noe").unwrap();
        assert_eq!(diags.len(), 1);

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
    fn positions_and_ranges_round_trip_between_wire_and_engine() {
        let p = Position::new(3, 7);
        assert_eq!(wire_position(ide_position(p)), p);
        let r = Range::new(Position::new(0, 1), Position::new(2, 4));
        assert_eq!(wire_range(ide_range(r)), r);
    }

    #[test]
    fn call_hierarchy_maps_to_wire_items_with_roles_in_detail() {
        let mut store = store();
        store.open(
            "file:///h.noe",
            "@attribute\n@role(Semantic.EntryPoint)\nstruct Route { path: string }\n\n#[Route(\"/x\")]\nfn handle(): int { return work() }\nfn work(): int { return 1 }\necho handle()\n"
                .to_string(),
        );
        // Prepare on `handle`'s name (line 5) → a wire item with the role in the detail.
        let item = store
            .function_at(
                "file:///h.noe",
                noeta_ide::Position::new(5, 4),
                Encoding::Utf8,
            )
            .expect("cursor on a fn name");
        let detail = hierarchy_detail(&item.roles, false);
        let wire = to_call_hierarchy_item(item, detail).expect("uri parses");
        assert_eq!(wire.name, "handle");
        assert_eq!(wire.kind, SymbolKind::FUNCTION);
        assert_eq!(wire.detail.as_deref(), Some("Semantic.EntryPoint"));
        assert_eq!(wire.selection_range.start.line, 5);

        // Outgoing from handle → work, sites in handle's document (fromRanges).
        let outgoing = store
            .outgoing_calls(
                "file:///h.noe",
                noeta_ide::Position::new(5, 4),
                Encoding::Utf8,
            )
            .expect("handle resolves");
        let (to, from_ranges) = to_hierarchy_call(outgoing.into_iter().next().unwrap()).unwrap();
        assert_eq!(to.name, "work");
        assert_eq!(to.detail, None, "no roles, syntactic call — no detail line");
        assert_eq!(from_ranges.len(), 1);
        assert_eq!(from_ranges[0].start.line, 5);
    }

    #[test]
    fn reference_only_groups_are_marked_in_the_detail() {
        assert_eq!(
            hierarchy_detail(&["Semantic.EntryPoint".to_string()], true).as_deref(),
            Some("Semantic.EntryPoint · reference (passed as value)")
        );
        assert_eq!(
            hierarchy_detail(&[], true).as_deref(),
            Some("reference (passed as value)")
        );
        assert_eq!(hierarchy_detail(&[], false), None);
    }

    #[test]
    fn role_lenses_map_to_wire_code_lenses() {
        let mut store = store();
        store.open(
            "file:///l.noe",
            "@attribute\n@role(Semantic.EntryPoint)\nstruct Route { path: string }\n\n#[Route(\"/x\")]\nfn handle(): int { return 1 }\n"
                .to_string(),
        );
        let lenses = store
            .role_lenses("file:///l.noe", Encoding::Utf8)
            .expect("open document");
        assert_eq!(lenses.len(), 1);
        let wire = to_code_lens("file:///l.noe", lenses[0].clone());
        let command = wire.command.expect("traceable lens carries the command");
        assert_eq!(command.title, "⚑ Semantic.EntryPoint · trace call paths");
        assert_eq!(command.command, "noeta.showTrace");
        assert_eq!(
            command.arguments.as_deref(),
            Some(
                &[
                    serde_json::Value::String("file:///l.noe".into()),
                    serde_json::Value::String("handle".into()),
                ][..]
            )
        );
        assert_eq!(wire.range.start.line, 5, "lens hangs on the fn name");
    }

    #[test]
    fn document_symbols_map_to_nested_wire_symbols() {
        let mut store = store();
        store.open(
            "file:///s.noe",
            "struct Point { x: int; y: int }\n".to_string(),
        );
        let symbols = store
            .document_symbols("file:///s.noe", Encoding::Utf8)
            .expect("open document has an outline");
        let wire: Vec<DocumentSymbol> = symbols.into_iter().map(to_document_symbol).collect();
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].name, "Point");
        assert_eq!(wire[0].kind, SymbolKind::STRUCT);
        let children = wire[0].children.as_ref().expect("fields nest");
        assert!(children.iter().any(|c| c.kind == SymbolKind::FIELD));
    }
}
