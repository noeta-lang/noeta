//! The deterministic sandbox host (M2.1). The host-capability *traits* (`FileSystem`/`Rng`/
//! `Clock`/`Env`/`Entropy`/`Ids`/`Network`/`Host` + `FileReader`, and `ReadSource`) live in the ABI
//! crate ([`noeta_native::host`], re-exported here); this module provides the concrete
//! [`SandboxHost`] — the in-memory VFS, seeded PRNG, logical clock, and pure network responder that
//! conformance and `--differential` always run. It owns the *bytes* the capabilities read/write, so
//! it stays with the modules ([`crate::fs`], [`crate::random`], [`crate::net`]) whose state it holds.

pub use noeta_native::host::{
    Clock, Entropy, Env, FileReader, FileSystem, Host, Ids, Network, P2p, ReadSource, Rng,
};
pub use noeta_native::Telemetry;

use crate::StdError;
use crate::env;
use crate::fs::Vfs;
use crate::random;
use noeta_native::{AttrValue, SpanData, SpanEvent, SpanId, SpanKind, SpanStatus, TraceContext};
use std::collections::BTreeMap;

/// The sandbox's fixed wall-clock epoch: 2026-01-01T00:00:00Z in unix milliseconds.
/// `clock_unix_ms` on the sandbox is `SANDBOX_EPOCH_MS + logical clock`, so wall-time reads (and the
/// v7 UUIDs built from them) are deterministic, plausibly-dated, and advance under `sleep`.
pub const SANDBOX_EPOCH_MS: u64 = 1_767_225_600_000;

/// The sandbox entropy stream's fixed seed — a different arbitrary odd constant than
/// [`random::DEFAULT_SEED`] so the entropy and user-`random` streams never coincide.
pub const SANDBOX_ENTROPY_SEED: u64 = 0xA076_1D64_78BD_642F;

/// The deterministic sandbox: in-memory VFS, seeded SplitMix64 state, and a logical
/// clock — fresh per run, identical across backends by construction. This is what
/// the conformance harness gives both backends, so `--differential` stays
/// deterministic regardless of which host real (CLI/server) runs use.
#[derive(Debug, Clone)]
pub struct SandboxHost {
    fs: Vfs,
    rng: u64,
    /// The entropy stream's SplitMix64 state — independent of `rng` (see [`Entropy`]).
    entropy: u64,
    /// The next sequential id `id_next` hands out (see [`Ids`]).
    ids: u64,
    clock: u64,
    env: BTreeMap<String, String>,
    args: Vec<String>,
    /// The inbound server state (http-server S1), armed by `net_listen`. A sandbox run serves at
    /// most one listener — a differential program calls `http.serve` once — so a single slot
    /// suffices; a second `net_listen` re-arms it.
    inbound: Option<InboundState>,
    /// The p2p broker (p2p P1/P2): the deterministic in-process pub/sub log that is the sandbox's
    /// whole "network", so a publish/receive-or-sync program is byte-identical across backends and
    /// terminates in-oracle once its topics drain.
    p2p: noeta_native::P2pBroker,
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
}

/// The sandbox's inbound-server state: the fixed request script (see
/// [`crate::net::sandbox_request_script`]), a cursor into it, and a transcript of the replies the
/// handler produced (for test introspection — the differential observes the handler's own output).
#[derive(Debug, Clone)]
struct InboundState {
    script: Vec<crate::NetRequest>,
    cursor: usize,
    transcript: Vec<(u64, crate::NetResponse)>,
}

impl SandboxHost {
    /// A fresh sandbox: empty filesystem, default PRNG seed, clock at zero, and the
    /// fixed `env`/`args` fixture — matching the deterministic defaults both backends
    /// used before M2.1 plus the M2.2 host-introspection fixture.
    pub fn new() -> SandboxHost {
        SandboxHost {
            fs: Vfs::new(),
            rng: random::DEFAULT_SEED,
            entropy: SANDBOX_ENTROPY_SEED,
            ids: 1,
            clock: 0,
            env: env::sandbox_vars(),
            args: env::sandbox_args(),
            inbound: None,
            p2p: noeta_native::P2pBroker::default(),
            tel: TelRecorder {
                next_span: 1,
                next_trace: 1,
                live: BTreeMap::new(),
                recorded: Vec::new(),
            },
        }
    }

    /// The spans this sandbox has recorded (ended), in end order — test introspection for the
    /// telemetry conformance oracle (native OTEL). Spans not yet ended are not included.
    pub fn recorded_spans(&self) -> &[SpanData] {
        &self.tel.recorded
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

impl Rng for SandboxHost {
    fn rng_seed(&mut self, seed: i64) {
        self.rng = random::seed_state(seed);
    }

    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError> {
        let (next_state, value) = random::int(self.rng, lo, hi)?;
        self.rng = next_state;
        Ok(value)
    }

    fn rng_float(&mut self) -> f64 {
        let (next_state, value) = random::float(self.rng);
        self.rng = next_state;
        value
    }
}

impl Clock for SandboxHost {
    fn clock_monotonic(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.clock = self.clock.saturating_add(ms.max(0) as u64);
    }

    fn clock_unix_ms(&mut self) -> u64 {
        // A derived READ (no advance) — see the trait doc for why.
        SANDBOX_EPOCH_MS + self.clock
    }
}

impl Entropy for SandboxHost {
    fn entropy_u64(&mut self) -> u64 {
        let (next_state, value) = random::next(self.entropy);
        self.entropy = next_state;
        value
    }
}

impl Ids for SandboxHost {
    fn id_next(&mut self) -> u64 {
        let id = self.ids;
        self.ids += 1;
        id
    }
}

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
}

impl P2p for SandboxHost {
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

impl Env for SandboxHost {
    fn env_get(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    fn env_keys(&self) -> Vec<String> {
        self.env.keys().cloned().collect()
    }

    fn args(&self) -> Vec<String> {
        self.args.clone()
    }
}

impl Telemetry for SandboxHost {
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
            self.tel.recorded.push(s);
        }
    }

    fn tel_span_context(&mut self, span: SpanId) -> TraceContext {
        self.tel.live.get(&span).map_or(
            TraceContext {
                trace_id: [0u8; 16],
                span_id: [0u8; 8],
                sampled: false,
            },
            |s| s.context,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
