//! The in-browser toolchain (P-WASM W2.1) — the engine behind the noeta.dev playground.
//!
//! Compiled to `wasm32-unknown-unknown`, this crate puts the *real* pipeline in a visitor's tab:
//! the same lexer → parser → checker → compiler → VM that `noeta run` uses, executing on the
//! deterministic [`SandboxHost`](noeta_stdlib::SandboxHost) (in-memory fs, seeded PRNG, logical
//! clock — exactly the conformance world, so playground output is oracle-grade, not a toy
//! approximation). Three operations, all JSON-in-prose-out over UTF-8 strings:
//!
//! - [`check_source`] — every lex/parse/type diagnostic in the stable
//!   [`JsonDiagnostic`](noeta_diagnostics::JsonDiagnostic) shape `noeta check --format json` uses.
//! - [`run_source`] — compile and execute; stdout, exit code, runtime diagnostics, and the
//!   rendered abort traceback.
//! - [`fmt_source`] — the canonical `noeta fmt` formatting.
//!
//! The wasm export surface (a hand-rolled `(ptr, len)` C ABI — see `abi.rs` for why not
//! wasm-bindgen) wraps these; natively the same functions back the unit tests. The embedder is
//! expected to run the module in a **Web Worker and terminate it on timeout** — that is the
//! runaway-loop guard; the VM deliberately has no fuel counter.
//!
//! Salsa is the compile path (not the lighter direct pipeline) on purpose: it is the IDE engine
//! `noeta-ide` sits on, so hover/completion/go-to-def in the browser (W2.3) become an additive
//! change rather than a second pipeline.

mod abi;
mod browser_executor;
mod browser_host;
mod ide;

pub use browser_executor::BrowserExecutor;
pub use browser_host::BrowserHost;
pub use ide::{complete_source, definition_source, hover_source, signature_source};

use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId, SourceMap};
use serde_json::json;

/// The display name the playground buffer carries in diagnostics ("playground.noe:3:1 …").
const SOURCE_NAME: &str = "playground.noe";

/// Lex/parse/type-check `text`, returning `{"diagnostics": [JsonDiagnostic…]}` — the stable
/// shape `noeta check --format json` emits, resolved against the real source text (line/column
/// and byte offsets both present, for editor squiggles).
pub fn check_source(text: &str) -> String {
    let (_, _, diagnostics) = front_end(text);
    json!({ "diagnostics": diagnostics }).to_string()
}

/// Compile and run `text` on the deterministic sandbox — the default world, so playground output
/// is oracle-grade. The result object always carries `compiled` and `diagnostics`; a compiled
/// program adds `stdout`, `exit_code`, and — after an abort with a call chain — `trace` (the
/// rendered traceback, exactly as the CLI prints it).
pub fn run_source(text: &str) -> String {
    run_with(text, Box::new(noeta_stdlib::SandboxHost::new()))
}

