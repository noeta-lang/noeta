//! The JSPI executor (P-WASM W3.1) — genuine async overlap in the browser, on the exact seam
//! `RealExecutor` uses, with **zero ABI changes**.
//!
//! The mechanism: JSPI (JavaScript Promise Integration) lets one wasm import suspend the whole
//! wasm stack until a JS Promise settles — the browser event loop runs in the gap. That single
//! suspending import is `js_wait`; everything else is ordinary. The pieces:
//!
//! - [`BrowserFetchIo`] — the descriptor `BrowserHost::net_spawn` hands out. Its `run_real`
//!   body is a plain Rust future over a **JS fetch ticket**: the first poll calls
//!   `js_fetch_start` (begins `fetch()` in JS, returns immediately — no suspension), later
//!   polls call `js_fetch_take` (the settled reply, or pending). Its `run_sync` body falls back
//!   to the synchronous `net_fetch` leaf, so the same descriptor is serial-but-correct under
//!   the sandbox executor and the non-JSPI worker path.
//! - [`BrowserExecutor`] — `spawn_ext` takes the descriptor's `run_real` future and polls it
//!   once (starting the fetch), so N spawns put N requests in flight before anything waits;
//!   `advance` suspends on `js_wait(next timer or no timeout)` and the scheduler re-polls.
//!   Exactly `RealExecutor`'s shape with "the tokio runtime" replaced by "the browser event
//!   loop reached through one suspension point".
//!
//! Wall-clock: `now()` is elapsed ms since construction (the `RealExecutor` convention), so
//! async `sleep(ms)` deadlines are **real time** — `js_wait`'s timeout is a real `setTimeout`.
//!
//! Natively (unit tests) the ticket imports answer a canned reply instantly, which exercises
//! the full spawn → poll-once → pending/ready → take path without a browser.

use std::collections::{BTreeSet, HashMap};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use noeta_stdlib::{Executor, ExternIo, Host, NativeOut, NetRequest, RealBody, StdError};

use crate::browser_host::{imports, parse_reply, request_json};

/// An in-flight async body — [`RealBody::Async`]'s future type, named for the executor's table.
type PendingFuture = Pin<Box<dyn Future<Output = Result<NativeOut, StdError>> + Send>>;

/// The browser fetch descriptor (see the module docs): async body = a JS-ticket future, sync
/// body = the ordinary `net_fetch` leaf.
#[derive(Debug)]
pub struct BrowserFetchIo {
    request: Option<NetRequest>,
}

impl BrowserFetchIo {
    pub fn new(request: NetRequest) -> BrowserFetchIo {
        BrowserFetchIo {
            request: Some(request),
        }
    }
}

impl ExternIo for BrowserFetchIo {
    fn run_sync(&mut self, host: &mut dyn Host) -> Result<NativeOut, StdError> {
        let request = self.request.take().expect("one-shot descriptor");
        Ok(noeta_stdlib::net::fetch_outcome(host.net_fetch(request)))
    }

    fn run_real(&mut self) -> Option<RealBody> {
        let request = self.request.take().expect("one-shot descriptor");
        let url = request.url.clone();
        Some(RealBody::Async(Box::pin(FetchFuture {
            request: Some(request),
            url,
            ticket: 0,
        })))
    }
}

/// A fetch in flight as a plain Rust future: start once on the first poll, then poll the JS
/// ticket. The one-shot start is load-bearing — a re-fired start would duplicate the request
/// (and did, in this future's first draft: the request was put back for its URL and every poll
/// re-started the fetch; the URL now lives in its own field).
#[derive(Debug)]
struct FetchFuture {
    /// The request, consumed by the first poll (`None` = started).
    request: Option<NetRequest>,
    /// The request URL, kept past the start for error rendering.
    url: String,
    ticket: u64,
}

impl Future for FetchFuture {
    type Output = Result<NativeOut, StdError>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(request) = self.request.take() {
            self.ticket = imports::fetch_start(&request_json(&request));
            // A canned embedder (the native test double) may already be ready — fall through.
        }
        match imports::fetch_take(self.ticket) {
            None => Poll::Pending,
            Some(reply) => Poll::Ready(Ok(noeta_stdlib::net::fetch_outcome(parse_reply(
                &reply, &self.url,
            )))),
        }
    }
}

