//! The browser host (P-WASM W3.0) — the fourth `Host`: the real world as a browser tab sees it.
//!
//! Where the playground's default `SandboxHost` is the deterministic conformance world, this
//! host backs the real-world capabilities with **wasm imports the embedding worker supplies**
//! (module `noeta_host`; the embedding worker lives in the separate noeta-playground repo —
//! `src/workers/engine.worker.ts`):
//!
//! - **Entropy** ← `js_entropy_u64` (`crypto.getRandomValues`) — real uuids.
//! - **Wall clock** ← `js_now_ms` (`Date.now`) — real `clock_unix_ms`.
//! - **Outbound HTTP** ← `js_net_fetch` — a synchronous XMLHttpRequest. Legal precisely because
//!   the engine only ever runs in a **Web Worker** (sync XHR is banned on the main thread, not
//!   in workers), which is what lets the synchronous [`Network::net_fetch`] leaf work without
//!   a VM seam change. Concurrent `*_async` fan-out degrades serial-but-correct, exactly as on
//!   any host whose executor resolves at spawn; genuine overlap is a later slice (JSPI or a
//!   pump), recorded in `plans/wasm/`.
//!
//! Everything a tab cannot provide keeps the deterministic/in-memory shape: the fs is a fresh
//! [`Vfs`], the user-facing PRNG stays seeded and `monotonic` logical (the RealHost rules), env
//! is an empty overlay, p2p is the loopback broker, and inbound serving / `os.exec` are honest
//! errors. Telemetry is the shared no-exporter [`SpanTracker`].
//!
//! On native builds (this crate's unit tests) the imports fall back to real
//! entropy/`SystemTime` and a canned network error — the JSON marshalling across the import is
//! proven by the node smoke test, which supplies the imports the way the worker does.

use std::collections::HashMap;

use noeta_stdlib::fs::Vfs;
use noeta_stdlib::{
    AttrValue, Clock, Console, Entropy, Env, ErrorKind, ExecResult, FileReader, FileSystem, Ids,
    InstrumentId, InstrumentKind, LogRecord, Logging, MetricData, MetricStore, MetricValue,
    Metrics, NetError, NetErrorKind, NetRequest, NetResponse, Network, Os, ReadSource, Rng, SpanId,
    SpanKind, SpanStatus, SpanTracker, StdError, Stream, TraceContext, Tracing,
};
use serde_json::json;

/// The real-world leaves, embedder-supplied on wasm. The response of `js_net_fetch` is a
/// length-prefixed buffer (`[len: u32 LE][json]`) the JS side allocates through `noeta_alloc` —
/// the same packing the export surface uses, so both directions share one convention.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) mod imports {
    #[link(wasm_import_module = "noeta_host")]
    unsafe extern "C" {
        pub fn js_entropy_u64() -> u64;
        pub fn js_now_ms() -> f64;
        pub fn js_net_fetch(ptr: *const u8, len: usize) -> *mut u8;
        // The JSPI pump (W3.1). `js_fetch_start` begins a fetch in JS and returns a ticket
        // WITHOUT suspending; `js_fetch_take` polls it (null pointer = still pending);
        // `js_wait` is the one SUSPENDING import — it parks the whole wasm stack on
        // `Promise.race([any pending ticket settles, optional timeout])`, letting the browser
        // event loop run. On a non-JSPI embedder these are plain stubs and never called (the
        // worker routes to the synchronous entry point instead).
        pub fn js_fetch_start(ptr: *const u8, len: usize) -> u64;
        pub fn js_fetch_take(ticket: u64) -> *mut u8;
        pub fn js_wait(timeout_ms: f64);
    }

    pub fn entropy_u64() -> u64 {
        unsafe { js_entropy_u64() }
    }

    pub fn now_ms() -> u64 {
        (unsafe { js_now_ms() }).max(0.0) as u64
    }

    /// Round-trip a request JSON through the embedder, reclaiming the packed response buffer.
    pub fn net_fetch(request_json: &str) -> String {
        unsafe { read_packed(js_net_fetch(request_json.as_ptr(), request_json.len())) }
    }

    /// Begin a fetch without suspending; the returned ticket is polled via [`fetch_take`].
    pub fn fetch_start(request_json: &str) -> u64 {
        unsafe { js_fetch_start(request_json.as_ptr(), request_json.len()) }
    }

    /// Poll a started fetch: the reply JSON once settled, `None` while in flight.
    pub fn fetch_take(ticket: u64) -> Option<String> {
        let ptr = unsafe { js_fetch_take(ticket) };
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { read_packed(ptr) })
    }

    /// Suspend until any pending ticket settles or `timeout_ms` elapses (negative = no timeout).
    pub fn wait(timeout_ms: f64) {
        unsafe { js_wait(timeout_ms) }
    }

    /// Reclaim a `[len: u32 LE][bytes]` buffer the embedder allocated through `noeta_alloc`.
    unsafe fn read_packed(ptr: *mut u8) -> String {
        #[allow(unsafe_code)]
        unsafe {
            let mut len_bytes = [0u8; 4];
            std::ptr::copy_nonoverlapping(ptr, len_bytes.as_mut_ptr(), 4);
            let len = u32::from_le_bytes(len_bytes) as usize;
            let buf = Vec::from_raw_parts(ptr, 4 + len, 4 + len);
            String::from_utf8_lossy(&buf[4..]).into_owned()
        }
    }
}

