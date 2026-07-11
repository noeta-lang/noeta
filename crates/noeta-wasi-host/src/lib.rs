//! The WASI host (P-WASM W1.0) — the third [`noeta_stdlib::Host`], for the `wasm32-wasip1`
//! runner.
//!
//! Where `SandboxHost` is the deterministic in-memory world and `RealHost` is the CLI's
//! tokio-backed real host, `WasiHost` is **real-but-synchronous**: it gives a program the world
//! WASI exposes — a preopened directory tree, the environment/args the embedder granted, wall
//! time, and real entropy (`random_get`) — through plain `std`, with no async runtime and no
//! threads (the VM runs its isolates cooperatively under this host). It compiles and behaves
//! identically on native targets, which is how its unit tests run; nothing here is
//! `cfg(target_family = "wasm")`-gated.
//!
//! Like `RealHost`, it is **never differential-tested** — the wasm oracle (W1.3) runs the *same
//! runner binary* on `SandboxHost` instead. Capabilities WASI p1 cannot provide are honest
//! runtime errors, not stubs that lie: outbound/inbound HTTP arrives with the `wasi:http`
//! component build (P-WASM W4), and process spawning does not exist on this target at all.
//!
//! The deterministic-vs-real split mirrors `RealHost` exactly: the user-facing PRNG and the
//! monotonic clock stay seeded/logical (`random.seed(n)` must make `random.*` a pure function of
//! `n` everywhere; `monotonic` is an ordering device), while `clock_unix_ms` and `Entropy` are
//! the real wall-time/entropy capabilities.

use compact_str::CompactString;
use noeta_stdlib::{
    AttrValue, Clock, Entropy, Env, ErrorKind, ExecResult, FileReader, FileSystem, Ids,
    InstrumentId, InstrumentKind, LogRecord, Logging, MetricData, MetricStore, MetricValue,
    Metrics, NetRequest, NetResponse, Network, Os, P2p, ReadSource, Rng, SpanId, SpanKind,
    SpanStatus, SpanTracker, StdError, TraceContext, Tracing,
};
use std::collections::HashMap;
use std::io::BufRead;

/// The WASI host: real preopened-directory file IO, real env/args, real wall time and entropy —
/// all through synchronous `std`. Constructed by the wasm runner, never by the differential.
#[derive(Debug)]
pub struct WasiHost {
    /// The user-facing seeded PRNG state (deterministic on every host — see the module docs).
    rng: u64,
    /// The logical monotonic clock (an ordering device, not wall time — same as `RealHost`).
    clock: u64,
    /// The next `id.next_id()` value — deterministic and sequential on every host.
    ids: u64,
    /// Open lazy read streams (P-LAZY), keyed by the id handed to the file handle — the
    /// `RealHost` scheme over a blocking `BufReader` instead of a tokio one.
    readers: HashMap<u64, std::io::BufReader<std::fs::File>>,
    /// Monotonic id source for `readers`.
    next_reader_id: u64,
    /// The program's argument vector reported through `args.all()`. Defaults to the WASI argv;
    /// the runner overrides it via [`WasiHost::with_args`] so the program sees the same argv
    /// shape (`[<bundle>, <pass-through…>]`) it would from `noeta run`.
    args: Vec<String>,
    /// The program's `env.set` writes: an overlay consulted before the real environment, which
    /// is never mutated (same rule as `RealHost`, and WASI environs are immutable anyway).
    env_overlay: HashMap<String, String>,
    /// The p2p backing: the in-process loopback broker (single-node, the same type the sandbox
    /// and ring-p2p-less `RealHost` use), so a p2p/synced program runs locally without peers.
    p2p: noeta_stdlib::P2pBroker,
    /// Telemetry state: in-flight spans are tracked (so `tel_span_context` and parenting stay
    /// correct for explicit `std.tracing` use) but there is no exporter — every signal reports
    /// disabled and ended spans drop at the null sink. An OTLP path needs outbound HTTP, so it
    /// arrives with the `wasi:http` build (W4) if ever.
    tel: WasiTelemetry,
}

