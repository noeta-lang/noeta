//! The cooperative **async / isolate scheduler**: the `poll_*` future-driving
//! methods, `spawn_isolate*` (cooperative + real-thread), `join_scope`,
//! `cancel_task`, and `drive_future`. These `impl Vm` methods run the stackless
//! executor — polling futures to their next suspend, spawning and joining
//! structured-concurrency scopes, and driving a top-level future to completion.
//! Moved verbatim from the crate root to shrink `lib.rs`; the dispatch loop and
//! `call_value` (in `lib.rs`) are the callers.

use std::sync::Arc;

use noeta_span::Span;
use noeta_value::{ScopeId, TaskId, Value};

use crate::*;

/// The cooperative async-scheduler state (audit-1 finding 3): the structured-concurrency
/// scope stack, the strand-local telemetry context, and the traced-future hook. One
/// sub-struct so a scheduler borrow (`&mut self.sched`) is disjoint from the module tables.
pub(crate) struct SchedState {
    /// The structured-concurrency scope stack (Track A.3b): one entry per open `concurrent { }` block,
    /// each a list of the tasks `spawn`ed in it. The scope owns one reference to each task's future (and
    /// its result once ready), released when the scope is joined and popped. Mirrors the tree-walker's
    /// `scopes`; both round-robin identically, so the differential holds by construction.
    ///
    /// A closed scope is **tombstoned** (task list drained, `scope_closed[i]` set), not removed, so scope
    /// indices stay stable for handles (Track A.7): a split `concurrent { }` in one task may finish while
    /// a *sibling* task's own `concurrent` scope is still open above it — out of structured-stack order —
    /// so popping the top would corrupt the sibling. Trailing tombstones are trimmed on close (the common
    /// LIFO case), so the Vec stays bounded by the concurrently-open high-water mark.
    pub(crate) scopes: Vec<Vec<Task>>,
    /// Whether each `scopes` slot is a closed tombstone (Track A.7). Parallel to `scopes`. Mirrors the
    /// tree-walker's `scope_closed`.
    pub(crate) scope_closed: Vec<bool>,
    /// The **current strand's task-local context** (native-otel T5a): an opaque `u64` stack
    /// extensions read through `NativeCtx::context_*` (telemetry's active-span stack is the first
    /// client). This cell always belongs to whichever strand is executing — the main strand (root)
    /// by default; the scheduler swaps a task's own saved context in around each poll of its step
    /// (`poll_all_scopes_round`), and a `spawn` snapshots it into the child. Mirrors the
    /// tree-walker's field, but carries no observable-output semantics (context is telemetry-only),
    /// so the differential is indifferent to it by construction.
    pub(crate) ctx_current: Vec<u64>,
    /// The **strand** currently executing (DAP worker debugging): main is `1`; the scheduler swaps
    /// a worker-isolate task's strand in around each poll (mirroring `ctx_current`) so the debugger
    /// reports a breakpoint inside a worker against that worker's DAP thread. `1` outside a polled
    /// isolate, and `next_strand` (from `2`) hands out ids at each cooperative `isolate` spawn.
    pub(crate) current_strand: u32,
    pub(crate) next_strand: u32,
    /// Whether telemetry is enabled, cached from the host at load (native-otel T5d perf): the
    /// enabled state is fixed per host (env-derived at construction), and the channel send/recv
    /// hot paths gate on it — a cached bool is one predictable branch instead of a virtual call.
    pub(crate) tel_on: bool,
    /// **Traced futures** (native-otel T5c) — the future-completion hook behind
    /// `NativeCtx::trace_future`: each entry holds one retained reference to a step future whose
    /// polls run under its saved context and whose completion (or abort) ends its telemetry span.
    /// Almost always empty (the hot check in `poll_once` is `is_empty()`); entries leave on
    /// completion, and teardown feeds strays into the collector roots then releases them, exactly
    /// like `ext_arena`.
    pub(crate) traced_futures: Vec<TracedFuture>,
}

impl<'m> Vm<'m> {
    /// Poll a future once (Track A.3 — the VM twin of the tree-walker's `Interpreter::poll_once`).
    /// A leaf timer is ready once the executor clock reaches its deadline, else it registers the
    /// deadline and reports `Pending`. A step future's poll runs the state machine to its next suspend:
    /// the step returns the raw completion value (`Ready`) or the pending sentinel (`Pending`).
    /// `future_step` hands back a retained reference the call borrows, released after. A non-future is
    /// passed through, freshly retained (totality; unreachable for a checked program).
    /// Poll a real-thread isolate future (isolates I.4b): non-blocking `try_recv` on the worker's
    /// result channel. A landed `Ok` rebuilds the marshalled result into this heap; an `Err` (the
    /// worker panicked) re-raises at this `.await`, consistent with a task; `Empty` is pending.
    fn poll_isolate(&mut self, id: u32, span: Span) -> Result<Poll, Abort> {
        use std::sync::mpsc::TryRecvError;
        let received = self.isolates.isolates[id as usize].result.try_recv();
        // **This is the harvest point, so this is where the worker's output block lands** — before
        // the outcome is turned into a value, a cancellation or an abort, so a failing worker's
        // `echo` still reaches the run's `RunResult` ahead of the error it raises here. See
        // [`IsolateOutput`](crate::lifecycle::IsolateOutput) for the ordering contract.
        let received = received.map(|report| {
            self.merge_isolate_output(report.output);
            report.outcome
        });
        match received {
            Ok(IsolateOutcome::Done(wire)) => {
                self.finish_isolate(id);
                Ok(Poll::Ready(isolate::rebuild(
                    &wire,
                    &self.persist.shapes,
                    &mut self.persist.channels,
                )))
            }
            // The worker honored a cancellation request at one of its safepoints (isolate-cancel):
            // it produced no value and its thread has ended, so this is the *terminal* cancelled
            // state — the only thing that lets the parent report `Err(Cancelled)` honestly.
            Ok(IsolateOutcome::Cancelled) => {
                self.finish_isolate(id);
                Ok(Poll::Cancelled)
            }
            Ok(IsolateOutcome::Failed(failure)) => {
                self.finish_isolate(id);
                // Install the worker's shipped traceback (if any) before raising, so the abort that
                // unwinds the parent renders the whole story: the worker's frames innermost, then the
                // parent's own frames (appended by `Vm::run`'s unwind as later segments). First abort
                // wins, as everywhere.
                if self.out.abort_trace.is_empty() {
                    self.out.abort_trace = failure.trace;
                }
                Err(self.error(
                    DiagnosticCode::Panic,
                    span,
                    format!("isolate panicked: {}", failure.message),
                ))
            }
            Err(TryRecvError::Empty) => Ok(Poll::Pending),
            Err(TryRecvError::Disconnected) => {
                self.finish_isolate(id);
                Err(self.error(
                    DiagnosticCode::Panic,
                    span,
                    "isolate worker terminated without a result".to_string(),
                ))
            }
        }
    }

    /// Join a finished isolate's worker thread and drop it from the in-flight count. When the
    /// last in-flight isolate is joined, no thread can be borrowing the shared region any more,
    /// so the promoted argument graphs are freed wholesale (P-PAR S2).
    fn finish_isolate(&mut self, id: u32) {
        if let Some(handle) = self.isolates.isolates[id as usize].handle.take() {
            let _ = handle.join();
        }
        self.isolates.inflight_isolates = self.isolates.inflight_isolates.saturating_sub(1);
        // Drop the worker's stall-registry slot now it is harvested (isolates I.4c) — on the parent
        // thread, balanced against the spawn-time `register_worker_stall`.
        self.deregister_worker_stall();
        if self.isolates.inflight_isolates == 0 {
            self.free_shared_region();
        }
    }

