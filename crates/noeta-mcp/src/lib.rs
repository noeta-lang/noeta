//! `noeta mcp` — the Model Context Protocol server: Noeta's agent-native tooling adapter.
//!
//! This is the third leg of the editor-tooling story. Where `noeta lsp` is a *read* adapter over
//! the compiler's salsa query graph (for a human at a cursor) and `noeta dap` is a *control*
//! adapter over the running VM (for a human debug UI), `noeta mcp` is the adapter for an **AI
//! agent** — a consumer that addresses code by name/snippet, has ~zero Noeta in its training data,
//! and lives in a tight "does this compile, what's wrong, what does `E0007` mean" loop.
//!
//! M0 stands up the server skeleton (MCP handshake + capability/instructions advertisement over
//! stdio, via the official `rmcp` SDK) and the single highest-value tool: [`check`], which runs a
//! program through the same whole-workspace `linked_checked` query the LSP reads and returns the
//! typed diagnostics (`E0xxx` code, severity, span → file/line/col, message, labels) as structured
//! content the agent can act on. Later slices add the Ground / Understand / Introspect / Execute
//! pillars (see `plans/mcp/README.md`).

mod corpus;

use noeta_diagnostics::{Diagnostic, DiagnosticCode, Severity};
use noeta_span::{Source, SourceId};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    Implementation, ListResourcesResult, PaginatedRequestParams, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The always-on orientation shipped in the MCP `instructions` field — cheap, and it
/// disproportionately raises an agent's first-shot correctness on a language it has never seen.
const INSTRUCTIONS: &str = "\
Noeta is an inferred-static typed language (file extension `.noe`). This server exposes the real \
compiler and its documentation as tools — every answer is ground truth, not a guess. You almost \
certainly have little Noeta in your training data, so ground yourself before writing it.

Tools:
- `docs_search` / `docs_get` — search and read the language documentation. Start here for any \
unfamiliar syntax or feature.
- `examples_find` — find real, CI-tested example programs by feature, concept, or diagnostic code. \
Copy the idioms rather than inventing them.
- `check` — type-check Noeta code and get its diagnostics (stable `E0xxx` codes, severities, source \
spans, help). Pass code inline via `source`, or a path via `file` (sibling `.noe` modules are \
resolved so imports type-check). Run this before claiming any Noeta code compiles.
- `explain_diagnostic` — when `check` returns an `E0xxx` code, look up what it means and see the \
real programs that trigger and fix it.

Do not invent syntax or standard-library calls; search the docs/examples, then `check` a snippet.";

/// A source span rendered for an agent: the raw byte offsets plus the resolved file and 1-based
/// line/column of the span's start (what an agent needs to point a human — or itself — at the code).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SpanOut {
    /// The file the span is in (the entry's display path, a sibling module's path, or `<inline>`).
    pub file: String,
    /// 1-based line of the span's start.
    pub line: u32,
    /// 1-based column of the span's start (counted in Unicode scalar values).
    pub column: u32,
    /// Start byte offset within the file.
    pub start: u32,
    /// End byte offset within the file (exclusive).
    pub end: u32,
}

/// A secondary label attached to a diagnostic (a span + a message).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LabelOut {
    pub span: SpanOut,
    pub message: String,
}

/// One diagnostic, flattened for an agent.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DiagnosticOut {
    /// The stable diagnostic code, e.g. `E0007`.
    pub code: String,
    /// `error`, `warning`, or `note`.
    pub severity: String,
    /// The headline message.
    pub message: String,
    /// The primary span.
    pub span: SpanOut,
    /// Any secondary labels.
    pub labels: Vec<LabelOut>,
    /// An optional help/suggestion line.
    pub help: Option<String>,
}

/// The `check` tool's result: whether the program is error-free, and every diagnostic it produced.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CheckOutput {
    /// True when there are no `error`-severity diagnostics (warnings/notes may still be present).
    pub ok: bool,
    /// Every diagnostic, in the order the checker produced them.
    pub diagnostics: Vec<DiagnosticOut>,
}

/// Arguments to `check`: inline `source` OR a `file` path (exactly one).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CheckArgs {
    /// Inline Noeta source to check. Provide this or `file`.
    #[serde(default)]
    pub source: Option<String>,
    /// Path to a `.noe` file to check. Sibling `.noe` files in its directory are resolved as
    /// modules so `use` imports type-check. Provide this or `source`.
    #[serde(default)]
    pub file: Option<String>,
}