/// The JSPI-backed executor (see the module docs). Constructed per `noeta_run_browser_async`.
#[derive(Default)]
pub struct BrowserExecutor {
    /// `now()`'s zero point — elapsed real time, the `RealExecutor` convention.
    epoch_ms: u64,
    /// Pending async-`sleep` deadlines (elapsed ms), earliest first.
    timers: BTreeSet<u64>,
    /// Ready outcomes (spawned work that completed, or sync-fallback results), by ticket.
    ready: HashMap<u64, Result<NativeOut, StdError>>,
    /// In-flight futures (fetches with unsettled JS tickets), by ticket.
    pending: HashMap<u64, PendingFuture>,
    next_id: u64,
}

impl std::fmt::Debug for BrowserExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserExecutor")
            .field("timers", &self.timers)
            .field("ready", &self.ready.keys().collect::<Vec<_>>())
            .field("pending", &self.pending.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl BrowserExecutor {
    pub fn new() -> BrowserExecutor {
        BrowserExecutor {
            epoch_ms: imports::now_ms(),
            ..BrowserExecutor::default()
        }
    }

    /// Poll one pending future (no waker — the scheduler's re-poll IS the wakeup, driven by
    /// [`Executor::advance`]'s suspension), migrating it to `ready` when it completes.
    fn poll_pending(&mut self, id: u64) {
        let Some(future) = self.pending.get_mut(&id) else {
            return;
        };
        let mut cx = Context::from_waker(Waker::noop());
        if let Poll::Ready(outcome) = future.as_mut().poll(&mut cx) {
            self.pending.remove(&id);
            self.ready.insert(id, outcome);
        }
    }
}

impl Executor for BrowserExecutor {
    fn now(&self) -> u64 {
        imports::now_ms().saturating_sub(self.epoch_ms)
    }

    fn register_timer(&mut self, deadline: u64) {
        if deadline > self.now() {
            self.timers.insert(deadline);
        }
    }

    fn advance(&mut self) -> Option<u64> {
        let earliest_timer = self.timers.iter().next().copied();
        if earliest_timer.is_none() && self.pending.is_empty() {
            // Nothing can ever make progress — a deterministic deadlock, like every executor.
            return None;
        }
        // THE suspension point: park the wasm stack until a fetch settles or the earliest timer
        // fires (negative = no timeout). The event loop runs while we are parked.
        let timeout = earliest_timer
            .map(|deadline| deadline.saturating_sub(self.now()) as f64)
            .unwrap_or(-1.0);
        imports::wait(timeout);
        let now = self.now();
        self.timers.retain(|&deadline| deadline > now);
        Some(now)
    }

    fn spawn_ext(&mut self, host: &mut dyn Host, mut io: Box<dyn ExternIo>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        match io.run_real() {
            // The overlap path: poll once now — the fetch starts in JS immediately — and park
            // the future; N spawns put N requests in flight before anything suspends.
            Some(RealBody::Async(future)) => {
                self.pending.insert(id, future);
                self.poll_pending(id);
            }
            // No blocking pool exists in a worker: run a blocking body inline (correct, serial).
            Some(RealBody::Blocking(body)) => {
                self.ready.insert(id, body());
            }
            // No real body: the deterministic sync fallback, like every executor.
            None => {
                let outcome = io.run_sync(host);
                self.ready.insert(id, outcome);
            }
        }
        id
    }

    fn poll_ext(&mut self, id: u64) -> Option<Result<NativeOut, StdError>> {
        self.poll_pending(id);
        self.ready.remove(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_stdlib::SandboxHost;

    fn request(url: &str) -> NetRequest {
        NetRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: None,
            redirect_limit: None,
        }
    }

    #[test]
    fn the_fetch_future_path_runs_natively_on_the_canned_double() {
        // The native `imports` double answers instantly, so spawn → ready on the first poll —
        // exercising start/take/parse without a browser.
        let mut executor = BrowserExecutor::new();
        let mut host = SandboxHost::new();
        let a = executor.spawn_ext(
            &mut host,
            Box::new(BrowserFetchIo::new(request("https://x/a"))),
        );
        let b = executor.spawn_ext(
            &mut host,
            Box::new(BrowserFetchIo::new(request("https://x/b"))),
        );
        for id in [a, b] {
            let outcome = executor.poll_ext(id).expect("canned double is instant");
            assert!(outcome.is_ok(), "{outcome:?}");
        }
    }

    #[test]
    fn timers_register_and_advance_clears_them() {
        let mut executor = BrowserExecutor::new();
        assert!(executor.advance().is_none(), "nothing pending = deadlock");
        executor.register_timer(executor.now() + 1);
        // The native `wait` double returns immediately; real time then passes the deadline.
        loop {
            let now = executor.advance().expect("a timer is pending");
            if executor.timers.is_empty() {
                assert!(now >= 1);
                break;
            }
        }
    }
}
