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

mod analyze;
mod corpus;
mod debug;
mod execute;
mod format;
mod introspect;
mod navigate;
mod stdlib;
mod trace;
mod understand;

use noeta_diagnostics::{DiagnosticCode, JsonDiagnostic, to_json};
use noeta_span::{Source, SourceId, SourceMap};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, ListResourcesResult,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResult,
    Resource, ResourceContents, ServerCapabilities, ServerInfo,
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
- `stdlib_api` — list the real standard-library module/function/method signatures from the \
compiler's own registry. Call this before writing any `std.*` call — do not guess stdlib APIs.
- `check` — type-check Noeta code and get its diagnostics (stable `E0xxx` codes, severities, source \
spans, help). Pass code inline via `source`, or a path via `file` (sibling `.noe` modules are \
resolved so imports type-check). Run this before claiming any Noeta code compiles.
- `explain_diagnostic` — when `check` returns an `E0xxx` code, look up what it means and see the \
real programs that trigger and fix it.
- `type_at` / `symbols` — the inferred type at a symbol/position, and a file's declaration outline.
- `definition` / `references` / `completions` / `signature` — navigate code with the same engine \
the editor uses: where a symbol is declared (cross-file), every place it is used, what completes \
at a position, and the signature of the call under a position.
- `ast` / `bytecode` / `pipeline` / `module_graph` / `reflect` — inspect the compiler's artifacts: \
the syntax tree, the VM disassembly, a per-stage health summary, the `use` import graph (with \
per-module role summaries), and the `@role` architectural graph (with source locations).
- `trace` — unfold the static call path from a `@role` or function: `trace(from: \"EntryPoint\")` \
shows the full flow a request takes and every role boundary it crosses.
- `run` / `eval` / `test` — actually execute code: run a program (stdout/exit/traceback), evaluate \
an expression (value + type), or run its `@test` blocks. Sandboxed and deterministic by default \
(pass `real: true` for the real host); every run is bounded by liveness limits.
- `debug_start` / `debug_inspect` / `debug_step` / `debug_eval` / `debug_stop` — interactively \
debug a program: pause at entry or breakpoints, read the call stack and live locals, step by \
line, evaluate expressions in a paused frame, resume. A runaway resume pauses with reason \
`limit` instead of hanging. The ground-truth way to inspect live state at a line.
- `format` — format Noeta source into its canonical style.

Do not invent syntax or standard-library calls; search the docs/examples, then `check` a snippet. \
When you claim code works, `run` or `test` it — do not assert behavior you have not executed.";

/// The `check` tool's result. The per-diagnostic shape is `noeta_diagnostics::JsonDiagnostic` — the
/// *same* canonical form `noeta check --format json` emits — so an agent parses one schema whether it
/// calls this tool or the CLI. `ok` and the counts are the MCP-friendly summary on top.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CheckOutput {
    /// True when there are no `error`-severity diagnostics (warnings/notes may still be present).
    pub ok: bool,
    /// The number of error-severity diagnostics.
    pub errors: usize,
    /// The number of warning-severity diagnostics.
    pub warnings: usize,
    /// Every diagnostic, resolved to file + line/column + byte offsets, in checker order.
    pub diagnostics: Vec<JsonDiagnostic>,
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

/// A `check`-style source input shared by the M3 analysis tools: inline `source` OR a `file` path.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct AnalyzeArgs {
    /// Inline Noeta source to analyze. Provide this or `file`.
    #[serde(default)]
    pub source: Option<String>,
    /// Path to a `.noe` file to analyze. Sibling `.noe` modules are resolved. Provide this or `source`.
    #[serde(default)]
    pub file: Option<String>,
}

/// Arguments to `type_at`: a source, plus a site addressed by `symbol` name or `line`/`column`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct TypeAtArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// A symbol name to locate (its first whole-word occurrence in the entry file). Preferred over a
    /// position for an agent that has a name, not a cursor.
    #[serde(default)]
    pub symbol: Option<String>,
    /// 1-based line of the site (use with `column` when no `symbol` is given).
    #[serde(default)]
    pub line: Option<u32>,
    /// 1-based, UTF-8-byte column of the site.
    #[serde(default)]
    pub column: Option<u32>,
}

/// Arguments to `definition`/`references`: a source, plus a site addressed by `symbol` name or
/// `line`/`column` (same addressing as `type_at`).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct NavigateArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// A symbol name to locate (its first whole-word occurrence in the entry file). Preferred over a
    /// position for an agent that has a name, not a cursor.
    #[serde(default)]
    pub symbol: Option<String>,
    /// 1-based line of the site (use with `column` when no `symbol` is given).
    #[serde(default)]
    pub line: Option<u32>,
    /// 1-based, UTF-8-byte column of the site.
    #[serde(default)]
    pub column: Option<u32>,
    /// `references` only: include the declaration among the results (default true).
    #[serde(default)]
    pub include_declaration: Option<bool>,
}

/// Arguments to `completions`/`signature`: a source plus a required 1-based position.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct PositionArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// 1-based line of the position.
    pub line: u32,
    /// 1-based, UTF-8-byte column of the position (for completions after a `.`, the column just
    /// past the dot).
    pub column: u32,
}

/// Arguments to `doc_browse`: a source plus an optional node `id` to expand (roots when omitted).
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DocBrowseArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// The node id to expand (from a previous `doc_browse`). Omit to list the corpus roots.
    #[serde(default)]
    pub id: Option<String>,
}

/// Arguments to `doc_page`: a source plus the required node `id` to render.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DocPageArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// The node id to render (from `doc_browse`).
    pub id: String,
}