/// `WasiHost`'s telemetry state — the shared no-exporter shape ([`SpanTracker`], live-span
/// tracking for context/parenting) plus metric aggregation with nothing reading it.
#[derive(Debug, Default)]
struct WasiTelemetry {
    spans: SpanTracker,
    metrics: MetricStore,
}

impl WasiHost {
    /// A fresh WASI host over the process's own WASI world (argv, environ, preopens).
    pub fn new() -> WasiHost {
        WasiHost {
            rng: noeta_stdlib::random::DEFAULT_SEED,
            clock: 0,
            ids: 1,
            readers: HashMap::new(),
            next_reader_id: 0,
            args: std::env::args().collect(),
            env_overlay: HashMap::new(),
            p2p: noeta_stdlib::P2pBroker::default(),
            tel: WasiTelemetry::default(),
        }
    }

    /// Override the argument vector this host reports through `args.all()` (consuming-builder
    /// style, like `RealHost::with_args`).
    pub fn with_args(mut self, args: Vec<String>) -> WasiHost {
        self.args = args;
        self
    }

    /// The sorted base names of the entries in directory `dir` — the blocking analogue of
    /// `RealHost::read_dir_names`, shared by `fs_list` (cwd) and `fs_list_dir` (any path).
    fn read_dir_names(&self, dir: &str) -> Result<Vec<String>, StdError> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| io_error(format!("cannot list directory `{dir}`: {e}")))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_error(format!("cannot read directory entry: {e}")))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }
}

impl Default for WasiHost {
    fn default() -> WasiHost {
        WasiHost::new()
    }
}

/// Build an `ErrorKind::Io` (`E0021`) error from a WASI failure.
fn io_error(message: String) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message,
    }
}

/// The error every network leaf returns on this target: honest and directional, not a stub that
/// pretends. One builder so the wording stays uniform across the leaves.
fn no_network(what: &str) -> StdError {
    io_error(format!(
        "{what} is not available on the wasm/WASI target: networking arrives with the wasi:http \
         component build (see plans/wasm/, W4)"
    ))
}

impl FileReader for WasiHost {
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError> {
        // P-LAZY, the RealHost scheme: open now (a missing file is the same IO error as an eager
        // read), register a buffered reader, and stream lines through `fs_read_more`.
        let file = std::fs::File::open(path)
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}")))?;
        let id = self.next_reader_id;
        self.next_reader_id += 1;
        self.readers.insert(id, std::io::BufReader::new(file));
        Ok(ReadSource::Lazy(id))
    }

    fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, StdError> {
        let Some(reader) = self.readers.get_mut(&id) else {
            // The stream was already drained (dropped at EOF); nothing more to give.
            return Ok(None);
        };
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
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

impl FileSystem for WasiHost {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        std::fs::write(path, content).map_err(|e| io_error(format!("cannot write `{path}`: {e}")))
    }

    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| io_error(format!("cannot open `{path}` for append: {e}")))?;
        file.write_all(content.as_bytes())
            .map_err(|e| io_error(format!("cannot append to `{path}`: {e}")))
    }

    fn fs_read(&self, path: &str) -> Result<String, StdError> {
        std::fs::read_to_string(path).map_err(|e| io_error(format!("cannot read `{path}`: {e}")))
    }

    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError> {
        std::fs::write(path, data).map_err(|e| io_error(format!("cannot write `{path}`: {e}")))
    }

    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError> {
        std::fs::read(path).map_err(|e| io_error(format!("cannot read `{path}`: {e}")))
    }

    fn fs_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError> {
        if !std::path::Path::new(path).exists() {
            return Ok(false);
        }
        std::fs::remove_file(path)
            .map(|()| true)
            .map_err(|e| io_error(format!("cannot remove `{path}`: {e}")))
    }

    fn fs_list(&self) -> Result<Vec<String>, StdError> {
        self.read_dir_names(".")
    }

    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError> {
        let dir = if dir.is_empty() { "." } else { dir };
        self.read_dir_names(dir)
    }

    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError> {
        std::fs::create_dir_all(path)
            .map_err(|e| io_error(format!("cannot create directory `{path}`: {e}")))
    }

    fn fs_is_dir(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }
}

impl Rng for WasiHost {
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

impl Clock for WasiHost {
    fn clock_monotonic(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.clock = self.clock.saturating_add(ms.max(0) as u64);
    }