    /// Add a stall-registry slot for a spawned isolate worker (isolates I.4c), on the **parent
    /// thread** at spawn — so `active` counts the worker before its own thread has started and
    /// registered itself, which is the window that produced the false deadlock (the parent, briefly
    /// the only registered scheduler, saw `parked == active` and latched a join as a deadlock).
    /// No-op unless this parent participates in the registry (`stall_active`).
    pub(crate) fn register_worker_stall(&mut self) {
        if self.stall_active {
            isolate::STALL.register();
            self.registered_workers += 1;
        }
    }

    /// Drop one isolate-worker stall slot — at harvest ([`finish_isolate`](Self::finish_isolate)), or
    /// at teardown for any worker joined without a harvest. Balanced against `register_worker_stall`
    /// by the `registered_workers` count, so the registry returns to a clean state.
    pub(crate) fn deregister_worker_stall(&mut self) {
        if self.registered_workers > 0 {
            self.registered_workers -= 1;
            isolate::STALL.deregister();
        }
    }

    /// Free the borrow-share region and its promote-once memo (P-PAR S2). Sound only when no
    /// worker thread can still borrow it: every in-flight isolate joined (`finish_isolate` at
    /// count 0) or VM teardown after the defensive join loop. Idempotent (everything drains).
    pub(crate) fn free_shared_region(&mut self) {
        std::mem::take(&mut self.isolates.shared_region).free_all();
        self.isolates.promote_memo.clear();
        for source in std::mem::take(&mut self.isolates.promote_sources) {
            release(source);
        }
    }

    /// At a scheduler stall (no task completed, no channel op, no timer to advance): decide whether to
    /// keep looping (`true`, another thread could still make this VM progress) or declare a deadlock
    /// (`false`, the caller then raises E0010). Returns `false` immediately when no cross-thread work
    /// is outstanding — the deterministic cooperative deadlock the sandbox always hits (no real
    /// isolates, all channels `Local`), so in-oracle behavior is unchanged.
    ///
    /// When cross-thread work *is* outstanding (a real worker in flight, or an open shared channel),
    /// a **registered** parallel scheduler (isolates I.4c — the root parent and every isolate worker
    /// join the [`isolate::STALL`] registry for their driving lifetime) participates in the global
    /// **all-parties-blocked** check: it marks itself parked and, if *every* live registered scheduler
    /// is simultaneously parked here — none with a timer, pending IO, or a live counterparty — with no
    /// wake during the confirm window (a real progress event bumps [`isolate::WAKE`] past the pre-round
    /// generation `seen`), it latches the deadlock so every party unwinds with the same E0010 the
    /// sandbox produces, instead of spinning forever. An **unregistered** parallel scheduler keeps the
    /// pre-existing behavior — park a 5 ms quantum and keep looping — since it cannot judge a global
    /// deadlock (its counterparty may live on a thread it does not track), so it never false-positives.
    pub(crate) fn isolate_in_flight_wait(&self, seen: u64) -> bool {
        let cross_thread_pending = self.isolates.inflight_isolates > 0
            || self
                .persist
                .channels
                .iter()
                .any(|c| matches!(c, Channel::Shared(core) if core.is_open()));
        if !cross_thread_pending {
            return false; // no cross-thread work outstanding — the cooperative deadlock (sandbox path).
        }
        // A scheduler not registered in the stall registry keeps the pre-existing behavior: park a
        // quantum and keep looping. It cannot judge a *global* deadlock (its counterparty may live on
        // an unregistered thread), so it never false-positives.
        if !self.stall_active {
            isolate::WAKE.wait_past(seen, std::time::Duration::from_millis(5));
            return true;
        }
        // Registered: participate in the global all-parties-blocked check (isolates I.4c). Reaching
        // here means this scheduler has no local progress, no timer, and no pending IO. Mark parked;
        // if *every* live registered scheduler is now parked, no thread can issue a wake — a genuine
        // deadlock — confirmed by a wake window that a real progress event (which bumps `WAKE`) would
        // return from early. `seen` is the generation before this poll round, so progress made during
        // the round returns immediately.
        // Another party may have already confirmed the deadlock — unwind too (see the latch below).
        if isolate::STALL.is_deadlocked() {
            return false;
        }
        let all_parked = isolate::STALL.park();
        isolate::WAKE.wait_past(seen, std::time::Duration::from_millis(5));
        // A cross-thread progress event bumps the wake generation past `seen`; if it did not move,
        // nothing progressed during our wait. Re-check the all-parked state **before** unparking, so
        // the confirming read still counts this scheduler.
        let progressed = isolate::WAKE.generation() != seen;
        let deadlocked = isolate::STALL.is_deadlocked()
            || (all_parked && !progressed && isolate::STALL.all_parked());
        isolate::STALL.unpark();
        if deadlocked {
            // Latch it (and wake the other parked parties) so **every** scheduler observes the global
            // deadlock and unwinds — otherwise the rest stay blocked and the parent hangs joining
            // them. The caller then raises E0010.
            isolate::STALL.set_deadlocked();
            return false;
        }
        true
    }

    /// Poll a future once. The thin outer layer is the **traced-future hook** (native-otel T5c):
    /// a future registered via `NativeCtx::trace_future` polls under its own saved context (the
    /// task-swap discipline, applied to a bare future) and, on completion or abort, has its
    /// telemetry span ended here — the completion hook `with_span` over an async body needs.
    /// The untraced path is one `is_empty()` branch.
    pub(crate) fn poll_once(&mut self, future: Value, span: Span) -> Result<Poll, Abort> {
        if self.sched.traced_futures.is_empty() {
            return self.poll_once_inner(future, span);
        }
        let bits = future.bits();
        let Some(idx) = self
            .sched
            .traced_futures
            .iter()
            .position(|t| t.future.bits() == bits)
        else {
            return self.poll_once_inner(future, span);
        };
        let ctx = std::mem::take(&mut self.sched.traced_futures[idx].context);
        let saved = std::mem::replace(&mut self.sched.ctx_current, ctx);
        let polled = self.poll_once_inner(future, span);
        let ctx = std::mem::replace(&mut self.sched.ctx_current, saved);
        // Re-find by identity: a nested poll may have completed *another* traced future
        // (`swap_remove` moves entries), so `idx` cannot be trusted across the poll.
        if let Some(idx) = self
            .sched
            .traced_futures
            .iter()
            .position(|t| t.future.bits() == bits)
        {
            match &polled {
                Ok(Poll::Pending) => self.sched.traced_futures[idx].context = ctx,
                // `Cancelled` is terminal like `Ready` — the span ends, the future is reclaimed.
                Ok(Poll::Ready(_)) | Ok(Poll::Cancelled) | Err(_) => {
                    let traced = self.sched.traced_futures.swap_remove(idx);
                    if polled.is_err() {
                        self.persist.host.tel_span_set_status(
                            traced.span,
                            noeta_stdlib::SpanStatus::Error("span body aborted".into()),
                        );
                    }
                    self.persist.host.tel_span_end(traced.span);
                    self.release_value(traced.future);
                }
            }
        }
        polled
    }

