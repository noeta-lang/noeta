//! The M2 runtime: a per-isolate async runtime and the **real host** (`RealHost`).
//!
//! This is the non-sandbox side of the M2.1 [`noeta_stdlib::Host`] split. Where
//! `SandboxHost` is the deterministic in-memory world the conformance differential
//! always runs, `RealHost` is what the CLI/REPL/server give a real program: it reads
//! the **real process environment/args** and performs **real-disk** file IO. It is
//! never used in the differential, so determinism is not its job.
//!
//! ## Async-first IO internals
//!
//! Disk IO runs on a per-isolate `tokio` `current_thread` runtime (matching the
//! shared-nothing isolate model — no work-stealing across heaps). The `Host` surface
//! is still synchronous, so each IO method drives its future to completion with
//! `block_on` **at the leaf** and returns a plain value: no opcode and no surface
//! syntax knows about futures yet. Building the IO path on `tokio` now means the
//! later `async`/`await` surface (a separate M2 pass) is an additive change — these
//! `tokio::fs` calls get `await`ed instead of `block_on`-ed — rather than a rewrite.
//!
//! The filesystem is a real-disk surface (paths relative to the process working
//! directory) with a directory hierarchy (M2.5): `fs_list_dir`/`fs_mkdir`/`fs_is_dir`
//! map onto `tokio::fs`'s `read_dir`/`create_dir_all` and `Path::is_dir`, mirroring the
//! sandbox VFS's directory model.

pub mod executor;
pub use executor::RealExecutor;
/// Re-exported for drivers that arm [`RealExecutor::set_wake`] (the CLI's hot-reload watcher)
/// without taking their own tokio dependency.
pub use tokio::sync::Notify;

/// The process-wide **shutdown wake** (server-hmr S0): a [`Notify`] every [`RealExecutor`] arms
/// itself with at construction, so a blocked executor — an idle server parked on its accept — can
/// be roused. The CLI's SIGINT handler `notify_one()`s this after setting the serve shutdown flag,
/// so a graceful drain begins immediately instead of at the next connection. Lazily created; a
/// program that never serves never notifies it, so the wake stays inert (a never-fired `notified()`
/// branch in the executor's select).
pub fn shutdown_notify() -> std::sync::Arc<Notify> {
    static NOTIFY: std::sync::OnceLock<std::sync::Arc<Notify>> = std::sync::OnceLock::new();
    NOTIFY
        .get_or_init(|| std::sync::Arc::new(Notify::new()))
        .clone()
}

#[cfg(feature = "telemetry")]
mod telemetry;
mod ws;
// Real p2p transport (p2p P3) — the p2panda-net node + its group encryption — lives in the leaf
// crate `noeta-para-p2p-net` (para-namespace F2b). Post-F2b `RealHost` no longer owns it (the
// `para.p2p` extension does), so this crate depends on neither the leaf crate nor the p2panda tree;
// `RealHost` only carries the p2p app-id config it surfaces via `P2pProvider::real_p2p`.

use compact_str::CompactString;
use noeta_stdlib::net::accept_outcome;
use noeta_stdlib::{
    AttrValue, Clock, Entropy, Env, ErrorKind, ExecResult, ExternBox, ExternIo, FileReader,
    FileSystem, Ids, InstrumentId, InstrumentKind, LogRecord, Logging, MetricData, MetricStore,
    MetricValue, Metrics, NativeOut, NetError, NetErrorKind, NetRequest, NetResponse, Network, Os,
    ReadSource, RealBody, Rng, SpanData, SpanEvent, SpanId, SpanKind, SpanStatus, StdError,
    TraceContext, Tracing,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;

/// The real host: real process `env`/`args` and real-disk file IO over a per-isolate
/// `tokio` runtime. Constructed by the CLI/REPL/server, never by the differential.
#[derive(Debug)]
pub struct RealHost {
    /// One `current_thread` runtime per host/isolate; disk IO is driven on it and
    /// blocked-on at the call boundary (no async surface yet).
    runtime: Runtime,
    /// The user-facing PRNG and the monotonic clock stay deterministic (seeded / logical)
    /// even on the real host: `random.seed(n)` must make `random.*` a pure function of `n`
    /// everywhere, and `monotonic` is an ordering device, not wall time. Real time and real
    /// entropy are the *separate* capabilities added by id-entropy U1 — `clock_unix_ms`
    /// (`SystemTime`) and `Entropy` (OS entropy) — so ids get real randomness without
    /// making the user's seeded stream lie.
    rng: u64,
    clock: u64,
    /// The next `id.next_id()` value — deterministic and sequential on every host (see
    /// [`noeta_stdlib::host::Ids`]).
    ids: u64,
    /// Open lazy read streams (P-LAZY), keyed by the id handed to the file handle. A read handle
    /// pulls a line at a time via `fs_read_more` rather than buffering the whole file at open. An
    /// entry is dropped at EOF; any handle closed before EOF leaves its stream here until the host
    /// (the isolate) is dropped — acceptable for the short-lived CLI runs that use `RealHost`.
    readers: HashMap<u64, BufReader<File>>,
    /// Monotonic id source for `readers`.
    next_reader_id: u64,
    /// The real HTTP client for the `Network` capability (http arc H1). Cheap to clone (an inner
    /// `Arc`), holds the connection pool; built once per host. Requests are driven on `runtime`.
    /// Present only under `ring-http-client` (DCE Axis B): a binary that never imports `std.http` links
    /// without reqwest, so this field — and the TLS stack behind it — is gated out.
    #[cfg(feature = "ring-http-client")]
    http: reqwest::Client,
    /// Inbound listeners (http-server S1), keyed by the id `net_listen` hands out. Each holds a
    /// bound socket; the tokio listener is created lazily on the executor's runtime at first accept
    /// (so all server socket IO stays on the runtime that drives the accept future, never this
    /// host's runtime — a `TcpStream` is bound to the runtime it was accepted on).
    servers: HashMap<u64, Arc<ServerState>>,
    /// Monotonic id source for `servers`.
    next_listener: u64,
    /// A **pre-bound** listener a multi-core worker inherits (server-hmr S1): `noeta serve
    /// --parallel N` binds the socket once and hands each worker a `try_clone`d fd, so
    /// `net_listen(addr)` returns this dup instead of binding again (a second bind on the same
    /// address fails without `SO_REUSEPORT`). The kernel load-balances `accept()` across the
    /// workers' shared listening socket. `None` for an ordinary single-worker host.
    prebound: Option<std::net::TcpListener>,
    /// Open inbound connections awaiting a reply, keyed by conn id. Shared (`Arc`) into every accept
    /// descriptor (which inserts an accepted stream) and reply descriptor (which removes and writes
    /// it), both running on the executor's runtime.
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    /// Monotonic, thread-safe id source for `conns` (accept futures run concurrently on the
    /// executor).
    next_conn: Arc<AtomicU64>,
    /// Upgraded websocket connections (server-hmr L0b): the split halves behind async locks,
    /// shared into every ws descriptor. A conn moves here from `conns` at upgrade and leaves on
    /// close/peer-EOF.
    ws_conns: ws::WsConns,
    /// The program's argument vector reported through `args.all()` (M2.2). Defaults to the real
    /// process argv (`std::env::args()`), which is exactly what a shipped `noeta build --exe` binary
    /// wants when invoked directly. `noeta run app.noe -- a b c` overrides it via
    /// [`RealHost::with_args`] with `[app.noe, a, b, c]`, so a program sees the identical argv whether
    /// run from source or shipped as an executable — the toolchain's own `noeta run` prefix never
    /// leaks into the program.
    args: Vec<String>,
    /// The program's `env.set` writes (stdlib-gaps): an overlay consulted before the real process
    /// environment, and layered into `os.exec` children. `std::env::set_var` is unsafe with live
    /// threads (isolates are OS threads), so the real environment is never mutated.
    env_overlay: HashMap<String, String>,
    /// Spawned child processes (process-handle arc), keyed by the handle id `os_spawn` hands out.
    /// Each holds the live `Child` plus the drain threads capturing its piped stdout/stderr, so a
    /// high-output child never deadlocks on a full pipe while the program polls it.
    procs: HashMap<u64, ChildProc>,
    /// Monotonic id source for `procs`.
    next_proc: u64,
    /// Telemetry state (native OTEL): in-flight spans + (behind the `telemetry` feature) the OTLP
    /// exporter and its span buffer. Per-isolate, like everything else on `RealHost`.
    tel: RealTelemetry,
    /// The application namespace the `para.p2p` extension's real node keys its persistent identity /
    /// store dir on (p2p P3.4) — the toolchain sets it to the running program's package name so two
    /// different Noeta apps never share one p2p dir. `None` ⇒ the node's own default (exe stem /
    /// env). Post-F2b `RealHost` no longer owns a p2p transport at all; it only *carries* this config
    /// and surfaces it (with "real networking permitted") through [`P2pProvider::real_p2p`], so the
    /// extension can build the node.
    p2p_app_id: Option<String>,
}

/// `RealHost`'s telemetry state. In-flight spans are tracked even with the `telemetry` feature off
/// (so `tel_span_context` and parenting stay correct); only the OTLP export path is gated.
#[derive(Debug)]
struct RealTelemetry {
    /// Opaque span-handle counter (the [`SpanId`] map key). The W3C `span_id` *bytes* are real
    /// entropy, drawn per span — the handle is just a local key.
    next_span: u64,
    /// In-flight spans by handle, ended entries removed.
    live: HashMap<SpanId, SpanData>,
    /// Remote-interned contexts (T5d): pseudo-handles for contexts that arrived over a channel /
    /// isolate boundary. Read by `tel_span_context`; everything else no-ops on them. Bounded by
    /// live seeds (`tel_release_remote` frees replaced ones).
    remote: HashMap<SpanId, TraceContext>,
    /// The configured OTLP exporter, or `None` when no endpoint is set (the null sink).
    #[cfg(feature = "telemetry")]
    exporter: Option<telemetry::OtlpExporter>,
    /// Ended spans awaiting export (flushed at [`telemetry::FLUSH_THRESHOLD`] or on teardown).
    #[cfg(feature = "telemetry")]
    buffer: Vec<SpanData>,
    /// Emitted log records awaiting export (flushed at [`telemetry::FLUSH_THRESHOLD`] or teardown).
    #[cfg(feature = "telemetry")]
    logs_buffer: Vec<LogRecord>,
    /// Host-side metric aggregation (native OTEL Phase M) — the same shared store the sandbox uses,
    /// so aggregation is byte-identical. Behind an `Arc<Mutex<_>>` because the **periodic export
    /// reader** (M2) reads it from a background thread while the interpreter records into it.
    metrics: Arc<Mutex<MetricStore>>,
    /// The metrics periodic-export reader (M2): a background thread that snapshots [`Self::metrics`]
    /// every `OTEL_METRIC_EXPORT_INTERVAL` and POSTs it, plus a final flush on shutdown. Lazily
    /// spawned on the first instrument creation when metrics are enabled; `None` otherwise. Dropping
    /// it signals shutdown and joins (the thread does the final export).
    #[cfg(feature = "telemetry")]
    metric_exporter: Option<MetricExporter>,
}

impl RealTelemetry {
    fn new() -> RealTelemetry {
        RealTelemetry {
            next_span: 1,
            live: HashMap::new(),
            remote: HashMap::new(),
            #[cfg(feature = "telemetry")]
            exporter: telemetry::OtlpExporter::from_env(),
            #[cfg(feature = "telemetry")]
            buffer: Vec::new(),
            #[cfg(feature = "telemetry")]
            logs_buffer: Vec::new(),
            metrics: Arc::new(Mutex::new(MetricStore::default())),
            #[cfg(feature = "telemetry")]
            metric_exporter: None,
        }
    }
}

/// One inbound listener's shared state. The socket is bound at `net_listen` (runtime-free, via
/// `std::net`); the tokio listener is built once on first accept — on the executor's runtime — and
/// reused for every subsequent accept.
#[derive(Debug)]
struct ServerState {
    /// The bound, non-blocking std socket, taken when the tokio listener is first built.
    pending_std: std::sync::Mutex<Option<std::net::TcpListener>>,
    /// The tokio listener, created lazily on the executor runtime, then shared across accepts.
    tokio: std::sync::Mutex<Option<Arc<TcpListener>>>,
}

impl RealHost {
    /// Build a real host with its own `current_thread` runtime. Fails only if the OS
    /// refuses to create the runtime.
    pub fn new() -> std::io::Result<RealHost> {
        // `enable_all` (was time-free): reqwest/hyper need the IO driver, and request timeouts the
        // time driver. `tokio::fs` is unaffected.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(RealHost {
            runtime,
            rng: noeta_stdlib::random::DEFAULT_SEED,
            clock: 0,
            ids: 1,
            readers: HashMap::new(),
            next_reader_id: 0,
            #[cfg(feature = "ring-http-client")]
            http: reqwest::Client::new(),
            servers: HashMap::new(),
            next_listener: 0,
            prebound: None,
            conns: Arc::new(Mutex::new(HashMap::new())),
            next_conn: Arc::new(AtomicU64::new(0)),
            ws_conns: Arc::new(Mutex::new(HashMap::new())),
            args: std::env::args().collect(),
            env_overlay: HashMap::new(),
            procs: HashMap::new(),
            next_proc: 1,
            tel: RealTelemetry::new(),
            p2p_app_id: None,
        })
    }

    /// Seed a **pre-bound listener** (server-hmr S1): the multi-core `noeta serve --parallel`
    /// driver binds once and gives each worker a `try_clone`d socket, so the worker's
    /// `net_listen` adopts this fd rather than binding the address a second time.
    pub fn with_prebound_listener(mut self, listener: std::net::TcpListener) -> RealHost {
        self.prebound = Some(listener);
        self
    }

    /// Set the p2p application namespace ([`Self::p2p_app_id`]) — the running program's package
    /// name, so the `para.p2p` extension's node keeps its persistent state under its own dir rather
    /// than a shared one. Builder style so the per-isolate factory clones it into each host. Surfaced
    /// to the extension via [`P2pProvider::real_p2p`].
    pub fn with_p2p_app(mut self, app_id: Option<String>) -> RealHost {
        self.p2p_app_id = app_id;
        self
    }

    /// Override the argument vector this host reports through `args.all()`. Used by
    /// `noeta run … -- <args>` to present the program with `[<script path>, <pass-through args…>]`
    /// in place of the toolchain's process argv (`noeta run …`). Consuming-builder style so the
    /// per-isolate factory can clone the vector into each fresh host it mints.
    pub fn with_args(mut self, args: Vec<String>) -> RealHost {
        self.args = args;
        self
    }

    /// The sorted base names of the entries in directory `dir` — the real-disk analogue of the
    /// sandbox `Vfs::list`/`list_dir`. Shared by `fs_list` (cwd) and `fs_list_dir` (any path).
    fn read_dir_names(&self, dir: &str) -> Result<Vec<String>, StdError> {
        self.runtime.block_on(async {
            let mut entries = tokio::fs::read_dir(dir)
                .await
                .map_err(|e| io_error(format!("cannot list directory `{dir}`: {e}")))?;
            let mut names = Vec::new();
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|e| io_error(format!("cannot read directory entry: {e}")))?
            {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            Ok(names)
        })
    }
}

