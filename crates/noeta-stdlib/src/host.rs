//! The deterministic sandbox host (M2.1). The host-capability *traits* (`FileSystem`/`Rng`/
//! `Clock`/`Env`/`Entropy`/`Ids`/`Network`/`Host` + `FileReader`, and `ReadSource`) live in the ABI
//! crate ([`noeta_ext_abi::host`], re-exported here); this module provides the concrete
//! [`SandboxHost`] — the in-memory VFS, seeded PRNG, logical clock, and pure network responder that
//! conformance and `--differential` always run. It owns the *bytes* the capabilities read/write, so
//! it stays with the modules ([`crate::fs`], [`crate::random`], [`crate::net`]) whose state it holds.

pub use noeta_ext_abi::host::{
    Clock, Entropy, Env, FileReader, FileSystem, Host, Ids, Network, Os, P2p, P2pProvider,
    ReadSource, RealP2pConfig, Rng,
};
pub use noeta_ext_abi::{Logging, Metrics, Tracing};

use crate::env;
use crate::fs::Vfs;
use crate::random;
use crate::{ErrorKind, StdError};
use noeta_ext_abi::ExecResult;
use noeta_ext_abi::{
    AttrValue, InstrumentId, InstrumentKind, LogRecord, MetricData, MetricStore, MetricValue,
    SpanData, SpanEvent, SpanId, SpanKind, SpanStatus, TraceContext,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The sandbox's fixed wall-clock epoch: 2026-01-01T00:00:00Z in unix milliseconds.
/// `clock_unix_ms` on the sandbox is `SANDBOX_EPOCH_MS + logical clock`, so wall-time reads (and the
/// v7 UUIDs built from them) are deterministic, plausibly-dated, and advance under `sleep`.
pub const SANDBOX_EPOCH_MS: u64 = 1_767_225_600_000;

/// The sandbox entropy stream's fixed seed — a different arbitrary odd constant than
/// [`random::DEFAULT_SEED`] so the entropy and user-`random` streams never coincide.
pub const SANDBOX_ENTROPY_SEED: u64 = 0xA076_1D64_78BD_642F;

// --- embeddable deterministic capability components (audit-6 F9) --------------------------------
//
// The `Rng`/`Clock`/`Entropy`/`Ids` impls of the deterministic hosts (Sandbox here, Wasi, Browser)
// were line-identical state-threading around the shared pure kernels (`random::*`) — 3x per
// capability, with each host's real differences (e.g. its `clock_unix_ms` reading) hiding among
// identical-looking blocks. Each capability now lives ONCE as an embeddable component struct; a
// host embeds the components it wants and forwards with `noeta_ext_abi::delegate_host!`, keeping
// hand-written impls only where it genuinely differs. `SandboxHost` below is the in-tree proof;
// the Wasi/browser hosts can adopt the same components without behavior change.

/// The user-facing **seeded PRNG** state ([`Rng`]): a SplitMix64 word threaded through the pure
/// steppers in [`random`]. `random.seed` rewinds exactly this stream and nothing else.
#[derive(Debug, Clone)]
pub struct SeededRng {
    state: u64,
}

impl SeededRng {
    /// A stream starting at `state` (a raw SplitMix64 word, e.g. [`random::DEFAULT_SEED`]).
    pub fn new(state: u64) -> SeededRng {
        SeededRng { state }
    }
}

impl Rng for SeededRng {
    fn rng_seed(&mut self, seed: i64) {
        self.state = random::seed_state(seed);
    }

    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError> {
        let (next_state, value) = random::int(self.state, lo, hi)?;
        self.state = next_state;
        Ok(value)
    }

    fn rng_float(&mut self) -> f64 {
        let (next_state, value) = random::float(self.state);
        self.state = next_state;
        value
    }
}

/// A **deterministic entropy** stream ([`Entropy`]): an independent fixed-seed SplitMix64 word,
/// deliberately separate from [`SeededRng`] so drawing ids never perturbs the user's `random`
/// sequence (and `random.seed` never rewinds ids).
#[derive(Debug, Clone)]
pub struct DeterministicEntropy {
    state: u64,
}

impl DeterministicEntropy {
    pub fn new(state: u64) -> DeterministicEntropy {
        DeterministicEntropy { state }
    }
}

impl Entropy for DeterministicEntropy {
    fn entropy_u64(&mut self) -> u64 {
        let (next_state, value) = random::next(self.state);
        self.state = next_state;
        value
    }
}

/// The **sequential-id** counter ([`Ids`]): 1, 2, 3, ... — deterministic on every host (ids are
/// an ordering device, not entropy).
#[derive(Debug, Clone, Default)]
pub struct CounterIds {
    handed_out: u64,
}

impl CounterIds {
    /// A counter whose first `id_next` answers 1.
    pub fn new() -> CounterIds {
        CounterIds::default()
    }
}

impl Ids for CounterIds {
    fn id_next(&mut self) -> u64 {
        self.handed_out += 1;
        self.handed_out
    }
}

/// The **logical clock** ([`Clock`]): `monotonic` reads-then-advances, `sleep` advances without
/// blocking, and `clock_unix_ms` is a **derived read** of the counter against a fixed epoch — it
/// must not advance (a derived reading like a v7 UUID must not perturb the user's observable
/// `monotonic` stream), but it moves under `sleep` like everything else, so time-ordered ids
/// still order deterministically.
#[derive(Debug, Clone)]
pub struct DeterministicClock {
    epoch_ms: u64,
    ms: u64,
}

impl DeterministicClock {
    /// A clock at zero whose wall-time view starts at `epoch_ms`.
    pub fn new(epoch_ms: u64) -> DeterministicClock {
        DeterministicClock { epoch_ms, ms: 0 }
    }

    /// The current wall-time view WITHOUT the `&mut` a capability call needs — for host-internal
    /// reads while something else is borrowed (the sandbox's teardown metric collection).
    pub fn unix_ms(&self) -> u64 {
        self.epoch_ms + self.ms
    }
}

impl Clock for DeterministicClock {
    fn clock_monotonic(&mut self) -> u64 {
        let now = self.ms;
        self.ms += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.ms = self.ms.saturating_add(ms.max(0) as u64);
    }

    fn clock_unix_ms(&mut self) -> u64 {
        self.unix_ms()
    }
}

/// The deterministic sandbox: in-memory VFS, seeded SplitMix64 state, and a logical
/// clock — fresh per run, identical across backends by construction. This is what
/// the conformance harness gives both backends, so `--differential` stays
/// deterministic regardless of which host real (CLI/server) runs use.
#[derive(Debug, Clone)]
pub struct SandboxHost {
    fs: Vfs,
    // The deterministic quartet, as embedded components (audit-6 F9): `delegate_host!` below
    // forwards each capability to its field, so the state-threading lives once in the component.
    rng: SeededRng,
    entropy: DeterministicEntropy,
    ids: CounterIds,
    clock: DeterministicClock,
    env: BTreeMap<String, String>,
    args: Vec<String>,
    /// Scripted spawned processes (process-handle arc): id → the pre-computed instant-complete
    /// outcome plus a stdout read cursor (process-streaming arc, for `read_line`). A sandbox
    /// "child" runs to completion at spawn (the scripted `os_exec`), so `wait` returns its stored
    /// outcome and `try_wait` always reports it done — deterministic and in-oracle, the process
    /// analogue of the Vfs / logical clock.
    procs: BTreeMap<u64, ScriptedProc>,
    /// The next spawned-process handle id (starts at 1, like `ids`).
    next_proc: u64,
    /// The inbound server state (http-server S1), armed by `net_listen`. A sandbox run serves at
    /// most one listener — a differential program calls `http.serve` once — so a single slot
    /// suffices; a second `net_listen` re-arms it.
    inbound: Option<InboundState>,
    /// The deterministic telemetry recorder (native OTEL): in-progress spans by id, ended spans in
    /// end order, and the counters deriving deterministic span/trace ids. Since spans are write-only
    /// (never program output), this exists only so conformance can assert on emitted spans; the
    /// differential never observes it.
    tel: TelRecorder,
}

/// The sandbox's in-memory span recorder. Span ids count from 1; trace ids from 1; both derive
/// their W3C bytes from those counters (big-endian), so recorded spans are byte-reproducible.
#[derive(Debug, Clone, Default)]
struct TelRecorder {
    next_span: u64,
    next_trace: u64,
    live: BTreeMap<SpanId, SpanData>,
    recorded: Vec<SpanData>,
    /// Remote-interned contexts (T5d): pseudo-handles minted by `tel_intern_remote` for contexts
    /// that arrived over a channel/isolate boundary. Not spans — `tel_span_context` reads them,
    /// everything else no-ops. Bounded by live seeds (`tel_release_remote` frees replaced ones).
    remote: BTreeMap<SpanId, TraceContext>,
    /// An optional external sink that also receives every ended span. `None` in every normal run;
    /// installed by [`SandboxHost::set_span_sink`] so a caller that only sees the host by value (it
    /// is moved into the VM and dropped at teardown) can still observe the spans a program emitted.
    sink: Option<Arc<Mutex<Vec<SpanData>>>>,
    /// Emitted log records, in emission order — the logs signal's recorder (native OTEL Phase L).
    logs: Vec<LogRecord>,
    /// An optional external sink that also receives every emitted [`LogRecord`] — the logs analogue
    /// of `sink`, installed by [`SandboxHost::set_log_sink`] for the logs parity oracle.
    log_sink: Option<Arc<Mutex<Vec<LogRecord>>>>,
    /// Host-side metric aggregation (native OTEL Phase M). Shared logic with the real host, so a
    /// given call sequence collects byte-identically.
    metrics: MetricStore,
    /// An optional external sink that receives the collected [`MetricData`] at **teardown** (the
    /// deterministic collection point for the sandbox); installed by [`SandboxHost::set_metric_sink`]
    /// for the metrics parity oracle. Metrics aggregate during the run and collect once on drop.
    metric_sink: Option<Arc<Mutex<Vec<MetricData>>>>,
}

/// The sandbox's inbound-server state: the fixed request script (see
/// [`crate::net::sandbox_request_script`]), a cursor into it, and a transcript of the replies the
/// handler produced (for test introspection — the differential observes the handler's own output).
#[derive(Debug, Clone)]
struct InboundState {
    script: Vec<crate::NetRequest>,
    cursor: usize,
    transcript: Vec<(u64, crate::NetResponse)>,
    /// Upgraded websocket connections (server-hmr L0): each drives the fixed scripted client
    /// conversation ([`crate::net::sandbox_ws_client_frames`]) — the value is the cursor into it —
    /// and records the handler's outbound frames in `ws_transcript`. Deterministic and finite,
    /// so an upgraded handler terminates in-oracle exactly like the request script itself.
    ws_conns: BTreeMap<u64, usize>,
    ws_transcript: Vec<(u64, String)>,
}

/// A scripted spawned process (process-handle + streaming arcs): its instant-complete outcome and
/// independent stdout/stderr read cursors for `read_line`/`read`/`read_err_line`.
#[derive(Debug, Clone)]
struct ScriptedProc {
    result: ExecResult,
    stdout_cursor: usize,
    stderr_cursor: usize,
}

/// The next line of `text` from `*cursor` (without its trailing newline), advancing past it; `None`
/// once exhausted. A final unterminated line is returned once (like `str::lines`). Shared by the
/// sandbox's scripted stdout/stderr streaming.
fn scripted_read_line(text: &str, cursor: &mut usize) -> Option<String> {
    let rest = &text[*cursor..];
    if rest.is_empty() {
        return None;
    }
    match rest.find('\n') {
        Some(nl) => {
            let line = rest[..nl].to_string();
            *cursor += nl + 1;
            Some(line)
        }
        None => {
            let line = rest.to_string();
            *cursor = text.len();
            Some(line)
        }
    }
}

/// Up to `count` characters of `text` from `*cursor`, advancing past them; `None` once exhausted.
/// `count <= 0` yields the empty string without consuming input (mirrors the real host's `read`).
fn scripted_read(text: &str, cursor: &mut usize, count: i64) -> Option<String> {
    if *cursor >= text.len() {
        return None;
    }
    let want = count.max(0) as usize;
    let chunk: String = text[*cursor..].chars().take(want).collect();
    *cursor += chunk.len();
    Some(chunk)
}

impl SandboxHost {
    /// A fresh sandbox: empty filesystem, default PRNG seed, clock at zero, and the
    /// fixed `env`/`args` fixture — matching the deterministic defaults both backends
    /// used before M2.1 plus the M2.2 host-introspection fixture.
    pub fn new() -> SandboxHost {
        SandboxHost {
            fs: Vfs::new(),
            rng: SeededRng::new(random::DEFAULT_SEED),
            entropy: DeterministicEntropy::new(SANDBOX_ENTROPY_SEED),
            ids: CounterIds::new(),
            clock: DeterministicClock::new(SANDBOX_EPOCH_MS),
            env: env::sandbox_vars(),
            args: env::sandbox_args(),
            procs: BTreeMap::new(),
            next_proc: 1,
            inbound: None,
            tel: TelRecorder {
                next_span: 1,
                next_trace: 1,
                live: BTreeMap::new(),
                recorded: Vec::new(),
                remote: BTreeMap::new(),
                sink: None,
                logs: Vec::new(),
                log_sink: None,
                metrics: MetricStore::default(),
                metric_sink: None,
            },
        }
    }

    /// The spans this sandbox has recorded (ended), in end order — test introspection for the
    /// telemetry conformance oracle (native OTEL). Spans not yet ended are not included.
    pub fn recorded_spans(&self) -> &[SpanData] {
        &self.tel.recorded
    }

    /// Install a shared sink that also receives every ended [`SpanData`]. The telemetry conformance
    /// oracle uses this to observe the spans a *program* emits: the host is moved into the VM and
    /// dropped at teardown, so [`recorded_spans`](Self::recorded_spans) is unreachable afterward, but
    /// a sink handed in before the run survives it. No effect on normal runs (the sink stays `None`).
    pub fn set_span_sink(&mut self, sink: Arc<Mutex<Vec<SpanData>>>) {
        self.tel.sink = Some(sink);
    }

    /// The log records this sandbox has emitted, in emission order — test introspection for the logs
    /// parity oracle (native OTEL Phase L).
    pub fn recorded_logs(&self) -> &[LogRecord] {
        &self.tel.logs
    }

    /// Install a shared sink that also receives every emitted [`LogRecord`] — the logs analogue of
    /// [`set_span_sink`](Self::set_span_sink), so the parity oracle can observe a program's logs
    /// after the host is dropped at teardown. No effect on normal runs (the sink stays `None`).
    pub fn set_log_sink(&mut self, sink: Arc<Mutex<Vec<LogRecord>>>) {
        self.tel.log_sink = Some(sink);
    }

    /// Install a shared sink that receives the collected [`MetricData`] when this host is dropped —
    /// the deterministic teardown-only collection the metrics parity oracle asserts on (the sandbox
    /// never collects mid-run, so wall-time periodicity can't perturb it). No effect on normal runs.
    pub fn set_metric_sink(&mut self, sink: Arc<Mutex<Vec<MetricData>>>) {
        self.tel.metric_sink = Some(sink);
    }
}

// Collect aggregated metrics to the sink exactly once, at teardown — the sandbox's deterministic
// collection point (the `Host` is moved into the VM and dropped at end-of-run; a plain run never
// clones it, and isolates get fresh hosts, so this fires once per host). Only the oracle installs a
// sink, so normal runs do nothing here.
impl Drop for SandboxHost {
    fn drop(&mut self) {
        if let Some(sink) = &self.tel.metric_sink {
            let now = self.clock.unix_ms();
            let data = self.tel.metrics.collect(now);
            sink.lock().expect("metric sink not poisoned").extend(data);
        }
    }
}

impl Default for SandboxHost {
    fn default() -> SandboxHost {
        SandboxHost::new()
    }
}

impl FileReader for SandboxHost {
    /// The sandbox is in-memory with tiny fixtures, so it always snapshots — keeping reads
    /// deterministic and behavior byte-identical to the pre-P-LAZY handle. It therefore never hands
    /// out a lazy id, so `fs_read_more` is unreachable here.
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError> {
        Ok(ReadSource::Snapshot(self.fs.read(path)?))
    }

    fn fs_read_more(&mut self, _id: u64) -> Result<Option<String>, StdError> {
        unreachable!("SandboxHost never opens a lazy reader, so it is never asked for more")
    }
}

impl FileSystem for SandboxHost {
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

// The deterministic quartet forwards to the embedded components — the capability semantics
// (including `clock_unix_ms`'s derived-read subtlety) live on the component types above.
noeta_ext_abi::delegate_host!(SandboxHost => rng : Rng);
noeta_ext_abi::delegate_host!(SandboxHost => clock : Clock);
noeta_ext_abi::delegate_host!(SandboxHost => entropy : Entropy);
noeta_ext_abi::delegate_host!(SandboxHost => ids : Ids);

impl Network for SandboxHost {
    /// The whole outbound network is the pure sandbox responder — deterministic, so both backends
    /// agree.
    fn net_fetch(&mut self, request: crate::NetRequest) -> Result<crate::NetResponse, StdError> {
        Ok(crate::net::sandbox_respond(&request))
    }

    /// Arm the fixed inbound request script (http-server S1); `addr` is ignored (the sandbox binds
    /// no socket). One listener per run — always id `1`.
    fn net_listen(&mut self, _addr: &str) -> Result<u64, StdError> {
        self.inbound = Some(InboundState {
            script: crate::net::sandbox_request_script(),
            cursor: 0,
            transcript: Vec::new(),
            ws_conns: BTreeMap::new(),
            ws_transcript: Vec::new(),
        });
        Ok(1)
    }

    /// Pop the next scripted request (conn id = its position), or `None` once the script is
    /// exhausted — which is what lets a served program terminate under the differential.
    fn net_accept_next(
        &mut self,
        _listener: u64,
    ) -> Result<Option<(u64, crate::NetRequest)>, StdError> {
        let state = self
            .inbound
            .as_mut()
            .expect("net_accept_next before net_listen");
        match state.script.get(state.cursor) {
            Some(request) => {
                let conn = state.cursor as u64;
                state.cursor += 1;
                Ok(Some((conn, request.clone())))
            }
            None => Ok(None),
        }
    }

    /// Record the handler's reply. The differential observes the handler's own output, so this only
    /// backs test introspection — but recording it keeps the reply path honestly exercised.
    fn net_reply_now(&mut self, conn: u64, response: crate::NetResponse) -> Result<(), StdError> {
        self.inbound
            .as_mut()
            .expect("net_reply_now before net_listen")
            .transcript
            .push((conn, response));
        Ok(())
    }

    /// Arm the fixed scripted client conversation on `conn` (the key is irrelevant to the
    /// sandbox — no real handshake happens).
    fn net_ws_upgrade_now(&mut self, conn: u64, _key: &str) -> Result<(), StdError> {
        self.inbound
            .as_mut()
            .expect("net_ws_upgrade_now before net_listen")
            .ws_conns
            .insert(conn, 0);
        Ok(())
    }

    /// Pop the next scripted client frame on `conn`; `None` once the conversation is exhausted —
    /// the deterministic "peer closed" that lets an upgraded handler return in-oracle.
    fn net_ws_recv_next(&mut self, conn: u64) -> Result<Option<String>, StdError> {
        let state = self
            .inbound
            .as_mut()
            .expect("net_ws_recv_next before net_listen");
        let Some(cursor) = state.ws_conns.get_mut(&conn) else {
            return Ok(None);
        };
        let frames = crate::net::sandbox_ws_client_frames();
        match frames.get(*cursor) {
            Some(frame) => {
                *cursor += 1;
                Ok(Some(frame.clone()))
            }
            None => Ok(None),
        }
    }

    /// Record the handler's outbound frame (test introspection; the differential observes the
    /// handler's own output, exactly like replies).
    fn net_ws_send_now(&mut self, conn: u64, text: &str) -> Result<(), StdError> {
        self.inbound
            .as_mut()
            .expect("net_ws_send_now before net_listen")
            .ws_transcript
            .push((conn, text.to_string()));
        Ok(())
    }

    fn net_ws_close_now(&mut self, conn: u64) -> Result<(), StdError> {
        self.inbound
            .as_mut()
            .expect("net_ws_close_now before net_listen")
            .ws_conns
            .remove(&conn);
        Ok(())
    }
}

// The sandbox no longer bakes in the P2p capability (para-namespace follow-on F2): the loopback
// broker moved into the `para.p2p` extension, which owns it in per-run ctx state. So the sandbox
// keeps the **default** `P2pProvider` (`as_p2p` → `None`) — a host with no peer networking — and the
// `para.p2p`/`para.synced` surface serves itself from the extension broker.
impl P2pProvider for SandboxHost {}

impl Env for SandboxHost {
    fn env_get(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    fn env_set(&mut self, key: &str, value: &str) {
        self.env.insert(key.to_string(), value.to_string());
    }

    fn env_keys(&self) -> Vec<String> {
        self.env.keys().cloned().collect()
    }

    fn args(&self) -> Vec<String> {
        self.args.clone()
    }
}

impl Os for SandboxHost {
    // Fixed introspection fixtures, like `env`'s (`sandbox_vars`): constants both backends and
    // every run observe identically.
    fn os_platform(&self) -> String {
        "sandbox".to_string()
    }

    fn os_arch(&self) -> String {
        "sandbox".to_string()
    }

    fn os_hostname(&self) -> String {
        "sandbox".to_string()
    }

    fn os_cpus(&self) -> i64 {
        1
    }

    fn os_cwd(&self) -> String {
        "/".to_string()
    }

    fn os_pid(&self) -> i64 {
        1
    }

    /// The scripted exec interpreter — a tiny fixed command set so exec-driving programs stay
    /// in-oracle (the exec analogue of the Vfs / the inbound request script):
    ///
    /// - `echo <args…>` → status 0, stdout = the args joined with spaces + `\n`.
    /// - `status <n> [message…]` → status `n`, stderr = the message + `\n` when present — the
    ///   fixture for exercising failure paths.
    /// - anything else → an `Io` error, like launching a missing binary on a real host.
    fn os_exec(&mut self, command: &str, args: &[String]) -> Result<ExecResult, StdError> {
        match command {
            "echo" => Ok(ExecResult {
                status: 0,
                stdout: format!("{}\n", args.join(" ")),
                stderr: String::new(),
            }),
            "status" => {
                let status = args
                    .first()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                let message = args.get(1..).unwrap_or(&[]).join(" ");
                Ok(ExecResult {
                    status,
                    stdout: String::new(),
                    stderr: if message.is_empty() {
                        String::new()
                    } else {
                        format!("{message}\n")
                    },
                })
            }
            other => Err(StdError {
                kind: ErrorKind::Io,
                message: format!("exec: command not found: {other}"),
            }),
        }
    }

    /// Spawn a scripted process: run the same fixed command set as `os_exec` **eagerly** (the
    /// sandbox has no real concurrency), store the outcome, and hand back a deterministic handle
    /// id. An unknown command is an `Io` error at spawn, exactly as a real `Command::spawn` fails
    /// for a missing binary.
    fn os_spawn(&mut self, command: &str, args: &[String]) -> Result<u64, StdError> {
        let result = self.os_exec(command, args)?;
        let id = self.next_proc;
        self.next_proc += 1;
        self.procs.insert(
            id,
            ScriptedProc {
                result,
                stdout_cursor: 0,
                stderr_cursor: 0,
            },
        );
        Ok(id)
    }

    /// A deterministic pseudo-pid derived from the handle (real pids are non-deterministic, so the
    /// sandbox invents a stable one), or `None` for an unknown handle.
    fn os_proc_pid(&self, handle: u64) -> Option<i64> {
        self.procs
            .contains_key(&handle)
            .then(|| 10_000 + handle as i64)
    }

    fn os_proc_wait(&mut self, handle: u64) -> Result<ExecResult, StdError> {
        self.procs
            .get(&handle)
            .map(|p| p.result.clone())
            .ok_or_else(|| noeta_ext_abi::os::unknown_process_error(handle))
    }

    /// The scripted child completed at spawn, so `try_wait` always reports it done — a program's
    /// supervise loop terminates on its first poll, deterministically.
    fn os_proc_try_wait(&mut self, handle: u64) -> Result<Option<ExecResult>, StdError> {
        Ok(Some(self.os_proc_wait(handle)?))
    }

    /// The scripted child is already complete, so a kill is a harmless no-op (the handle stays
    /// valid, matching the real host where a later `wait` still returns the outcome).
    fn os_proc_kill(&mut self, handle: u64) -> Result<(), StdError> {
        if self.procs.contains_key(&handle) {
            Ok(())
        } else {
            Err(noeta_ext_abi::os::unknown_process_error(handle))
        }
    }

    /// The scripted child is already complete, so every signal is a harmless, validated no-op —
    /// deterministic and in-oracle, exactly like `os_proc_kill` (`signal` is `kill`'s general form).
    /// The handle must exist; the signal itself has no observable sandbox effect.
    fn os_proc_signal(
        &mut self,
        handle: u64,
        _signal: noeta_ext_abi::os::Signal,
    ) -> Result<(), StdError> {
        self.require_proc(handle)
    }

    /// Stream the scripted stdout a line at a time from the stored output via the per-handle
    /// cursor — deterministic, and identical in shape to the real host's line streaming. `None`
    /// once the output is exhausted; a final unterminated line is returned once (like `str::lines`).
    fn os_proc_read_line(&mut self, handle: u64) -> Result<Option<String>, StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_ext_abi::os::unknown_process_error(handle))?;
        Ok(scripted_read_line(
            &proc.result.stdout,
            &mut proc.stdout_cursor,
        ))
    }

    /// Up to `count` characters from the scripted stdout via the shared stdout cursor.
    fn os_proc_read(&mut self, handle: u64, count: i64) -> Result<Option<String>, StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_ext_abi::os::unknown_process_error(handle))?;
        Ok(scripted_read(
            &proc.result.stdout,
            &mut proc.stdout_cursor,
            count,
        ))
    }

    /// The scripted stderr a line at a time via its own cursor — the stderr twin of `read_line`.
    fn os_proc_read_stderr_line(&mut self, handle: u64) -> Result<Option<String>, StdError> {
        let proc = self
            .procs
            .get_mut(&handle)
            .ok_or_else(|| noeta_ext_abi::os::unknown_process_error(handle))?;
        Ok(scripted_read_line(
            &proc.result.stderr,
            &mut proc.stderr_cursor,
        ))
    }

    /// The scripted commands don't read stdin, so a write is a validated no-op (the handle must
    /// exist). `close_stdin` likewise no-ops. This keeps a stdin-feeding program deterministic and
    /// terminating in-oracle.
    fn os_proc_write_stdin(&mut self, handle: u64, _data: &str) -> Result<(), StdError> {
        self.require_proc(handle)
    }

    fn os_proc_close_stdin(&mut self, handle: u64) -> Result<(), StdError> {
        self.require_proc(handle)
    }
}