    fn clock_unix_ms(&mut self) -> u64 {
        // WASI's wall clock through std. Saturate to 0 on a pre-1970 host clock rather than panic.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

impl Ids for WasiHost {
    fn id_next(&mut self) -> u64 {
        let id = self.ids;
        self.ids += 1;
        id
    }
}

impl Entropy for WasiHost {
    fn entropy_u64(&mut self) -> u64 {
        // WASI `random_get`. Same posture as RealHost: an environment with no entropy source
        // cannot mint ids at all; failing loudly beats a guessable stream.
        getrandom::u64().expect("the WASI entropy source is unavailable")
    }
}

impl Env for WasiHost {
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

impl Os for WasiHost {
    fn os_platform(&self) -> String {
        // `"wasi"` on the wasm runner; the build target's OS in this crate's native unit tests.
        // Rust leaves `consts::OS` **empty** on wasm targets (verified on wasm32-wasip1), so name
        // the platform ourselves rather than report "".
        let os = std::env::consts::OS;
        if os.is_empty() { "wasi" } else { os }.to_string()
    }

    fn os_arch(&self) -> String {
        std::env::consts::ARCH.to_string()
    }

    fn os_hostname(&self) -> String {
        // WASI exposes no hostname; a fixed name beats an error for an introspection leaf.
        "wasm".to_string()
    }

    fn os_cpus(&self) -> i64 {
        // WASI p1 reports no parallelism (and the VM runs cooperatively here anyway) — `Err` ⇒ 1.
        std::thread::available_parallelism().map_or(1, |n| n.get() as i64)
    }

    fn os_cwd(&self) -> String {
        // The guest cwd is the preopen root; if the embedder granted no preopens, report `/`
        // rather than error — introspection leaves stay total.
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string())
    }

    fn os_pid(&self) -> i64 {
        // There is no process id in a wasm module; a fixed pid 1 (the sandbox's convention).
        1
    }

    fn os_exec(&mut self, command: &str, _args: &[String]) -> Result<ExecResult, StdError> {
        // Not a W4 gap: WASI has no process spawning at all, so this is a permanent target fact.
        Err(io_error(format!(
            "os.exec: cannot run `{command}`: the wasm/WASI target has no subprocesses"
        )))
    }
}

impl Network for WasiHost {
    fn net_fetch(&mut self, _request: NetRequest) -> Result<NetResponse, StdError> {
        Err(no_network("outbound HTTP (`http.*`)"))
    }

    fn net_listen(&mut self, _addr: &str) -> Result<u64, StdError> {
        Err(no_network("serving (`http.serve`)"))
    }

    fn net_accept_next(&mut self, _listener: u64) -> Result<Option<(u64, NetRequest)>, StdError> {
        // Unreachable in practice — `net_listen` already refused — but stay honest if called.
        Err(no_network("serving (`http.serve`)"))
    }

    fn net_reply_now(&mut self, _conn: u64, _response: NetResponse) -> Result<(), StdError> {
        Err(no_network("serving (`http.serve`)"))
    }
}

impl P2p for WasiHost {
    // The loopback broker (single-node, in-process) — the same backing `RealHost` uses without
    // the `ring-p2p` ring, so a p2p/synced program runs locally, just without real peers. The
    // durable/group/identity defaults on the trait are exactly right for a broker.
    fn p2p_publish(&mut self, topic: &str, message: Vec<u8>) -> Result<(), StdError> {
        self.p2p.publish(topic, message);
        Ok(())
    }

    fn p2p_poll(&mut self, topic: &str) -> Result<Option<Vec<u8>>, StdError> {
        Ok(self.p2p.poll_default(topic))
    }

    fn p2p_subscribe(&mut self, topic: &str) -> Result<u64, StdError> {
        Ok(self.p2p.subscribe(topic))
    }

    fn p2p_poll_sub(&mut self, sub: u64) -> Result<Option<Vec<u8>>, StdError> {
        Ok(self.p2p.poll_sub(sub))
    }
}

impl Tracing for WasiHost {
    fn tel_enabled(&self) -> bool {
        // No exporter exists on this target (OTLP needs outbound HTTP — W4), so
        // auto-instrumentation short-circuits; explicit `std.tracing` spans still mint below.
        false
    }

    fn tel_span_start(
        &mut self,
        name: &str,
        kind: SpanKind,
        parent: Option<TraceContext>,
    ) -> SpanId {
        // Real entropy for the W3C ids, like RealHost: a propagated context must not collide
        // across processes even if the spans themselves are never exported.
        let span_id = self.entropy_u64().to_be_bytes();
        let mut trace_id = [0u8; 16];
        trace_id[..8].copy_from_slice(&self.entropy_u64().to_be_bytes());
        trace_id[8..].copy_from_slice(&self.entropy_u64().to_be_bytes());
        let now = self.clock_unix_ms();
        self.tel
            .spans
            .start(name, kind, parent, span_id, trace_id, now)
    }

    fn tel_span_set_attr(&mut self, span: SpanId, key: &str, value: AttrValue) {
        self.tel.spans.set_attr(span, key, value);
    }

    fn tel_span_add_event(
        &mut self,
        span: SpanId,
        name: &str,
        attrs: Vec<(CompactString, AttrValue)>,
    ) {
        let now = self.clock_unix_ms();
        self.tel.spans.add_event(span, name, attrs, now);
    }

    fn tel_span_set_status(&mut self, span: SpanId, status: SpanStatus) {
        self.tel.spans.set_status(span, status);
    }

    fn tel_span_end(&mut self, span: SpanId) {
        // The null sink: the completed record drops. Removal is still load-bearing — it bounds
        // the live table and makes `tel_span_context` on an ended span yield the zero context.
        let now = self.clock_unix_ms();
        drop(self.tel.spans.end(span, now));
    }

    fn tel_span_context(&mut self, span: SpanId) -> TraceContext {
        self.tel.spans.context(span)
    }

    fn tel_intern_remote(&mut self, context: TraceContext) -> SpanId {
        self.tel.spans.intern_remote(context)
    }

    fn tel_is_remote(&self, span: SpanId) -> bool {
        self.tel.spans.is_remote(span)
    }

    fn tel_release_remote(&mut self, span: SpanId) {
        self.tel.spans.release_remote(span);
    }
}

impl Logging for WasiHost {
    fn tel_logs_enabled(&self) -> bool {
        false
    }

    fn log_emit(&mut self, _record: LogRecord) {}
}

impl Metrics for WasiHost {
    fn tel_metrics_enabled(&self) -> bool {
        false
    }

    fn metric_get_or_create(
        &mut self,
        name: &str,
        unit: &str,
        kind: InstrumentKind,
    ) -> InstrumentId {
        self.tel.metrics.get_or_create(name, unit, kind)
    }

    fn metric_observe(
        &mut self,
        inst: InstrumentId,
        value: MetricValue,
        attrs: Vec<(CompactString, AttrValue)>,
    ) {
        let now = self.clock_unix_ms();
        self.tel.metrics.observe(inst, value, attrs, now);
    }

    fn metric_collect(&mut self) -> Vec<MetricData> {
        let now = self.clock_unix_ms();
        self.tel.metrics.collect(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_stdlib::Host;

    /// The compile-time proof that `WasiHost` provides all twelve capabilities — the blanket
    /// impl only fires when every supertrait is satisfied.
    #[test]
    fn is_a_host() {
        fn assert_host(_: &dyn Host) {}
        assert_host(&WasiHost::new());
    }

    #[test]
    fn fs_round_trips_on_the_real_filesystem() {
        let dir = std::env::temp_dir().join(format!("noeta-wasi-host-test-{}", std::process::id()));
        let dir_s = dir.to_string_lossy().into_owned();
        let mut host = WasiHost::new();
        host.fs_mkdir(&dir_s).expect("mkdir");
        assert!(host.fs_is_dir(&dir_s));

        let path = format!("{dir_s}/a.txt");
        host.fs_write(&path, "hello").expect("write");
        host.fs_append(&path, " world").expect("append");
        assert_eq!(host.fs_read(&path).expect("read"), "hello world");
        assert!(host.fs_exists(&path));
        assert_eq!(host.fs_list_dir(&dir_s).expect("list"), vec!["a.txt"]);

        // The lazy read handle streams lines, keeping the trailing newline.
        host.fs_write(&path, "one\ntwo\n").expect("rewrite");
        let ReadSource::Lazy(id) = host.fs_open_read(&path).expect("open") else {
            panic!("WasiHost reads are lazy");
        };
        assert_eq!(host.fs_read_more(id).expect("line"), Some("one\n".into()));
        assert_eq!(host.fs_read_more(id).expect("line"), Some("two\n".into()));
        assert_eq!(host.fs_read_more(id).expect("eof"), None);

        assert!(host.fs_remove(&path).expect("remove"));
        assert!(!host.fs_remove(&path).expect("remove missing"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn env_overlay_shadows_without_mutating() {
        let mut host = WasiHost::new();
        host.env_set("NOETA_WASI_HOST_TEST", "overlay");
        assert_eq!(
            host.env_get("NOETA_WASI_HOST_TEST").as_deref(),
            Some("overlay")
        );
        // The real process environment is untouched.
        assert!(std::env::var("NOETA_WASI_HOST_TEST").is_err());
        assert!(
            host.env_keys()
                .contains(&"NOETA_WASI_HOST_TEST".to_string())
        );
    }

    #[test]
    fn deterministic_streams_match_the_real_host_rules() {
        let mut host = WasiHost::new();
        // Seeded PRNG: a pure function of the seed.
        host.rng_seed(42);
        let a = host.rng_int(0, 100).expect("int");
        host.rng_seed(42);
        assert_eq!(host.rng_int(0, 100).expect("int"), a);
        // Logical clock: reads-then-advances; sleep advances without blocking.
        assert_eq!(host.clock_monotonic(), 0);
        host.clock_sleep(10);
        assert_eq!(host.clock_monotonic(), 11);
        // Sequential ids from 1.
        assert_eq!(host.id_next(), 1);
        assert_eq!(host.id_next(), 2);
        // Entropy is independent of the seeded stream (and available).
        let e1 = host.entropy_u64();
        let e2 = host.entropy_u64();
        assert_ne!(e1, e2, "two entropy draws colliding is ~impossible");
    }

    #[test]
    fn network_leaves_error_honestly() {
        let mut host = WasiHost::new();
        let err = host.net_listen("127.0.0.1:0").expect_err("no server");
        assert_eq!(err.kind, ErrorKind::Io);
        assert!(err.message.contains("wasi:http"), "{}", err.message);
        let err = host
            .net_fetch(NetRequest {
                method: "GET".into(),
                url: "http://example.com".into(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .expect_err("no client");
        assert!(err.message.contains("wasi:http"), "{}", err.message);
    }

    #[test]
    fn spans_track_context_without_an_exporter() {
        let mut host = WasiHost::new();
        assert!(!host.tel_enabled());
        let root = host.tel_span_start("root", SpanKind::Internal, None);
        let root_ctx = host.tel_span_context(root);
        assert!(root_ctx.sampled);
        let child = host.tel_span_start("child", SpanKind::Internal, Some(root_ctx));
        let child_ctx = host.tel_span_context(child);
        assert_eq!(
            child_ctx.trace_id, root_ctx.trace_id,
            "child inherits trace"
        );
        assert_ne!(child_ctx.span_id, root_ctx.span_id);
        host.tel_span_end(child);
        host.tel_span_end(root);
        // Ended spans dropped at the null sink; contexts of unknown spans are the zero context.
        assert!(!host.tel_span_context(root).sampled);
        // Remote interning round-trips and releases.
        let seed = host.tel_intern_remote(root_ctx);
        assert!(host.tel_is_remote(seed));
        assert_eq!(host.tel_span_context(seed), root_ctx);
        host.tel_release_remote(seed);
        assert!(!host.tel_is_remote(seed));
    }

    #[test]
    fn p2p_loopback_broker_round_trips() {
        let mut host = WasiHost::new();
        host.p2p_publish("t", b"m1".to_vec()).expect("publish");
        let sub = host.p2p_subscribe("t").expect("subscribe");
        assert_eq!(host.p2p_poll_sub(sub).expect("poll"), Some(b"m1".to_vec()));
        assert_eq!(host.p2p_poll_sub(sub).expect("caught up"), None);
    }
}