/// [`run_source`] on the [`BrowserHost`] (W3.0): real entropy, wall clock, and outbound HTTP
/// through the embedder's `noeta_host` imports — the playground's "real host" mode. Serial
/// async (the sandbox executor resolves at spawn); the JSPI embedder uses
/// [`run_source_browser_async`] instead.
pub fn run_source_browser(text: &str) -> String {
    run_with_executor(
        text,
        Box::new(BrowserHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
    )
}

/// [`run_source_browser`] under the JSPI pump (W3.1): the [`BrowserExecutor`] puts async work
/// genuinely in flight (overlapping fetches, real-time `sleep`), suspending the wasm stack on
/// its one suspending import while the browser event loop runs. Only callable from an embedder
/// that wrapped the imports with `WebAssembly.Suspending` and this entry with
/// `WebAssembly.promising` — the worker feature-detects and falls back to the serial entry.
pub fn run_source_browser_async(text: &str) -> String {
    run_with_executor(
        text,
        Box::new(BrowserHost::new()),
        Box::new(BrowserExecutor::new()),
    )
}

/// The shared run tail: compile through the salsa front end and execute on `host` (cooperative,
/// tier-0, with the abort traceback — `run_module_debug(…, None)` is the documented plain run).
fn run_with(text: &str, host: Box<dyn noeta_stdlib::Host>) -> String {
    run_with_executor(text, host, Box::new(noeta_stdlib::SandboxExecutor::new()))
}

/// [`run_with`] under an explicit executor (the JSPI pump swaps it — W3.1).
fn run_with_executor(
    text: &str,
    host: Box<dyn noeta_stdlib::Host>,
    executor: Box<dyn noeta_stdlib::Executor>,
) -> String {
    let (db, src, diagnostics) = front_end(text);
    if !diagnostics.is_empty() {
        return json!({ "compiled": false, "diagnostics": diagnostics }).to_string();
    }

    let sources = source_map(text);
    let module = match &noeta_db::bytecode(&db, src).0 {
        Ok(module) => module,
        Err(unsupported) => {
            // By construction every checked program compiles (the differential holds the VM at
            // 100% coverage); surface the invariant breach rather than mislabel it a user error.
            return json!({
                "compiled": false,
                "diagnostics": [],
                "error": format!("internal error: the VM cannot compile this program: {}", unsupported.reason),
            })
            .to_string();
        }
    };

    let (result, trace) = noeta_vm::VmBackend::new().run_module_debug(module, host, executor, None);
    let runtime_diagnostics: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| noeta_diagnostics::to_json(&sources, d))
        .collect();
    let rendered_trace = (trace.len() >= 2).then(|| noeta_vm::render_trace(&trace, &sources));
    json!({
        "compiled": true,
        "stdout": result.stdout,
        "exit_code": result.exit_code,
        "diagnostics": runtime_diagnostics,
        "trace": rendered_trace,
    })
    .to_string()
}

/// Format `text` with the canonical formatter (default config — the playground has no
/// `noeta.toml` to discover). `{"ok": true, "formatted": …}` or `{"ok": false, "error": …}`.
pub fn fmt_source(text: &str) -> String {
    match noeta_fmt::format_source(SOURCE_NAME, text, &noeta_fmt::FmtConfig::default()) {
        Ok(formatted) => json!({ "ok": true, "formatted": formatted }).to_string(),
        Err(noeta_fmt::FmtError::Parse(diags)) => json!({
            "ok": false,
            "error": format!("source does not parse ({} diagnostic(s))", diags.len()),
        })
        .to_string(),
        Err(noeta_fmt::FmtError::Safety(why)) => json!({
            "ok": false,
            "error": format!("internal safety check failed: {why}"),
        })
        .to_string(),
    }
}

/// The shared front end: lex + parse + type-check `text` through the salsa graph, with every
/// diagnostic resolved to its [`JsonDiagnostic`](noeta_diagnostics::JsonDiagnostic) form.
fn front_end(
    text: &str,
) -> (
    LangDatabase,
    noeta_db::SourceProgram,
    Vec<noeta_diagnostics::JsonDiagnostic>,
) {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, SOURCE_NAME, text);
    let src = noeta_db::source_program(&db, &source);
    let sources = source_map(text);

    let mut diagnostics = Vec::new();
    for d in &noeta_db::tokens(&db, src).0.diagnostics {
        diagnostics.push(noeta_diagnostics::to_json(&sources, d));
    }
    for d in &noeta_db::ast(&db, src).0.diagnostics {
        diagnostics.push(noeta_diagnostics::to_json(&sources, d));
    }
    // Type-check only what parsed — a parse error's downstream type noise would drown the cause,
    // matching the CLI's staging.
    if diagnostics.is_empty() {
        for d in &noeta_db::checked(&db, src).diagnostics {
            diagnostics.push(noeta_diagnostics::to_json(&sources, d));
        }
    }
    (db, src, diagnostics)
}