/// Build an `ErrorKind::Io` (`E0021`) error from a real-disk failure.
fn io_error(message: String) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message,
    }
}

impl FileReader for RealHost {
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError> {
        // P-LAZY: stream the file instead of snapshotting it. Open it now (so a missing file is the
        // same IO error as the old eager `fs_read`), register a buffered reader, and hand the handle
        // an id to pull lines from — so a large file is never read past the cursor.
        let file = self
            .runtime
            .block_on(File::open(path))
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}")))?;
        let id = self.next_reader_id;
        self.next_reader_id += 1;
        self.readers.insert(id, BufReader::new(file));
        Ok(ReadSource::Lazy(id))
    }

    fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, StdError> {
        use tokio::io::AsyncBufReadExt;
        let Some(reader) = self.readers.get_mut(&id) else {
            // The stream was already drained (dropped at EOF); nothing more to give.
            return Ok(None);
        };
        let mut line = String::new();
        let read = self
            .runtime
            .block_on(reader.read_line(&mut line))
            .map_err(|e| io_error(format!("cannot read line: {e}")))?;
        if read == 0 {
            // EOF — drop the stream so its descriptor is released promptly.
            self.readers.remove(&id);
            Ok(None)
        } else {
            // `read_line` keeps the trailing `\n`; the handle splits on it, so pass it through.
            Ok(Some(line))
        }
    }
}

impl FileSystem for RealHost {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.runtime
            .block_on(tokio::fs::write(path, content))
            .map_err(|e| io_error(format!("cannot write `{path}`: {e}")))
    }

    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.runtime.block_on(async {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|e| io_error(format!("cannot open `{path}` for append: {e}")))?;
            file.write_all(content.as_bytes())
                .await
                .map_err(|e| io_error(format!("cannot append to `{path}`: {e}")))
        })
    }

    fn fs_read(&self, path: &str) -> Result<String, StdError> {
        self.runtime
            .block_on(tokio::fs::read_to_string(path))
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}")))
    }

    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError> {
        self.runtime
            .block_on(tokio::fs::write(path, data))
            .map_err(|e| io_error(format!("cannot write `{path}`: {e}")))
    }

    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError> {
        self.runtime
            .block_on(tokio::fs::read(path))
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}")))
    }

    fn fs_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError> {
        if !std::path::Path::new(path).exists() {
            return Ok(false);
        }
        self.runtime
            .block_on(tokio::fs::remove_file(path))
            .map(|()| true)
            .map_err(|e| io_error(format!("cannot remove `{path}`: {e}")))
    }

    fn fs_list(&self) -> Result<Vec<String>, StdError> {
        self.read_dir_names(".")
    }

    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError> {
        // A directory's immediate children, by base name — the sandbox `list_dir` shape.
        let dir = if dir.is_empty() { "." } else { dir };
        self.read_dir_names(dir)
    }

    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError> {
        self.runtime
            .block_on(tokio::fs::create_dir_all(path))
            .map_err(|e| io_error(format!("cannot create directory `{path}`: {e}")))
    }

    fn fs_is_dir(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }
}

/// Perform an HTTP request with reqwest, collecting the whole [`NetResponse`]. Shared by the sync
/// path ([`RealHost::net_fetch`] via `block_on`) and the async path ([`HttpIo`] spawned on the
/// executor's runtime), so both build the request and read the response identically.
#[cfg(feature = "ring-http-client")]
async fn reqwest_fetch(
    client: &reqwest::Client,
    request: NetRequest,
) -> Result<NetResponse, NetError> {
    let url = request.url.clone();
    let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
        NetError::new(
            NetErrorKind::InvalidUrl,
            &url,
            format!("invalid HTTP method `{}`", request.method),
        )
    })?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body);
    }
    let response = builder.send().await.map_err(|e| net_error(&url, &e))?;
    let status = response.status().as_u16();
    // reqwest tracks the redirect chain, so this is the URL the body actually came from — the
    // correct base for resolving a relative `Link`/`Location`, which the request URL would not be.
    let final_url = response.url().to_string();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    // A failure *reading the body* is a truncated/mangled response, not a failure to connect —
    // classify through the same reqwest predicates, which report it as a decode/body error.
    let body = response
        .bytes()
        .await
        .map_err(|e| net_error(&url, &e))?
        .to_vec();
    Ok(NetResponse {
        status,
        headers,
        body,
        url: final_url,
    })
}

/// Classify a reqwest failure into the seam's [`NetErrorKind`] (http arc H6).
///
/// reqwest exposes the class as a set of predicates rather than an enum, and it does not surface
/// DNS or TLS distinctly — both arrive as `is_connect`, with the distinguishing detail only in the
/// source chain. We walk that chain for the two markers worth separating: a resolver failure is
/// transient and worth retrying, a certificate failure never is. Anything unrecognised stays
/// `Connect`, which is the conservative (retryable) reading of a connect-class failure.
#[cfg(feature = "ring-http-client")]
fn net_error(url: &str, error: &reqwest::Error) -> NetError {
    let kind = if error.is_timeout() {
        NetErrorKind::Timeout
    } else if error.is_connect() {
        match connect_cause(error) {
            Some(cause) => cause,
            None => NetErrorKind::Connect,
        }
    } else if error.is_body() || error.is_decode() {
        NetErrorKind::Protocol
    } else if error.is_builder() || error.is_request() {
        NetErrorKind::InvalidUrl
    } else {
        NetErrorKind::Other
    };
    NetError::new(kind, url, error.to_string())
}