/// Native fallbacks: honest time/entropy; the sync fetch is an honest error (the import
/// marshalling is node-proven), while the ticketed async pair answers a canned reply — that is
/// what lets the executor's whole future path be unit-tested natively.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) mod imports {
    pub fn entropy_u64() -> u64 {
        getrandom::u64().expect("the OS entropy source is unavailable")
    }

    pub fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn net_fetch(_request_json: &str) -> String {
        r#"{"error":"the browser host's network runs only in a JS embedding"}"#.to_string()
    }

    pub fn fetch_start(_request_json: &str) -> u64 {
        1
    }

    pub fn fetch_take(_ticket: u64) -> Option<String> {
        Some(r#"{"status":200,"headers":[],"body":"native-pong"}"#.to_string())
    }

    pub fn wait(_timeout_ms: f64) {}
}

/// The browser host: real entropy/wall-clock/outbound-HTTP through the embedder's imports;
/// everything else in-memory or deterministic. Constructed per run by `noeta_run_browser`.
#[derive(Debug, Default)]
pub struct BrowserHost {
    fs: Vfs,
    rng: u64,
    clock: u64,
    ids: u64,
    /// The program's `env.set` writes — a tab inherits no environment, so the overlay is all
    /// there is.
    env: HashMap<String, String>,
    spans: SpanTracker,
    metrics: MetricStore,
}

impl BrowserHost {
    pub fn new() -> BrowserHost {
        BrowserHost {
            rng: noeta_stdlib::random::DEFAULT_SEED,
            ids: 1,
            ..BrowserHost::default()
        }
    }
}

fn io_error(message: String) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message,
    }
}

impl FileReader for BrowserHost {
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError> {
        // In-memory files snapshot, like the sandbox — nothing to stream lazily.
        Ok(ReadSource::Snapshot(self.fs.read(path)?))
    }

    fn fs_read_more(&mut self, _id: u64) -> Result<Option<String>, StdError> {
        // Snapshots never hand out a lazy id; answer EOF defensively rather than panic.
        Ok(None)
    }
}

impl FileSystem for BrowserHost {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.fs.write(path, content);
        Ok(())
    }

    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.fs.append(path, content);
        Ok(())
    }

    fn fs_read(&self, path: &str) -> Result<String, StdError> {
        self.fs.read(path)
    }

    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError> {
        self.fs.write_bytes(path, data);
        Ok(())
    }

    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError> {
        self.fs.read_bytes(path)
    }

    fn fs_exists(&self, path: &str) -> bool {
        self.fs.exists(path)
    }

    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError> {
        Ok(self.fs.remove(path))
    }

    fn fs_list(&self) -> Result<Vec<String>, StdError> {
        Ok(self.fs.list())
    }

    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError> {
        Ok(self.fs.list_dir(dir))
    }

    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError> {
        self.fs.mkdir(path);
        Ok(())
    }

    fn fs_is_dir(&self, path: &str) -> bool {
        self.fs.is_dir(path)
    }
}

