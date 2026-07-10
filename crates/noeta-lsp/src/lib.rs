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
//! The [`Backend`] holds the store behind a `Mutex`; request handlers lock it, do their
//! (synchronous, fast) salsa work, and release it before awaiting any client I/O.

use std::sync::Mutex;

use noeta_ide::{DocumentStore, Encoding, LineIndex, TOP_LEVEL, completion, inlay, semtokens};
use tower_lsp_server::jsonrpc::Result;
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

    fn encoding(&self) -> Encoding {
        *self.encoding.lock().expect("encoding lock poisoned")
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
        let encoding = self.encoding();
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
                    trigger_characters: Some(vec![".".to_string()]),
                    ..Default::default()
                }),
                // Call hierarchy (ide-ui U1) over the shared static call graph — the same
                // engine the MCP `trace` tool reads; items carry `@role` bindings in the detail.
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
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
        let hints = {
            let store = self.store.lock().expect("document store poisoned");
            store.inlay_hints(uri.as_str(), ide_range(params.range), encoding)
        };
        Ok(hints.map(|hints| {
            hints
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
                .collect()
        }))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position_params = params.text_document_position_params;
        let uri = position_params.text_document.uri;
        let found = {
            let store = self.store.lock().expect("document store poisoned");
            store.hover_type(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
            )
        };
        Ok(found.map(|(repr, note, range)| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                // `TypeRepr` displays as its Noeta surface spelling (`impl Display` in
                // `noeta_ast::reflect`) — the same rendering the debugger's Variables view uses.
                // A non-default storage fact (`@packed` / flat list) follows as a plain line.
                value: match note {
                    Some(note) => format!("```noeta\n{repr}\n```\n{note}"),
                    None => format!("```noeta\n{repr}\n```"),
                },
            }),
            range: Some(wire_range(range)),
        }))
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

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let data = {
            let store = self.store.lock().expect("document store poisoned");
            store.semantic_tokens(uri.as_str(), self.encoding())
        };
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

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position_params = params.text_document_position;
        let uri = position_params.text_document.uri;
        let candidates = {
            let store = self.store.lock().expect("document store poisoned");
            store.completions(
                uri.as_str(),
                ide_position(position_params.position),
                self.encoding(),
            )
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
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_maps_to_lsp_wire_form() {
        let mut store = DocumentStore::default();
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
        let mut store = DocumentStore::default();
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
    fn document_symbols_map_to_nested_wire_symbols() {
        let mut store = DocumentStore::default();
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
