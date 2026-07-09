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

#[cfg(feature = "telemetry")]
mod telemetry;

use compact_str::CompactString;
use noeta_stdlib::net::accept_outcome;
use noeta_stdlib::{
    AttrValue, Clock, Entropy, Env, ErrorKind, ExternIo, FileReader, FileSystem, Ids, InstrumentId,
    InstrumentKind, LogRecord, Logging, MetricData, MetricStore, MetricValue, Metrics, NativeOut,
    NetRequest, NetResponse, Network, P2p, ReadSource, RealBody, Rng, SpanData, SpanEvent, SpanId,
    SpanKind, SpanStatus, StdError, Tracing, TraceContext,
};
// Only the outbound-client (`ring-http-client`) path builds an `ExternBox` response body.
#[cfg(feature = "ring-http-client")]
use noeta_stdlib::ExternBox;
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
    /// Open inbound connections awaiting a reply, keyed by conn id. Shared (`Arc`) into every accept
    /// descriptor (which inserts an accepted stream) and reply descriptor (which removes and writes
    /// it), both running on the executor's runtime.
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    /// Monotonic, thread-safe id source for `conns` (accept futures run concurrently on the
    /// executor).
    next_conn: Arc<AtomicU64>,
    /// The program's argument vector reported through `args.all()` (M2.2). Defaults to the real
    /// process argv (`std::env::args()`), which is exactly what a shipped `noeta build --exe` binary
    /// wants when invoked directly. `noeta run app.noe -- a b c` overrides it via
    /// [`RealHost::with_args`] with `[app.noe, a, b, c]`, so a program sees the identical argv whether
    /// run from source or shipped as an executable — the toolchain's own `noeta run` prefix never
    /// leaks into the program.
    args: Vec<String>,
    /// The p2p broker (p2p P1/P2): the same in-process pub/sub log as the sandbox. Real p2p
    /// transport (p2panda gossip, cross-node) is P3; until then the real host is a single-node
    /// loopback broker, so `noeta run` of a p2p program works locally without a network.
    p2p: noeta_stdlib::P2pBroker,
    /// Telemetry state (native OTEL): in-flight spans + (behind the `telemetry` feature) the OTLP
    /// exporter and its span buffer. Per-isolate, like everything else on `RealHost`.
    tel: RealTelemetry,
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
            conns: Arc::new(Mutex::new(HashMap::new())),
            next_conn: Arc::new(AtomicU64::new(0)),
            args: std::env::args().collect(),
            p2p: noeta_stdlib::P2pBroker::default(),
            tel: RealTelemetry::new(),
        })
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
) -> Result<NetResponse, StdError> {
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .map_err(|_| io_error(format!("invalid HTTP method `{}`", request.method)))?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if !request.body.is_empty() {
        builder = builder.body(request.body);
    }
    let url = request.url;
    let response = builder
        .send()
        .await
        .map_err(|e| io_error(format!("request to `{url}` failed: {e}")))?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = response
        .bytes()
        .await
        .map_err(|e| {
            io_error(format!(
                "reading the response body from `{url}` failed: {e}"
            ))
        })?
        .to_vec();
    Ok(NetResponse {
        status,
        headers,
        body,
    })
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
        let response = rt.block_on(reqwest_fetch(&self.client, self.request.clone()))?;
        Ok(NativeOut::Extern(ExternBox::new(response)))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let client = self.client.clone();
        let request = self.request.clone();
        Some(RealBody::Async(Box::pin(async move {
            let response = reqwest_fetch(&client, request).await?;
            Ok(NativeOut::Extern(ExternBox::new(response)))
        })))
    }
}

impl Network for RealHost {
    /// Perform the request over the real network, blocking on the host's runtime (the sync
    /// `http.*` surface). A transport failure is an `ErrorKind::Io` error; an HTTP error *status*
    /// comes back as an ordinary [`NetResponse`].
    fn net_fetch(&mut self, request: NetRequest) -> Result<NetResponse, StdError> {
        #[cfg(feature = "ring-http-client")]
        {
            self.runtime.block_on(reqwest_fetch(&self.http, request))
        }
        // Without the `ring-http-client` ring the outbound client isn't linked. A program that never imports
        // `std.http` never reaches here; a build that stripped the ring while the program *did* use it
        // would be a footprint-selection bug, so this is a hard error rather than a silent no-op.
        #[cfg(not(feature = "ring-http-client"))]
        {
            let _ = request;
            Err(io_error(
                "the HTTP client (std.http) is not built into this binary".to_string(),
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
        let listener = std::net::TcpListener::bind(addr)
            .map_err(|e| io_error(format!("cannot bind `{addr}`: {e}")))?;
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

impl P2p for RealHost {
    // p2p P1/P2: the same in-process pub/sub broker as the sandbox (single-node loopback). Real
    // p2panda gossip transport, cross-node delivery, and a genuine async subscription future are
    // P3 — at which point this becomes the non-deterministic, CLI-only side (like the real Network
    // client).
    fn p2p_publish(&mut self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        self.p2p.publish(topic, message);
        Ok(())
    }

    fn p2p_poll(&mut self, topic: &str) -> Result<Option<Vec<u8>>, StdError> {
        Ok(self.p2p.poll_default(topic))
    }

    fn p2p_subscribe(&mut self, topic: &str) -> u64 {
        self.p2p.subscribe(topic)
    }

    fn p2p_poll_sub(&mut self, sub: u64) -> Result<Option<Vec<u8>>, StdError> {
        Ok(self.p2p.poll_sub(sub))
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
        std::env::var(key).ok()
    }

    fn env_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        keys.sort();
        keys
    }

    fn args(&self) -> Vec<String> {
        self.args.clone()
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

    fn metric_get_or_create(&mut self, name: &str, unit: &str, kind: InstrumentKind) -> InstrumentId {
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
            (!self.tel.buffer.is_empty())
                .then(|| (endpoint, e.headers.clone(), e.request_body(&self.tel.buffer)))
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
    /// run explicitly (`cargo test -p noeta-runtime -- --ignored real_host_fetches`) when network
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
        path.push("noeta_runtime_roundtrip_test.txt");
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
        root.push("noeta_runtime_dirs_test");
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
        path.push("noeta_runtime_lazy_read_test.txt");
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