    /// The sender's trace context to ride an outbound channel message (native-otel T5d): the
    /// current strand's active span's W3C context — `None` when telemetry is off or no span is
    /// active. The enabled state is a bool cached at `Vm::load` (`tel_on` — it is fixed per host),
    /// so an untraced program pays one predictable branch per send, not a virtual host call.
    fn outbound_trace_context(&mut self) -> Option<noeta_stdlib::TraceContext> {
        if !self.sched.tel_on {
            return None;
        }
        let top = *self.sched.ctx_current.last()?;
        Some(self.persist.host.tel_span_context(top))
    }

    /// Seed the receiving strand's context from a dequeued message's (native-otel T5d) — but only
    /// when the strand is **at top level**: an empty context, or exactly one remote seed left by a
    /// previous message (replaced, and released so a queue-worker's interned table stays bounded).
    /// A strand inside real active spans is never hijacked; a context-less message at top level
    /// *clears* a stale seed (work caused by an untraced producer starts a fresh trace).
    fn seed_context_from_message(&mut self, context: Option<noeta_stdlib::TraceContext>) {
        // Telemetry off ⇒ no message ever carries a context and seeding is pointless — one
        // predictable branch per recv (mirrors the send side).
        if !self.sched.tel_on {
            return;
        }
        let at_top = match self.sched.ctx_current.as_slice() {
            [] => true,
            [only] => self.persist.host.tel_is_remote(*only),
            _ => false,
        };
        if !at_top {
            return;
        }
        if let [old] = self.sched.ctx_current.as_slice() {
            let old = *old;
            self.persist.host.tel_release_remote(old);
            self.sched.ctx_current.clear();
        }
        if let Some(ctx) = context {
            let seed = self.persist.host.tel_intern_remote(ctx);
            self.sched.ctx_current.push(seed);
        }
    }

    fn poll_once_inner(&mut self, future: Value, span: Span) -> Result<Poll, Abort> {
        // A real-thread isolate future (I.4b): harvest the worker's marshalled result if it has landed.
        if let Some(id) = future.isolate_future_id() {
            return self.poll_isolate(id, span);
        }
        if future.is_timer() {
            let deadline = future
                .timer_deadline()
                .expect("a timer carries its deadline");
            if self.persist.executor.now() >= deadline {
                return Ok(Poll::Ready(Value::unit()));
            }
            self.persist.executor.register_timer(deadline);
            return Ok(Poll::Pending);
        }
        // A task handle (Track A.3b): ready iff its task has a stored result — polling a handle only
        // *reads* the task (the scheduler polls the task itself), so it retains and hands back the
        // stored result. A stale handle (its scope popped) reads as ready-unit, defensively.
        if let Some((si, ti)) = future.handle_parts() {
            let (si, ti) = (si.index(), ti.index());
            return Ok(match self.sched.scopes.get(si).and_then(|s| s.get(ti)) {
                Some(task) => match task.result {
                    Some(result) => {
                        retain(result);
                        Poll::Ready(result)
                    }
                    None => Poll::Pending,
                },
                None => Poll::Ready(Value::unit()),
            });
        }
        // A leaf async-IO future (Track A.4c/A.10): ask the executor whether the request completed.
        // Ready → the outcome as a value (read → a fresh `string`, write/append → unit); an IO failure
        // aborts (E0021) at the `.await`, matching the synchronous `fs.*`; pending → `Poll::Pending`
        // (the sandbox always resolves on the first poll).
        if let Some(id) = future.async_io_id() {
            return match self.persist.executor.poll_ext(id) {
                // Ready → materialize the descriptor's `NativeOut` exactly like a synchronous
                // dispatch result (extern-types X5); an IO failure aborts (E0021) at the
                // `.await`, matching the synchronous `fs.*`.
                Some(Ok(out)) => Ok(Poll::Ready(crate::values::materialize_native(out))),
                Some(Err(error)) => {
                    Err(self.error(stdlib_error_code(error.kind), span, error.message))
                }
                None => Ok(Poll::Pending),
            };
        }
        // A channel-send future (isolates I.1 + I.4c rendezvous): the shared `channel` policy decides
        // the action from the channel's scalar state and this send's rendezvous phase (carried on the
        // future). `channel_send_parts` hands back the channel id and a **freshly-retained** message —
        // transferred to the buffer on a deposit/deliver, released otherwise.
        if let Some((id, msg)) = future.channel_send_parts() {
            use noeta_stdlib::channel::{SendAction, SendPhase};
            let phase = future.channel_send_phase().unwrap_or(SendPhase::Fresh);
            let id = id.index();
            match &self.persist.channels[id] {
                Channel::Local {
                    buffer,
                    capacity,
                    closed,
                    ..
                } => {
                    let action =
                        noeta_stdlib::channel::poll_send(*capacity, buffer.len(), *closed, phase);
                    match action {
                        SendAction::Closed => {
                            release(msg);
                            return Err(self.error(
                                DiagnosticCode::Panic,
                                span,
                                "cannot send on a closed channel".to_string(),
                            ));
                        }
                        // Buffered deliver (complete now) or rendezvous deposit (park until taken):
                        // the message enters the one queue either way; the difference is the poll
                        // result and the phase transition.
                        SendAction::DeliverBuffered | SendAction::Deposit => {
                            // The sender's trace context rides the message (T5d) — automatic
                            // propagation without touching the message type.
                            let context = self.outbound_trace_context();
                            let Channel::Local { buffer, .. } = &mut self.persist.channels[id]
                            else {
                                unreachable!("just matched Local");
                            };
                            buffer.push_back((msg, context)); // ownership transfers to the queue
                            self.persist.channel_progress += 1;
                            return Ok(if action == SendAction::Deposit {
                                // A rendezvous send parks, recording that its message is now in the
                                // handoff, and completes only once a receiver takes it.
                                future.set_channel_send_phase(SendPhase::Deposited);
                                Poll::Pending
                            } else {
                                Poll::Ready(Value::unit())
                            });
                        }
                        // Rendezvous: the deposited message has been taken — complete. The fresh
                        // message copy this poll retained is not needed, so release it.
                        SendAction::Complete => {
                            release(msg);
                            self.persist.channel_progress += 1;
                            return Ok(Poll::Ready(Value::unit()));
                        }
                        SendAction::Park => {
                            release(msg);
                            return Ok(Poll::Pending);
                        }
                    }
                }
                // Shared cross-thread channel (I.4c): decide cheaply (no marshalling) first, then
                // marshal the message to `Wire` and push. A `Send` message graph is copied across the
                // thread boundary; the original reference is released once it lands (or on park).
                Channel::Shared(core) => {
                    let core = Arc::clone(core);
                    let action = core.send_action(phase);
                    match action {
                        SendAction::Closed => {
                            release(msg);
                            return Err(self.error(
                                DiagnosticCode::Panic,
                                span,
                                "cannot send on a closed channel".to_string(),
                            ));
                        }
                        SendAction::Complete => {
                            release(msg);
                            self.persist.channel_progress += 1;
                            return Ok(Poll::Ready(Value::unit()));
                        }
                        SendAction::Park => return Ok(Poll::Pending),
                        SendAction::DeliverBuffered | SendAction::Deposit => {
                            let wire = match isolate::marshal(
                                msg,
                                &self.persist.shapes,
                                &self.persist.channels,
                            ) {
                                Ok(w) => w,
                                Err(e) => {
                                    release(msg);
                                    return Err(self.error(
                                        DiagnosticCode::Panic,
                                        span,
                                        format!("channel message is not shippable: {e}"),
                                    ));
                                }
                            };
                            // The sender's trace context crosses the thread with the payload (T5d).
                            let context = self.outbound_trace_context();
                            if core.try_push(wire, context) {
                                release(msg);
                                self.persist.channel_progress += 1;
                                // A rendezvous deposit parks (recording its handoff) until a receiver
                                // takes it; a buffered deliver completes immediately.
                                return Ok(if action == SendAction::Deposit {
                                    future.set_channel_send_phase(SendPhase::Deposited);
                                    Poll::Pending
                                } else {
                                    Poll::Ready(Value::unit())
                                });
                            }
                            // Lost the race (filled/closed between the check and the push) — retry.
                            return Ok(Poll::Pending);
                        }
                    }
                }
            }
        }
        // A channel-recv future (isolates I.1): dequeue the next message (ready → `some(v)`), yield
        // `none` once closed and drained, else suspend on an empty open buffer. A dequeued message's
        // reference transfers out of the queue into the `some(..)` wrapper.
        if let Some(id) = future.channel_recv_id() {
            let id = id.index();
            match &self.persist.channels[id] {
                Channel::Local { buffer, closed, .. } => {
                    match noeta_stdlib::channel::poll_recv(buffer.len(), *closed) {
                        noeta_stdlib::channel::RecvAction::Deliver => {
                            let Channel::Local { buffer, .. } = &mut self.persist.channels[id]
                            else {
                                unreachable!("just matched Local");
                            };
                            let (msg, context) = buffer.pop_front().expect("non-empty");
                            // Seed the receiving strand from the message's context (T5d).
                            self.seed_context_from_message(context);
                            self.persist.channel_progress += 1;
                            return Ok(Poll::Ready(make_some(msg)));
                        }
                        noeta_stdlib::channel::RecvAction::ClosedEmpty => {
                            return Ok(Poll::Ready(make_none()));
                        }
                        noeta_stdlib::channel::RecvAction::Park => return Ok(Poll::Pending),
                    }
                }
                // Shared cross-thread channel (I.4c): dequeue a `Wire` and rebuild it into this heap.
                Channel::Shared(core) => {
                    let core = Arc::clone(core);
                    match core.try_recv() {
                        isolate::RecvState::Got(wire, context) => {
                            let value = isolate::rebuild(
                                &wire,
                                &self.persist.shapes,
                                &mut self.persist.channels,
                            );
                            // Seed the receiving strand from the message's context (T5d).
                            self.seed_context_from_message(context);
                            self.persist.channel_progress += 1;
                            return Ok(Poll::Ready(make_some(value)));
                        }
                        isolate::RecvState::ClosedEmpty => return Ok(Poll::Ready(make_none())),
                        isolate::RecvState::Empty => return Ok(Poll::Pending),
                    }
                }
            }
        }
        match future.future_step() {
            Some(step) => {
                let result = self.call_value(step, vec![Value::unit()], span);
                release(step);
                let result = result?;
                if result.is_pending() {
                    Ok(Poll::Pending)
                } else {
                    Ok(Poll::Ready(result))
                }
            }
            None => {
                retain(future);
                Ok(Poll::Ready(future))
            }
        }
    }