impl SandboxHost {
    /// Assert a scripted-process handle is live (else an `Io` error) — shared by the no-op stdin
    /// operations.
    fn require_proc(&self, handle: u64) -> Result<(), StdError> {
        if self.procs.contains_key(&handle) {
            Ok(())
        } else {
            Err(noeta_ext_abi::os::unknown_process_error(handle))
        }
    }
}

impl Tracing for SandboxHost {
    // The deterministic recorder is always on, so auto-instrumentation runs under the sandbox and
    // conformance can assert on the emitted server spans.
    fn tel_enabled(&self) -> bool {
        true
    }

    fn tel_span_start(
        &mut self,
        name: &str,
        kind: SpanKind,
        parent: Option<TraceContext>,
    ) -> SpanId {
        let id = self.tel.next_span;
        self.tel.next_span += 1;
        let trace_id = match parent {
            Some(p) => p.trace_id,
            None => {
                let t = self.tel.next_trace;
                self.tel.next_trace += 1;
                let mut bytes = [0u8; 16];
                bytes[8..].copy_from_slice(&t.to_be_bytes());
                bytes
            }
        };
        let context = TraceContext {
            trace_id,
            span_id: id.to_be_bytes(),
            sampled: true,
        };
        let now = self.clock_unix_ms();
        self.tel.live.insert(
            id,
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
        id
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
        attrs: Vec<(compact_str::CompactString, AttrValue)>,
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
            if let Some(sink) = &self.tel.sink {
                sink.lock().expect("span sink not poisoned").push(s.clone());
            }
            self.tel.recorded.push(s);
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

impl Logging for SandboxHost {
    // The deterministic recorder is always on (like tracing), so the logs signal runs under the
    // sandbox and conformance can assert on the emitted records.
    fn tel_logs_enabled(&self) -> bool {
        true
    }

    fn log_emit(&mut self, record: LogRecord) {
        if let Some(sink) = &self.tel.log_sink {
            sink.lock()
                .expect("log sink not poisoned")
                .push(record.clone());
        }
        self.tel.logs.push(record);
    }
}

impl Metrics for SandboxHost {
    // The deterministic recorder is always on (like tracing), so the metrics signal runs under the
    // sandbox and conformance can assert on the collected series.
    fn tel_metrics_enabled(&self) -> bool {
        true
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
        attrs: Vec<(compact_str::CompactString, AttrValue)>,
    ) {
        let now = self.clock.unix_ms();
        self.tel.metrics.observe(inst, value, attrs, now);
    }

    fn metric_collect(&mut self) -> Vec<MetricData> {
        let now = self.clock.unix_ms();
        self.tel.metrics.collect(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The overlay pattern (audit-2 F7): a custom host overrides ONE capability by hand and
    /// forwards the rest to a wrapped base with `delegate_host!` — instead of ~70 hand-written
    /// forwarding methods. This is the seam the embed docs advertise ("or a custom Host
    /// implementation"); the test pins that the result is a full `Box<dyn Host>`, the override
    /// is observed, and everything else genuinely reaches the base.
    #[test]
    fn a_delegating_overlay_overrides_one_capability_and_forwards_the_rest() {
        struct EngineHost {
            base: SandboxHost,
        }

        // The override: the "engine" exposes its own environment view.
        impl Env for EngineHost {
            fn env_get(&self, key: &str) -> Option<String> {
                (key == "WORLD").then(|| "engine".to_string())
            }
            fn env_set(&mut self, _key: &str, _value: &str) {}
            fn env_keys(&self) -> Vec<String> {
                vec!["WORLD".to_string()]
            }
            fn args(&self) -> Vec<String> {
                self.base.args()
            }
        }

        // Everything else forwards — one line per capability, provided methods included.
        noeta_ext_abi::delegate_host!(EngineHost => base :
            FileReader, FileSystem, Rng, Clock, Os, Entropy, Ids, Network, P2pProvider,
            Tracing, Metrics, Logging);

        // The blanket impl makes the overlay a full Host — the boxed form every backend holds.
        let mut host: Box<dyn Host> = Box::new(EngineHost {
            base: SandboxHost::new(),
        });

        // The override is live…
        assert_eq!(host.env_get("WORLD"), Some("engine".to_string()));
        assert_eq!(
            host.env_get("HOME"),
            None,
            "the sandbox fixture is shadowed"
        );

        // …and the forwarded capabilities reach the base: same first draws as a bare sandbox.
        let mut bare = SandboxHost::new();
        assert_eq!(host.rng_float(), bare.rng_float());
        assert_eq!(host.entropy_u64(), bare.entropy_u64());
        assert_eq!(host.id_next(), 1);
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS);
        host.fs_write("/f", "x").unwrap();
        assert_eq!(host.fs_read("/f").unwrap(), "x");
    }

    #[test]
    fn entropy_is_deterministic_and_independent_of_the_user_rng() {
        // Two fresh sandboxes produce the same entropy stream (the differential depends on it)…
        let mut a = SandboxHost::new();
        let mut b = SandboxHost::new();
        let draws: Vec<u64> = (0..4).map(|_| a.entropy_u64()).collect();
        assert_eq!(draws, (0..4).map(|_| b.entropy_u64()).collect::<Vec<_>>());

        // …drawing entropy must not perturb the user's `random` stream…
        let mut untouched = SandboxHost::new();
        assert_eq!(a.rng_float(), untouched.rng_float());

        // …and `random.seed` must not rewind the entropy stream: `a` has drawn 4, so its next
        // entropy value differs from a fresh stream's first, seed or no seed.
        a.rng_seed(42);
        assert_ne!(a.entropy_u64(), SandboxHost::new().entropy_u64());
    }

    #[test]
    fn os_fixtures_are_deterministic_and_exec_is_scripted() {
        let mut host = SandboxHost::new();
        // The introspection leaves are fixed fixtures — identical on every run/backend.
        assert_eq!(host.os_platform(), "sandbox");
        assert_eq!(host.os_arch(), "sandbox");
        assert_eq!(host.os_hostname(), "sandbox");
        assert_eq!(host.os_cpus(), 1);
        assert_eq!(host.os_cwd(), "/");
        assert_eq!(host.os_pid(), 1);
        // `echo` — status 0, stdout = args joined + newline.
        let r = host.os_exec("echo", &["a".into(), "b".into()]).unwrap();
        assert_eq!(
            (r.status, r.stdout.as_str(), r.stderr.as_str()),
            (0, "a b\n", "")
        );
        // `status n msg` — the failure fixture.
        let f = host
            .os_exec("status", &["3".into(), "boom".into()])
            .unwrap();
        assert_eq!(
            (f.status, f.stdout.as_str(), f.stderr.as_str()),
            (3, "", "boom\n")
        );
        // An unscripted command cannot start — an Io error, like a missing binary.
        let e = host.os_exec("frobnicate", &[]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Io);
    }

    #[test]
    fn os_spawn_is_scripted_and_deterministic() {
        let mut host = SandboxHost::new();
        // Spawned children get sequential handles and deterministic pseudo-pids.
        let a = host.os_spawn("echo", &["hi".into()]).unwrap();
        let b = host.os_spawn("echo", &["yo".into()]).unwrap();
        assert_eq!((a, b), (1, 2));
        assert_eq!(host.os_proc_pid(a), Some(10_001));
        assert_eq!(host.os_proc_pid(b), Some(10_002));
        assert_eq!(host.os_proc_pid(999), None);
        // The scripted child completed at spawn: wait returns its outcome, try_wait reports done.
        assert_eq!(host.os_proc_wait(a).unwrap().stdout, "hi\n");
        assert_eq!(
            host.os_proc_try_wait(a).unwrap().map(|r| r.stdout),
            Some("hi\n".to_string())
        );
        // kill is a no-op on the completed child; wait remains valid afterward.
        assert!(host.os_proc_kill(a).is_ok());
        assert_eq!(host.os_proc_wait(a).unwrap().stdout, "hi\n");
        // signal is a validated no-op on the completed child, idempotent across signals.
        use noeta_ext_abi::os::Signal;
        assert!(host.os_proc_signal(a, Signal::Term).is_ok());
        assert!(host.os_proc_signal(a, Signal::Hup).is_ok());
        assert_eq!(host.os_proc_wait(a).unwrap().stdout, "hi\n");
        // Operating on an unknown handle is an Io error.
        assert_eq!(host.os_proc_wait(999).unwrap_err().kind, ErrorKind::Io);
        assert_eq!(host.os_proc_kill(999).unwrap_err().kind, ErrorKind::Io);
        assert_eq!(
            host.os_proc_signal(999, Signal::Kill).unwrap_err().kind,
            ErrorKind::Io
        );
        // A spawn of an unstartable command fails at spawn, like a real missing binary.
        assert_eq!(
            host.os_spawn("frobnicate", &[]).unwrap_err().kind,
            ErrorKind::Io
        );
    }

    #[test]
    fn os_proc_read_line_streams_scripted_stdout() {
        let mut host = SandboxHost::new();
        // The scripted `echo` joins args with spaces; an embedded newline makes it multi-line.
        let p = host
            .os_spawn("echo", &["first\nsecond\nthird".into()])
            .unwrap();
        assert_eq!(host.os_proc_read_line(p).unwrap().as_deref(), Some("first"));
        assert_eq!(
            host.os_proc_read_line(p).unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(host.os_proc_read_line(p).unwrap().as_deref(), Some("third"));
        // Exhausted → None, and it stays None.
        assert_eq!(host.os_proc_read_line(p).unwrap(), None);
        assert_eq!(host.os_proc_read_line(p).unwrap(), None);
        // `wait` still returns the whole captured stdout, independent of the read cursor.
        assert_eq!(
            host.os_proc_wait(p).unwrap().stdout,
            "first\nsecond\nthird\n"
        );
        // stdin ops are validated no-ops on the scripted child; unknown handle is an Io error.
        assert!(host.os_proc_write_stdin(p, "x").is_ok());
        assert!(host.os_proc_close_stdin(p).is_ok());
        assert_eq!(host.os_proc_read_line(999).unwrap_err().kind, ErrorKind::Io);
        assert_eq!(
            host.os_proc_write_stdin(999, "x").unwrap_err().kind,
            ErrorKind::Io
        );
    }

    #[test]
    fn os_proc_read_chars_and_stderr_line() {
        let mut host = SandboxHost::new();
        // `read(n)` reads up to n characters (multibyte-aware) sharing the stdout cursor.
        let p = host.os_spawn("echo", &["héllo".into()]).unwrap(); // stdout "héllo\n"
        assert_eq!(host.os_proc_read(p, 3).unwrap().as_deref(), Some("hél"));
        assert_eq!(host.os_proc_read(p, 2).unwrap().as_deref(), Some("lo"));
        // read_line consumes the trailing newline as an empty final line; then exhausted.
        assert_eq!(host.os_proc_read_line(p).unwrap().as_deref(), Some(""));
        assert_eq!(host.os_proc_read(p, 1).unwrap(), None);
        // `read(0)` reads zero characters without consuming, until exhausted.
        let z = host.os_spawn("echo", &["z".into()]).unwrap();
        assert_eq!(host.os_proc_read(z, 0).unwrap().as_deref(), Some(""));

        // stderr streams on an independent cursor (scripted `status` writes stderr).
        let q = host
            .os_spawn("status", &["0".into(), "e1\ne2".into()])
            .unwrap();
        assert_eq!(
            host.os_proc_read_stderr_line(q).unwrap().as_deref(),
            Some("e1")
        );
        assert_eq!(
            host.os_proc_read_stderr_line(q).unwrap().as_deref(),
            Some("e2")
        );
        assert_eq!(host.os_proc_read_stderr_line(q).unwrap(), None);
        // Unknown handle → Io error on the new readers too.
        assert_eq!(host.os_proc_read(999, 1).unwrap_err().kind, ErrorKind::Io);
        assert_eq!(
            host.os_proc_read_stderr_line(999).unwrap_err().kind,
            ErrorKind::Io
        );
    }

    #[test]
    fn env_set_writes_the_fixture_view() {
        let mut host = SandboxHost::new();
        assert_eq!(host.env_get("K"), None);
        host.env_set("K", "v1");
        assert_eq!(host.env_get("K"), Some("v1".to_string()));
        host.env_set("K", "v2");
        assert_eq!(host.env_get("K"), Some("v2".to_string()));
        // Keys stay sorted with the fixture entries.
        assert_eq!(host.env_keys(), vec!["HOME", "K", "USER"]);
    }

    #[test]
    fn telemetry_recorder_captures_spans_deterministically() {
        let mut host = SandboxHost::new();

        // A root span with a child; the child inherits the root's trace id, the root gets a fresh
        // one. Ids derive from counters, so two fresh sandboxes agree byte-for-byte.
        let root = host.tel_span_start("request", SpanKind::Server, None);
        let parent_ctx = host.tel_span_context(root);
        host.tel_span_set_attr(root, "http.method", AttrValue::Str("GET".into()));
        let child = host.tel_span_start("db.query", SpanKind::Client, Some(parent_ctx));
        host.tel_span_set_status(child, SpanStatus::Ok);
        host.tel_span_end(child);
        host.tel_span_set_status(root, SpanStatus::Error("500".into()));
        host.tel_span_end(root);

        // Ended in end order (child first).
        let spans = host.recorded_spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].name, "db.query");
        assert_eq!(spans[1].name, "request");

        // The child's parent link + shared trace id.
        assert_eq!(spans[0].parent, Some(parent_ctx));
        assert_eq!(spans[0].context.trace_id, spans[1].context.trace_id);
        assert_ne!(spans[0].context.span_id, spans[1].context.span_id);

        // Attributes/status recorded; timestamps are the derived wall clock.
        assert_eq!(
            spans[1].attributes,
            vec![("http.method".into(), AttrValue::Str("GET".into()))]
        );
        assert_eq!(spans[1].status, SpanStatus::Error("500".into()));
        assert_eq!(spans[1].start_unix_ms, SANDBOX_EPOCH_MS);

        // Determinism: a second run records byte-identical spans.
        let mut host2 = SandboxHost::new();
        let r2 = host2.tel_span_start("request", SpanKind::Server, None);
        let p2 = host2.tel_span_context(r2);
        host2.tel_span_set_attr(r2, "http.method", AttrValue::Str("GET".into()));
        let c2 = host2.tel_span_start("db.query", SpanKind::Client, Some(p2));
        host2.tel_span_set_status(c2, SpanStatus::Ok);
        host2.tel_span_end(c2);
        host2.tel_span_set_status(r2, SpanStatus::Error("500".into()));
        host2.tel_span_end(r2);
        assert_eq!(host2.recorded_spans(), spans);
    }

    #[test]
    fn log_recorder_captures_records_deterministically() {
        use noeta_ext_abi::Severity;

        let emit = |host: &mut SandboxHost| {
            // A correlated record (emitted "inside" a span) + a top-level one.
            let span = host.tel_span_start("op", SpanKind::Internal, None);
            let ctx = host.tel_span_context(span);
            host.log_emit(LogRecord {
                unix_ms: 5,
                severity: Severity::Info,
                body: "inside".into(),
                attributes: vec![("k".into(), AttrValue::Int(1))],
                trace_context: Some(ctx),
            });
            host.tel_span_end(span);
            host.log_emit(LogRecord {
                unix_ms: 6,
                severity: Severity::Error,
                body: "top".into(),
                attributes: vec![],
                trace_context: None,
            });
        };

        let mut host = SandboxHost::new();
        emit(&mut host);
        let logs = host.recorded_logs();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].body, "inside");
        assert_eq!(logs[0].severity, Severity::Info);
        assert!(logs[0].trace_context.is_some());
        assert_eq!(logs[1].body, "top");
        assert!(logs[1].trace_context.is_none());

        // Determinism: a second sandbox records byte-identical log records (ids/timestamps derive
        // from counters + the logical clock).
        let mut host2 = SandboxHost::new();
        emit(&mut host2);
        assert_eq!(host2.recorded_logs(), logs);
    }

    #[test]
    fn traceparent_round_trips() {
        let mut host = SandboxHost::new();
        let span = host.tel_span_start("s", SpanKind::Internal, None);
        let ctx = host.tel_span_context(span);
        let header = ctx.to_traceparent();
        assert_eq!(header.len(), 55); // 00-<32>-<16>-<2>
        assert_eq!(TraceContext::parse(&header), Some(ctx));
        // A forgiving reader rejects malformed / all-zero contexts.
        assert_eq!(TraceContext::parse("garbage"), None);
        assert_eq!(
            TraceContext::parse("00-00000000000000000000000000000000-0000000000000000-01"),
            None
        );
    }

    #[test]
    fn unix_ms_is_a_derived_read_of_the_logical_clock() {
        let mut host = SandboxHost::new();
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS);
        // Reading wall time twice must not advance anything — not itself, not `monotonic`.
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS);
        assert_eq!(host.clock_monotonic(), 0);