/// Arguments to `docs_search`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DocsSearchArgs {
    /// What to look for, e.g. "pattern matching", "how do generics bound a type".
    pub query: String,
    /// Max hits to return (default 5).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The `docs_search` result. Wrapped in an object because MCP tool output schemas must have an
/// object root (a bare array is rejected).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocsSearchOutput {
    pub hits: Vec<DocHitOut>,
}

/// One `docs_search` hit: the page + section that matched, with a short snippet.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocHitOut {
    /// The page slug — pass to `docs_get` to read the whole page.
    pub page: String,
    pub title: String,
    pub heading: String,
    /// A GitHub-style heading anchor, so the section can be cited as `page#anchor`.
    pub anchor: String,
    pub snippet: String,
}

/// Arguments to `docs_get`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DocsGetArgs {
    /// The page slug or title (case-insensitive; a substring works, e.g. `types` → `Type-System`).
    pub page: String,
}

/// The `docs_get` result: the page's full markdown, or the index if the page was not found.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DocGetOut {
    /// True when `page` resolved; false when `available` lists what exists instead.
    pub found: bool,
    /// The full markdown (empty when not found).
    pub markdown: String,
    /// When not found, the `(slug, title)` of every page so the agent can retry.
    pub available: Vec<[String; 2]>,
}

/// Arguments to `examples_find`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExamplesFindArgs {
    /// What to look for: a feature ("generics", "async"), a concept, or a diagnostic code.
    pub query: String,
    /// Max examples to return (default 5).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The `examples_find` result (object-wrapped for the same MCP schema reason as `DocsSearchOutput`).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExamplesFindOutput {
    pub examples: Vec<ExampleOut>,
}

/// One `examples_find` / `explain_diagnostic` example: a real, CI-tested `.noe` program.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExampleOut {
    /// The feature directory, e.g. `generics`.
    pub feature: String,
    /// The example name, e.g. `bounded`.
    pub name: String,
    /// The case's own one-line description (its header comment).
    pub description: String,
    /// The full Noeta source.
    pub code: String,
    /// Any `E0xxx` diagnostics this example is expected to raise (empty for a passing example).
    pub expects: Vec<String>,
}

/// Arguments to `explain_diagnostic`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ExplainArgs {
    /// The diagnostic code to explain, e.g. `E0007` (case-insensitive; a bare `7` also resolves).
    pub code: String,
}

/// The `explain_diagnostic` result: what the code means plus the real programs that trigger it.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExplainOut {
    /// The canonical code, e.g. `E0007`.
    pub code: String,
    /// A human title derived from the diagnostic's name, e.g. `Type Mismatch`.
    pub title: String,
    /// Whether `code` is a known diagnostic.
    pub known: bool,
    /// Real, CI-tested example programs that raise this diagnostic (with their descriptions).
    pub examples: Vec<ExampleOut>,
    /// Documentation pages that mention this code, as `[slug, title]`.
    pub docs: Vec<[String; 2]>,
}

/// The MCP server. Stateless in M0 — each `check` runs on a fresh salsa database (a later slice
/// will hold one `LangDatabase` across calls for incrementality). The `#[tool_router]` macro
/// generates the tool table as an associated function, so no per-instance state is needed.
#[derive(Clone, Debug, Default)]
pub struct NoetaMcp;

impl NoetaMcp {
    pub fn new() -> Self {
        Self
    }
}

