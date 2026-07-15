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
//! The program arrives by **stapling** (the same slot mechanism as the wasip1 runner):
//! `noeta build --serve` patches the `.noeb` into a prebuilt generic component's data section
//! (`noeta_bundle::staple_wasm`, component-aware) — ~1 ms, no cargo at build time. The decoded
//! module is cached in a thread-local so platforms that *do* reuse an instance skip the decode.
//! A handler that never replies (a non-serving program, an abort, an unstapled generic
//! component) answers **500** with the run's output as the body — the debugging view you want
//! at the edge, not a hung connection.
//!
//! Split on purpose: this module (request → run → response over neutral `NetRequest`/
//! `NetResponse`) is target-agnostic and natively unit-tested; the `wasi:http` type glue lives
//! in `component.rs`, compiled only for the wasi target.

#[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
mod component;
#[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
mod outbound;

use std::cell::OnceCell;

use noeta_stdlib::{NetRequest, NetResponse};
use noeta_wasi_host::{OutboundHook, WasiHost};

/// The patchable bundle slot — the `noeta build --serve` staple contract, identical to the
/// wasip1 runner's (`noeta-wasm-runner/src/embedded.rs`): the shared magic, then the bundle's
/// linear-memory address and length as little-endian `u32`s. `repr(C)` fixes the patch offsets;
/// `#[used]` keeps the zero slot in the emitted data section.
#[repr(C)]
struct BundleSlot {
    magic: [u8; 16],
    ptr: u32,
    len: u32,
}

#[used]
static BUNDLE_SLOT: BundleSlot = BundleSlot {
    magic: noeta_bundle::WASM_SLOT_MAGIC,
    ptr: 0,
    len: 0,
};

/// The stapled bundle, if this component has been patched — `None` in the generic build. The
/// volatile reads are load-bearing: the static's compile-time value IS zero, and a plain read
/// would constant-fold to it, blinding the component to the patch (see the runner's twin).
fn stapled_bundle() -> Option<&'static [u8]> {
    // SAFETY: same contract as the runner's `embedded::bundle()` — when `len` is non-zero the
    // patcher guaranteed `[ptr, ptr+len)` is an initialized active data segment inside the
    // bumped memory minimum, disjoint from every Rust allocation; the address is materialized
    // with exposed provenance.
    #[allow(unsafe_code)]
    unsafe {
        let len = std::ptr::read_volatile(&raw const BUNDLE_SLOT.len);
        if len == 0 {
            return None;
        }
        let ptr = std::ptr::read_volatile(&raw const BUNDLE_SLOT.ptr);
        Some(std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<u8>(ptr as usize),
            len as usize,
        ))
    }
}

thread_local! {
    /// The decoded module, cached across invocations when the platform reuses an instance.
    static MODULE: OnceCell<Result<noeta_bytecode::Module, String>> = const { OnceCell::new() };
}

/// Serve one request: run the embedded program against a one-request inbound script and return
/// the reply its handler produced — or a diagnostic `500` when it produced none. The decoded
/// module is cached (the embedded bundle never changes), so instance reuse skips the decode.
pub fn serve_once(request: NetRequest) -> NetResponse {
    let Some(bundle) = stapled_bundle() else {
        return failure(
            "this is the generic noeta-wasm-serve component with no program stapled in: \
             build your app with `noeta build --serve app.noe` (see plans/wasm/, W4)",
        );
    };
    MODULE.with(|cell| {
        let module = cell.get_or_init(|| {
            noeta_bundle::read(bundle).map_err(|e| format!("cannot load the stapled bundle: {e}"))
        });
        match module {
            Ok(module) => run(module, request, platform_outbound()),
            Err(e) => failure(e),
        }
    })
}

/// The platform's outbound client: the `wasi:http/outgoing-handler` dance on the wasi target
/// (edge handlers can call upstream); `None` natively, where the lib tests inject their own.
fn platform_outbound() -> Option<OutboundHook> {
    #[cfg(all(target_arch = "wasm32", target_os = "wasi"))]
    {
        Some(Box::new(outbound::fetch))
    }
    #[cfg(not(all(target_arch = "wasm32", target_os = "wasi")))]
    {
        None
    }
}

/// [`serve_once`] over an explicit bundle — the natively-testable seam. Decodes fresh per call
/// (no cache: the cache is keyed to the one embedded bundle, not arbitrary inputs).
pub fn serve_bundle(bundle: &[u8], request: NetRequest) -> NetResponse {
    if bundle.is_empty() {
        return failure(
            "this is the generic noeta-wasm-serve component with no program stapled in: \
             build your app with `noeta build --serve app.noe` (see plans/wasm/, W4)",
        );
    }
    match noeta_bundle::read(bundle) {
        Ok(module) => run(&module, request, platform_outbound()),
        Err(e) => failure(&format!("cannot load the embedded bundle: {e}")),
    }
}

/// [`serve_bundle`] with an explicit outbound client — the natively-testable seam for the
/// upstream-fetch path (the wasi build injects the real `wasi:http` client instead).
pub fn serve_bundle_with_outbound(
    bundle: &[u8],
    request: NetRequest,
    outbound: OutboundHook,
) -> NetResponse {
    match noeta_bundle::read(bundle) {
        Ok(module) => run(&module, request, Some(outbound)),
        Err(e) => failure(&format!("cannot load the embedded bundle: {e}")),
    }
}

fn run(
    module: &noeta_bytecode::Module,
    request: NetRequest,
    outbound: Option<OutboundHook>,
) -> NetResponse {
    let mut host = WasiHost::new().with_args(vec!["app".to_string()]);
    if let Some(hook) = outbound {
        host = host.with_outbound(hook);
    }
    let (host, reply) = host.with_inbound(request);
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
        assert!(String::from_utf8_lossy(&response.body).contains("noeta build --serve"));
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
    fn a_handler_reaches_upstream_through_the_outbound_hook() {
        // The proxy shape: the handler fetches an upstream service and composes its reply. The
        // hook stands in for the wasi:http client (injected identically on the wasi build).
        let module = compile(
            "use std.http.server\nuse std.http.client\nuse std.http.{Request, Response}\n\n\
             fn handle(req: Request): Response {\n\
                 upstream = client.get(\"http://api.internal/data\")\n\
                 return server.response(200, \"upstream said: ${upstream.body()}\")\n\
             }\n\n\
             server.serve(8080, handle)",
        );
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = std::sync::Arc::clone(&seen);
        let hook: OutboundHook = Box::new(move |request| {
            log.lock().expect("log").push(request.url.clone());
            Ok(NetResponse {
                status: 200,
                headers: Vec::new(),
                body: b"42".to_vec(),
            })
        });
        let response =
            serve_bundle_with_outbound(&noeta_bundle::write(&module), get("/compose"), hook);
        assert_eq!(response.status, 200);
        assert_eq!(String::from_utf8_lossy(&response.body), "upstream said: 42");
        assert_eq!(*seen.lock().expect("log"), vec!["http://api.internal/data"]);
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
        let src = noeta_db::source_program(&db, &source, noeta_db::Edition::DEFAULT);
        noeta_db::bytecode(&db, src)
            .0
            .as_ref()
            .expect("test program compiles")
            .clone()
    }
}
