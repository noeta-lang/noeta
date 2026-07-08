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
        match self.isolates[id as usize].result.try_recv() {
            Ok(Ok(wire)) => {
                self.finish_isolate(id);
                Ok(Poll::Ready(isolate::rebuild(
                    &wire,
                    &self.shapes,
                    &mut self.channels,
                )))
            }
            Ok(Err(failure)) => {
                self.finish_isolate(id);
                // Install the worker's shipped traceback (if any) before raising, so the abort that
                // unwinds the parent renders the whole story: the worker's frames innermost, then the
                // parent's own frames (appended by `Vm::run`'s unwind as later segments). First abort
                // wins, as everywhere.
                if self.abort_trace.is_empty() {
                    self.abort_trace = failure.trace;
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
        if let Some(handle) = self.isolates[id as usize].handle.take() {
            let _ = handle.join();
        }
        self.inflight_isolates = self.inflight_isolates.saturating_sub(1);
        if self.inflight_isolates == 0 {
            self.free_shared_region();
        }
    }

    /// Free the borrow-share region and its promote-once memo (P-PAR S2). Sound only when no
    /// worker thread can still borrow it: every in-flight isolate joined (`finish_isolate` at
    /// count 0) or VM teardown after the defensive join loop. Idempotent (everything drains).
    pub(crate) fn free_shared_region(&mut self) {
        std::mem::take(&mut self.shared_region).free_all();
        self.promote_memo.clear();
        for source in std::mem::take(&mut self.promote_sources) {
            release(source);
        }
    }

    /// At a scheduler stall (no task completed, no channel op, no timer to advance): if another isolate
    /// thread could still make this VM progress — a real isolate worker (I.4b) is still running, or an
    /// open *shared* channel (I.4c) could yet be fed/drained by a worker — park on the [`isolate::WAKE`]
    /// eventcount (P-PAR S3) and report `true` ("keep looping") rather than declaring a deadlock.
    /// `seen` is the generation snapshot the caller took **before** its poll round, so progress made
    /// during the round returns immediately; the 5 ms timeout is a liveness backstop for a
    /// missed-notify bug, not part of the protocol (S0b measured the old 100 µs sleep-spin at
    /// ~160 µs/round on a cross-thread ping-pong). `false` when no cross-thread work is outstanding,
    /// so the caller raises the deterministic deadlock. Always `false` in the sandbox (no real
    /// isolates, all channels `Local`), so cooperative deadlock detection is unchanged in-oracle.
    pub(crate) fn isolate_in_flight_wait(&self, seen: u64) -> bool {
        let cross_thread_pending = self.inflight_isolates > 0
            || self
                .channels
                .iter()
                .any(|c| matches!(c, Channel::Shared(core) if core.is_open()));
        if cross_thread_pending {
            isolate::WAKE.wait_past(seen, std::time::Duration::from_millis(5));
            true
        } else {
            false
        }
    }

    pub(crate) fn poll_once(&mut self, future: Value, span: Span) -> Result<Poll, Abort> {
        // A real-thread isolate future (I.4b): harvest the worker's marshalled result if it has landed.
        if let Some(id) = future.isolate_future_id() {
            return self.poll_isolate(id, span);
        }
        if future.is_timer() {
            let deadline = future
                .timer_deadline()
                .expect("a timer carries its deadline");
            if self.executor.now() >= deadline {
                return Ok(Poll::Ready(Value::unit()));
            }
            self.executor.register_timer(deadline);
            return Ok(Poll::Pending);
        }
        // A task handle (Track A.3b): ready iff its task has a stored result — polling a handle only
        // *reads* the task (the scheduler polls the task itself), so it retains and hands back the
        // stored result. A stale handle (its scope popped) reads as ready-unit, defensively.
        if let Some((si, ti)) = future.handle_parts() {
            let (si, ti) = (si.index(), ti.index());
            return Ok(match self.scopes.get(si).and_then(|s| s.get(ti)) {
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
            return match self.executor.poll_ext(id) {
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
        // A channel-send future (isolates I.1): enqueue when the buffer has room (ready → unit), else
        // suspend. `channel_send_parts` hands back the channel id and a **freshly-retained** message —
        // transferred to the buffer on a push, released otherwise. Sending on a closed channel is a bug.
        if let Some((id, msg)) = future.channel_send_parts() {
            let id = id.index();
            match &self.channels[id] {
                Channel::Local {
                    buffer,
                    capacity,
                    closed,
                } => {
                    if *closed {
                        release(msg);
                        return Err(self.error(
                            DiagnosticCode::Panic,
                            span,
                            "cannot send on a closed channel".to_string(),
                        ));
                    }
                    if buffer.len() < *capacity {
                        let Channel::Local { buffer, .. } = &mut self.channels[id] else {
                            unreachable!("just matched Local");
                        };
                        buffer.push_back(msg); // ownership transfers to the queue
                        self.channel_progress += 1;
                        return Ok(Poll::Ready(Value::unit()));
                    }
                    release(msg);
                    return Ok(Poll::Pending);
                }
                // Shared cross-thread channel (I.4c): check room cheaply first (no marshalling on a
                // full-buffer poll), then marshal the message to `Wire` and push. A `Send` message
                // graph is copied across the thread boundary; the original reference is released once
                // it lands in the queue.
                Channel::Shared(core) => {
                    let core = Arc::clone(core);
                    match core.send_state() {
                        isolate::SendState::Closed => {
                            release(msg);
                            return Err(self.error(
                                DiagnosticCode::Panic,
                                span,
                                "cannot send on a closed channel".to_string(),
                            ));
                        }
                        isolate::SendState::Full => return Ok(Poll::Pending),
                        isolate::SendState::Room => {
                            let wire = match isolate::marshal(msg, &self.shapes, &self.channels) {
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
                            if core.try_send(wire) {
                                release(msg);
                                self.channel_progress += 1;
                                return Ok(Poll::Ready(Value::unit()));
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
            match &self.channels[id] {
                Channel::Local { buffer, closed, .. } => {
                    if !buffer.is_empty() {
                        let Channel::Local { buffer, .. } = &mut self.channels[id] else {
                            unreachable!("just matched Local");
                        };
                        let msg = buffer.pop_front().expect("non-empty");
                        self.channel_progress += 1;
                        return Ok(Poll::Ready(make_some(msg)));
                    }
                    if *closed {
                        return Ok(Poll::Ready(make_none()));
                    }
                    return Ok(Poll::Pending);
                }
                // Shared cross-thread channel (I.4c): dequeue a `Wire` and rebuild it into this heap.
                Channel::Shared(core) => {
                    let core = Arc::clone(core);
                    match core.try_recv() {
                        isolate::RecvState::Got(wire) => {
                            let value = isolate::rebuild(&wire, &self.shapes, &mut self.channels);
                            self.channel_progress += 1;
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
        while si < self.scopes.len() {
            let mut ti = 0;
            while ti < self.scopes[si].len() {
                let task = &self.scopes[si][ti];
                // Skip a task whose step is *currently executing* (`polling`): a nested round — a
                // `concurrent` join inside that task's own body — reaching the task again must not
                // re-enter its mid-execution state machine (that re-runs the current segment:
                // infinite recursion). It is already progressing on the stack above us.
                if task.result.is_none() && !task.cancelled && !task.polling {
                    let future = task.future;
                    self.scopes[si][ti].polling = true;
                    // Swap the task's own context in for the duration of its poll (T5a), so
                    // telemetry scope follows the task, not the interleaving. The paired swaps
                    // nest like parentheses across re-entrant rounds (a nested join inside this
                    // poll swaps inner tasks against *this* saved cell and restores it), and the
                    // `polling` guard above keeps each task's pair balanced.
                    let ctx = std::mem::take(&mut self.scopes[si][ti].context);
                    let saved = std::mem::replace(&mut self.ctx_current, ctx);
                    let polled = self.poll_once(future, span);
                    self.scopes[si][ti].context =
                        std::mem::replace(&mut self.ctx_current, saved);
                    self.scopes[si][ti].polling = false;
                    if let Poll::Ready(value) = polled? {
                        self.scopes[si][ti].result = Some(value);
                        completed = true;
                    }
                }
                ti += 1;
            }
            si += 1;
        }
        Ok(completed)
    }

    /// Cancel the task a handle references (Track A.8) — a `race` loser. A task that has already
    /// completed keeps its result; otherwise it is marked cancelled so the scheduler stops polling it
    /// and the join treats it as done. Its future is *not* released here — `ScopeEnd` reclaims it with
    /// the rest (so cancellation frees identically to a normal join, keeping both backends and the leak
    /// oracle in agreement). Cooperative: the task never resumes past its last suspension.
    pub(crate) fn cancel_task(&mut self, handle: Value) {
        if let Some((si, ti)) = handle.handle_parts()
            && let Some(task) = self
                .scopes
                .get_mut(si.index())
                .and_then(|s| s.get_mut(ti.index()))
            && task.result.is_none()
        {
            task.cancelled = true;
        }
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
        let real = self.parallel_isolates
            && self.isolate_module.is_some()
            && self.isolate_factory.is_some();
        // Channel endpoints over *shared* channels ship into a real isolate (I.4c); an unshippable
        // argument makes `try_spawn_isolate_real` decline (`None`), and either way we fall through to a
        // cooperative task.
        if real && let Some(handle) = self.try_spawn_isolate_real(callee, args, span)? {
            return Ok(handle);
        }
        self.spawn_isolate_coop(callee, args, span)
    }

    /// Register `future` as a task in the innermost scope (or hand it back bare if there is no scope —
    /// an orphan, already E0041 at check). The shared tail of the cooperative-spawn paths.
    fn register_task(&mut self, future: Value) -> Value {
        if self.scopes.is_empty() {
            return future;
        }
        let scope_idx = self.scopes.len() - 1;
        let task_idx = self.scopes[scope_idx].len();
        // The child inherits a snapshot of the spawner's task-local context (T5a).
        let context = self.ctx_current.clone();
        self.scopes[scope_idx].push(Task {
            future,
            result: None,
            cancelled: false,
            polling: false,
            context,
        });
        Value::make_handle(ScopeId::from_index(scope_idx), TaskId::from_index(task_idx))
    }

    /// The cooperative isolate path: build the future by calling `callee(args)` (a lazy `async fn`
    /// call constructs the state machine without running the body), then register it as a task —
    /// observationally identical to `spawn callee(args)`.
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
        let future = self.call_value(callee, owned, span)?;
        Ok(self.register_task(future))
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
                if !self.promote_memo.contains_key(&v.bits()) {
                    retain(v);
                    self.promote_sources.push(v);
                }
                let root = self.shared_region.promote_with(v, &mut self.promote_memo);
                iso_args.push(isolate::IsoArg::Borrowed(isolate::SharedRoot::new(root)));
                continue;
            }
            match isolate::marshal(v, &self.shapes, &self.channels) {
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
        for (slot, v) in self.globals.iter().enumerate() {
            if !v.is_unbound()
                && let Ok(w) = isolate::marshal(*v, &self.shapes, &self.channels)
            {
                wire_globals.push((slot as u32, w));
            }
        }
        let module = Arc::clone(
            self.isolate_module
                .as_ref()
                .expect("parallel VM has a module"),
        );
        let factory = self
            .isolate_factory
            .as_ref()
            .expect("parallel VM has a factory")
            .clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_handle = std::thread::spawn(move || {
            let msg = run_isolate_worker(&module, &factory, proto, iso_args, wire_globals, span);
            let _ = tx.send(msg);
            // The result landing is cross-thread progress: wake the parent's parked scheduler
            // (P-PAR S3) so it harvests immediately instead of sleeping out its stall quantum.
            isolate::WAKE.notify();
        });
        let id = self.isolates.len() as u32;
        self.isolates.push(IsolateSlot {
            result: rx,
            handle: Some(thread_handle),
        });
        self.inflight_isolates += 1;
        Ok(Some(self.register_task(Value::make_isolate_future(id))))
    }

    /// Join the innermost scope (Track A.3b): drive tasks round-robin until the innermost scope's tasks
    /// all complete. Each round polls **all** open scopes (A.7) so an outer scope's siblings interleave
    /// with the inner join; the loop exits on the *innermost* scope alone (outer scopes are joined by
    /// their own `ScopeEnd`). On a round where nothing completed, advance the logical clock; a pending
    /// scope with no timer to advance is a deterministic deadlock.
    pub(crate) fn join_scope(&mut self, span: Span) -> Result<(), Abort> {
        let si = self.scopes.len() - 1;
        loop {
            // Snapshot the wake generation before polling (P-PAR S3): progress a worker makes
            // *during* this round then returns the stall wait immediately instead of parking.
            let wake_gen = isolate::WAKE.generation();
            let before = self.channel_progress;
            let progressed = self.poll_all_scopes_round(span)?;
            if self.scopes[si]
                .iter()
                .all(|t| t.result.is_some() || t.cancelled)
            {
                return Ok(());
            }
            // A channel op (a `send` unblocked, a `recv` drained) is progress even when no task
            // completed this round — otherwise a producer/consumer pair would look deadlocked.
            let progressed = progressed || self.channel_progress != before;
            if !progressed
                && self.executor.advance().is_none()
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
    pub(crate) fn drive_future(&mut self, future: Value, span: Span) -> Result<Value, Abort> {
        loop {
            let wake_gen = isolate::WAKE.generation();
            let before = self.channel_progress;
            if let Poll::Ready(value) = self.poll_once(future, span)? {
                return Ok(value);
            }
            let progressed = if self.scopes.is_empty() {
                false
            } else {
                self.poll_all_scopes_round(span)?
            };
            // A channel op during any poll this iteration is progress (see `join_scope`).
            let progressed = progressed || self.channel_progress != before;
            if !progressed
                && self.executor.advance().is_none()
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
}
