//! The **concurrency value kinds** (Tracks A/C + isolates): futures (thunk, timer, isolate,
//! async-IO), channel endpoints, and task handles. `impl Value` methods moved verbatim from the
//! crate root (audit-1 finding 8) — same crate, so private access is preserved; no behavior
//! change.

use crate::Value;
use crate::heap::{self, Payload};
use crate::ids::{ChannelId, ScopeId, TaskId};

impl Value {
    /// An async future (Track A): wraps `step`, the lazy thunk that runs the `async fn` body and
    /// returns the completion value (A.1). Owns one reference to the closure.
    pub fn make_future(step: Value) -> Value {
        step.inc_ref();
        heap::alloc(Payload::Future(step))
    }

    /// A **leaf timer future** (Track A.2): `sleep(ms)` produces one, carrying the absolute logical
    /// deadline (ms) at which it becomes ready. Unlike [`Self::make_future`] it wraps no closure — it
    /// is polled by consulting the executor clock (see [`Self::timer_deadline`]).
    pub fn make_timer(deadline: u64) -> Value {
        heap::alloc(Payload::Timer(deadline))
    }

    /// Whether this is a future — a step/thunk future ([`Payload::Future`]), a leaf timer
    /// ([`Payload::Timer`]), a task handle ([`Payload::Handle`]), or a leaf async-read
    /// ([`Payload::AsyncIo`]). All name their type "future" and display opaquely.
    pub fn is_future(self) -> bool {
        self.is_pointer()
            && heap::with_payload(self, |p| {
                matches!(
                    p,
                    Payload::Future(_)
                        | Payload::Timer(_)
                        | Payload::Handle(..)
                        | Payload::AsyncIo(_)
                        | Payload::ChannelSend(..)
                        | Payload::ChannelRecv(_)
                        | Payload::IsolateFuture(_)
                )
            })
    }