/// Distinguish DNS and TLS inside a connect-class reqwest failure by inspecting the source chain's
/// rendered text — the only signal reqwest exposes without depending on hyper/rustls error types
/// directly, which would couple this crate to their versions.
#[cfg(feature = "ring-http-client")]
fn connect_cause(error: &reqwest::Error) -> Option<NetErrorKind> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string().to_ascii_lowercase();
        if text.contains("dns") || text.contains("name or service not known") {
            return Some(NetErrorKind::Dns);
        }
        if text.contains("certificate") || text.contains("tls") || text.contains("handshake") {
            return Some(NetErrorKind::Tls);
        }
        source = std::error::Error::source(cause);
    }
    None
}

/// The real host's async network descriptor (http arc H3): its real body is a genuine reqwest
/// future driven on the executor's runtime, so `http.*_async` requests fan out concurrently.
#[cfg(feature = "ring-http-client")]
#[derive(Debug)]
struct HttpIo {
    request: NetRequest,
    /// Cloned from `RealHost::http` — a reqwest `Client` is a cheap `Arc` handle, used across the
    /// executor's runtime here (reqwest is runtime-agnostic; connections bind lazily to whichever
    /// runtime drives the future).
    client: reqwest::Client,
}

#[cfg(feature = "ring-http-client")]
impl ExternIo for HttpIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        // Only reached if some executor lacks a real body path; the real executor uses `run_real`
        // and the sandbox uses the default `NetFetchIo`. Perform it on a throwaway runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| io_error(format!("cannot start a runtime for the request: {e}")))?;
        Ok(noeta_stdlib::net::fetch_outcome(rt.block_on(
            reqwest_fetch(&self.client, self.request.clone()),
        )))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let client = self.client.clone();
        let request = self.request.clone();
        Some(RealBody::Async(Box::pin(async move {
            Ok(noeta_stdlib::net::fetch_outcome(
                reqwest_fetch(&client, request).await,
            ))
        })))
    }
}

impl Network for RealHost {
    /// Perform the request over the real network, blocking on the host's runtime (the sync
    /// `http.*` surface). A transport failure is a classified [`NetError`]; an HTTP error *status*
    /// comes back as an ordinary [`NetResponse`].
    fn net_fetch(&mut self, request: NetRequest) -> Result<NetResponse, NetError> {
        #[cfg(feature = "ring-http-client")]
        {
            self.runtime.block_on(reqwest_fetch(&self.http, request))
        }
        // Without the `ring-http-client` ring the outbound client isn't linked. A program that never imports
        // `std.http` never reaches here; a build that stripped the ring while the program *did* use it
        // would be a footprint-selection bug, so this is a hard error rather than a silent no-op.
        #[cfg(not(feature = "ring-http-client"))]
        {
            Err(NetError::new(
                NetErrorKind::Other,
                request.url,
                "the HTTP client (std.http) is not built into this binary",
            ))
        }
    }

    /// The async `http.*_async` surface: hand out a reqwest-backed descriptor whose real body runs
    /// concurrently on the executor's runtime (overriding the default serial-at-spawn descriptor).
    /// Under `ring-http-client` only; without it the trait default routes async fetches through
    /// [`Self::net_fetch`] (the stub above), matching the sync path.
    #[cfg(feature = "ring-http-client")]
    fn net_spawn(&self, request: NetRequest) -> Box<dyn ExternIo> {
        Box::new(HttpIo {
            request,
            client: self.http.clone(),
        })
    }

    /// Bind a real listener (http-server S1). Uses `std::net` so the bind is runtime-free — the
    /// tokio listener is attached lazily on the executor runtime at first accept. The socket is set
    /// non-blocking (required by `TcpListener::from_std`), and `SO_REUSEADDR` is the std default.
    fn net_listen(&mut self, addr: &str) -> Result<u64, StdError> {
        // A multi-core worker (server-hmr S1) adopts the pre-bound, `try_clone`d listener instead
        // of binding — the socket is already listening on `addr`, and a second bind would fail.
        let listener = match self.prebound.take() {
            Some(prebound) => prebound,
            None => std::net::TcpListener::bind(addr)
                .map_err(|e| io_error(format!("cannot bind `{addr}`: {e}")))?,
        };
        listener
            .set_nonblocking(true)
            .map_err(|e| io_error(format!("cannot configure `{addr}`: {e}")))?;
        let id = self.next_listener;
        self.next_listener += 1;
        self.servers.insert(
            id,
            Arc::new(ServerState {
                pending_std: std::sync::Mutex::new(Some(listener)),
                tokio: std::sync::Mutex::new(None),
            }),
        );
        Ok(id)
    }

    /// A genuine async accept: `TcpListener::accept().await` on the executor's runtime, so a slow
    /// handler yields cooperatively while the next connection is awaited (overriding the default
    /// serial descriptor).
    fn net_accept(&self, listener: u64) -> Box<dyn ExternIo> {
        Box::new(RealAcceptIo {
            state: self.servers.get(&listener).cloned(),
            conns: self.conns.clone(),
            next_conn: self.next_conn.clone(),
        })
    }

    /// A genuine async reply: write the response to the held stream and close it, on the executor's
    /// runtime (the runtime that accepted the connection).
    fn net_reply(&self, conn: u64, response: NetResponse) -> Box<dyn ExternIo> {
        Box::new(RealReplyIo {
            conns: self.conns.clone(),
            conn,
            response: Some(response),
        })
    }

    /// The real host always overrides [`Network::net_accept`] with the async descriptor, so the
    /// synchronous fallback is unreachable (like `fs_read_more` on the sandbox).
    fn net_accept_next(&mut self, _listener: u64) -> Result<Option<(u64, NetRequest)>, StdError> {
        unreachable!("RealHost serves via the async net_accept descriptor, never the sync fallback")
    }

    /// The real host always overrides [`Network::net_reply`] with the async descriptor.
    fn net_reply_now(&mut self, _conn: u64, _response: NetResponse) -> Result<(), StdError> {
        unreachable!("RealHost replies via the async net_reply descriptor, never the sync fallback")
    }

    // --- Websocket hijack (server-hmr L0b): the async descriptors; the sync fallbacks are never
    // reached (same contract as accept/reply above).

    fn net_ws_upgrade(&self, conn: u64, key: String) -> Box<dyn ExternIo> {
        Box::new(ws::RealWsUpgradeIo {
            conns: self.conns.clone(),
            ws_conns: self.ws_conns.clone(),
            conn,
            key: Some(key),
        })
    }

    fn net_ws_recv(&self, conn: u64) -> Box<dyn ExternIo> {
        Box::new(ws::RealWsRecvIo {
            ws_conns: self.ws_conns.clone(),
            conn,
        })
    }

    fn net_ws_send(&self, conn: u64, text: String) -> Box<dyn ExternIo> {
        Box::new(ws::RealWsSendIo {
            ws_conns: self.ws_conns.clone(),
            conn,
            text: Some(text),
        })
    }

    fn net_ws_close(&self, conn: u64) -> Box<dyn ExternIo> {
        Box::new(ws::RealWsCloseIo {
            ws_conns: self.ws_conns.clone(),
            conn,
        })
    }

    fn net_ws_upgrade_now(&mut self, _conn: u64, _key: &str) -> Result<(), StdError> {
        unreachable!("RealHost upgrades via the async descriptor, never the sync fallback")
    }

    fn net_ws_recv_next(&mut self, _conn: u64) -> Result<Option<String>, StdError> {
        unreachable!("RealHost receives via the async descriptor, never the sync fallback")
    }

    fn net_ws_send_now(&mut self, _conn: u64, _text: &str) -> Result<(), StdError> {
        unreachable!("RealHost sends via the async descriptor, never the sync fallback")
    }

    fn net_ws_close_now(&mut self, _conn: u64) -> Result<(), StdError> {
        unreachable!("RealHost closes via the async descriptor, never the sync fallback")
    }
}

/// The real host's async accept descriptor (http-server S1): its body attaches the tokio listener
/// on first use, accepts one connection on the executor's runtime, reads the request off it, and
/// parks the stream in the shared `conns` map keyed by a fresh conn id for the reply to pick up.
#[derive(Debug)]
struct RealAcceptIo {
    /// The listener's shared state — `None` if the id was never bound (a defensive miss → Io error).
    state: Option<Arc<ServerState>>,
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    next_conn: Arc<AtomicU64>,
}

impl ExternIo for RealAcceptIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        // Never hit: the real executor calls `run_real`, and only the real host builds this
        // descriptor. If some executor lacked a real-body path, accept still needs a live runtime,
        // so surface an error rather than silently degrade.
        Err(io_error(
            "async accept requires the real executor's runtime".to_string(),
        ))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let state = self.state.clone();
        let conns = self.conns.clone();
        let next_conn = self.next_conn.clone();
        Some(RealBody::Async(Box::pin(async move {
            let state =
                state.ok_or_else(|| io_error("accept on an unbound listener".to_string()))?;
            // Attach the tokio listener once, on this (the executor's) runtime, then reuse it.
            let listener = {
                let mut slot = state.tokio.lock().unwrap();
                if slot.is_none() {
                    let std = state
                        .pending_std
                        .lock()
                        .unwrap()
                        .take()
                        .ok_or_else(|| io_error("listener already attached".to_string()))?;
                    let tl = TcpListener::from_std(std)
                        .map_err(|e| io_error(format!("cannot attach listener: {e}")))?;
                    *slot = Some(Arc::new(tl));
                }
                slot.as_ref().unwrap().clone()
            };
            // A connection that breaks before a complete request (a port scan, a load balancer's
            // TCP health probe, a client that gave up) is not an event the program observes: drop
            // it and keep accepting. Only listener-level failure surfaces — propagating the wire
            // error was a real bug (any bare connect-and-close killed `noeta serve` with E0021).
            loop {
                let (mut stream, _peer) = listener
                    .accept()
                    .await
                    .map_err(|e| io_error(format!("accept failed: {e}")))?;
                let Ok(request) = read_request(&mut stream).await else {
                    continue;
                };
                let conn = next_conn.fetch_add(1, Ordering::Relaxed);
                conns.lock().unwrap().insert(conn, stream);
                return Ok(accept_outcome(Some((conn, request))));
            }
        })))
    }
}