    /// Poll every not-yet-complete task in scope `si` once, storing any ready results; returns whether
    /// any task completed this round. Re-reads the task count each step, so tasks `spawn`ed mid-round
    /// are polled in the same round. The stored result carries the scope's owning reference.
    /// Poll every not-yet-complete task in **every open scope** once (Track A.7 — nested-`concurrent`
    /// interleaving), storing any ready results; returns whether any task completed this round. Polling
    /// across all scope levels (not just the innermost) is what lets an outer scope's spawned siblings
    /// keep making progress while an inner `concurrent` block is being joined. Re-reads the scope/task
    /// counts each step so tasks `spawn`ed mid-round are polled in the same round; a nested `concurrent`
    /// inside a *task body* still pushes/pops its own scope within that task's poll (balanced), so the
    /// scope stack is stable between polls here.
    pub(crate) fn poll_all_scopes_round(&mut self, span: Span) -> Result<bool, Abort> {
        let mut completed = false;
        let mut si = 0;
        while si < self.sched.scopes.len() {
            let mut ti = 0;
            while ti < self.sched.scopes[si].len() {
                let task = &self.sched.scopes[si][ti];
                // Skip a task whose step is *currently executing* (`polling`): a nested round — a
                // `concurrent` join inside that task's own body — reaching the task again must not
                // re-enter its mid-execution state machine (that re-runs the current segment:
                // infinite recursion). It is already progressing on the stack above us.
                if task.result.is_none() && !task.cancelled && !task.polling {
                    let future = task.future;
                    self.sched.scopes[si][ti].polling = true;
                    // Swap the task's own context in for the duration of its poll (T5a), so
                    // telemetry scope follows the task, not the interleaving. The paired swaps
                    // nest like parentheses across re-entrant rounds (a nested join inside this
                    // poll swaps inner tasks against *this* saved cell and restores it), and the
                    // `polling` guard above keeps each task's pair balanced.
                    let ctx = std::mem::take(&mut self.sched.scopes[si][ti].context);
                    let saved = std::mem::replace(&mut self.sched.ctx_current, ctx);
                    // Swap this task's strand in for the duration of its poll (DAP worker
                    // debugging), mirroring the context swap: a breakpoint tripped inside the poll
                    // reports the task's strand as the stopped DAP thread. Restored after, paired
                    // like the context swap (the `polling` guard keeps the pairs balanced).
                    let saved_strand = self.sched.current_strand;
                    self.sched.current_strand = self.sched.scopes[si][ti].strand;
                    let polled = self.poll_once(future, span);
                    self.sched.current_strand = saved_strand;
                    self.sched.scopes[si][ti].context =
                        std::mem::replace(&mut self.sched.ctx_current, saved);
                    self.sched.scopes[si][ti].polling = false;
                    match polled? {
                        Poll::Pending => {}
                        Poll::Ready(value) => {
                            // A worker-isolate root task finishing ends its strand (DAP worker
                            // debugging): tell the debugger so it emits the `thread` exited event.
                            if let Some(id) = self.sched.scopes[si][ti].isolate_strand
                                && let Some(dbg) = self.debugger.as_mut()
                            {
                                dbg.on_strand_exited(id);
                            }
                            self.sched.scopes[si][ti].result = Some(value);
                            completed = true;
                            // The task is done, so its **producer holds** end now — auto-closing any
                            // channel whose last producer just completed, while the scope is still
                            // open, so a sibling receiver drains then observes `none` instead of
                            // deadlocking (isolates I.4c). The future itself is left for `ScopeEnd`
                            // to reclaim, so captured-local destructors still fire at the join
                            // (unchanged, both backends agree); only the producer accounting
                            // resolves eagerly here.
                            let mut holds = std::mem::take(&mut self.sched.scopes[si][ti].holds);
                            self.release_task_holds(&mut holds);
                        }
                        // A real isolate that honored its cancellation request (isolate-cancel).
                        // Terminal, exactly like a completion — the worker's thread has ended — but
                        // there is no value, so the task goes to the `cancelled` state the join
                        // reads as `Err(Cancelled)`. Counts as progress so the scheduler does not
                        // mistake this round for a stall, and releases the producer holds the
                        // worker's `Sender`s were counted for, exactly as a completion does.
                        Poll::Cancelled => {
                            if let Some(id) = self.sched.scopes[si][ti].isolate_strand
                                && let Some(dbg) = self.debugger.as_mut()
                            {
                                dbg.on_strand_exited(id);
                            }
                            self.sched.scopes[si][ti].cancelled = true;
                            completed = true;
                            let mut holds = std::mem::take(&mut self.sched.scopes[si][ti].holds);
                            self.release_task_holds(&mut holds);
                        }
                    }
                }
                ti += 1;
            }
            si += 1;
        }
        Ok(completed)
    }