/// Arguments to `debug_start`: a source, optional breakpoints, and the host choice.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DebugStartArgs {
    /// Inline Noeta source to debug. Provide this or `file`.
    #[serde(default)]
    pub source: Option<String>,
    /// Path to a `.noe` file to debug (sibling modules resolve). Provide this or `source`.
    #[serde(default)]
    pub file: Option<String>,
    /// Breakpoints to arm before the run starts (1-based lines; entry file unless `file` is set
    /// on the breakpoint).
    #[serde(default)]
    pub breakpoints: Option<Vec<debug::BreakpointArg>>,
    /// Pause before the first instruction. Defaults to true when no breakpoints are given.
    #[serde(default)]
    pub stop_on_entry: Option<bool>,
    /// Run on the real host (real disk/env/network) instead of the deterministic sandbox.
    #[serde(default)]
    pub real: Option<bool>,
}

/// Arguments addressing an existing debug session.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DebugSessionArgs {
    /// The session id `debug_start` returned.
    pub session: u64,
}

/// Arguments to `debug_step`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DebugStepArgs {
    /// The session id `debug_start` returned.
    pub session: u64,
    /// `continue` (to the next breakpoint/limit/exit), or a line-granular `over` / `into` / `out`
    /// step. Defaults to `over`.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Arguments to `debug_eval`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct DebugEvalArgs {
    /// The session id `debug_start` returned.
    pub session: u64,
    /// The expression (or statements — a trailing bare expression is the value) to evaluate in
    /// the paused frame's scope.
    pub expr: String,
    /// Which stack frame's scope to evaluate in (index from the reported frames, innermost = 0).
    #[serde(default)]
    pub frame: Option<usize>,
}

/// Arguments to `trace`: a source, a starting role or function, and a depth.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct TraceArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// Where to start: a role (`EntryPoint` or `Semantic.EntryPoint` — traces from every function
    /// bearing it) or a function name (`handle`, `Counter.bump`). Omitted: every role-bearing
    /// function.
    #[serde(default)]
    pub from: Option<String>,
    /// How many call levels to unfold (default 6, max 16).
    #[serde(default)]
    pub max_depth: Option<usize>,
}

/// Arguments to `reflect`: a source, plus an optional architectural-role filter.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ReflectArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// Narrow to declarations bearing this role — the bare variant (`EntryPoint`) or qualified
    /// (`Semantic.EntryPoint`), case-insensitive. Omit for every role.
    #[serde(default)]
    pub role: Option<String>,
}

/// Arguments to `stdlib_api`.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct StdlibApiArgs {
    /// A module identity (`std.math`, or bare `math`; a prefix like `http` expands to
    /// `std.http.client`/`std.http.server`) or an extern type name (`Uuid`, `Response`) to narrow to.
    /// Omit to list the entire standard-library surface.
    #[serde(default)]
    pub module: Option<String>,
}

/// Arguments to `run`: a source, plus how to run it — real host, program args, and liveness limits.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RunArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// Program arguments (`args.all()`), for a `file` run against the real host. Ignored under the
    /// deterministic sandbox.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Run against the **real host** (real disk/env/network) instead of the deterministic sandbox
    /// (in-memory fs, logical clock, seeded random, pure network). Default false. Real effects are
    /// gated by your own tool approval.
    #[serde(default)]
    pub real: Option<bool>,
    /// Liveness limits (timeout / step budget / output cap). All optional and defaulted; always on.
    #[serde(default)]
    pub limits: Option<execute::RunLimits>,
}

/// Arguments to `eval`: an expression, optional prior `context`, and the real-host opt-in.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct EvalArgs {
    /// The Noeta expression to evaluate (e.g. `1 + 2`, `[1, 2, 3].map(fn(x) { return x * 2; })`).
    pub expr: String,
    /// Prior statements/bindings/definitions run before `expr` (e.g. `xs = [1, 2, 3];`), REPL-style.
    #[serde(default)]
    pub context: Option<String>,
    /// Evaluate against the real host instead of the sandbox. Default false.
    #[serde(default)]
    pub real: Option<bool>,
    /// Liveness limits (timeout / step budget). All optional and defaulted; always on — a runaway
    /// loop in `expr` or `context` trips the bound and returns with `limit_hit` set.
    #[serde(default)]
    pub limits: Option<execute::RunLimits>,
}

/// Arguments to `test`: a source with `@test` blocks, an optional filter, and the real-host opt-in.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct TestArgs {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
    /// Keep only tests whose name or `#[Group(...)]` contains this substring (case-insensitive).
    #[serde(default)]
    pub filter: Option<String>,
    /// Run the tests against the real host instead of the sandbox. Default false.
    #[serde(default)]
    pub real: Option<bool>,
    /// Liveness limits (timeout / step budget) applied per case. All optional and defaulted; always
    /// on — a runaway loop in one case fails that case (with `limit_hit`) instead of hanging.
    #[serde(default)]
    pub limits: Option<execute::RunLimits>,
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

/// The MCP server. Analysis tools are stateless — each runs on a fresh salsa database (a later
/// slice may hold one `LangDatabase` across calls for incrementality). The `debug_*` sessions are
/// the stateful exception: live program runs keyed by session id, shared across handler clones.
#[derive(Clone, Debug, Default)]
pub struct NoetaMcp {
    debug: debug::Registry,
}