/// The real host's async reply descriptor: pull the held stream for `conn`, write the response, and
/// close — on the executor's runtime.
#[derive(Debug)]
struct RealReplyIo {
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    conn: u64,
    response: Option<NetResponse>,
}

impl ExternIo for RealReplyIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Err(io_error(
            "async reply requires the real executor's runtime".to_string(),
        ))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let conns = self.conns.clone();
        let conn = self.conn;
        let response = self.response.take();
        Some(RealBody::Async(Box::pin(async move {
            let mut stream = conns
                .lock()
                .unwrap()
                .remove(&conn)
                .ok_or_else(|| io_error(format!("reply on a closed connection {conn}")))?;
            let response =
                response.ok_or_else(|| io_error("reply descriptor run twice".to_string()))?;
            write_response(&mut stream, &response).await?;
            // Best-effort graceful close; the client has the full framed response by now.
            let _ = stream.shutdown().await;
            Ok(NativeOut::Unit)
        })))
    }
}

/// Read one HTTP/1.1 request off `stream`: the request line, headers to the blank line, and a body
/// of exactly `Content-Length` bytes (0 if absent). A minimal, dependency-free parser — enough for
/// the JSON/form APIs this server targets; chunked transfer-encoding and pipelining are follow-ons.
async fn read_request(stream: &mut TcpStream) -> Result<NetRequest, StdError> {
    // Read until the header terminator `\r\n\r\n`, keeping any body bytes that arrive in the same
    // read for after the headers.
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let mut chunk = [0u8; 1024];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| io_error(format!("reading request failed: {e}")))?;
        if n == 0 {
            return Err(io_error(
                "client closed before a complete request".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_string();
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }

    // Body: bytes already buffered past the headers, plus any remaining up to Content-Length.
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| io_error(format!("reading body failed: {e}")))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(NetRequest {
        method,
        url: target,
        headers,
        body,
    })
}

/// Serialize `response` as an HTTP/1.1 response and write it to `stream`. Always frames the body
/// with `Content-Length` and requests connection close (one request per connection in v1).
async fn write_response(stream: &mut TcpStream, response: &NetResponse) -> Result<(), StdError> {
    let reason = reason_phrase(response.status);
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    let mut has_length = false;
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_length = true;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_length {
        head.push_str(&format!("content-length: {}\r\n", response.body.len()));
    }
    head.push_str("connection: close\r\n\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| io_error(format!("writing response head failed: {e}")))?;
    stream
        .write_all(&response.body)
        .await
        .map_err(|e| io_error(format!("writing response body failed: {e}")))?;
    stream
        .flush()
        .await
        .map_err(|e| io_error(format!("flushing response failed: {e}")))
}

/// A compact reason phrase for the common statuses; anything else is `"Status"` (the phrase is
/// advisory in HTTP/1.1 — clients key off the numeric code).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

/// The first index of `needle` in `haystack`, or `None` — a tiny substring search for the header
/// terminator (no dependency, and the search window is a single small request head).
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

impl Rng for RealHost {
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

impl Clock for RealHost {
    fn clock_monotonic(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.clock = self.clock.saturating_add(ms.max(0) as u64);
    }

    fn clock_unix_ms(&mut self) -> u64 {
        // A clock before 1970 would need a deliberately broken host; saturate to 0 rather
        // than panic in that case.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl Ids for RealHost {
    fn id_next(&mut self) -> u64 {
        let id = self.ids;
        self.ids += 1;
        id
    }
}

// Post-F2b `RealHost` no longer owns a p2p transport (the p2panda node moved into the `para.p2p`
// extension). It only declares, through the optional `P2pProvider` seam, that real peer networking
// is permitted here and with what app namespace — the extension reads this to build the real node
// (vs the deterministic loopback broker on a host that returns `None`).
impl noeta_stdlib::host::P2pProvider for RealHost {
    fn real_p2p(&self) -> Option<noeta_stdlib::host::RealP2pConfig> {
        Some(noeta_stdlib::host::RealP2pConfig {
            app_id: self.p2p_app_id.clone(),
        })
    }
}

impl Entropy for RealHost {
    fn entropy_u64(&mut self) -> u64 {
        // OS entropy. `getrandom` only fails on platforms/configurations with no entropy
        // source at all — an environment where ids (and TLS, and everything else) cannot
        // work; failing loudly beats silently degrading to a guessable stream.
        getrandom::u64().expect("the OS entropy source is unavailable")
    }
}

impl Env for RealHost {
    fn env_get(&self, key: &str) -> Option<String> {
        // The overlay (this program's `env.set` writes) shadows the inherited environment.
        self.env_overlay
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
    }

    fn env_set(&mut self, key: &str, value: &str) {
        self.env_overlay.insert(key.to_string(), value.to_string());
    }

    fn env_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        keys.extend(self.env_overlay.keys().cloned());
        keys.sort();
        keys.dedup();
        keys
    }

    fn args(&self) -> Vec<String> {
        self.args.clone()
    }
}

/// Run `command` with `args` through `std::process::Command`, capturing status and output — the
/// shared body of the sync `os.exec` leaf and the async descriptor's blocking body. `overlay` is
/// the program's `env.set` writes, layered onto the inherited environment so children observe
/// them. A command that cannot be *started* is an `Io` error; one that runs and fails is a
/// successful [`ExecResult`] carrying its non-zero status (`-1` when killed by a signal).
fn real_exec(
    command: &str,
    args: &[String],
    overlay: &HashMap<String, String>,
) -> Result<ExecResult, StdError> {
    let output = std::process::Command::new(command)
        .args(args)
        .envs(overlay)
        .output()
        .map_err(|e| io_error(format!("exec: cannot run `{command}`: {e}")))?;
    Ok(ExecResult {
        status: i64::from(output.status.code().unwrap_or(-1)),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// A growing capture buffer for one of a child's output pipes, shared between the draining thread
/// (which appends and marks EOF) and the interpreter thread (which reads it, whole at `wait` or
/// line-by-line at `read_line`). The condvar lets a blocking `read_line` sleep until more bytes
/// arrive or the stream ends, instead of busy-polling.
#[derive(Debug, Default)]
struct StreamBuf {
    data: Vec<u8>,
    eof: bool,
}

#[derive(Debug, Default)]
struct SharedStream {
    buf: Mutex<StreamBuf>,
    more: std::sync::Condvar,
}

impl SharedStream {
    /// The next line (without its trailing newline), from `cursor`, blocking until a full line is
    /// buffered or the stream ends. `None` at end of output; a final unterminated line is returned
    /// once (like `str::lines`). `cursor` advances past the line (and its newline).
    fn read_line(&self, cursor: &mut usize) -> Option<String> {
        let mut buf = self.buf.lock().unwrap();
        loop {
            if let Some(rel) = buf.data[*cursor..].iter().position(|&b| b == b'\n') {
                let end = *cursor + rel;
                let line = String::from_utf8_lossy(&buf.data[*cursor..end]).into_owned();
                *cursor = end + 1;
                return Some(line);
            }
            if buf.eof {
                if *cursor < buf.data.len() {
                    let line = String::from_utf8_lossy(&buf.data[*cursor..]).into_owned();
                    *cursor = buf.data.len();
                    return Some(line);
                }
                return None;
            }
            buf = self.more.wait(buf).unwrap();
        }
    }

    /// Up to `count` characters from `cursor`, blocking only until at least one character is
    /// available, then returning up to `count` of them (POSIX `read` shape); `None` at end of
    /// output. Decodes the valid-UTF-8 prefix from the cursor — a multi-byte character split across
    /// a drain chunk waits for its continuation. `count <= 0` yields the empty string.
    fn read(&self, cursor: &mut usize, count: usize) -> Option<String> {
        let mut buf = self.buf.lock().unwrap();
        loop {
            // Exhausted (nothing left and the stream ended) → end of output.
            if *cursor >= buf.data.len() && buf.eof {
                return None;
            }
            let rest = &buf.data[*cursor..];
            // The complete-character prefix (a trailing partial UTF-8 sequence is excluded until
            // its bytes arrive).
            let valid = match std::str::from_utf8(rest) {
                Ok(s) => s,
                Err(e) => std::str::from_utf8(&rest[..e.valid_up_to()]).unwrap_or(""),
            };
            if !valid.is_empty() || count == 0 {
                let chunk: String = valid.chars().take(count).collect();
                *cursor += chunk.len();
                return Some(chunk);
            }
            if buf.eof {
                return None;
            }
            buf = self.more.wait(buf).unwrap();
        }
    }

    /// The whole captured content decoded lossily — read at `wait` once the draining thread has
    /// finished (so `data` is complete).
    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap().data).into_owned()
    }
}

/// Read `pipe` to EOF, appending into `shared` and waking any blocked `read_line`. Runs on its own
/// thread per pipe so a chatty child never blocks on a full pipe buffer while the program
/// supervises it (the classic capture-without-draining deadlock).
fn drain_pipe(mut pipe: impl std::io::Read, shared: Arc<SharedStream>) {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                shared
                    .buf
                    .lock()
                    .unwrap()
                    .data
                    .extend_from_slice(&chunk[..n]);
                shared.more.notify_all();
            }
        }
    }
    shared.buf.lock().unwrap().eof = true;
    shared.more.notify_all();
}

/// Which output stream a streaming read targets — selects the buffer and cursor in `stream_read`.
#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// A spawned, still-running child (process-handle + streaming arcs). Both output pipes drain on
/// background threads into shared buffers (see [`SharedStream`]): `read_line` streams stdout
/// incrementally while the child runs, `wait` returns the whole captured output at exit, and
/// `write`/`close_stdin` feed the child's stdin. `result` caches the outcome so `wait`/`try_wait`
/// are idempotent.
#[derive(Debug)]
struct ChildProc {
    /// The OS child, `None` once ownership has been handed to a `wait_async` background waiter (the
    /// waiter reaps it and publishes the outcome through [`ChildProc::awaiting`]).
    child: Option<std::process::Child>,
    /// The child's OS pid, captured at spawn (stays valid after reap for `pid()`/`signal()`).
    pid: i64,
    stdout: Arc<SharedStream>,
    stderr: Arc<SharedStream>,
    /// The stdout/stderr drain threads, joined when the child is reaped.
    stdout_join: Option<std::thread::JoinHandle<()>>,
    stderr_join: Option<std::thread::JoinHandle<()>>,
    /// `read_line`/`read`'s cursor into the stdout buffer — independent of the whole-output `wait`.
    stdout_cursor: usize,
    /// `read_err_line`'s cursor into the stderr buffer.
    stderr_cursor: usize,
    /// The child's stdin pipe; `None` once closed. Dropping it signals EOF to the child.
    stdin: Option<std::process::ChildStdin>,
    /// The reaped outcome, cached so a second `wait`/`try_wait` returns it without re-waiting.
    result: Option<ExecResult>,
    /// Set once `wait_async` detaches the child onto a background waiter: a synchronous `wait`/
    /// `try_wait` on the same handle then observes the outcome through this shared slot rather than
    /// the (now moved-out) [`ChildProc::child`].
    awaiting: Option<Arc<WaitSlot>>,
}