    /// Cancel the task a handle references (Track A.8) — a `race` loser, or a `h.cancel()`. A task
    /// that has already completed keeps its result; otherwise cancellation is *requested*, and how
    /// that request is honored depends on what kind of task it is:
    ///
    /// * A **cooperative task** is already parked between polls, so the request is exact and
    ///   immediate: mark it cancelled, the scheduler stops polling it, and the join treats it as
    ///   done. It never resumes past its last suspension. (Unchanged; this is the sandbox and
    ///   differential path, and it was always honest.)
    /// * A **real isolate** is an OS thread that is *running*, so the flag it needs is on the other
    ///   side of the thread boundary (isolate-cancel). Set the worker's shared flag and leave the
    ///   task live: the worker polls that flag at its safepoints and ships
    ///   [`IsolateOutcome::Cancelled`] home, which is what turns the task terminally cancelled
    ///   (`poll_isolate` → [`Poll::Cancelled`]). Until then the task is neither done nor cancelled,
    ///   so `join` and the scope's closing brace both keep driving — which is exactly the point:
    ///   marking it cancelled *here* is what used to let `join` report `Err(Cancelled)` for a
    ///   worker that then ran to completion anyway.
    ///
    /// A task's future is *not* released here either way — `ScopeEnd` reclaims it with the rest (so
    /// cancellation frees identically to a normal join, keeping both backends and the leak oracle in
    /// agreement).
    pub(crate) fn cancel_task(&mut self, handle: Value) {
        let Some((si, ti)) = handle.handle_parts() else {
            return;
        };
        let Some(task) = self
            .sched
            .scopes
            .get_mut(si.index())
            .and_then(|s| s.get_mut(ti.index()))
        else {
            return;
        };
        if task.result.is_some() {
            return; // already completed — a no-op, its result is preserved.
        }
        if let Some(id) = task.future.isolate_future_id() {
            self.request_isolate_cancel(id);
            return;
        }
        task.cancelled = true;
    }

    /// Signal a real-isolate worker to stop (isolate-cancel): a relaxed store on the flag the worker
    /// reads at every safepoint. Idempotent (the flag only ever goes `false → true`) and cheap —
    /// the worker does the noticing. Also wakes any parked scheduler, so a parent already blocked in
    /// the stall wait re-polls promptly once the worker's outcome lands.
    pub(crate) fn request_isolate_cancel(&mut self, id: u32) {
        self.isolates.isolates[id as usize]
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        isolate::WAKE.notify();
    }