/// The single-source map diagnostics and tracebacks resolve against — the real text, so
/// line/column and snippets are exact.
fn source_map(text: &str) -> SourceMap {
    SourceMap::new(vec![Source::new(SourceId::FIRST, SOURCE_NAME, text)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("playground output is valid JSON")
    }

    #[test]
    fn clean_program_checks_empty_and_runs() {
        let check = parsed(&check_source("echo \"hello\";"));
        assert_eq!(check["diagnostics"].as_array().unwrap().len(), 0);

        let run = parsed(&run_source("echo \"hello\";"));
        assert_eq!(run["compiled"], true);
        assert_eq!(run["stdout"], "hello\n");
        assert_eq!(run["exit_code"], 0);
        assert!(run["trace"].is_null());
    }

    #[test]
    fn type_errors_surface_as_stable_json_diagnostics() {
        // `mut` is stably typed: reassigning a string over an int is a compile-time E0007.
        let text = "mut x = 1;\nx = \"s\";";
        let check = parsed(&check_source(text));
        let diags = check["diagnostics"].as_array().unwrap();
        assert!(!diags.is_empty());
        let d = &diags[0];
        assert!(d["code"].as_str().unwrap().starts_with('E'));
        assert_eq!(d["severity"], "error");
        assert_eq!(d["file"], "playground.noe");
        assert_eq!(d["line"], 2);

        // A run of the same program refuses to compile, carrying the same diagnostics.
        let run = parsed(&run_source(text));
        assert_eq!(run["compiled"], false);
        assert!(!run["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn deterministic_sandbox_world() {
        // The playground is the conformance world: seeded PRNG, byte-stable output.
        let text = "use std.random;\nrandom.seed(7);\necho random.int(0, 100);";
        let a = parsed(&run_source(text));
        let b = parsed(&run_source(text));
        assert_eq!(a["stdout"], b["stdout"]);
    }

    #[test]
    fn aborts_carry_the_rendered_traceback() {
        let run = parsed(&run_source(
            "fn boom(): int {\n  panic(\"kaboom\");\n}\necho boom();",
        ));
        assert_eq!(run["compiled"], true);
        assert_ne!(run["exit_code"], 0);
        let trace = run["trace"].as_str().expect("multi-frame abort renders");
        assert!(trace.contains("boom"), "trace: {trace}");
    }

    #[test]
    fn ide_smarts_answer_over_the_persistent_store() {
        // One buffer exercises all four smarts; positions are 0-based (line, UTF-16 character).
        let text = "fn add(a: int, b: int): int {\n  return a + b;\n}\n\necho add(1, 2);";

        // Hover over `a` in `a + b` (line 1, col 9): its type.
        let hover = parsed(&hover_source(text, 1, 9));
        assert_eq!(hover["found"], true);
        assert_eq!(hover["type"], "int");

        // Definition of `add` at the call site (line 4, col 5) → the declaration on line 0.
        let def = parsed(&definition_source(text, 4, 5));
        assert_eq!(def["found"], true);
        assert_eq!(def["range"]["start"]["line"], 0);

        // Signature help inside `add(1, |2)` (line 4, col 12): second parameter active.
        let sig = parsed(&signature_source(text, 4, 12));
        assert_eq!(sig["found"], true);
        assert!(sig["label"].as_str().unwrap().contains("add("));
        assert_eq!(sig["active"], 1);

        // Identifier completion at top level offers the declared function.
        let items = parsed(&complete_source(text, 4, 0));
        let labels: Vec<_> = items["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect();
        assert!(labels.iter().any(|l| l == "add"), "labels: {labels:?}");

        // Member completion after a bare dot: a struct receiver offers its fields. This also
        // proves the persistent store took the *changed* buffer (a different program than above).
        let member_text = "class Counter { n: int\n  fn get(): int { return self.n }\n}\nc = Counter { n: 1 }\nv = c.";
        let items = parsed(&complete_source(member_text, 4, 6));
        let labels: Vec<_> = items["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect();
        assert!(labels.iter().any(|l| l == "n"), "labels: {labels:?}");
    }

    #[test]
    fn fmt_round_trips_and_rejects_unparseable_source() {
        let ok = parsed(&fmt_source("echo   \"hello\"  ;"));
        assert_eq!(ok["ok"], true);
        assert!(ok["formatted"].as_str().unwrap().contains("echo"));

        let bad = parsed(&fmt_source("fn broken( {"));
        assert_eq!(bad["ok"], false);
    }
}