impl ChildProc {
    /// Build the [`ExecResult`] for an exited child: join the drain threads (so the buffers are
    /// complete), snapshot the full captured output, and pair it with `status`. Caches and returns.
    fn reap(&mut self, status: std::process::ExitStatus) -> ExecResult {
        if let Some(h) = self.stdout_join.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_join.take() {
            let _ = h.join();
        }
        let result = ExecResult {
            status: i64::from(status.code().unwrap_or(-1)),
            stdout: self.stdout.snapshot(),
            stderr: self.stderr.snapshot(),
        };
        self.result = Some(result.clone());
        result
    }
}

/// The shared exit slot a `wait_async` background waiter publishes to (process-signals arc): the
/// blocking body reaps the detached child and stores the outcome here, so a later synchronous
/// `wait`/`try_wait` on the same handle can block on / poll it. The single hop between the off-thread
/// waiter and the isolate's synchronous process API.
#[derive(Debug, Default)]
struct WaitSlot {
    done: Mutex<Option<ExecResult>>,
    ready: std::sync::Condvar,
}

/// The real host's `wait_async` descriptor: it owns the detached child (and its drain threads), so
/// its blocking body reaps on the runtime's blocking pool — genuinely overlapping the isolate — then
/// publishes the outcome to the shared [`WaitSlot`] before yielding it. The `Ready` variant short-
/// circuits an already-reaped child (nothing to wait on).
struct RealProcWaitIo {
    handle: u64,
    ready: Option<ExecResult>,
    child: Option<std::process::Child>,
    stdout: Arc<SharedStream>,
    stderr: Arc<SharedStream>,
    stdout_join: Option<std::thread::JoinHandle<()>>,
    stderr_join: Option<std::thread::JoinHandle<()>>,
    slot: Option<Arc<WaitSlot>>,
}

impl std::fmt::Debug for RealProcWaitIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealProcWaitIo")
            .field("handle", &self.handle)
            .field("ready", &self.ready.is_some())
            .finish()
    }
}

impl RealProcWaitIo {
    /// Reap the detached child: wait for exit, join the drain threads, snapshot the captured output,
    /// publish the outcome to the shared slot, and return it. Runs on the blocking pool (real host)
    /// or synchronously at spawn (fallback) — either way off the child's owning registry.
    fn reap(&mut self) -> Result<NativeOut, StdError> {
        if let Some(result) = self.ready.take() {
            return Ok(NativeOut::Extern(ExternBox::new(result)));
        }
        let mut child = self
            .child
            .take()
            .expect("a pending RealProcWaitIo always owns the child");
        let status = child.wait().map_err(|e| io_error(format!("wait: {e}")))?;
        if let Some(h) = self.stdout_join.take() {
            let _ = h.join();
        }
        if let Some(h) = self.stderr_join.take() {
            let _ = h.join();
        }
        let result = ExecResult {
            status: i64::from(status.code().unwrap_or(-1)),
            stdout: self.stdout.snapshot(),
            stderr: self.stderr.snapshot(),
        };
        if let Some(slot) = self.slot.take() {
            *slot.done.lock().unwrap() = Some(result.clone());
            slot.ready.notify_all();
        }
        Ok(NativeOut::Extern(ExternBox::new(result)))
    }
}

impl ExternIo for RealProcWaitIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        self.reap()
    }

    fn run_real(&mut self) -> Option<RealBody> {
        if let Some(result) = self.ready.take() {
            return Some(RealBody::Blocking(Box::new(move || {
                Ok(NativeOut::Extern(ExternBox::new(result)))
            })));
        }
        let mut child = self.child.take();
        let stdout = Arc::clone(&self.stdout);
        let stderr = Arc::clone(&self.stderr);
        let mut stdout_join = self.stdout_join.take();
        let mut stderr_join = self.stderr_join.take();
        let slot = self.slot.take();
        Some(RealBody::Blocking(Box::new(move || {
            let mut child = child.take().expect("pending waiter owns the child");
            let status = child.wait().map_err(|e| io_error(format!("wait: {e}")))?;
            if let Some(h) = stdout_join.take() {
                let _ = h.join();
            }
            if let Some(h) = stderr_join.take() {
                let _ = h.join();
            }
            let result = ExecResult {
                status: i64::from(status.code().unwrap_or(-1)),
                stdout: stdout.snapshot(),
                stderr: stderr.snapshot(),
            };
            if let Some(slot) = slot {
                *slot.done.lock().unwrap() = Some(result.clone());
                slot.ready.notify_all();
            }
            Ok(NativeOut::Extern(ExternBox::new(result)))
        })))
    }
}

/// A `wait_async` descriptor for a child a *previous* `wait_async` already detached: it owns no
/// child, only a clone of the shared [`WaitSlot`], and blocks on it — so a second (or racing)
/// `wait_async` resolves to the same outcome the first waiter publishes.
#[derive(Debug)]
struct SlotWaitIo {
    slot: Arc<WaitSlot>,
}

fn await_slot(slot: &WaitSlot) -> ExecResult {
    let mut done = slot.done.lock().unwrap();
    while done.is_none() {
        done = slot.ready.wait(done).unwrap();
    }
    done.clone().expect("slot filled")
}

impl ExternIo for SlotWaitIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        Ok(NativeOut::Extern(ExternBox::new(await_slot(&self.slot))))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let slot = Arc::clone(&self.slot);
        Some(RealBody::Blocking(Box::new(move || {
            Ok(NativeOut::Extern(ExternBox::new(await_slot(&slot))))
        })))
    }
}

/// Map a Noeta [`noeta_stdlib::os::Signal`] to `nix`'s (Unix only).
#[cfg(unix)]
fn nix_signal(signal: noeta_stdlib::os::Signal) -> nix::sys::signal::Signal {
    use nix::sys::signal::Signal as N;
    use noeta_stdlib::os::Signal;
    match signal {
        Signal::Hup => N::SIGHUP,
        Signal::Int => N::SIGINT,
        Signal::Quit => N::SIGQUIT,
        Signal::Kill => N::SIGKILL,
        Signal::Usr1 => N::SIGUSR1,
        Signal::Usr2 => N::SIGUSR2,
        Signal::Term => N::SIGTERM,
        Signal::Cont => N::SIGCONT,
        Signal::Stop => N::SIGSTOP,
    }
}

/// Deliver `signal` to a spawned child. On Unix this is `kill(2)` (via `nix`'s safe wrapper) on the
/// child's pid (working whether or not the child has been detached onto a `wait_async` waiter);
/// `ESRCH` (already exited) is a harmless no-op, matching `kill`'s idempotence. On non-Unix hosts
/// only `Kill`/`Term` are expressible (a forceful terminate); any other signal is an `Io` error.
#[cfg(unix)]
fn deliver_signal(proc: &mut ChildProc, signal: noeta_stdlib::os::Signal) -> Result<(), StdError> {
    let pid = nix::unistd::Pid::from_raw(proc.pid as i32);
    match nix::sys::signal::kill(pid, nix_signal(signal)) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(e) => Err(io_error(format!("signal SIG{}: {e}", signal.label()))),
    }
}

#[cfg(not(unix))]
fn deliver_signal(proc: &mut ChildProc, signal: noeta_stdlib::os::Signal) -> Result<(), StdError> {
    use noeta_stdlib::os::Signal;
    match signal {
        Signal::Kill | Signal::Term => {
            if let Some(child) = proc.child.as_mut() {
                let _ = child.kill();
            }
            Ok(())
        }
        other => Err(io_error(format!(
            "signal SIG{}: only KILL/TERM are supported on this platform",
            other.label()
        ))),
    }
}

/// Forcefully signal a detached child by pid — the fallback `os_proc_kill` uses once the child has
/// been moved onto a `wait_async` waiter (Unix only; a no-op elsewhere).
#[cfg(unix)]
fn kill_pid(pid: i64, signal: noeta_stdlib::os::Signal) {
    // Best-effort: ESRCH (already exited) and any other error are ignored (idempotent kill).
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix_signal(signal));
}

#[cfg(not(unix))]
fn kill_pid(_pid: i64, _signal: noeta_stdlib::os::Signal) {}

/// The real host's async exec descriptor: its real body runs the subprocess on the executor's
/// blocking pool (a `Command::output` is exactly a blocking op), so `os.exec_async` genuinely
/// overlaps with the program — the exec analogue of `FsIo`'s real bodies.
#[derive(Debug)]
struct RealExecIo {
    command: String,
    args: Vec<String>,
    /// A snapshot of the host's env overlay at spawn (the descriptor outlives the borrow).
    overlay: HashMap<String, String>,
}

impl ExternIo for RealExecIo {
    fn run_sync(&mut self, _host: &mut dyn noeta_stdlib::Host) -> Result<NativeOut, StdError> {
        let result = real_exec(&self.command, &self.args, &self.overlay)?;
        Ok(NativeOut::Extern(ExternBox::new(result)))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let command = std::mem::take(&mut self.command);
        let args = std::mem::take(&mut self.args);
        let overlay = std::mem::take(&mut self.overlay);
        Some(RealBody::Blocking(Box::new(move || {
            let result = real_exec(&command, &args, &overlay)?;
            Ok(NativeOut::Extern(ExternBox::new(result)))
        })))
    }
}