impl Rng for BrowserHost {
    fn rng_seed(&mut self, seed: i64) {
        self.rng = noeta_stdlib::random::seed_state(seed);
    }

    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError> {
        let (next_state, value) = noeta_stdlib::random::int(self.rng, lo, hi)?;
        self.rng = next_state;
        Ok(value)
    }

    fn rng_float(&mut self) -> f64 {
        let (next_state, value) = noeta_stdlib::random::float(self.rng);
        self.rng = next_state;
        value
    }
}

impl Clock for BrowserHost {
    fn clock_monotonic(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.clock = self.clock.saturating_add(ms.max(0) as u64);
    }

    fn clock_unix_ms(&mut self) -> u64 {
        imports::now_ms()
    }
}

impl Ids for BrowserHost {
    fn id_next(&mut self) -> u64 {
        let id = self.ids;
        self.ids += 1;
        id
    }
}

impl Entropy for BrowserHost {
    fn entropy_u64(&mut self) -> u64 {
        imports::entropy_u64()
    }
}

impl Env for BrowserHost {
    fn env_get(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    fn env_set(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    fn env_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.env.keys().cloned().collect();
        keys.sort();
        keys
    }

    fn args(&self) -> Vec<String> {
        vec![crate::SOURCE_NAME.to_string()]
    }
}

impl Console for BrowserHost {
    // The browser has no console stdin: reads report EOF and every stream is non-interactive, the
    // same non-interactive shape the sandbox presents (a playground program never blocks on input).
    fn stdin_read_line(&mut self) -> Option<String> {
        None
    }

    fn stdin_read_all(&mut self) -> String {
        String::new()
    }

    fn is_tty(&self, _stream: Stream) -> bool {
        false
    }

    fn prompt(&mut self, _msg: &str) -> Option<String> {
        None
    }
}

impl Os for BrowserHost {
    fn os_platform(&self) -> String {
        "web".to_string()
    }

    fn os_arch(&self) -> String {
        "wasm32".to_string()
    }

    fn os_hostname(&self) -> String {
        "browser".to_string()
    }

    fn os_cpus(&self) -> i64 {
        // The engine is single-threaded in its worker regardless of the machine.
        1
    }

    fn os_cwd(&self) -> String {
        "/".to_string()
    }

    fn os_pid(&self) -> i64 {
        1
    }

    fn os_exec(&mut self, command: &str, _args: &[String]) -> Result<ExecResult, StdError> {
        Err(io_error(format!(
            "os.exec: cannot run `{command}`: a browser tab has no subprocesses"
        )))
    }

    // --- Process supervision (the process-streaming arc): a browser tab / WASI guest has no
    // subprocesses, so the whole family is the same honest error as `os_exec` — and since
    // `os_spawn` never succeeds, no handle can exist for the query leaves. ---

    fn os_try_spawn(
        &mut self,
        command: &str,
        _args: &[String],
    ) -> Result<u64, noeta_stdlib::os::OsError> {
        Err(noeta_stdlib::os::OsError::new(
            "os.spawn",
            noeta_stdlib::os::OsErrorKind::NotFound,
            format!("cannot run `{command}`: a browser tab has no subprocesses"),
        ))
    }

    fn os_proc_pid(&self, _handle: u64) -> Option<i64> {
        None
    }

    fn os_proc_wait(&mut self, _handle: u64) -> Result<ExecResult, StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_try_wait(&mut self, _handle: u64) -> Result<Option<ExecResult>, StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_kill(&mut self, _handle: u64) -> Result<(), StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_signal(
        &mut self,
        _handle: u64,
        _signal: noeta_stdlib::os::Signal,
    ) -> Result<(), StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_read_line(&mut self, _handle: u64) -> Result<Option<String>, StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_read(&mut self, _handle: u64, _count: i64) -> Result<Option<String>, StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_read_stderr_line(&mut self, _handle: u64) -> Result<Option<String>, StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }

    fn os_proc_try_write_stdin(
        &mut self,
        _handle: u64,
        _data: &str,
    ) -> Result<(), noeta_stdlib::os::OsError> {
        Err(noeta_stdlib::os::OsError::new(
            "write",
            noeta_stdlib::os::OsErrorKind::Other,
            "no child process exists: a browser tab has no subprocesses",
        ))
    }

    fn os_proc_close_stdin(&mut self, _handle: u64) -> Result<(), StdError> {
        Err(io_error(
            "no child process exists: a browser tab has no subprocesses".to_string(),
        ))
    }
}

/// Lower a request to the JSON that crosses the `noeta_host` import boundary. Bodies cross as
/// text (an HTTP API's common case); a binary body would arrive lossy — acceptable for the
/// playground, noted here rather than hidden.
pub(crate) fn request_json(request: &NetRequest) -> String {
    json!({
        "method": request.method,
        "url": request.url,
        "headers": request.headers,
        "body": String::from_utf8_lossy(&request.body),
    })
    .to_string()
}

/// Raise the embedder's reply JSON (`{status, headers, body}` or `{error}`) back into a
/// [`NetResponse`]. Shared by the synchronous leaf and the JSPI future (W3.1).
pub(crate) fn parse_reply(reply: &str, url: &str) -> Result<NetResponse, NetError> {
    let parsed: serde_json::Value = serde_json::from_str(reply).map_err(|e| {
        NetError::new(
            NetErrorKind::Protocol,
            url,
            format!("malformed embedder fetch reply: {e}"),
        )
    })?;
    // A browser `fetch` rejection collapses every transport class into one opaque string (the
    // Fetch spec deliberately withholds the distinction from script), so it can only be `Other`.
    if let Some(error) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(NetError::new(NetErrorKind::Other, url, error));
    }
    let headers = parsed
        .get("headers")
        .and_then(|h| h.as_array())
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    Some((
                        pair.get(0)?.as_str()?.to_string(),
                        pair.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(NetResponse {
        url: url.to_string(),
        status: parsed.get("status").and_then(|s| s.as_u64()).unwrap_or(0) as u16,
        headers,
        body: parsed
            .get("body")
            .and_then(|b| b.as_str())
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
    })
}

impl Network for BrowserHost {
    fn net_fetch(&mut self, request: NetRequest) -> Result<NetResponse, NetError> {
        let reply = imports::net_fetch(&request_json(&request));
        parse_reply(&reply, &request.url)
    }

    fn net_spawn(&self, request: NetRequest) -> Box<dyn noeta_stdlib::ExternIo> {
        // The JSPI seam (W3.1): hand the executor a descriptor whose real body is a plain Rust
        // future over a JS fetch ticket — `BrowserExecutor` starts it without suspending, so
        // `all([get_async(..), ..])` genuinely overlaps. Under the synchronous entry point the
        // sandbox executor takes the `run_sync` body instead (serial-but-correct, unchanged).
        Box::new(crate::browser_executor::BrowserFetchIo::new(request))
    }

    fn net_listen(&mut self, _addr: &str) -> Result<u64, StdError> {
        Err(io_error(
            "serving (`http.serve`) is not available in a browser tab — deploy the program with \
             `noeta build --wasm` instead (and see plans/wasm/ W4 for wasi:http)"
                .to_string(),
        ))
    }

    fn net_accept_next(&mut self, _listener: u64) -> Result<Option<(u64, NetRequest)>, StdError> {
        Err(io_error(
            "no inbound listener exists in a browser tab".to_string(),
        ))
    }

    fn net_reply_now(&mut self, _conn: u64, _response: NetResponse) -> Result<(), StdError> {
        Err(io_error(
            "no inbound listener exists in a browser tab".to_string(),
        ))
    }
}

// The playground's browser host no longer bakes in the loopback broker (para-namespace follow-on
// F2) — the `para.p2p` extension owns it — so it keeps the default `P2pProvider` (`as_p2p` → `None`).
impl noeta_stdlib::host::P2pProvider for BrowserHost {}

impl Tracing for BrowserHost {
    fn tel_enabled(&self) -> bool {
        false
    }

    fn tel_span_start(
        &mut self,
        name: &str,
        kind: SpanKind,
        parent: Option<TraceContext>,
    ) -> SpanId {
        let span_id = self.entropy_u64().to_be_bytes();
        let mut trace_id = [0u8; 16];
        trace_id[..8].copy_from_slice(&self.entropy_u64().to_be_bytes());
        trace_id[8..].copy_from_slice(&self.entropy_u64().to_be_bytes());
        let now = self.clock_unix_ms();
        self.spans.start(name, kind, parent, span_id, trace_id, now)
    }

    fn tel_span_set_attr(&mut self, span: SpanId, key: &str, value: AttrValue) {
        self.spans.set_attr(span, key, value);
    }

    fn tel_span_add_event(
        &mut self,
        span: SpanId,
        name: &str,
        attrs: Vec<(compact_str::CompactString, AttrValue)>,
    ) {
        let now = self.clock_unix_ms();
        self.spans.add_event(span, name, attrs, now);
    }

    fn tel_span_set_status(&mut self, span: SpanId, status: SpanStatus) {
        self.spans.set_status(span, status);
    }

    fn tel_span_end(&mut self, span: SpanId) {
        let now = self.clock_unix_ms();
        drop(self.spans.end(span, now));
    }

    fn tel_span_context(&mut self, span: SpanId) -> TraceContext {
        self.spans.context(span)
    }

    fn tel_intern_remote(&mut self, context: TraceContext) -> SpanId {
        self.spans.intern_remote(context)
    }

    fn tel_is_remote(&self, span: SpanId) -> bool {
        self.spans.is_remote(span)
    }

    fn tel_release_remote(&mut self, span: SpanId) {
        self.spans.release_remote(span);
    }
}

impl Logging for BrowserHost {
    fn tel_logs_enabled(&self) -> bool {
        false
    }

    fn log_emit(&mut self, _record: LogRecord) {}
}

impl Metrics for BrowserHost {
    fn tel_metrics_enabled(&self) -> bool {
        false
    }

    fn metric_get_or_create(
        &mut self,
        name: &str,
        unit: &str,
        kind: InstrumentKind,
    ) -> InstrumentId {
        self.metrics.get_or_create(name, unit, kind)
    }

    fn metric_observe(
        &mut self,
        inst: InstrumentId,
        value: MetricValue,
        attrs: Vec<(compact_str::CompactString, AttrValue)>,
    ) {
        let now = self.clock_unix_ms();
        self.metrics.observe(inst, value, attrs, now);
    }

    fn metric_collect(&mut self) -> Vec<MetricData> {
        let now = self.clock_unix_ms();
        self.metrics.collect(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_stdlib::Host;

    #[test]
    fn is_a_host() {
        fn assert_host(_: &dyn Host) {}
        assert_host(&BrowserHost::new());
    }

    #[test]
    fn real_leaves_answer_and_sandbox_leaves_stay_deterministic() {
        let mut host = BrowserHost::new();
        // Entropy and wall clock are real (native fallbacks here; JS imports on wasm).
        assert_ne!(host.entropy_u64(), host.entropy_u64());
        assert!(host.clock_unix_ms() > 1_600_000_000_000);
        // The seeded PRNG and logical monotonic keep the every-host rules.
        host.rng_seed(7);
        let a = host.rng_int(0, 100).expect("int");
        host.rng_seed(7);
        assert_eq!(host.rng_int(0, 100).expect("int"), a);
        assert_eq!(host.clock_monotonic(), 0);
        // The fs is in-memory.
        host.fs_write("f.txt", "x").expect("write");
        assert_eq!(host.fs_read("f.txt").expect("read"), "x");
        // Inbound serving and exec are honest errors.
        assert!(host.net_listen("0.0.0.0:80").is_err());
        assert!(host.os_exec("ls", &[]).is_err());
    }

    #[test]
    fn native_fetch_fallback_is_an_honest_error() {
        let mut host = BrowserHost::new();
        let err = host
            .net_fetch(NetRequest {
                method: "GET".into(),
                url: "https://example.com".into(),
                headers: Vec::new(),
                body: Vec::new(),
                timeout_ms: None,
            })
            .expect_err("no network natively");
        assert!(err.message().contains("JS embedding"), "{}", err.message());
    }
}