#[tool_router]
impl NoetaMcp {
    /// Type-check Noeta code and return its diagnostics — the agent's compile feedback loop.
    #[tool(
        description = "Type-check Noeta (.noe) code and return its diagnostics (stable E0xxx code, \
severity, source span, message, help). Provide `source` (inline) or `file` (a path; sibling .noe \
modules are resolved so imports check). Run this before claiming Noeta code compiles."
    )]
    async fn check(
        &self,
        Parameters(args): Parameters<CheckArgs>,
    ) -> Result<Json<CheckOutput>, ErrorData> {
        let sources = resolve_sources(&args)?;
        Ok(Json(run_check(&sources)))
    }

    /// Search the Noeta documentation — the first stop before writing unfamiliar Noeta.
    #[tool(
        description = "Search the Noeta language documentation for a concept, syntax, or feature. \
Returns ranked page sections with snippets; pass a `page` slug to `docs_get` to read the full \
page. Use this to ground yourself before writing Noeta — do not guess syntax."
    )]
    async fn docs_search(
        &self,
        Parameters(args): Parameters<DocsSearchArgs>,
    ) -> Json<DocsSearchOutput> {
        let limit = args.limit.unwrap_or(5).clamp(1, 25);
        let hits = corpus::search_docs(&args.query, limit)
            .into_iter()
            .map(|h| DocHitOut {
                page: h.page,
                title: h.title,
                heading: h.heading,
                anchor: h.anchor,
                snippet: h.snippet,
            })
            .collect();
        Json(DocsSearchOutput { hits })
    }

    /// Fetch a full documentation page by slug or title.
    #[tool(
        description = "Fetch the full markdown of a Noeta documentation page by slug or title (e.g. \
`Type-System`, `types`, `Pattern Matching`). If not found, returns the index of available pages."
    )]
    async fn docs_get(&self, Parameters(args): Parameters<DocsGetArgs>) -> Json<DocGetOut> {
        match corpus::get_doc(&args.page) {
            Some(markdown) => Json(DocGetOut {
                found: true,
                markdown: markdown.to_string(),
                available: Vec::new(),
            }),
            None => Json(DocGetOut {
                found: false,
                markdown: String::new(),
                available: corpus::doc_index()
                    .into_iter()
                    .map(|(slug, title)| [slug, title])
                    .collect(),
            }),
        }
    }

    /// Find real, runnable example programs for a feature or concept.
    #[tool(
        description = "Find real, CI-tested Noeta example programs by feature, concept, or \
diagnostic code (e.g. `generics`, `pattern matching`, `E0007`). Returns full source with a \
description — copy the idioms rather than inventing them."
    )]
    async fn examples_find(
        &self,
        Parameters(args): Parameters<ExamplesFindArgs>,
    ) -> Json<ExamplesFindOutput> {
        let limit = args.limit.unwrap_or(5).clamp(1, 25);
        let examples = corpus::search_examples(&args.query, limit)
            .into_iter()
            .map(example_out)
            .collect();
        Json(ExamplesFindOutput { examples })
    }

    /// Explain a diagnostic code with real programs that trigger it.
    #[tool(
        description = "Explain a Noeta diagnostic code (e.g. `E0007`): its name, the real CI-tested \
example programs that raise it (so you can see the cause and the fix), and the docs that cover it. \
Call this whenever `check` returns a code you want to resolve."
    )]
    async fn explain_diagnostic(
        &self,
        Parameters(args): Parameters<ExplainArgs>,
    ) -> Json<ExplainOut> {
        let code = normalize_code(&args.code);
        let (title, known) = match diagnostic_title(&code) {
            Some(t) => (t, true),
            None => (String::new(), false),
        };
        Json(ExplainOut {
            title,
            known,
            // Cap the examples so `explain_diagnostic` stays token-frugal — a common code can appear
            // in dozens of cases; the `diagnostics/`-dir canonical repros sort first.
            examples: corpus::examples_for_code(&code)
                .into_iter()
                .take(6)
                .map(example_out)
                .collect(),
            docs: corpus::docs_mentioning(&code)
                .into_iter()
                .map(|(slug, title)| [slug, title])
                .collect(),
            code,
        })
    }
}

#[tool_handler]
impl ServerHandler for NoetaMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }

    /// List the documentation pages as browsable resources (`noeta-doc://<slug>`). Examples are not
    /// listed (there are hundreds) but remain readable by URI (`noeta-example://<feature>/<name>`)
    /// once surfaced by `examples_find`.
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let resources = corpus::doc_index()
            .into_iter()
            .map(|(slug, title)| {
                Resource::new(format!("noeta-doc://{slug}"), slug)
                    .with_title(title)
                    .with_mime_type("text/markdown")
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    /// Read a `noeta-doc://<slug>` page or a `noeta-example://<feature>/<name>` program.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let uri = request.uri;
        let contents = if let Some(slug) = uri.strip_prefix("noeta-doc://") {
            corpus::get_doc(slug).map(|md| ResourceContents::text(md, uri.clone()))
        } else if let Some(rest) = uri.strip_prefix("noeta-example://") {
            rest.split_once('/').and_then(|(feature, name)| {
                corpus::get_example(feature, name)
                    .map(|src| ResourceContents::text(src, uri.clone()))
            })
        } else {
            return Err(ErrorData::invalid_params(
                format!("unknown resource scheme: {uri}"),
                None,
            ));
        };
        match contents {
            Some(c) => Ok(ReadResourceResult::new(vec![c])),
            None => Err(ErrorData::resource_not_found(
                format!("no such resource: {uri}"),
                None,
            )),
        }
    }
}