impl RealHost {
    /// Shared body of the streaming reads: clone the target stream's `Arc` and cursor out under a
    /// short `procs` borrow, run `read` **without** holding it (the read may block on the condvar,
    /// and the drain thread must stay free to append), then write the advanced cursor back.
    fn stream_read(
        &mut self,
        handle: u64,
        which: Stream,
        read: impl FnOnce(&SharedStream, &mut usize) -> Option<String>,
    ) -> Result<Option<String>, StdError> {
        let (stream, mut cursor) = {
            let proc = self
                .procs
                .get(&handle)
                .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
            match which {
                Stream::Stdout => (Arc::clone(&proc.stdout), proc.stdout_cursor),
                Stream::Stderr => (Arc::clone(&proc.stderr), proc.stderr_cursor),
            }
        };
        let out = read(&stream, &mut cursor);
        if let Some(proc) = self.procs.get_mut(&handle) {
            match which {
                Stream::Stdout => proc.stdout_cursor = cursor,
                Stream::Stderr => proc.stderr_cursor = cursor,
            }
        }
        Ok(out)
    }
}

impl Os for RealHost {
    fn os_platform(&self) -> String {
        std::env::consts::OS.to_string()
    }

    fn os_arch(&self) -> String {
        std::env::consts::ARCH.to_string()
    }

    fn os_hostname(&self) -> String {
        gethostname::gethostname().to_string_lossy().into_owned()
    }

    fn os_cpus(&self) -> i64 {
        std::thread::available_parallelism().map_or(1, |n| n.get() as i64)
    }

    fn os_cwd(&self) -> String {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn os_pid(&self) -> i64 {
        i64::from(std::process::id())
    }

    fn os_exec(&mut self, command: &str, args: &[String]) -> Result<ExecResult, StdError> {
        real_exec(command, args, &self.env_overlay)
    }

    fn os_exec_spawn(&self, command: String, args: Vec<String>) -> Box<dyn ExternIo> {
        Box::new(RealExecIo {
            command,
            args,
            overlay: self.env_overlay.clone(),
        })
    }

    fn os_spawn(&mut self, command: &str, args: &[String]) -> Result<u64, StdError> {
        use std::process::Stdio;
        let mut child = std::process::Command::new(command)
            .args(args)
            .envs(&self.env_overlay)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io_error(format!("spawn: cannot start `{command}`: {e}")))?;
        let pid = i64::from(child.id());
        // Drain both pipes on background threads into shared buffers, so a chatty child never
        // blocks on a full pipe while the program supervises it, and `read_line` can stream stdout.
        let stdout = Arc::new(SharedStream::default());
        let stderr = Arc::new(SharedStream::default());
        let stdout_join = child.stdout.take().map(|pipe| {
            let shared = Arc::clone(&stdout);
            std::thread::spawn(move || drain_pipe(pipe, shared))
        });
        let stderr_join = child.stderr.take().map(|pipe| {
            let shared = Arc::clone(&stderr);
            std::thread::spawn(move || drain_pipe(pipe, shared))
        });
        let stdin = child.stdin.take();
        let id = self.next_proc;
        self.next_proc += 1;
        self.procs.insert(
            id,
            ChildProc {
                child: Some(child),
                pid,
                stdout,
                stderr,
                stdout_join,
                stderr_join,
                stdout_cursor: 0,
                stderr_cursor: 0,
                stdin,
                result: None,
                awaiting: None,
            },
        );
        Ok(id)
    }

    fn os_proc_pid(&self, handle: u64) -> Option<i64> {
        self.procs.get(&handle).map(|p| p.pid)
    }

    fn os_proc_wait(&mut self, handle: u64) -> Result<ExecResult, StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
        if let Some(result) = &proc.result {
            return Ok(result.clone());
        }
        // `wait_async` detached the child onto a background waiter: block on its shared slot until
        // the waiter publishes the outcome, then cache it here so this stays idempotent.
        if let Some(slot) = proc.awaiting.clone() {
            let mut done = slot.done.lock().unwrap();
            while done.is_none() {
                done = slot.ready.wait(done).unwrap();
            }
            let result = done.clone().expect("slot filled");
            proc.result = Some(result.clone());
            return Ok(result);
        }
        let status = proc
            .child
            .as_mut()
            .expect("a not-yet-detached child is always present")
            .wait()
            .map_err(|e| io_error(format!("wait: {e}")))?;
        Ok(proc.reap(status))
    }

    fn os_proc_try_wait(&mut self, handle: u64) -> Result<Option<ExecResult>, StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
        if let Some(result) = &proc.result {
            return Ok(Some(result.clone()));
        }
        // Detached onto a `wait_async` waiter: poll its slot without blocking.
        if let Some(slot) = proc.awaiting.clone() {
            let result = slot.done.lock().unwrap().clone();
            if let Some(result) = &result {
                proc.result = Some(result.clone());
            }
            return Ok(result);
        }
        match proc
            .child
            .as_mut()
            .expect("a not-yet-detached child is always present")
            .try_wait()
            .map_err(|e| io_error(format!("try_wait: {e}")))?
        {
            Some(status) => Ok(Some(proc.reap(status))),
            None => Ok(None),
        }
    }

    fn os_proc_kill(&mut self, handle: u64) -> Result<(), StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
        match proc.child.as_mut() {
            // Killing an already-exited child returns `InvalidInput`; that is a harmless no-op here.
            Some(child) => {
                let _ = child.kill();
            }
            // The child was detached onto a `wait_async` waiter; signal it by pid instead.
            None => kill_pid(proc.pid, noeta_stdlib::os::Signal::Kill),
        }
        Ok(())
    }

    fn os_proc_signal(
        &mut self,
        handle: u64,
        signal: noeta_stdlib::os::Signal,
    ) -> Result<(), StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
        deliver_signal(proc, signal)
    }

    fn os_proc_wait_spawn(&mut self, handle: u64) -> Box<dyn ExternIo> {
        let Some(proc) = self.procs.get_mut(&handle) else {
            // Unknown handle: a descriptor that surfaces the error when awaited (mirrors the
            // default `ProcWaitIo`, whose `run_sync` errors through `os_proc_wait`).
            return Box::new(noeta_stdlib::os::ProcWaitIo { handle });
        };
        // Already reaped: hand back the cached outcome, nothing to wait on.
        if let Some(result) = proc.result.clone() {
            return Box::new(RealProcWaitIo {
                handle,
                ready: Some(result),
                child: None,
                stdout: Arc::clone(&proc.stdout),
                stderr: Arc::clone(&proc.stderr),
                stdout_join: None,
                stderr_join: None,
                slot: None,
            });
        }
        // Already detached by an earlier `wait_async`: block on the shared slot rather than the
        // (moved-out) child, so a second `wait_async` still resolves.
        if let Some(slot) = proc.awaiting.clone() {
            return Box::new(SlotWaitIo { slot });
        }
        // First `wait_async`: detach the child + its drain threads onto the descriptor and leave a
        // shared slot behind so synchronous `wait`/`try_wait` still observe the outcome.
        let slot = Arc::new(WaitSlot::default());
        proc.awaiting = Some(Arc::clone(&slot));
        Box::new(RealProcWaitIo {
            handle,
            ready: None,
            child: proc.child.take(),
            stdout: Arc::clone(&proc.stdout),
            stderr: Arc::clone(&proc.stderr),
            stdout_join: proc.stdout_join.take(),
            stderr_join: proc.stderr_join.take(),
            slot: Some(slot),
        })
    }

    fn os_proc_read_line(&mut self, handle: u64) -> Result<Option<String>, StdError> {
        self.stream_read(handle, Stream::Stdout, |s, c| s.read_line(c))
    }

    fn os_proc_read(&mut self, handle: u64, count: i64) -> Result<Option<String>, StdError> {
        let want = count.max(0) as usize;
        self.stream_read(handle, Stream::Stdout, move |s, c| s.read(c, want))
    }

    fn os_proc_read_stderr_line(&mut self, handle: u64) -> Result<Option<String>, StdError> {
        self.stream_read(handle, Stream::Stderr, |s, c| s.read_line(c))
    }

    fn os_proc_write_stdin(&mut self, handle: u64, data: &str) -> Result<(), StdError> {
        use std::io::Write;
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
        let stdin = proc
            .stdin
            .as_mut()
            .ok_or_else(|| io_error("write: the child's stdin is closed".to_string()))?;
        stdin
            .write_all(data.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| io_error(format!("write: {e}")))
    }

    fn os_proc_close_stdin(&mut self, handle: u64) -> Result<(), StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_stdlib::os::unknown_process_error(handle))?;
        // Dropping the `ChildStdin` closes the pipe (EOF to the child). Idempotent.
        proc.stdin = None;
        Ok(())
    }
}

impl Tracing for RealHost {
    fn tel_enabled(&self) -> bool {
        // On when an OTLP **traces** endpoint is configured (and the `telemetry` feature is compiled
        // in); otherwise the null sink, and auto-instrumentation short-circuits. Traces can be turned
        // off independently of logs/metrics via `OTEL_TRACES_EXPORTER=none`.
        #[cfg(feature = "telemetry")]
        {
            self.tel
                .exporter
                .as_ref()
                .is_some_and(|e| e.traces_endpoint.is_some())
        }
        #[cfg(not(feature = "telemetry"))]
        {
            false
        }
    }

    fn tel_span_start(
        &mut self,
        name: &str,
        kind: SpanKind,
        parent: Option<TraceContext>,
    ) -> SpanId {
        let handle = self.tel.next_span;
        self.tel.next_span += 1;
        // Real entropy for the W3C ids (a root mints a fresh 16-byte trace id; a child inherits its
        // parent's). Predictable/colliding ids across processes would be wrong for real traces.
        let span_id = self.entropy_u64().to_be_bytes();
        let trace_id = match parent {
            Some(p) => p.trace_id,
            None => {
                let hi = self.entropy_u64().to_be_bytes();
                let lo = self.entropy_u64().to_be_bytes();
                let mut t = [0u8; 16];
                t[..8].copy_from_slice(&hi);
                t[8..].copy_from_slice(&lo);
                t
            }
        };
        let now = self.clock_unix_ms();
        let context = TraceContext {
            trace_id,
            span_id,
            sampled: true,
        };
        self.tel.live.insert(
            handle,
            SpanData {
                name: name.into(),
                kind,
                context,
                parent,
                start_unix_ms: now,
                end_unix_ms: None,
                attributes: Vec::new(),
                events: Vec::new(),
                status: SpanStatus::Unset,
            },
        );
        handle
    }