    /// **The cancellation poll** (isolate-cancel): has whoever owns this run asked it to stop? A
    /// relaxed load — the flag only ever goes `false → true`, nothing is ordered against it, and
    /// the reaction (unwinding) is entirely local — so on x86-64 this is a plain load.
    ///
    /// The owner is the isolate's parent (`h.cancel()`) on a worker VM, or the embedder
    /// ([`RunOptions::cancel`](crate::RunOptions::cancel)) on a cancellable top-level run — the
    /// `noeta test` rail's overrunning case. `cancel_flag` is `None` on every *other* run, which is
    /// the case the dispatch loop's safepoints must not pay for: it compiles to a null test on a
    /// field already in cache, perfectly predicted. Measured cost on the tier-0 empty-loop floor:
    /// see the `isolate-cancel` notes in `docs/Concurrency-Internals.md`.
    #[inline]
    pub(crate) fn cancel_requested(&self) -> bool {
        self.isolates
            .cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Honor an observed cancellation request: latch it (so the worker's `Abort` is reported as
    /// [`IsolateOutcome::Cancelled`] rather than a failure), propagate it to every isolate this
    /// worker itself spawned — structured concurrency, downward: cancelling a subtree's root
    /// cancels the subtree — and hand back the `Abort` that unwinds this worker's frames.
    ///
    /// **Disarms the poll on the way out**, which is load-bearing rather than tidy: the abort's
    /// unwind and the teardown behind it both *run user code* — a frame local's `destruct`, then
    /// every global's — on fresh frame stacks that re-enter the dispatch loop, and `run_destructor`
    /// discards the `Abort` it gets back. A flag still set would therefore abort each destructor at
    /// its very first frame transfer, silently, and a cancelled worker would skip the destructors a
    /// completed one runs. The request has been honored exactly once; `cancel_observed` remembers
    /// that, so nothing is lost by no longer asking.
    ///
    /// On a **top-level** cancellable run (the `noeta test` rail) there is no `IsolateOutcome` to
    /// ship, so the latch has no reader and the abort simply unwinds into the ordinary teardown —
    /// which is the whole point: the run frees its heap, runs its destructors, and joins any
    /// isolates it spawned (`Vm::teardown`), so the thread ends cleanly instead of being abandoned.
    /// The resulting [`RunResult`](noeta_backend::RunResult) carries no diagnostic and exit code
    /// `0`; it describes a body that never finished and the caller that asked for the stop is
    /// expected to discard it.
    pub(crate) fn observe_cancel(&mut self) -> Abort {
        self.isolates.cancel_observed = true;
        self.isolates.cancel_flag = None;
        for id in 0..self.isolates.isolates.len() as u32 {
            self.request_isolate_cancel(id);
        }
        Abort
    }

    /// The safepoint form of [`Self::cancel_requested`]: unwind if this worker has been cancelled.
    /// Called from the scheduler's driving loops (a worker parked on a timer, an async-IO leaf, or a
    /// channel is *not* running bytecode, so the dispatch loop's safepoints never come around).
    #[inline]
    pub(crate) fn check_cancel(&mut self) -> Result<(), Abort> {
        if self.cancel_requested() {
            return Err(self.observe_cancel());
        }
        Ok(())
    }

    /// Spawn `isolate callee(args)` (isolates I.4b) and return its handle. Runs on a real OS thread
    /// when the VM is parallel and no argument ships a channel endpoint; otherwise a cooperative task
    /// (a non-parallel VM, or a channel-shipping isolate whose cross-thread support is I.4c).
    pub(crate) fn spawn_isolate(
        &mut self,
        callee: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        let real = self.isolates.parallel_isolates
            && self.isolates.isolate_module.is_some()
            && self.isolates.isolate_factory.is_some();
        // Channel endpoints over *shared* channels ship into a real isolate (I.4c); an unshippable
        // argument makes `try_spawn_isolate_real` decline (`None`), and either way we fall through to a
        // cooperative task.
        if real && let Some(handle) = self.try_spawn_isolate_real(callee, args, span)? {
            return Ok(handle);
        }
        self.spawn_isolate_coop(callee, args, span)
    }

    /// Register `future` as a task in the innermost scope (or hand it back bare if there is no scope —
    /// an orphan, already E0041 at check). The shared tail of the cooperative-spawn paths. `holds`
    /// are the channels the task holds a producer `Sender` for (isolates I.4c auto-close); they are
    /// counted onto those channels here and released when the task's future is reclaimed.
    fn register_task(&mut self, future: Value, holds: Vec<usize>) -> Value {
        if self.sched.scopes.is_empty() {
            return future;
        }
        for &cid in &holds {
            self.add_producer_hold(cid);
        }
        let scope_idx = self.innermost_open();
        let task_idx = self.sched.scopes[scope_idx].len();
        // The child inherits a snapshot of the spawner's task-local context (T5a).
        let context = self.sched.ctx_current.clone();
        // ...and the spawner's strand (DAP worker debugging): a plain task is cooperative
        // concurrency *within* the current thread, not a new one. An isolate root overrides this
        // (see `spawn_isolate_coop`).
        let strand = self.sched.current_strand;
        self.sched.scopes[scope_idx].push(Task {
            future,
            result: None,
            cancelled: false,
            polling: false,
            context,
            holds,
            strand,
            isolate_strand: None,
        });
        Value::make_handle(ScopeId::from_index(scope_idx), TaskId::from_index(task_idx))
    }

    /// The channel indices of every `Sender<T>` reachable from a spawned future's captures (isolates
    /// I.4c auto-close): a cycle-safe walk of the value graph. For the VM a future is a step closure
    /// whose upvalue **cells** hold the captured senders — `gc_children` follows only those explicit
    /// captures (never a lexical scope chain up to globals), so this mirrors `noeta-eval`'s
    /// immediate-capture walk and both backends count the same producer holds.
    pub(crate) fn collect_producer_channels(root: Value) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(v) = stack.pop() {
            if !v.is_pointer() || !seen.insert(v.bits()) {
                continue;
            }
            if let Some(cid) = v.sender_id() {
                out.push(cid.index());
                continue;
            }
            stack.extend(v.gc_children());
        }
        out
    }

    /// Register one producer hold on channel `cid` (isolates I.4c): a spawned task/isolate captured a
    /// `Sender` for it.
    pub(crate) fn add_producer_hold(&mut self, cid: usize) {
        match &mut self.persist.channels[cid] {
            Channel::Local { producers, .. } => *producers += 1,
            Channel::Shared(core) => core.add_producer(),
        }
    }

    /// End one producer hold on channel `cid` (isolates I.4c): its task completed or was reclaimed.
    /// Auto-closes the channel when its last producer is gone, marking channel progress so a parked
    /// receiver re-polls and observes the close.
    pub(crate) fn end_producer_hold(&mut self, cid: usize) {
        let now_closed = match &mut self.persist.channels[cid] {
            Channel::Local {
                producers, closed, ..
            } => {
                if noeta_stdlib::channel::producer_left(producers) {
                    *closed = true;
                    true
                } else {
                    false
                }
            }
            Channel::Shared(core) => core.drop_producer(),
        };
        if now_closed {
            self.persist.channel_progress += 1;
        }
    }

    /// Release the producer holds a task recorded, if not already released (isolates I.4c). Called
    /// when a task's future is reclaimed — early on completion, or at `ScopeEnd` for a task that
    /// never completed. Empties the list so the two paths never double-count.
    pub(crate) fn release_task_holds(&mut self, holds: &mut Vec<usize>) {
        for cid in std::mem::take(holds) {
            self.end_producer_hold(cid);
        }
    }

    /// The cooperative isolate path: build the future by calling `callee(args)` (a lazy `async fn`
    /// call constructs the state machine without running the body), then register it as a task —
    /// observationally identical to `spawn callee(args)`.
    ///
    /// The debugger's single-thread run always takes this path (real isolates are never armed under
    /// the debugger), so it is where a worker isolate becomes a debuggable DAP **thread**: the task
    /// gets a fresh strand id and the debugger is told the strand started (a `thread` event). The
    /// strand travels with the task through every poll (`poll_all_scopes_round`), so a breakpoint
    /// inside the isolate body reports this worker's thread; its completion fires `on_strand_exited`.
    fn spawn_isolate_coop(
        &mut self,
        callee: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Value, Abort> {
        // `call_value` takes ownership of its arguments; the values live in our caller's registers, so
        // retain one reference each to transfer.
        let owned: Vec<Value> = args
            .iter()
            .map(|&v| {
                retain(v);
                v
            })
            .collect();
        // A sender in the isolate's args is a producer hold (isolates I.4c auto-close).
        let holds: Vec<usize> = args
            .iter()
            .flat_map(|&v| Self::collect_producer_channels(v))
            .collect();
        // Mint this worker's strand id and announce it (DAP worker debugging) *before* the body can
        // run, so the `thread` started event precedes any `stopped` from inside it. Only meaningful
        // when a debugger is attached; on an ordinary run the name lookup and counter bump are the
        // whole cost, and the id is simply never observed.
        let strand = self.sched.next_strand;
        self.sched.next_strand += 1;
        let name = callee
            .as_closure()
            .and_then(|proto| self.module.protos[proto as usize].name.clone())
            .unwrap_or_else(|| "<isolate>".to_string());
        if let Some(dbg) = self.debugger.as_mut() {
            dbg.on_strand_started(strand, &name);
        }
        let future = self.call_value(callee, owned, span)?;
        let handle = self.register_task(future, holds);
        // Promote the just-registered task to a worker-isolate root on its own strand (register_task
        // defaulted it to the spawner's strand / no isolate marker).
        if let Some((si, ti)) = handle.handle_parts()
            && let Some(task) = self
                .sched
                .scopes
                .get_mut(si.index())
                .and_then(|s| s.get_mut(ti.index()))
        {
            task.strand = strand;
            task.isolate_strand = Some(strand);
        }
        Ok(handle)
    }

    /// The real-thread isolate path: marshal the arguments (and the current globals) into `Send` wire
    /// form, spawn an OS thread with its own VM + host + executor to run `callee(args)` to completion
    /// and marshal the result back, and register an [`Value::make_isolate_future`] task the scheduler
    /// harvests. Returns `Ok(None)` if an argument is unshippable (a channel endpoint), so the caller
    /// falls back to a cooperative task.
    fn try_spawn_isolate_real(
        &mut self,
        callee: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Option<Value>, Abort> {
        let Some(proto) = callee.as_closure() else {
            return Ok(None); // not a plain function value — cooperative fallback
        };
        // A `Sender` shipped into the worker is a producer hold on its shared channel; the parent
        // tracks it over the isolate's lifetime and auto-closes when the worker (its last producer)
        // completes (isolates I.4c). Computed before marshalling consumes the args.
        let holds: Vec<usize> = args
            .iter()
            .flat_map(|&v| Self::collect_producer_channels(v))
            .collect();
        let mut iso_args = Vec::with_capacity(args.len());
        for &v in args {
            // Borrow-share a promotable data graph (P-PAR S2): promote it into the VM's shared
            // region once — the memo makes the same corpus fanned to N workers a single
            // promotion — and hand the worker a zero-copy borrowed root. Immediates, channel
            // endpoints, function values, and anything else non-promotable keep the `Wire` copy.
            if v.is_pointer() && v.is_promotable_graph() {
                // The memo keys on the source's address: on first promotion, retain the source
                // into the region's lifetime so the entry can never alias a freed-and-reallocated
                // object. (Children get their own memo entries but stay alive through the root.)
                if !self.isolates.promote_memo.contains_key(&v.bits()) {
                    retain(v);
                    self.isolates.promote_sources.push(v);
                }
                let root = self
                    .isolates
                    .shared_region
                    .promote_with(v, &mut self.isolates.promote_memo);
                iso_args.push(isolate::IsoArg::Borrowed(isolate::SharedRoot::new(root)));
                continue;
            }
            match isolate::marshal(v, &self.persist.shapes, &self.persist.channels) {
                Ok(w) => iso_args.push(isolate::IsoArg::Copied(w)),
                Err(_) => return Ok(None), // unshippable arg — cooperative fallback
            }
        }
        // Snapshot the globals the worker can see (functions + value-type constants); skip any that are
        // unshippable (e.g. a class instance) — a v1 limitation, documented, since an isolate body that
        // referenced one would then fail at use rather than silently observing parent state.
        // Ship globals by slot id (P-VMT-GSLOT): the worker shares the same `Arc<Module>`, so slots
        // line up on both sides. A `None` (unbound) or unshippable slot is skipped.
        let mut wire_globals: Vec<(u32, isolate::Wire)> = Vec::new();
        // Globals the worker cannot see because they don't marshal (a `class` — reference identity;
        // a closure with captures; a `Local` channel endpoint). Their slots stay unbound on the
        // worker; recorded here (slot → type name) so a worker body that *reads* one gets a precise
        // diagnostic at use rather than a confusing "cannot find `x`" (isolates I.4b).
        let mut unshippable_globals: Vec<(u32, String)> = Vec::new();
        for (slot, v) in self.persist.globals.iter().enumerate() {
            if v.is_unbound() {
                continue;
            }
            match isolate::marshal(*v, &self.persist.shapes, &self.persist.channels) {
                Ok(w) => wire_globals.push((slot as u32, w)),
                // A non-value-type global: the worker only *needs* it if its body reads it, so this
                // is not a spawn-time error — the slot is skipped and flagged for a use-site error.
                // Prefer the shape name (the `class`/type name, e.g. `Counter`) over the generic
                // kind (`object`); fall back to the value kind for the shapeless cases.
                Err(_) => {
                    let ty = v
                        .shape()
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| v.type_name().to_string());
                    unshippable_globals.push((slot as u32, ty));
                }
            }
        }
        let module = Arc::clone(
            self.isolates
                .isolate_module
                .as_ref()
                .expect("parallel VM has a module"),
        );
        let factory = self
            .isolates
            .isolate_factory
            .as_ref()
            .expect("parallel VM has a factory")
            .clone();
        // The spawner's trace context crosses with the args (T5d): the worker interns it as its
        // root seed, so the isolate's spans continue this trace — real-path parity with the
        // cooperative task inheritance (T5a).
        let trace = self.outbound_trace_context();
        // The worker inherits this VM's registry across the thread boundary (instance-registry
        // IR3): a `&'static Registry` is `Send`, so a session with its own extension set resolves
        // native names identically on its isolates. `None` (the default) keeps the worker on the
        // process-global default, exactly as the parent.
        let registry = self.persist.registry;
        let profile_seam = self.isolates.profile_seam.clone();
        // The worker participates in the stall registry iff this parent does (isolates I.4c); its
        // `active` slot is already registered above, on the parent thread.
        let stall_tracked = self.stall_active;
        // This worker's cancellation flag (isolate-cancel): the parent stores through it from
        // `h.cancel()`, the worker reads it at every safepoint. A fresh flag per worker, so a nested
        // isolate is cancellable independently of the isolate that spawned it; a *cancelled* parent
        // worker propagates to its children explicitly (see `observe_cancel`).
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_handle = std::thread::spawn(move || {
            let msg = run_isolate_worker(
                &module,
                &factory,
                profile_seam,
                proto,
                iso_args,
                wire_globals,
                unshippable_globals,
                trace,
                registry,
                stall_tracked,
                worker_cancel,
                span,
            );
            let _ = tx.send(msg);
            // The result landing is cross-thread progress: wake the parent's parked scheduler
            // (P-PAR S3) so it harvests immediately instead of sleeping out its stall quantum.
            isolate::WAKE.notify();
        });
        let id = self.isolates.isolates.len() as u32;
        self.isolates.isolates.push(IsolateSlot {
            result: rx,
            handle: Some(thread_handle),
            cancel,
        });
        self.isolates.inflight_isolates += 1;
        // A worker that has *already* been cancelled must not leave a freshly spawned child running
        // (isolate-cancel): the child inherits the request immediately. Normally false — one relaxed
        // load per real spawn.
        if self.cancel_requested() {
            self.request_isolate_cancel(id);
        }
        // Register the worker's stall slot up front, on this (parent) thread — before the worker's
        // own thread starts — so `active` never lags a starting worker (isolates I.4c false-positive
        // fix).
        self.register_worker_stall();
        Ok(Some(
            self.register_task(Value::make_isolate_future(id), holds),
        ))
    }

    /// Open a structured-concurrency scope and return its (stable) index (Track A.7). Appends a fresh
    /// slot, so the new scope is the innermost; a subsequent `spawn` in the same straight-line segment
    /// lands in it. Mirrors the tree-walker's `open_scope`.
    pub(crate) fn open_scope(&mut self) -> usize {
        self.sched.scopes.push(Vec::new());
        self.sched.scope_closed.push(false);
        self.sched.scopes.len() - 1
    }

    /// The innermost still-open scope index (Track A.7) — the highest non-tombstoned slot. Used by
    /// `spawn` and the synchronous join/close (a split `concurrent { }` closes by its *captured* index).
    /// Panics only for a `spawn`/join with no open scope, which is E0041 at check. Mirrors the tree-walker.
    pub(crate) fn innermost_open(&self) -> usize {
        self.sched
            .scope_closed
            .iter()
            .rposition(|closed| !closed)
            .expect("an open concurrency scope")
    }

    /// Close the (already-drained) scope at index `si` (Track A.7): release each task's producer holds,
    /// future, and result (destructor-aware, mirroring the old `ScopeEnd` reclaim), tombstone the slot,
    /// then trim trailing tombstones so the Vec stays bounded (the common LIFO case reclaims at once).
    /// Closing by index — not popping the top — keeps a sibling scope still open above it intact. Mirrors
    /// the tree-walker's `close_scope`.
    pub(crate) fn close_scope(&mut self, si: usize) {
        let scope = std::mem::take(&mut self.sched.scopes[si]);
        for mut task in scope {
            self.release_task_holds(&mut task.holds);
            self.release_value(task.future);
            if let Some(result) = task.result {
                self.release_value(result);
            }
        }
        self.sched.scope_closed[si] = true;
        while self.sched.scope_closed.last() == Some(&true) {
            self.sched.scopes.pop();
            self.sched.scope_closed.pop();
        }
    }

    /// Join the innermost scope (Track A.3b): drive tasks round-robin until the innermost scope's tasks
    /// all complete. Each round polls **all** open scopes (A.7) so an outer scope's siblings interleave
    /// with the inner join; the loop exits on the *innermost* scope alone (outer scopes are joined by
    /// their own `ScopeEnd`). On a round where nothing completed, advance the logical clock; a pending
    /// scope with no timer to advance is a deterministic deadlock.
    /// `safepoint` carries the calling dispatch loop's live frame stack + register windows so each
    /// round can poll the safepoint-GC trigger (the tasks themselves are rooted through
    /// `sched.scopes`); `None` = never collect here (a caller whose Rust frame holds
    /// non-enumerable values).
    pub(crate) fn join_scope(
        &mut self,
        span: Span,
        safepoint: Option<(&[Frame], &[Value])>,
    ) -> Result<(), Abort> {
        let si = self.innermost_open();
        loop {
            // Safepoint-GC poll between rounds (memory-management 6.x): every task is parked (its
            // step returned), so the scheduler state is fully enumerable.
            if let Some((frames, regs)) = safepoint
                && noeta_value::safepoint_gc_pending()
            {
                self.maybe_safepoint_gc(frames, regs);
            }
            // Cancellation poll (isolate-cancel): a worker isolate blocked here — joining its own
            // `concurrent` block — runs no bytecode, so the dispatch loop's safepoints never come
            // around. `None` outside a worker, so this is a predicted no-op on every other run.
            self.check_cancel()?;
            // Snapshot the wake generation before polling (P-PAR S3): progress a worker makes
            // *during* this round then returns the stall wait immediately instead of parking.
            let wake_gen = isolate::WAKE.generation();
            let before = self.persist.channel_progress;
            let progressed = self.poll_all_scopes_round(span)?;
            if self.sched.scopes[si]
                .iter()
                .all(|t| t.result.is_some() || t.cancelled)
            {
                return Ok(());
            }
            // A channel op (a `send` unblocked, a `recv` drained) is progress even when no task
            // completed this round — otherwise a producer/consumer pair would look deadlocked.
            let progressed = progressed || self.persist.channel_progress != before;
            if !progressed
                && self.persist.executor.advance().is_none()
                && !self.isolate_in_flight_wait(wake_gen)
            {
                return Err(self.error(
                    DiagnosticCode::Panic,
                    span,
                    "async deadlock: a `concurrent` task is stuck with no pending timers"
                        .to_string(),
                ));
            }
        }
    }

    /// Drive an awaited future to completion via the executor (Track A.2/A.3 — a `.await` in inlined
    /// context: the top level or a `concurrent` block body). Polls the target; each iteration also
    /// drives every open `concurrent` scope's sibling tasks a round (A.7 — across all scope levels) so
    /// they interleave; advances the logical clock when nothing progresses; deadlocks if nothing can
    /// advance. Returns the completion value (owned). The caller's register keeps owning the future.
    /// `safepoint` carries the calling dispatch loop's live frame stack + register windows so each
    /// round can poll the safepoint-GC trigger — the awaited `future` itself stays rooted by the
    /// caller's register (dispatch) or by [`Vm::transient_roots`] (a depth-0 worker drive).
    /// `None` = never collect here (a `NativeCtx` drive: extension Rust frames can hold values
    /// the VM cannot enumerate).
    pub(crate) fn drive_future(
        &mut self,
        future: Value,
        span: Span,
        safepoint: Option<(&[Frame], &[Value])>,
    ) -> Result<Value, Abort> {
        // `.await` on a cancelled task is a **loud error** (Track A.8, E0056): a cancelled task never
        // produces a value, so awaiting one would otherwise hang until the deadlock guard fires or
        // silently yield a zero. Cancel-aware code uses `h.join()` (which reads the same drive but
        // reports the cancelled outcome) instead.
        match self.drive_future_outcome(future, span, safepoint)? {
            Some(value) => Ok(value),
            None => Err(self.error(
                DiagnosticCode::AwaitCancelled,
                span,
                "cannot await a cancelled task; use `.join()` to observe the cancelled outcome"
                    .to_string(),
            )),
        }
    }

    /// The shared drive loop behind `.await` ([`Self::drive_future`]) and `h.join()`
    /// ([`Self::join_task`]) (Track A.8): poll the target to completion via the executor, interleaving
    /// every open `concurrent` scope's tasks each round. Returns `Some(value)` when the future
    /// completes, or `None` when the target is a task **handle whose task was cancelled** — the
    /// terminal state a cancelled task stays in (never polled again, never gets a result). The two
    /// callers differ only in how they render that `None`: `.await` raises E0056, `join` wraps it as
    /// `Err(Cancelled)`.
    fn drive_future_outcome(
        &mut self,
        future: Value,
        span: Span,
        safepoint: Option<(&[Frame], &[Value])>,
    ) -> Result<Option<Value>, Abort> {
        loop {
            // Safepoint-GC poll between rounds — see `join_scope`.
            if let Some((frames, regs)) = safepoint
                && noeta_value::safepoint_gc_pending()
            {
                self.maybe_safepoint_gc(frames, regs);
            }
            // Cancellation poll (isolate-cancel) — see `join_scope`. A worker parked on a timer, an
            // async-IO leaf, or a channel is driving here, not dispatching bytecode.
            self.check_cancel()?;
            let wake_gen = isolate::WAKE.generation();
            let before = self.persist.channel_progress;
            match self.poll_once(future, span)? {
                Poll::Ready(value) => return Ok(Some(value)),
                // Driving a real-isolate future directly (a depth-0 worker drive, or an `.await`
                // whose target *is* the isolate future) and the worker honored its cancellation:
                // terminal, and there is no value.
                Poll::Cancelled => return Ok(None),
                Poll::Pending => {}
            }
            // A cancelled handle never becomes ready — report the cancelled outcome now rather than
            // spinning to a deadlock. Checked after the poll so a task cancelled by a sibling this
            // round is observed at once.
            if self.handle_cancelled(future) {
                return Ok(None);
            }
            let progressed = if self.sched.scopes.is_empty() {
                false
            } else {
                self.poll_all_scopes_round(span)?
            };
            // A channel op during any poll this iteration is progress (see `join_scope`).
            let progressed = progressed || self.persist.channel_progress != before;
            if !progressed
                && self.persist.executor.advance().is_none()
                && !self.isolate_in_flight_wait(wake_gen)
            {
                return Err(self.error(
                    DiagnosticCode::Panic,
                    span,
                    "async deadlock: awaited a pending future with no pending timers".to_string(),
                ));
            }
        }
    }

    /// Drive a task handle for `h.join()` (Track A.8) and report its outcome as a typed
    /// `Result<T, Cancelled>`: `Ok(value)` once the task completes, `Err(Cancelled)` if it was
    /// cancelled. The explicit, cancel-aware counterpart to plain `.await` (which raises E0056 on a
    /// cancelled task). Reuses the same interleaving drive, so joining composes with sibling tasks and
    /// nested scopes exactly as awaiting does. A bare (non-handle) future never appears cancelled, so
    /// `join` on one equals `Ok(future.await)`.
    pub(crate) fn join_task(&mut self, future: Value, span: Span) -> Result<Value, Abort> {
        // No safepoint frames here (a method-dispatch drive, like the `NativeCtx` combinators): the
        // handle is rooted through `sched.scopes`, and the caller's Rust frame holds the receiver.
        match self.drive_future_outcome(future, span, None)? {
            Some(value) => Ok(crate::values::make_ok(value)),
            None => Ok(crate::values::make_err(crate::values::make_cancelled())),
        }
    }

    /// Whether `future` is a task **handle** whose task has been cancelled (Track A.8) — the terminal
    /// state after `h.cancel()` (or a `race` loser): the task is never polled again and never gets a
    /// result. A non-handle future, or a handle whose task completed or is still pending, is `false`.
    pub(crate) fn handle_cancelled(&self, future: Value) -> bool {
        future
            .handle_parts()
            .and_then(|(si, ti)| {
                self.sched
                    .scopes
                    .get(si.index())
                    .and_then(|s| s.get(ti.index()))
            })
            .is_some_and(|task| task.result.is_none() && task.cancelled)
    }
}