/// Turn `check`'s arguments into the ordered source list (entry first, then sibling modules) that
/// the salsa `Workspace` is built from. Inline `source` is a lone entry; a `file` pulls in siblings.
fn resolve_sources(args: &CheckArgs) -> Result<Vec<Source>, ErrorData> {
    match (&args.source, &args.file) {
        (Some(text), None) => Ok(vec![Source::new(
            SourceId::FIRST,
            "<inline>".to_string(),
            text.clone(),
        )]),
        (None, Some(path)) => {
            let raw = noeta_loader::read_workspace(Path::new(path))
                .map_err(|e| ErrorData::invalid_params(format!("cannot read {path}: {e}"), None))?;
            let mut sources = Vec::with_capacity(raw.modules.len() + 1);
            sources.push(raw.entry);
            sources.extend(raw.modules);
            Ok(sources)
        }
        (Some(_), Some(_)) => Err(ErrorData::invalid_params(
            "provide either `source` or `file`, not both",
            None,
        )),
        (None, None) => Err(ErrorData::invalid_params(
            "provide `source` (inline code) or `file` (a path)",
            None,
        )),
    }
}

/// Run the whole-workspace check over `sources` (entry at index 0) and marshal the diagnostics.
/// Uses a fresh `LangDatabase` — the memoization is per call in M0.
fn run_check(sources: &[Source]) -> CheckOutput {
    let db = noeta_db::LangDatabase::default();
    let (entry, modules) = sources
        .split_first()
        .expect("resolve_sources always yields at least the entry");
    let ws = noeta_db::workspace(&db, entry, modules);
    let checked = noeta_db::linked_checked(&db, ws);
    let diagnostics: Vec<DiagnosticOut> = checked
        .diagnostics
        .iter()
        .map(|d| marshal_diagnostic(sources, d))
        .collect();
    let ok = !diagnostics.iter().any(|d| d.severity == "error");
    CheckOutput { ok, diagnostics }
}

fn marshal_diagnostic(sources: &[Source], diag: &Diagnostic) -> DiagnosticOut {
    DiagnosticOut {
        code: diag.code.code().to_string(),
        severity: severity_str(diag.severity).to_string(),
        message: diag.message.clone(),
        span: marshal_span(sources, diag.span),
        labels: diag
            .labels
            .iter()
            .map(|l| LabelOut {
                span: marshal_span(sources, l.span),
                message: l.message.clone(),
            })
            .collect(),
        help: diag.help.clone(),
    }
}

/// Resolve a span to `{file, line, column, start, end}` against the source it indexes. A span whose
/// `SourceId` is out of range (should not happen for checker output) degrades to line/column 0.
fn marshal_span(sources: &[Source], span: noeta_span::Span) -> SpanOut {
    match sources.get(span.source.0 as usize) {
        Some(src) => {
            let lc = src.line_col(span.start);
            SpanOut {
                file: src.name().to_string(),
                line: lc.line,
                column: lc.col,
                start: span.start,
                end: span.end,
            }
        }
        None => SpanOut {
            file: String::new(),
            line: 0,
            column: 0,
            start: span.start,
            end: span.end,
        },
    }
}

fn severity_str(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

fn example_out(h: corpus::ExampleHit) -> ExampleOut {
    ExampleOut {
        feature: h.feature,
        name: h.name,
        description: h.description,
        code: h.code,
        expects: h.codes,
    }
}

/// Canonicalize a diagnostic code the agent may have typed loosely: `e0007`, `E7`, or a bare `7`
/// all become `E0007`.
fn normalize_code(input: &str) -> String {
    let t = input.trim().to_uppercase();
    let digits = t.strip_prefix('E').unwrap_or(&t);
    if !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && let Ok(n) = digits.parse::<u32>()
    {
        return format!("E{n:04}");
    }
    t
}

/// The human title for a diagnostic code, derived from its `DiagnosticCode` variant name (the
/// single source of truth), e.g. `E0007` → `Type Mismatch`. `None` for an unknown code.
fn diagnostic_title(code: &str) -> Option<String> {
    DiagnosticCode::ALL
        .iter()
        .find(|c| c.code() == code)
        .map(|c| split_camel_case(&format!("{c:?}")))
}

/// `TypeMismatch` → `Type Mismatch`. A space is inserted before an uppercase letter that follows a
/// lowercase one or is followed by a lowercase one (so acronym runs like `IoError` read cleanly).
fn split_camel_case(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &ch) in chars.iter().enumerate() {
        if i > 0 && ch.is_uppercase() {
            let prev_lower = chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            if prev_lower || next_lower {
                out.push(' ');
            }
        }
        out.push(ch);
    }
    out
}