    fn tel_span_set_attr(&mut self, span: SpanId, key: &str, value: AttrValue) {
        if let Some(s) = self.tel.live.get_mut(&span) {
            match s.attributes.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value,
                None => s.attributes.push((key.into(), value)),
            }
        }
    }

    fn tel_span_add_event(
        &mut self,
        span: SpanId,
        name: &str,
        attrs: Vec<(CompactString, AttrValue)>,
    ) {
        let now = self.clock_unix_ms();
        if let Some(s) = self.tel.live.get_mut(&span) {
            s.events.push(SpanEvent {
                name: name.into(),
                unix_ms: now,
                attributes: attrs,
            });
        }
    }

    fn tel_span_set_status(&mut self, span: SpanId, status: SpanStatus) {
        if let Some(s) = self.tel.live.get_mut(&span) {
            s.status = status;
        }
    }

    fn tel_span_end(&mut self, span: SpanId) {
        let now = self.clock_unix_ms();
        if let Some(mut s) = self.tel.live.remove(&span) {
            s.end_unix_ms = Some(now);
            self.record_ended_span(s);
        }
    }

    fn tel_span_context(&mut self, span: SpanId) -> TraceContext {
        if let Some(remote) = self.tel.remote.get(&span) {
            return *remote;
        }
        self.tel.live.get(&span).map_or(
            TraceContext {
                trace_id: [0u8; 16],
                span_id: [0u8; 8],
                sampled: false,
            },
            |s| s.context,
        )
    }

    fn tel_intern_remote(&mut self, context: TraceContext) -> SpanId {
        // Ids share the span counter, so a remote handle can never collide with a live span.
        let id = self.tel.next_span;
        self.tel.next_span += 1;
        self.tel.remote.insert(id, context);
        id
    }

    fn tel_is_remote(&self, span: SpanId) -> bool {
        self.tel.remote.contains_key(&span)
    }

    fn tel_release_remote(&mut self, span: SpanId) {
        self.tel.remote.remove(&span);
    }
}

impl Logging for RealHost {
    fn tel_logs_enabled(&self) -> bool {
        // On when an OTLP **logs** endpoint is configured (and the `telemetry` feature is compiled
        // in). Independent of traces/metrics — one signal can be on while the others are off.
        #[cfg(feature = "telemetry")]
        {
            self.tel
                .exporter
                .as_ref()
                .is_some_and(|e| e.logs_endpoint.is_some())
        }
        #[cfg(not(feature = "telemetry"))]
        {
            false
        }
    }

    #[cfg(feature = "telemetry")]
    fn log_emit(&mut self, record: LogRecord) {
        // Null sink for logs — no logs endpoint (or no exporter at all).
        if self
            .tel
            .exporter
            .as_ref()
            .is_none_or(|e| e.logs_endpoint.is_none())
        {
            return;
        }
        self.tel.logs_buffer.push(record);
        if self.tel.logs_buffer.len() >= telemetry::FLUSH_THRESHOLD {
            self.flush_logs();
        }
    }

    #[cfg(not(feature = "telemetry"))]
    fn log_emit(&mut self, _record: LogRecord) {}
}

impl Metrics for RealHost {
    fn tel_metrics_enabled(&self) -> bool {
        // On when an OTLP **metrics** endpoint is configured (and the `telemetry` feature is compiled
        // in). Independent of traces/logs — one signal can be on while the others are off.
        #[cfg(feature = "telemetry")]
        {
            self.tel
                .exporter
                .as_ref()
                .is_some_and(|e| e.metrics_endpoint.is_some())
        }
        #[cfg(not(feature = "telemetry"))]
        {
            false
        }
    }

    fn metric_get_or_create(
        &mut self,
        name: &str,
        unit: &str,
        kind: InstrumentKind,
    ) -> InstrumentId {
        // The first instrument creation lazily starts the periodic export reader (real host + metrics
        // enabled only), so a program that never uses metrics pays for no thread.
        #[cfg(feature = "telemetry")]
        self.ensure_metric_exporter();
        self.tel
            .metrics
            .lock()
            .expect("metric store not poisoned")
            .get_or_create(name, unit, kind)
    }

    fn metric_observe(
        &mut self,
        inst: InstrumentId,
        value: MetricValue,
        attrs: Vec<(CompactString, AttrValue)>,
    ) {
        let now = self.clock_unix_ms();
        self.tel
            .metrics
            .lock()
            .expect("metric store not poisoned")
            .observe(inst, value, attrs, now);
    }

    fn metric_collect(&mut self) -> Vec<MetricData> {
        let now = self.clock_unix_ms();
        self.tel
            .metrics
            .lock()
            .expect("metric store not poisoned")
            .collect(now)
    }
}

impl RealHost {
    /// Route an ended span to the exporter (feature on + traces endpoint configured) or drop it
    /// (null sink / feature off). Buffers and flushes in minimal batches.
    #[cfg(feature = "telemetry")]
    fn record_ended_span(&mut self, span: SpanData) {
        // Null sink for traces — no traces endpoint (or no exporter at all).
        if self
            .tel
            .exporter
            .as_ref()
            .is_none_or(|e| e.traces_endpoint.is_none())
        {
            return;
        }
        self.tel.buffer.push(span);
        if self.tel.buffer.len() >= telemetry::FLUSH_THRESHOLD {
            self.flush_telemetry();
        }
    }

    #[cfg(not(feature = "telemetry"))]
    fn record_ended_span(&mut self, _span: SpanData) {}

    /// Export the buffered spans as one OTLP/JSON POST, on the host's runtime. Best-effort: an
    /// export failure never affects the program (telemetry is a side effect).
    #[cfg(feature = "telemetry")]
    fn flush_telemetry(&mut self) {
        let Some((endpoint, headers, body)) = self.tel.exporter.as_ref().and_then(|e| {
            let endpoint = e.traces_endpoint.clone()?;
            (!self.tel.buffer.is_empty()).then(|| {
                (
                    endpoint,
                    e.headers.clone(),
                    e.request_body(&self.tel.buffer),
                )
            })
        }) else {
            // No traces endpoint, or nothing buffered — drop any buffered spans and return.
            self.tel.buffer.clear();
            return;
        };
        self.otlp_post(&endpoint, &headers, &body);
        self.tel.buffer.clear();
    }

    /// Export the buffered log records as one OTLP/JSON POST to the logs endpoint. Best-effort,
    /// mirroring [`flush_telemetry`](Self::flush_telemetry) for the logs signal.
    #[cfg(feature = "telemetry")]
    fn flush_logs(&mut self) {
        let Some((endpoint, headers, body)) = self.tel.exporter.as_ref().and_then(|e| {
            let endpoint = e.logs_endpoint.clone()?;
            (!self.tel.logs_buffer.is_empty()).then(|| {
                (
                    endpoint,
                    e.headers.clone(),
                    e.logs_request_body(&self.tel.logs_buffer),
                )
            })
        }) else {
            self.tel.logs_buffer.clear();
            return;
        };
        self.otlp_post(&endpoint, &headers, &body);
        self.tel.logs_buffer.clear();
    }

    /// Lazily start the metrics periodic-export reader on the first instrument creation — real host,
    /// `telemetry` feature, and a configured metrics endpoint only. Idempotent (spawns at most once
    /// per host). A program that never creates an instrument never spawns the thread.
    #[cfg(feature = "telemetry")]
    fn ensure_metric_exporter(&mut self) {
        if self.tel.metric_exporter.is_some() {
            return;
        }
        let Some((endpoint, headers, service_name)) = self.tel.exporter.as_ref().and_then(|e| {
            Some((
                e.metrics_endpoint.clone()?,
                e.headers.clone(),
                e.service_name().to_string(),
            ))
        }) else {
            return; // metrics not configured — no reader
        };
        self.tel.metric_exporter = Some(spawn_metric_exporter(
            Arc::clone(&self.tel.metrics),
            self.http.clone(),
            endpoint,
            headers,
            service_name,
            metric_export_interval(),
        ));
    }

    /// POST one OTLP/JSON body to `url` with `headers`, on the host's runtime. Shared by all three
    /// signals' flush paths. Best-effort — an export failure never affects the program.
    #[cfg(feature = "telemetry")]
    fn otlp_post(&self, url: &str, headers: &[(String, String)], body: &serde_json::Value) {
        let http = self.http.clone();
        let _ = self.runtime.block_on(async move {
            let mut req = http.post(url).json(body);
            for (k, v) in headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.send().await
        });
    }
}

// Flush any buffered spans and logs when the host (isolate) is torn down. Runs before the `runtime`
// field drops (explicit `Drop::drop` precedes field drops), so `block_on` is still valid here.
#[cfg(feature = "telemetry")]
impl Drop for RealHost {
    fn drop(&mut self) {
        self.flush_telemetry();
        self.flush_logs();
        // Drop the periodic reader: it signals shutdown, does a final export of the cumulative
        // aggregation, and joins — the metrics teardown flush. (No reader ⇒ no metrics recorded.)
        self.tel.metric_exporter.take();
    }
}

/// The metrics export interval from `OTEL_METRIC_EXPORT_INTERVAL` (milliseconds, OTel spec), default
/// 60s. A non-positive / unparseable value falls back to the default.
#[cfg(feature = "telemetry")]
fn metric_export_interval() -> std::time::Duration {
    let ms = std::env::var("OTEL_METRIC_EXPORT_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .unwrap_or(60_000);
    std::time::Duration::from_millis(ms)
}

/// The metrics periodic-export reader (M2): a background thread snapshotting the shared
/// [`MetricStore`] on an interval and POSTing the cumulative OTLP/JSON, plus a final export on
/// shutdown. Real-host-only (the sandbox collects deterministically at teardown instead).
#[cfg(feature = "telemetry")]
struct MetricExporter {
    /// Sending (or dropping) this signals the reader to do its final export and stop.
    shutdown: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "telemetry")]
impl std::fmt::Debug for MetricExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricExporter").finish_non_exhaustive()
    }
}