        // `sleep` advances it like every other clock view (v7 ids order across sleeps).
        host.clock_sleep(250);
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS + 251); // 250 slept + 1 monotonic read
    }

    #[test]
    fn inbound_drives_the_fixed_script_then_signals_close() {
        let mut host = SandboxHost::new();
        let listener = host.net_listen("127.0.0.1:0").unwrap();

        // Every scripted request comes back in order, with a sequential conn id, then `None`.
        let script = crate::net::sandbox_request_script();
        for (i, expected) in script.iter().enumerate() {
            let (conn, request) = host.net_accept_next(listener).unwrap().unwrap();
            assert_eq!(conn, i as u64);
            assert_eq!(&request, expected);
            // Reply on that connection — recorded for introspection.
            host.net_reply_now(
                conn,
                crate::NetResponse {
                    status: 200,
                    headers: vec![],
                    body: format!("re:{}", request.method).into_bytes(),
                },
            )
            .unwrap();
        }
        // Script exhausted → the serve loop's stop signal.
        assert!(host.net_accept_next(listener).unwrap().is_none());

        // The transcript captured one reply per scripted request, in order.
        let transcript = &host.inbound.as_ref().unwrap().transcript;
        assert_eq!(transcript.len(), script.len());
        assert_eq!(transcript[0].0, 0);
        assert_eq!(transcript[2].1.body, b"re:POST");
    }
}