/// Run the MCP server over stdio, blocking until the client disconnects. Called by the `noeta mcp`
/// CLI subcommand. Builds a dedicated multi-threaded tokio runtime (the CLI's `main` is
/// synchronous), mirroring `noeta_lsp::run_stdio` / `noeta_dap::run_stdio`.
pub fn run_stdio() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the MCP-server tokio runtime");
    runtime.block_on(serve());
}

async fn serve() {
    let transport = rmcp::transport::stdio();
    match NoetaMcp::new().serve(transport).await {
        Ok(service) => {
            if let Err(err) = service.waiting().await {
                eprintln!("noeta mcp: server loop ended: {err}");
            }
        }
        Err(err) => eprintln!("noeta mcp: failed to start: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_source(text: &str) -> CheckOutput {
        run_check(&[Source::new(
            SourceId::FIRST,
            "<inline>".to_string(),
            text.to_string(),
        )])
    }

    #[test]
    fn clean_program_has_no_diagnostics() {
        let out = check_source("fn main(): int {\n  return 1;\n}\n");
        assert!(out.ok, "expected ok, got {:?}", out.diagnostics);
        assert!(out.diagnostics.is_empty());
    }

    #[test]
    fn type_error_is_reported_with_code_and_position() {
        // A type mismatch surfaces as an error-severity diagnostic with a stable code and a span
        // that resolves to a 1-based line/column in the inline source.
        let out = check_source("fn main(): int {\n  return \"x\";\n}\n");
        assert!(!out.ok, "expected an error, got a clean check");
        let err = out
            .diagnostics
            .iter()
            .find(|d| d.severity == "error")
            .expect("an error diagnostic");
        assert!(err.code.starts_with('E'), "code was {:?}", err.code);
        assert_eq!(err.span.file, "<inline>");
        assert!(err.span.line >= 1 && err.span.column >= 1);
    }

    #[test]
    fn missing_arguments_is_an_invalid_params_error() {
        let err = resolve_sources(&CheckArgs {
            source: None,
            file: None,
        })
        .unwrap_err();
        assert!(err.message.contains("source"));
    }

    #[test]
    fn server_advertises_instructions_and_tools() {
        let info = NoetaMcp::new().get_info();
        assert!(
            info.instructions
                .as_deref()
                .unwrap_or_default()
                .contains("check")
        );
        assert!(info.capabilities.tools.is_some());
    }

    /// The gate fixture: drive a real MCP session (client ⇄ server over an in-memory duplex) and
    /// assert the wire contract — `check` is listed, and calling it returns structured diagnostics.
    #[tokio::test]
    async fn round_trip_check_over_a_duplex() {
        use rmcp::model::CallToolRequestParams;

        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let server = tokio::spawn(async move {
            if let Ok(svc) = NoetaMcp::new().serve(server_io).await {
                let _ = svc.waiting().await;
            }
        });

        let client = ().serve(client_io).await.expect("client initializes");

        let tools = client
            .list_tools(Default::default())
            .await
            .expect("tools/list");
        assert!(
            tools.tools.iter().any(|t| t.name == "check"),
            "check tool advertised"
        );

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "source".to_string(),
            serde_json::Value::String("fn main(): int {\n  return \"x\";\n}\n".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "check".into();
        params.arguments = Some(arguments);
        let result = client.call_tool(params).await.expect("tools/call check");

        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["ok"], serde_json::json!(false));
        assert_eq!(
            structured["diagnostics"][0]["code"],
            serde_json::json!("E0007")
        );

        client.cancel().await.expect("client shuts down");
        server.abort();
    }
}
