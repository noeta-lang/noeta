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

use noeta_diagnostics::{Diagnostic, Severity};
use noeta_span::{Source, SourceId};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The always-on orientation shipped in the MCP `instructions` field — cheap, and it
/// disproportionately raises an agent's first-shot correctness on a language it has never seen.
const INSTRUCTIONS: &str = "\
Noeta is an inferred-static typed language (file extension `.noe`). This server exposes the real \
compiler as tools — every answer is ground truth, not a guess.

Workflow:
- Run `check` on any Noeta code before claiming it compiles. It reports typed diagnostics with \
stable `E0xxx` codes, severities, source spans (file + 1-based line/column), and help text.
- Pass code inline via `source`, or point at a file on disk via `file` (sibling `.noe` modules in \
its directory are resolved so imports type-check).
- Do not invent syntax or standard-library calls; when unsure, `check` a small snippet first.";

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
}

#[tool_handler]
impl ServerHandler for NoetaMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
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