#[cfg(feature = "telemetry")]
impl Drop for MetricExporter {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the periodic-export reader thread. It owns its own current-thread tokio runtime (the
/// interpreter's runtime belongs to the interpreter thread and cannot be driven here); the `reqwest`
/// client is cheaply cloneable and runtime-agnostic. Best-effort throughout — an export failure, or
/// a failure to build the runtime, never affects the program.
#[cfg(feature = "telemetry")]
fn spawn_metric_exporter(
    store: Arc<Mutex<MetricStore>>,
    http: reqwest::Client,
    endpoint: String,
    headers: Vec<(String, String)>,
    service_name: String,
    interval: std::time::Duration,
) -> MetricExporter {
    use std::sync::mpsc::{self, RecvTimeoutError};
    let (shutdown, rx) = mpsc::channel::<()>();
    let handle = std::thread::Builder::new()
        .name("noeta-otel-metrics".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            loop {
                // Wake on the interval (a periodic export) or on shutdown (the final export).
                let stop = !matches!(rx.recv_timeout(interval), Err(RecvTimeoutError::Timeout));
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                // Cumulative temporality: each export is the full running snapshot.
                let data = store
                    .lock()
                    .expect("metric store not poisoned")
                    .collect(now);
                if !data.is_empty() {
                    let body = telemetry::metrics_to_json(&data, &service_name);
                    let _ = rt.block_on(async {
                        let mut req = http.post(&endpoint).json(&body);
                        for (k, v) in &headers {
                            req = req.header(k.as_str(), v.as_str());
                        }
                        req.send().await
                    });
                }
                if stop {
                    break;
                }
            }
        })
        .ok();
    MetricExporter { shutdown, handle }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M2 — the metrics periodic-export reader actually POSTs the aggregated OTLP/JSON. Runs the
    /// reader against a local `TcpListener` (a stand-in collector) with a short interval, records a
    /// counter into the shared store, and asserts the received request is an OTLP metrics body
    /// carrying the aggregated value. A short client timeout keeps the shutdown/final export from
    /// blocking on the unaccepted connection, so `drop`ping the reader (shutdown + join) is prompt.
    #[cfg(feature = "telemetry")]
    #[test]
    fn periodic_reader_posts_the_aggregated_metrics() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}/v1/metrics");

        let store = Arc::new(Mutex::new(MetricStore::default()));
        {
            let mut s = store.lock().unwrap();
            let id = s.get_or_create("hits", "", InstrumentKind::Counter);
            s.observe(id, MetricValue::Int(5), Vec::new(), 1_000);
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(500))
            .build()
            .unwrap();
        let exporter = spawn_metric_exporter(
            Arc::clone(&store),
            client,
            endpoint,
            Vec::new(),
            "svc".to_string(),
            std::time::Duration::from_millis(50),
        );

        // Accept the first periodic tick's POST and read its request.
        let (mut stream, _) = listener.accept().expect("reader connects");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).expect("read the request");
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");

        drop(exporter); // shutdown + join (final export times out on the unaccepted socket)

        assert!(
            request.contains("resourceMetrics"),
            "posted an OTLP metrics body; got:\n{request}"
        );
        assert!(request.contains("hits"), "carries the instrument name");
        assert!(
            request.contains("\"asInt\":\"5\""),
            "carries the aggregated counter value"
        );
    }

    /// The real `Network` capability against a live endpoint. `#[ignore]` so CI stays hermetic —
    /// run explicitly (`cargo test -p noeta-host-real -- --ignored real_host_fetches`) when network
    /// is available. Locks the reqwest wiring: a real GET returns 200 with a body and headers.
    #[test]
    #[ignore = "hits the real network; run explicitly"]
    fn real_host_fetches_over_the_real_network() {
        let mut host = RealHost::new().unwrap();
        let resp = host
            .net_fetch(NetRequest {
                method: "GET".to_string(),
                url: "https://example.com/".to_string(),
                headers: vec![],
                body: vec![],
            })
            .expect("real fetch should succeed when online");
        assert_eq!(resp.status, 200);
        assert!(!resp.body.is_empty());
        assert!(resp.headers.iter().any(|(k, _)| k == "content-type"));
    }

    #[test]
    fn real_host_disk_round_trip() {
        let mut host = RealHost::new().unwrap();
        let mut path = std::env::temp_dir();
        path.push("noeta_host_real_roundtrip_test.txt");
        let path = path.to_string_lossy().into_owned();
        let _ = host.fs_remove(&path);

        host.fs_write(&path, "hello disk").unwrap();
        assert!(host.fs_exists(&path));
        assert_eq!(host.fs_read(&path).unwrap(), "hello disk");
        // Append grows the real file.
        host.fs_append(&path, " + more").unwrap();
        assert_eq!(host.fs_read(&path).unwrap(), "hello disk + more");
        assert!(host.fs_remove(&path).unwrap());
        assert!(!host.fs_exists(&path));
        // Reading a now-missing file is an Io error (E0021).
        assert_eq!(host.fs_read(&path).unwrap_err().kind, ErrorKind::Io);
        // Removing a missing file reports "did not exist", not an error.
        assert!(!host.fs_remove(&path).unwrap());
    }

    #[test]
    fn real_host_directory_hierarchy() {
        let mut host = RealHost::new().unwrap();
        let mut root = std::env::temp_dir();
        root.push("noeta_host_real_dirs_test");
        let root = root.to_string_lossy().into_owned();
        // Start clean.
        let _ = std::fs::remove_dir_all(&root);

        let nested = format!("{root}/logs/sub");
        host.fs_mkdir(&nested).unwrap();
        assert!(host.fs_is_dir(&format!("{root}/logs")));
        assert!(host.fs_is_dir(&nested));

        host.fs_write(&format!("{root}/logs/a.txt"), "1").unwrap();
        host.fs_write(&format!("{root}/logs/b.txt"), "2").unwrap();
        // A directory lists its immediate children, sorted by base name.
        assert_eq!(
            host.fs_list_dir(&format!("{root}/logs")).unwrap(),
            vec!["a.txt".to_string(), "b.txt".to_string(), "sub".to_string()]
        );
        // A file is not a directory.
        assert!(!host.fs_is_dir(&format!("{root}/logs/a.txt")));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn real_host_entropy_is_real_and_time_is_wall_time() {
        let mut host = RealHost::new().unwrap();
        // OS entropy: two draws colliding is a 2^-64 event — a failure here means the
        // capability is wired to something constant, not that we got unlucky.
        assert_ne!(host.entropy_u64(), host.entropy_u64());
        // Real wall time: past the sandbox's fixed 2026-01-01 epoch, and non-decreasing.
        let first = host.clock_unix_ms();
        assert!(first > noeta_stdlib::host::SANDBOX_EPOCH_MS);
        assert!(host.clock_unix_ms() >= first);
        // Unlike the sandbox, drawing entropy or reading wall time never touches the
        // deterministic monotonic counter.
        assert_eq!(host.clock_monotonic(), 0);
    }

    #[test]
    fn real_host_reads_lazily_through_a_file_handle() {
        let mut host = RealHost::new().unwrap();
        let mut path = std::env::temp_dir();
        path.push("noeta_host_real_lazy_read_test.txt");
        let path = path.to_string_lossy().into_owned();
        let _ = host.fs_remove(&path);

        // A multi-line file with a multibyte character and a final unterminated line.
        let content = "alpha\nbéta\ngamma";
        host.fs_write(&path, content).unwrap();

        // Opening for read now hands out a lazy stream, not a whole-file snapshot.
        let source = host.fs_open_read(&path).unwrap();
        assert!(matches!(source, ReadSource::Lazy(_)));

        // Streaming lines back through the handle matches the eager read, line for line; the
        // trailing unterminated line is yielded, and EOF is sticky.
        let mut handle = noeta_stdlib::FileHandle::open_read(&path, source);
        let mut lines = Vec::new();
        while let Some(line) = handle.read_line(&mut host).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, vec!["alpha", "béta", "gamma"]);

        // A fresh lazy handle, char-wise: `read(n)` counts characters across the lazily-pulled
        // lines (7 chars = "alpha\nb", stopping just before the multibyte `é`).
        let source = host.fs_open_read(&path).unwrap();
        let mut handle = noeta_stdlib::FileHandle::open_read(&path, source);
        assert_eq!(
            handle.read(7, &mut host).unwrap(),
            Some("alpha\nb".to_string())
        );

        assert!(host.fs_remove(&path).unwrap());
        // Opening a now-missing file lazily is the same IO error as the old eager read.
        assert_eq!(host.fs_open_read(&path).unwrap_err().kind, ErrorKind::Io);
    }

    #[test]
    #[ignore = "binds a real loopback socket; run explicitly"]
    fn real_host_serves_one_request_over_loopback() {
        use std::io::{Read, Write};

        let mut host = RealHost::new().unwrap();
        let id = host.net_listen("127.0.0.1:0").unwrap();
        // The socket is bound at net_listen; read the OS-assigned port to connect to.
        let addr = host.servers[&id]
            .pending_std
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .local_addr()
            .unwrap();

        // A runtime to drive the accept/reply futures on (stands in for RealExecutor's runtime).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // Client on its own OS thread: send a framed POST, read the whole response back.
        let client = std::thread::spawn(move || {
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            sock.write_all(b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
            let mut resp = String::new();
            sock.read_to_string(&mut resp).unwrap();
            resp
        });

        // Server: accept one connection (parses the request off the wire)…
        let mut accept = host.net_accept(id);
        let outcome = match accept.run_real() {
            Some(RealBody::Async(fut)) => rt.block_on(fut).unwrap(),
            _ => panic!("real accept must have an async body"),
        };
        assert!(
            matches!(outcome, NativeOut::Some(_)),
            "a connection arrived"
        );

        // …then reply on the first connection (conn id 0).
        let mut reply = host.net_reply(
            0,
            NetResponse {
                status: 200,
                headers: vec![],
                body: b"pong".to_vec(),
                url: String::new(),
            },
        );
        match reply.run_real() {
            Some(RealBody::Async(fut)) => rt.block_on(fut).unwrap(),
            _ => panic!("real reply must have an async body"),
        };

        let response = client.join().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response}");
        assert!(response.trim_end().ends_with("pong"), "got: {response}");
    }

    #[test]
    fn rng_is_deterministic_like_the_sandbox() {
        let mut a = RealHost::new().unwrap();
        let mut b = RealHost::new().unwrap();
        // Same default seed → identical streams (real host keeps PRNG deterministic).
        assert_eq!(a.rng_int(0, 1000).unwrap(), b.rng_int(0, 1000).unwrap());
    }
}