impl NoetaMcp {
    pub fn new() -> Self {
        Self::default()
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
        // The whole program — siblings *and* dependency packages — under the entry's package
        // edition (from its `noeta.toml`), the default for inline source.
        let resolved = resolve_workspace(&args.source, &args.file)?;
        Ok(Json(run_check(&resolved)))
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

    /// Enumerate the real standard-library surface so the agent stops inventing stdlib calls.
    #[tool(
        description = "List the real Noeta standard-library surface — module and function signatures \
(and extern-type methods) straight from the compiler's native registry, the same source the type \
checker uses. Pass `module` (e.g. `std.math`, `math`, `http`, or a type like `Uuid`) to narrow; \
omit it for the whole surface. Use this instead of guessing stdlib calls — the signatures are ground \
truth."
    )]
    async fn stdlib_api(
        &self,
        Parameters(args): Parameters<StdlibApiArgs>,
    ) -> Json<stdlib::StdlibApiOutput> {
        Json(stdlib::query(args.module.as_deref()))
    }

    /// The inferred type at a symbol or position — the compiler's own answer, not a guess.
    #[tool(
        description = "Report the inferred type at a site in Noeta code. Address the site by \
`symbol` (a name — its first occurrence in the entry file) or by 1-based `line`+`column`. Returns \
the tightest typed expression's type in surface syntax. Ground truth from the type checker."
    )]
    async fn type_at(
        &self,
        Parameters(args): Parameters<TypeAtArgs>,
    ) -> Result<Json<understand::TypeAtOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(understand::type_at(
            &prepared,
            args.symbol.as_deref(),
            args.line,
            args.column,
        )))
    }

    /// Where the symbol at a site is declared — possibly in another file.
    #[tool(
        description = "Find where a symbol in Noeta code is declared. Address the site by `symbol` \
(a name) or 1-based `line`+`column`. Resolves through the same engine the editor uses — \
shadowing-correct locals, members via the receiver's type, imports into sibling modules and \
dependency packages (pass `file` for cross-file resolution). Returns the location and its source \
line."
    )]
    async fn definition(
        &self,
        Parameters(args): Parameters<NavigateArgs>,
    ) -> Result<Json<navigate::DefinitionOutput>, ErrorData> {
        let opened = navigate::open(&args.source, &args.file)?;
        Ok(Json(navigate::definition(
            &opened,
            args.symbol.as_deref(),
            args.line,
            args.column,
        )))
    }

    /// Every use of the symbol at a site.
    #[tool(
        description = "List every reference to a symbol in Noeta code — each use plus the \
declaration (set `include_declaration: false` to drop it). Address the site by `symbol` or \
1-based `line`+`column`. Value symbols resolve scope-aware; member symbols match by the \
receiver's type, so a same-named member on another type is not swept in. Cross-file with `file`."
    )]
    async fn references(
        &self,
        Parameters(args): Parameters<NavigateArgs>,
    ) -> Result<Json<navigate::ReferencesOutput>, ErrorData> {
        let opened = navigate::open(&args.source, &args.file)?;
        Ok(Json(navigate::references(
            &opened,
            args.symbol.as_deref(),
            args.line,
            args.column,
            args.include_declaration.unwrap_or(true),
        )))
    }

    /// What completes at a position.
    #[tool(
        description = "List completion candidates at a 1-based `line`+`column` in Noeta code — \
the same engine the editor uses. After a `.` it offers the receiver type's fields, variants, and \
methods (only); in a type-annotation position, type names; otherwise keywords, declarations, and \
the bindings in scope. Useful for discovering what an object offers."
    )]
    async fn completions(
        &self,
        Parameters(args): Parameters<PositionArgs>,
    ) -> Result<Json<navigate::CompletionsOutput>, ErrorData> {
        let opened = navigate::open(&args.source, &args.file)?;
        Ok(Json(navigate::completions(&opened, args.line, args.column)))
    }

    /// The signature of the call a position is inside.
    #[tool(
        description = "Report the signature of the function or method call surrounding a 1-based \
`line`+`column` in Noeta code, with the active parameter. Token-based, so it works mid-edit with \
an unclosed call; method calls resolve the receiver's type."
    )]
    async fn signature(
        &self,
        Parameters(args): Parameters<PositionArgs>,
    ) -> Result<Json<navigate::SignatureOutput>, ErrorData> {
        let opened = navigate::open(&args.source, &args.file)?;
        Ok(Json(navigate::signature(&opened, args.line, args.column)))
    }

    /// The declaration outline of a file — functions, types, and their members.
    #[tool(
        description = "Outline the declarations in Noeta code — top-level functions, structs, \
classes, enums, and impls, with their fields, variants, and methods as children (each with its \
source span). The map an agent reads before navigating a file."
    )]
    async fn symbols(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<understand::SymbolsOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(understand::symbols(&prepared)))
    }

    /// The project's own `@doc` documentation, adjacency-resolved.
    #[tool(
        description = "Collect the project's own `@doc { … }` documentation — every block across \
the entry and its linked modules, resolved to what it documents (`module`, a named `decl`, or a \
free `section`) with its file, line, and dedented Markdown body. Works from a parse alone, so it \
reads docs out of work-in-progress code. Use this to understand a codebase through its authored \
docs, or to answer \"where is X documented?\"."
    )]
    async fn project_docs(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<understand::ProjectDocsOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(understand::project_docs(&prepared)))
    }

    /// Browse the project's documentation tree — the same model the editor's docs browser shows.
    #[tool(
        description = "Browse the project's documentation as a navigable tree — the same unified \
model the editor's docs browser shows, so the agent and the human see the same docs. Omit `id` for \
the corpus roots; pass a node's `id` to expand one level (root → source modules → declarations → \
members). Each node reports whether it `has_page` (read it with `doc_page`) and whether it is \
`expandable`. Works from a parse alone, so it reads work-in-progress code."
    )]
    async fn doc_browse(
        &self,
        Parameters(args): Parameters<DocBrowseArgs>,
    ) -> Result<Json<understand::DocBrowseOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(understand::doc_browse(&prepared, args.id.as_deref())))
    }

    /// Read one documentation page — a declaration's signature and `@doc` prose.
    #[tool(
        description = "Render one node of the project documentation tree: its signature (for a \
declaration or member) and its `@doc` prose, with the source location. Pass an `id` from \
`doc_browse`. `found: false` when the id names nothing in the current program."
    )]
    async fn doc_page(
        &self,
        Parameters(args): Parameters<DocPageArgs>,
    ) -> Result<Json<understand::DocPageOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(understand::doc_page(&prepared, &args.id)))
    }

    /// The pretty-printed AST — the parsed syntax tree with spans.
    #[tool(
        description = "Return the parsed syntax tree of Noeta code as pretty-printed S-expressions \
with `@start..end` byte spans — the compiler's own AST rendering. For understanding exactly how a \
construct parsed."
    )]
    async fn ast(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<introspect::AstOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(introspect::ast(&prepared)))
    }

    /// The VM bytecode disassembly — what actually runs.
    #[tool(
        description = "Disassemble Noeta code to its register-VM bytecode (opcodes, constant pool, \
per-function protos) — what actually executes, including which fast paths fired. Or the first \
construct the VM does not support, with the reason."
    )]
    async fn bytecode(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<introspect::BytecodeOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(introspect::bytecode(&prepared)))
    }

    /// A per-stage health summary: lex → parse → check → compile.
    #[tool(
        description = "Summarize a Noeta program's compile pipeline stage by stage — token count, \
top-level item count, type-check error/warning counts, and whether it compiles to bytecode (with \
the first blocking reason). A quick 'what's the shape / where does it fall over' glance."
    )]
    async fn pipeline(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<introspect::PipelineOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(introspect::pipeline(&prepared)))
    }

    /// The module dependency graph — `namespace`/`use` import edges.
    #[tool(
        description = "Report the workspace's module graph: each file's declared `namespace` and \
the modules it imports via `use` (with the imported names). The import structure of a multi-file \
Noeta program."
    )]
    async fn module_graph(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<introspect::ModuleGraphOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(introspect::module_graph(&prepared)))
    }

    /// Unfold the static call path from a role or function.
    #[tool(
        description = "Trace the full static path a request takes through Noeta code: start from \
a `@role` (e.g. `EntryPoint` — traces every function bearing it) or a function name, and unfold \
the call graph — each node a function with its own roles, declaration site, and call site; \
external module calls (`http.response`, `fs.read`) and dynamic callees are labeled leaves. The \
`boundaries` summary lists every (function, role) the flow reaches — which persistence/trust \
boundaries an entry point crosses. Follows passed-function references (handler registrations, \
callbacks) as `reference` edges."
    )]
    async fn trace(
        &self,
        Parameters(args): Parameters<TraceArgs>,
    ) -> Result<Json<trace::TraceOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(trace::trace(
            &prepared,
            args.from.as_deref(),
            args.max_depth,
        )))
    }

    /// The `@role`/`@semantic` architectural graph plus the attribute manifest and declared types.
    #[tool(
        description = "Reflect over Noeta code: the `@role(Enum.Variant)` architectural graph \
(entry points, trust/persistence boundaries, sinks, layers), the `#[...]` attribute manifest, and \
the declared types — exactly what the program's own `roles_of()`/`attributes_of()` see. Filter with \
`role`."
    )]
    async fn reflect(
        &self,
        Parameters(args): Parameters<ReflectArgs>,
    ) -> Result<Json<introspect::ReflectOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(introspect::reflect(&prepared, args.role.as_deref())))
    }

    /// Run a program and report what it did — stdout, exit, traceback — under liveness limits.
    #[tool(
        description = "Run Noeta code and report what happened: stdout, exit code, any runtime panic \
(with a traceback), and whether a liveness limit stopped it. Runs against the deterministic sandbox \
by default (in-memory fs, logical clock, seeded random) — pass `real: true` for the real host \
(disk/env/network) to test end-to-end behavior. Every run is bounded by a timeout, an instruction \
budget, and an output cap (tune via `limits`). A program that does not type-check is not run."
    )]
    async fn run(
        &self,
        Parameters(args): Parameters<RunArgs>,
    ) -> Result<Json<execute::RunOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        let out = execute::run(
            &prepared,
            args.args.unwrap_or_default(),
            args.real.unwrap_or(false),
            &args.limits.unwrap_or_default(),
        )?;
        Ok(Json(out))
    }

    /// Evaluate an expression against an optional context — a one-shot REPL.
    #[tool(
        description = "Evaluate a Noeta expression and get its value and type — a one-shot REPL. \
Provide `context` (prior bindings/definitions, e.g. `xs = [1, 2, 3];`) that run before `expr`. \
Sandbox by default; `real: true` uses the real host. Bounded by liveness limits (a timeout and an \
instruction budget, tune via `limits`): a runaway loop trips the bound and returns with `limit_hit` \
set instead of hanging. Use this to check what an expression produces without writing a whole program."
    )]
    async fn eval(&self, Parameters(args): Parameters<EvalArgs>) -> Json<execute::EvalOutput> {
        Json(execute::eval(
            &args.expr,
            args.context.as_deref(),
            args.real.unwrap_or(false),
            &args.limits.unwrap_or_default(),
        ))
    }

    /// Run a program's `@test` blocks and report each case.
    #[tool(
        description = "Run the `@test` blocks in Noeta code and report each case (pass/fail/skip, \
with the failing assertion and any stdout). Honors `#[Data([...])]` rows, `#[Skip]`, `#[Name]`, and \
`#[Group]`; `filter` keeps only tests whose name or group matches. Sandbox by default; `real: true` \
uses the real host. Each case is bounded by liveness limits (a timeout and an instruction budget, \
tune via `limits`): a runaway loop fails that case (with `limit_hit`) instead of hanging the suite."
    )]
    async fn test(
        &self,
        Parameters(args): Parameters<TestArgs>,
    ) -> Result<Json<execute::TestOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(execute::test(
            &prepared,
            args.filter.as_deref(),
            args.real.unwrap_or(false),
            &args.limits.unwrap_or_default(),
        )))
    }

    /// Start an interactive debug session.
    #[tool(
        description = "Start debugging a Noeta program: compile it with debug info and run it \
paused under the VM's debugger. Arm `breakpoints` (1-based lines) or stop at entry (the default \
with no breakpoints). Returns a session id plus the first stop — the paused stack with live \
locals — or the exit if it ran through. Sandboxed by default (`real: true` for the real host); \
every resume is budget-bounded, so a runaway program pauses (reason `limit`) instead of hanging."
    )]
    async fn debug_start(
        &self,
        Parameters(args): Parameters<DebugStartArgs>,
    ) -> Result<Json<debug::DebugStateOutput>, ErrorData> {
        Ok(Json(
            debug::start(
                &self.debug,
                args.source,
                args.file,
                args.breakpoints.unwrap_or_default(),
                args.stop_on_entry,
                args.real.unwrap_or(false),
            )
            .await?,
        ))
    }

    /// The current state of a debug session.
    #[tool(
        description = "Report a debug session's current state without resuming it: paused (with \
the stack and live locals), running, or exited (with stdout and exit code)."
    )]
    async fn debug_inspect(
        &self,
        Parameters(args): Parameters<DebugSessionArgs>,
    ) -> Result<Json<debug::DebugStateOutput>, ErrorData> {
        Ok(Json(debug::inspect(&self.debug, args.session)?))
    }

    /// Resume a paused session and report the next stop.
    #[tool(
        description = "Resume a paused debug session and wait for the next stop: `mode` is \
`continue` (to the next breakpoint, budget limit, or exit) or a line-granular step — `over` \
(default), `into` (descend into calls), `out` (run to the caller). Returns the new paused state \
(stack + locals) or the exit (stdout + exit code)."
    )]
    async fn debug_step(
        &self,
        Parameters(args): Parameters<DebugStepArgs>,
    ) -> Result<Json<debug::DebugStateOutput>, ErrorData> {
        Ok(Json(
            debug::step(
                &self.debug,
                args.session,
                args.mode.as_deref().unwrap_or("over"),
            )
            .await?,
        ))
    }

    /// Evaluate an expression in a paused frame.
    #[tool(
        description = "Evaluate a Noeta expression in a paused debug session's frame — the frame's \
locals, the program's functions, types, and globals are all in scope; statements work and a \
trailing bare expression is the value. Type-checked against the program before running; execution \
is budget-bounded, and the program stays paused either way. The debugger's REPL."
    )]
    async fn debug_eval(
        &self,
        Parameters(args): Parameters<DebugEvalArgs>,
    ) -> Result<Json<debug::DebugEvalOutput>, ErrorData> {
        Ok(Json(
            debug::eval(
                &self.debug,
                args.session,
                &args.expr,
                args.frame.unwrap_or(0),
            )
            .await?,
        ))
    }

    /// Terminate a debug session.
    #[tool(
        description = "Terminate a debug session (running or paused), wait for it to unwind, and \
report the final state — the program's stdout so far and its exit code. Frees the session slot."
    )]
    async fn debug_stop(
        &self,
        Parameters(args): Parameters<DebugSessionArgs>,
    ) -> Result<Json<debug::DebugStateOutput>, ErrorData> {
        Ok(Json(debug::stop(&self.debug, args.session).await?))
    }

    /// Format Noeta source into its canonical style.
    #[tool(
        description = "Format Noeta code into its canonical style (the same formatter `noeta fmt` \
runs) and return the result. Reports whether the source was already canonical, and declines (leaving \
the source untouched) if it does not parse — format never guesses at broken source."
    )]
    async fn format(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<Json<format::FormatOutput>, ErrorData> {
        let prepared = analyze::prepare(&args.source, &args.file)?;
        Ok(Json(format::format(&prepared)))
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
    /// Dispatch one `tools/call`, **containing a panic in the tool** rather than letting it end the
    /// request. `#[tool_handler]` would generate exactly this body minus the `catch_unwind`; it
    /// skips generating one when the impl defines its own.
    ///
    /// A tool panic is not a hypothetical: every tool runs the compiler front end over whatever
    /// source the agent points at. rmcp spawns each request as its own task, so a panic there does
    /// not kill the server — but it kills the task *before* it can reply, and the client is left
    /// waiting on a response that will never come, with nothing on the wire to say why. Turning it
    /// into an `INTERNAL_ERROR` gives the agent something it can read, retry, or report, and leaves
    /// the session usable. (A stack **overflow** is not an unwind and cannot be caught here — that
    /// one is prevented up front, by sizing the runtime's threads — see
    /// [`noeta_parser::SERVER_STACK_SIZE`] and [`run_stdio`].)
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let tool = request.name.to_string();
        let context = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        let router = Self::tool_router();
        catching_panics(&tool, router.call(context)).await
    }

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

/// Everything one `check`-style `source`/`file` request analyzes over: the ordered sources, the
/// dependency packages, and the root package's edition — i.e. **the whole program**, not just the
/// files that happen to sit beside the entry.
#[derive(Debug)]
pub(crate) struct ResolvedWorkspace {
    /// Entry at index 0, then sibling modules, then each dependency package's modules — the
    /// loader's canonical [`SourceId`] assignment ([`noeta_loader::workspace_sources`]), so a span
    /// from any of them resolves back to its file by index.
    pub sources: Vec<Source>,
    /// The dependency packages as salsa inputs. Empty for inline `source`, for a bare script with
    /// no manifest, and whenever resolution fails (see [`resolve_workspace`]).
    pub deps: Vec<noeta_db::DepSources>,
    /// The root package's language edition — what the entry and its siblings are analyzed under
    /// (each dependency carries its own).
    pub edition: noeta_lexer::Edition,
}

impl ResolvedWorkspace {
    /// The **member** sources — the entry and its sibling modules, i.e. everything ahead of the
    /// dependency packages' modules in the canonical ordering.
    pub fn members(&self) -> &[Source] {
        let dep_len: usize = self.deps.iter().map(|d| d.modules.len()).sum();
        &self.sources[..self.sources.len() - dep_len]
    }

    /// Build the salsa [`Workspace`](noeta_db::Workspace) for this program: the members under the
    /// root package's edition, each dependency package's modules under its own. The one place the
    /// MCP surface turns a request into a program, so no tool can accidentally analyze a
    /// dependency-less slice of one.
    pub fn workspace(&self, db: &noeta_db::LangDatabase) -> noeta_db::Workspace {
        let (entry, modules) = self
            .members()
            .split_first()
            .expect("resolve_workspace always yields at least the entry");
        noeta_db::workspace_with_deps(db, entry, modules, &self.deps, self.edition)
    }
}

/// Turn a `check`-style `source`/`file` pair into the workspace the salsa `Workspace` is built
/// from. Inline `source` is a lone entry with no dependencies; a `file` pulls in its sibling
/// modules **and resolves its `noeta.toml` dependency graph**, exactly as the LSP does
/// (`noeta_ide::workspace`) and for the same reason: without the dependency packages, a program
/// that imports one is analyzed as a program whose imports do not exist. That made `check` report
/// an E0019/E0029 on code `noeta run` compiles cleanly, and made `reflect` miss every role a
/// dependency's `@role`-bearing attribute confers — the attribute *application* sits in the entry,
/// so it was listed, while the `@role` tag that gives it meaning sat in the unlinked package.
///
/// Resolution is the **query** walk (no lockfile refresh): answering an agent's question must not
/// rewrite `noeta.lock` as a side effect — the same rule the editor path follows. A resolution
/// failure degrades to no dependencies rather than failing the request; the entry's own analysis is
/// still worth returning, and the unresolved import then surfaces as an ordinary diagnostic.
pub(crate) fn resolve_workspace(
    source: &Option<String>,
    file: &Option<String>,
) -> Result<ResolvedWorkspace, ErrorData> {
    match (source, file) {
        (Some(text), None) => Ok(ResolvedWorkspace {
            sources: vec![Source::new(
                SourceId::FIRST,
                "<inline>".to_string(),
                text.clone(),
            )],
            deps: Vec::new(),
            edition: noeta_lexer::Edition::default(),
        }),
        (None, Some(path)) => {
            let path = Path::new(path);
            let raw = noeta_loader::read_workspace(path).map_err(|e| {
                ErrorData::invalid_params(format!("cannot read {}: {e}", path.display()), None)
            })?;
            let packages = noeta_pm::manifest::dependency_packages_query(path).unwrap_or_default();
            // THE one ordering authority — the same `SourceId` assignment the CLI's
            // `link_with_deps` and the startup cache use, so a dependency module's span located
            // here names the same file the compiler would.
            let sources = noeta_loader::workspace_sources(&raw, &packages);
            let mut next = 1 + raw.modules.len();
            let deps = packages
                .iter()
                .map(|package| {
                    let modules = sources[next..next + package.modules.len()].to_vec();
                    next += package.modules.len();
                    noeta_db::DepSources {
                        root: package.root.clone(),
                        key: package.key.clone(),
                        renames: package
                            .dep_renames
                            .iter()
                            .map(|(local, global)| (local.clone(), global.clone()))
                            .collect(),
                        modules,
                        edition: package.edition,
                    }
                })
                .collect();
            Ok(ResolvedWorkspace {
                sources,
                deps,
                edition: noeta_pm::manifest::root_edition(path),
            })
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

/// Run the whole-program check over `resolved` — the entry, its siblings, **and** its dependency
/// packages — and resolve the diagnostics into the canonical `JsonDiagnostic` form (the same one
/// `noeta check --format json` emits). Uses a fresh `LangDatabase` — the memoization is per call.
fn run_check(resolved: &ResolvedWorkspace) -> CheckOutput {
    let db = noeta_db::LangDatabase::default();
    let ws = resolved.workspace(&db);
    let checked = noeta_db::linked_checked(&db, ws);
    // The `SourceMap` resolves each diagnostic's span → file + line/column (entry is SourceId 0).
    let source_map = SourceMap::new(resolved.sources.clone());
    let diagnostics: Vec<JsonDiagnostic> = checked
        .diagnostics
        .iter()
        .map(|d| to_json(&source_map, d))
        .collect();
    let errors = diagnostics.iter().filter(|d| d.severity == "error").count();
    let warnings = diagnostics
        .iter()
        .filter(|d| d.severity == "warning")
        .count();
    CheckOutput {
        ok: errors == 0,
        errors,
        warnings,
        diagnostics,
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
        // Every tool runs the compiler front end on a runtime thread, and tokio's **2 MiB** default
        // is a quarter of the stack a `main` gets — so a tool that the equivalent CLI verb runs
        // happily aborted the *whole server* with "thread 'tokio-rt-worker' has overflowed its
        // stack", taking the client's session down mid-request. tokio hands the same size to the
        // blocking pool, so this covers `spawn_blocking` work too.
        .thread_stack_size(noeta_parser::SERVER_STACK_SIZE)
        .enable_all()
        .build()
        .expect("failed to build the MCP-server tokio runtime");
    runtime.block_on(serve());
}

/// Await a tool's future, turning a **panic** into a JSON-RPC `INTERNAL_ERROR` naming the tool.
/// Extracted from [`NoetaMcp::call_tool`] so the containment is testable on its own — the tool
/// router is not a place a panic can be injected.
async fn catching_panics(
    tool: &str,
    call: impl Future<Output = Result<CallToolResult, ErrorData>>,
) -> Result<CallToolResult, ErrorData> {
    match futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(call)).await {
        Ok(result) => result,
        // `&*payload`, not `&payload`: the latter unsizes the *`Box`* to `dyn Any`, so every
        // downcast misses and every panic renders as "non-string payload".
        Err(payload) => Err(ErrorData::internal_error(
            format!("the `{tool}` tool panicked: {}", panic_message(&*payload)),
            None,
        )),
    }
}

/// The human-readable half of a caught panic payload (`panic!("…")` / a failed `unwrap`), or a
/// placeholder when the payload is neither of the two standard string shapes.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
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
        run_check(&resolve_workspace(&Some(text.to_string()), &None).expect("inline source"))
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
        assert_eq!(out.errors, 1);
        let err = out
            .diagnostics
            .iter()
            .find(|d| d.severity == "error")
            .expect("an error diagnostic");
        assert!(err.code.starts_with('E'), "code was {:?}", err.code);
        assert_eq!(err.file, "<inline>");
        assert!(err.location.line >= 1 && err.location.column >= 1);
    }

    #[test]
    fn missing_arguments_is_an_invalid_params_error() {
        let err = resolve_workspace(&None, &None).unwrap_err();
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

    #[tokio::test]
    async fn a_panicking_tool_becomes_a_json_rpc_error() {
        // A panic inside a tool must reach the client as an error it can read. rmcp spawns each
        // request as its own task, so an escaping panic does not kill the server — it kills the
        // task before it replies, leaving the client waiting forever on a response that will never
        // come. `catching_panics` is what `call_tool` wraps every dispatch in.
        let err = catching_panics("probe", async { panic!("boom") })
            .await
            .expect_err("a panicking tool must produce an error, not a hang");
        assert!(
            err.message.contains("`probe`") && err.message.contains("boom"),
            "the error should name the tool and carry the panic message: {}",
            err.message
        );
        // The happy path is untouched.
        let ok = catching_panics("probe", async { Ok(CallToolResult::success(vec![])) })
            .await
            .expect("a well-behaved tool passes through");
        assert!(ok.content.is_empty());
    }

    #[test]
    fn a_panic_payload_renders_both_standard_shapes() {
        assert_eq!(
            panic_message(&"literal" as &(dyn std::any::Any + Send)),
            "literal"
        );
        assert_eq!(
            panic_message(&String::from("formatted") as &(dyn std::any::Any + Send)),
            "formatted"
        );
        assert_eq!(
            panic_message(&7u8 as &(dyn std::any::Any + Send)),
            "<non-string panic payload>"
        );
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

    /// M2 gate fixture: drive a real MCP session and call `stdlib_api` with a module filter — the
    /// tool is advertised and returns rendered signatures straight from the native registry.
    #[tokio::test]
    async fn round_trip_stdlib_api_over_a_duplex() {
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
            tools.tools.iter().any(|t| t.name == "stdlib_api"),
            "stdlib_api tool advertised"
        );

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "module".to_string(),
            serde_json::Value::String("std.math".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "stdlib_api".into();
        params.arguments = Some(arguments);
        let result = client
            .call_tool(params)
            .await
            .expect("tools/call stdlib_api");

        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["not_found"], serde_json::json!(false));
        assert_eq!(
            structured["modules"][0]["module"],
            serde_json::json!("std.math")
        );
        let sig = structured["modules"][0]["functions"][0]["signature"]
            .as_str()
            .expect("a rendered signature string");
        assert!(sig.starts_with("fn "), "signature was {sig:?}");

        client.cancel().await.expect("client shuts down");
        server.abort();
    }

    /// M3 gate fixture: drive a real MCP session and call `type_at` — an Introspect/Understand tool
    /// is advertised and answers a `symbol` query with a type straight off the salsa graph.
    #[tokio::test]
    async fn round_trip_type_at_over_a_duplex() {
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
        for expected in [
            "type_at",
            "symbols",
            "ast",
            "bytecode",
            "module_graph",
            "reflect",
        ] {
            assert!(
                tools.tools.iter().any(|t| t.name == expected),
                "{expected} tool advertised"
            );
        }

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "source".to_string(),
            serde_json::Value::String(
                "fn f(): int {\n  xs = [1, 2, 3];\n  return xs.len();\n}\n".to_string(),
            ),
        );
        arguments.insert(
            "symbol".to_string(),
            serde_json::Value::String("xs".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "type_at".into();
        params.arguments = Some(arguments);
        let result = client.call_tool(params).await.expect("tools/call type_at");

        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["found"], serde_json::json!(true));
        assert_eq!(structured["type"], serde_json::json!("List<int>"));

        client.cancel().await.expect("client shuts down");
        server.abort();
    }

    /// M4 gate fixture: drive a real MCP session and call `run` — an Execute-pillar tool is
    /// advertised and runs a program against the sandbox, returning its stdout and clean exit.
    #[tokio::test]
    async fn round_trip_run_over_a_duplex() {
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
        for expected in ["run", "eval", "test", "format"] {
            assert!(
                tools.tools.iter().any(|t| t.name == expected),
                "{expected} tool advertised"
            );
        }

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "source".to_string(),
            serde_json::Value::String("echo \"from mcp\";\n".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "run".into();
        params.arguments = Some(arguments);
        let result = client.call_tool(params).await.expect("tools/call run");

        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["ran"], serde_json::json!(true));
        assert_eq!(structured["ok"], serde_json::json!(true));
        assert_eq!(structured["host"], serde_json::json!("sandbox"));
        assert!(
            structured["stdout"]
                .as_str()
                .expect("stdout string")
                .contains("from mcp")
        );

        client.cancel().await.expect("client shuts down");
        server.abort();
    }

    /// M5 slice gate: the navigation tools are advertised and `definition` resolves over a real
    /// client⇄server duplex.
    #[tokio::test]
    async fn round_trip_definition_over_a_duplex() {
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
        for expected in ["definition", "references", "completions", "signature"] {
            assert!(
                tools.tools.iter().any(|t| t.name == expected),
                "{expected} tool advertised"
            );
        }

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "source".to_string(),
            serde_json::Value::String("fn greet(): int { return 1 }\ntotal = greet()".to_string()),
        );
        arguments.insert(
            "symbol".to_string(),
            serde_json::Value::String("greet".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "definition".into();
        params.arguments = Some(arguments);
        let result = client
            .call_tool(params)
            .await
            .expect("tools/call definition");

        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["found"], serde_json::json!(true));
        assert_eq!(
            structured["location"]["range"]["start"]["line"],
            serde_json::json!(1)
        );
        assert!(
            structured["snippet"]
                .as_str()
                .expect("snippet string")
                .contains("fn greet")
        );

        client.cancel().await.expect("client shuts down");
        server.abort();
    }

    /// M6 slice gate: the debug tools are advertised and a start → eval → stop session round-trips
    /// over a real client⇄server duplex.
    #[tokio::test(flavor = "multi_thread")]
    async fn round_trip_debug_session_over_a_duplex() {
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
        for expected in [
            "debug_start",
            "debug_inspect",
            "debug_step",
            "debug_eval",
            "debug_stop",
        ] {
            assert!(
                tools.tools.iter().any(|t| t.name == expected),
                "{expected} tool advertised"
            );
        }

        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "source".to_string(),
            serde_json::Value::String("total = 7\necho total\n".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "debug_start".into();
        params.arguments = Some(arguments);
        let started = client.call_tool(params).await.expect("debug_start");
        let started = started.structured_content.expect("structured content");
        assert_eq!(started["state"], serde_json::json!("paused"));
        assert_eq!(started["reason"], serde_json::json!("entry"));
        let session = started["session"].as_u64().expect("session id");

        let mut arguments = serde_json::Map::new();
        arguments.insert("session".to_string(), serde_json::json!(session));
        arguments.insert(
            "expr".to_string(),
            serde_json::Value::String("1 + 2".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "debug_eval".into();
        params.arguments = Some(arguments);
        let evaled = client.call_tool(params).await.expect("debug_eval");
        let evaled = evaled.structured_content.expect("structured content");
        assert_eq!(evaled["ok"], serde_json::json!(true));
        assert_eq!(evaled["value"], serde_json::json!("3"));

        let mut arguments = serde_json::Map::new();
        arguments.insert("session".to_string(), serde_json::json!(session));
        let mut params = CallToolRequestParams::default();
        params.name = "debug_stop".into();
        params.arguments = Some(arguments);
        let stopped = client.call_tool(params).await.expect("debug_stop");
        let stopped = stopped.structured_content.expect("structured content");
        assert_eq!(stopped["state"], serde_json::json!("exited"));

        client.cancel().await.expect("client shuts down");
        server.abort();
    }

    /// R3 slice gate: `trace` is advertised and a role-driven trace round-trips over a duplex.
    #[tokio::test]
    async fn round_trip_trace_over_a_duplex() {
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
        assert!(tools.tools.iter().any(|t| t.name == "trace"));

        let source = "\
@attribute
@role(Semantic.EntryPoint)
struct Route { path: string }

#[Route(\"/x\")]
fn handle(n: int): int { return helper(n) }

fn helper(n: int): int { return n + 1 }

echo handle(1)
";
        let mut arguments = serde_json::Map::new();
        arguments.insert(
            "source".to_string(),
            serde_json::Value::String(source.to_string()),
        );
        arguments.insert(
            "from".to_string(),
            serde_json::Value::String("EntryPoint".to_string()),
        );
        let mut params = CallToolRequestParams::default();
        params.name = "trace".into();
        params.arguments = Some(arguments);
        let result = client.call_tool(params).await.expect("tools/call trace");
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["found"], serde_json::json!(true));
        assert_eq!(structured["traces"][0]["name"], serde_json::json!("handle"));
        assert_eq!(
            structured["traces"][0]["children"][0]["name"],
            serde_json::json!("helper")
        );

        client.cancel().await.expect("client shuts down");
        server.abort();
    }
}