    /// Whether this is specifically a **step/thunk future** ([`Payload::Future`], a lowered
    /// `async fn` body) — the only future flavor the telemetry completion hook traces, on
    /// both backends identically. Non-retaining, unlike [`Self::future_step`].
    pub fn is_step_future(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Future(_)))
    }

    /// A **leaf isolate-result future** (isolates I.4b): a real-thread `isolate f(args)` yields one,
    /// carrying an id into the backend's isolate table (the worker's join handle + result receiver).
    /// Polled by harvesting the worker's marshalled result. VM-real path only.
    pub fn make_isolate_future(id: u32) -> Value {
        heap::alloc(Payload::IsolateFuture(id))
    }

    /// The backend isolate-table id of an [`Self::make_isolate_future`], if this is one.
    pub fn isolate_future_id(self) -> Option<u32> {
        if self.is_pointer() {
            heap::with_payload(self, |p| match p {
                Payload::IsolateFuture(id) => Some(*id),
                _ => None,
            })
        } else {
            None
        }
    }

    /// A **leaf async-read future** (Track A.4c): `fs.read_async(path)` produces one, carrying the id
    /// that tickets the pending read in the injected [`noeta_ext_abi::Executor`]. Polled by consulting
    /// the executor (see [`Self::async_io_id`]); it wraps no closure.
    pub fn make_async_io(id: u64) -> Value {
        heap::alloc(Payload::AsyncIo(id))
    }

    /// The executor ticket id of a leaf async-read future, or `None` if this is not one.
    pub fn async_io_id(self) -> Option<u64> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::AsyncIo(id) => Some(*id),
            _ => None,
        })
    }

    /// Whether this is a leaf timer future.
    pub fn is_timer(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Timer(_)))
    }

    /// A **channel sender endpoint** (isolates I.1): the `Sender<T>` `channel::<T>(cap)` yields,
    /// carrying the channel's id into the backend's channel table. A GC leaf.
    pub fn make_sender(id: ChannelId) -> Value {
        heap::alloc(Payload::Sender(id))
    }

    /// A **channel receiver endpoint** (isolates I.1). A GC leaf like [`Self::make_sender`].
    pub fn make_receiver(id: ChannelId) -> Value {
        heap::alloc(Payload::Receiver(id))
    }

    /// The channel id of a sender endpoint, or `None` if this is not one.
    pub fn sender_id(self) -> Option<ChannelId> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Sender(id) => Some(*id),
            _ => None,
        })
    }

    /// The channel id of a receiver endpoint, or `None` if this is not one.
    pub fn receiver_id(self) -> Option<ChannelId> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Receiver(id) => Some(*id),
            _ => None,
        })
    }

    /// A **leaf channel-send future** (isolates I.1): `tx.send(v)` produces one, carrying the channel
    /// `id` and **retaining its own reference** to the message `value` (like [`Self::make_future`] with
    /// its closure) — held until the message is enqueued or the future is dropped. The caller's own
    /// reference to `value` is released by its normal end-of-life.
    pub fn make_channel_send(id: ChannelId, value: Value) -> Value {
        value.inc_ref();
        heap::alloc(Payload::ChannelSend(
            id,
            value,
            noeta_ext_abi::channel::SendPhase::Fresh,
        ))
    }

    /// The rendezvous handoff phase of a channel-send future (isolates I.4c), or `None` if this is
    /// not one. Carried on the future so a capacity-0 send remembers, across re-polls, whether it has
    /// already deposited its message into the one-slot handoff.
    pub fn channel_send_phase(self) -> Option<noeta_ext_abi::channel::SendPhase> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::ChannelSend(_, _, phase) => Some(*phase),
            _ => None,
        })
    }

    /// Record the rendezvous handoff phase of a channel-send future (isolates I.4c). The future is a
    /// stable heap object across re-polls, so the transition to `Deposited` persists.
    pub fn set_channel_send_phase(self, phase: noeta_ext_abi::channel::SendPhase) {
        if !self.is_pointer() {
            return;
        }
        heap::with_payload_mut(self, |p| {
            if let Payload::ChannelSend(_, _, slot) = p {
                *slot = phase;
            }
        });
    }

    /// A **leaf channel-recv future** (isolates I.1): `rx.recv()` produces one, carrying the channel id.
    pub fn make_channel_recv(id: ChannelId) -> Value {
        heap::alloc(Payload::ChannelRecv(id))
    }

    /// The channel id and a freshly-retained owning reference to the queued message of a channel-send
    /// future, or `None` if this is not one. The caller takes ownership of the returned message.
    pub fn channel_send_parts(self) -> Option<(ChannelId, Value)> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::ChannelSend(id, value, _) => {
                value.inc_ref();
                Some((*id, *value))
            }
            _ => None,
        })
    }

    /// The channel id of a channel-recv future, or `None` if this is not one.
    pub fn channel_recv_id(self) -> Option<ChannelId> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::ChannelRecv(id) => Some(*id),
            _ => None,
        })
    }

    /// A **task handle** (Track A.3b): the `Future<T>` `spawn e` returns, referencing a task by its
    /// `(scope index, task index)` in the backend's concurrency-scope stack. A GC leaf.
    pub fn make_handle(scope: ScopeId, task: TaskId) -> Value {
        heap::alloc(Payload::Handle(scope, task))
    }

    /// Whether this is a task handle.
    pub fn is_handle(self) -> bool {
        self.is_pointer() && heap::with_payload(self, |p| matches!(p, Payload::Handle(..)))
    }

    /// The `(scope index, task index)` a task handle references, or `None` if this is not a handle.
    pub fn handle_parts(self) -> Option<(ScopeId, TaskId)> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Handle(scope, task) => Some((*scope, *task)),
            _ => None,
        })
    }

    /// The absolute logical deadline (ms) of a leaf timer future, or `None` if this is not a timer.
    pub fn timer_deadline(self) -> Option<u64> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Timer(deadline) => Some(*deadline),
            _ => None,
        })
    }

    /// The thunk/step closure a future wraps — a freshly-retained owning reference the caller drives
    /// via the backend's call machinery (Track A). `None` if this is not a future.
    pub fn future_step(self) -> Option<Value> {
        if !self.is_pointer() {
            return None;
        }
        heap::with_payload(self, |p| match p {
            Payload::Future(step) => {
                step.inc_ref();
                Some(*step)
            }
            _ => None,
        })
    }
}
