//! The edge-serve component (P-WASM W4): unchanged `http.serve` programs running on
//! `wasmtime serve`-class platforms as a `wasi:http/incoming-handler` component.
//!
//! The inversion that makes this a zero-VM-change slice: a wasi:http component is invoked **per
//! request**, and the deterministic sandbox already models inbound serving as a **finite request
//! script that ends the serve loop**. So each invocation runs the embedded program on a
//! [`WasiHost`] armed with a one-request script ([`WasiHost::with_inbound`]): the program's
//! `http.serve(port, handler)` accepts exactly this request, the handler replies through the
//! ordinary inbound `Network` capability, the next accept yields `None`, the serve loop returns,
//! and the captured reply becomes the component's response. Per-request isolation is the
//! platform's own model (`wasmtime serve` instantiates per request), so a fresh VM per request
//! is the *natural* shape here, not an inefficiency.
//!
//! The program arrives as a `.noeb` baked in at build time (`NOETA_SERVE_BUNDLE`, see
//! `build.rs` — interim, like `--native`'s workspace ladder); the decoded module is cached in a
//! thread-local so platforms that *do* reuse an instance skip the decode. A handler that never
//! replies (a non-serving program, an abort, an empty placeholder bundle) answers **500** with
//! the run's output as the body — the debugging view you want at the edge, not a hung
//! connection.
//!
//! Split on purpose: this module (request → run → response over neutral `NetRequest`/
//! `NetResponse`) is target-agnostic and natively unit-tested; the `wasi:http` type glue lives
//! in `component.rs`, compiled only for the wasi target.

#[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
mod component;

use std::cell::OnceCell;

use noeta_stdlib::{NetRequest, NetResponse};
use noeta_wasi_host::WasiHost;

/// The program this component serves, baked in by `build.rs` (empty ⇒ the 500 build hint).
static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bundle.noeb"));

thread_local! {
    /// The decoded module, cached across invocations when the platform reuses an instance.
    static MODULE: OnceCell<Result<noeta_bytecode::Module, String>> = const { OnceCell::new() };
}

/// Serve one request: run the embedded program against a one-request inbound script and return
/// the reply its handler produced — or a diagnostic `500` when it produced none. The decoded
/// module is cached (the embedded bundle never changes), so instance reuse skips the decode.
pub fn serve_once(request: NetRequest) -> NetResponse {
    if BUNDLE.is_empty() {
        return failure(
            "this noeta-wasm-serve component was built without a program: \
             rebuild with NOETA_SERVE_BUNDLE=<app.noeb> (see plans/wasm/, W4)",
        );
    }
    MODULE.with(|cell| {
        let module = cell.get_or_init(|| {
            noeta_bundle::read(BUNDLE).map_err(|e| format!("cannot load the embedded bundle: {e}"))
        });
        match module {
            Ok(module) => run(module, request),
            Err(e) => failure(e),
        }
    })
}

/// [`serve_once`] over an explicit bundle — the natively-testable seam. Decodes fresh per call
/// (no cache: the cache is keyed to the one embedded bundle, not arbitrary inputs).
pub fn serve_bundle(bundle: &[u8], request: NetRequest) -> NetResponse {
    if bundle.is_empty() {
        return failure(
            "this noeta-wasm-serve component was built without a program: \
             rebuild with NOETA_SERVE_BUNDLE=<app.noeb> (see plans/wasm/, W4)",
        );
    }
    match noeta_bundle::read(bundle) {
        Ok(module) => run(&module, request),
        Err(e) => failure(&format!("cannot load the embedded bundle: {e}")),
    }
}

fn run(module: &noeta_bytecode::Module, request: NetRequest) -> NetResponse {
    let (host, reply) = WasiHost::new()
        .with_args(vec!["app".to_string()])
        .with_inbound(request);
    let executor: Box<dyn noeta_stdlib::Executor> = Box::new(noeta_stdlib::SandboxExecutor::new());
    // The documented plain-run-plus-traceback entry — cooperative, tier-0, the wasm shape.
    let (result, _trace) =
        noeta_vm::VmBackend::new().run_module_debug(module, Box::new(host), executor, None);

    if let Some(response) = reply.lock().expect("reply slot not poisoned").take() {
        return response;
    }
    // No reply reached the slot: the program never served, or it aborted first. Surface what it
    // did produce — stdout and any diagnostics — as a 500 body instead of hanging the client.
    let mut body = String::new();
    body.push_str("the program produced no HTTP response\n");
    if !result.stdout.is_empty() {
        body.push_str("--- stdout ---\n");
        body.push_str(&result.stdout);
    }
    for diagnostic in &result.diagnostics {
        body.push_str(&format!("--- diagnostic ---\n{}\n", diagnostic.message));
    }
    failure(&body)
}

fn failure(message: &str) -> NetResponse {
    NetResponse {
        status: 500,
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: message.as_bytes().to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> NetRequest {
        NetRequest {
            method: "GET".to_string(),
            url: path.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn an_empty_bundle_answers_the_build_hint() {
        let response = serve_bundle(&[], get("/"));
        assert_eq!(response.status, 500);
        assert!(String::from_utf8_lossy(&response.body).contains("NOETA_SERVE_BUNDLE"));
    }

    #[test]
    fn a_serving_program_answers_through_the_one_request_script() {
        let module = compile(
            "use std.http.server\nuse std.http.{Request, Response}\n\n\
             fn handle(req: Request): Response {\n\
                 return server.response(200, \"you asked for ${req.path()}\")\n\
             }\n\n\
             server.serve(8080, handle)",
        );
        let response = serve_bundle(&noeta_bundle::write(&module), get("/hello"));
        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8_lossy(&response.body),
            "you asked for /hello"
        );
    }

    #[test]
    fn a_non_serving_program_answers_500_with_its_output() {
        let module = compile("echo \"just a script\";");
        let response = serve_bundle(&noeta_bundle::write(&module), get("/"));
        assert_eq!(response.status, 500);
        let body = String::from_utf8_lossy(&response.body);
        assert!(body.contains("no HTTP response"), "{body}");
        assert!(body.contains("just a script"), "{body}");
    }

    /// Compile through the salsa pipeline (dev-dependency), like the runner's tests.
    fn compile(text: &str) -> noeta_bytecode::Module {
        let db = noeta_db::LangDatabase::default();
        let source = noeta_span::Source::new(noeta_span::SourceId::FIRST, "app.noe", text);
        let src = noeta_db::source_program(&db, &source);
        noeta_db::bytecode(&db, src)
            .0
            .as_ref()
            .expect("test program compiles")
            .clone()
    }
}
