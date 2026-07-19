//! Channel semantics **policy** — the value-model-neutral rules both backends share so a
//! `Channel<T>`'s FIFO, bounded-capacity, capacity-0 rendezvous, close-state, and last-sender
//! auto-close behavior agree by construction (the differential holds without a shared value model).
//!
//! Only the *decision* lives here; the message buffers, the endpoint values, and the actual moves
//! stay per-backend (an `Rc`-based enum in `noeta-eval`, NaN-boxed heap words in `noeta-vm`). Each
//! backend threads its own scalar channel state — buffer length, capacity, closed flag, and the
//! current send's rendezvous [`SendPhase`] — into [`poll_send`]/[`poll_recv`] and performs the
//! returned [`SendAction`]/[`RecvAction`].
//!
//! Lifted out of the two backends' inline channel code when the rules changed (isolates I.4c v1
//! limits: rendezvous, auto-close, cross-isolate deadlock) — see `plans/backend-mirror.md`, whose
//! standing rule is "lift the next time its logic changes". It lives in `noeta-ext-abi` (rather than
//! `noeta-stdlib`) because the [`SendPhase`] tag rides on a `noeta-value` `Payload`, and the value
//! model depends on this contract crate, not on `noeta-stdlib`; `noeta-stdlib` re-exports it (its
//! `pub use noeta_ext_abi::*`), so both backends reach it as `noeta_stdlib::channel::*`.

/// A capacity-0 (**rendezvous**) send's handoff phase, carried by the send future itself. A
/// rendezvous send completes only once a receiver has *taken* its message, so it must remember,
/// across re-polls, whether it has already deposited into the one-slot handoff. Meaningless for a
/// buffered channel (a buffered send completes on deposit and never leaves [`SendPhase::Fresh`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SendPhase {
    /// The message has not yet been handed off — the send is still trying to deposit.
    #[default]
    Fresh,
    /// The message is parked in the rendezvous handoff slot, awaiting a receiver to take it.
    Deposited,
}

/// What a `send` poll should do, decided purely from the channel's scalar state and the send's
/// [`SendPhase`]. The backend performs the move the variant names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SendAction {
    /// The channel is closed and the message can never be received — abort ("cannot send on a
    /// closed channel", the E0010 family).
    Closed,
    /// Buffered channel with room: enqueue the message and complete (ready → unit).
    DeliverBuffered,
    /// Rendezvous: deposit the message into the (empty) one-slot handoff, transition to
    /// [`SendPhase::Deposited`], and park — completion waits for a receiver to take it.
    Deposit,
    /// Rendezvous: the deposited message has been taken — complete (ready → unit).
    Complete,
    /// No room (buffered) or the handoff is occupied / no receiver yet (rendezvous) — park (pending).
    Park,
}

/// The bounded/rendezvous **send** decision. `capacity == 0` is a rendezvous channel: the send hands
/// off directly and completes only after a receiver takes the message (so ordering is observable —
/// the sender proceeds *after* the receive). `capacity >= 1` is a buffered channel: the send
/// completes as soon as the message is enqueued into an open buffer with room.
pub fn poll_send(capacity: usize, buffer_len: usize, closed: bool, phase: SendPhase) -> SendAction {
    if capacity == 0 {
        // Rendezvous: the buffer is the single handoff slot (transiently holds 0 or 1 message).
        match phase {
            SendPhase::Fresh => {
                if closed {
                    SendAction::Closed
                } else if buffer_len == 0 {
                    SendAction::Deposit
                } else {
                    // Another sender's handoff is in flight — rendezvous serializes; park.
                    SendAction::Park
                }
            }
            // Once deposited, a close does not fail the send — a receiver may still take the parked
            // message (recv drains before it observes closed). Completion is purely "was it taken?".
            SendPhase::Deposited => {
                if buffer_len == 0 {
                    SendAction::Complete
                } else {
                    SendAction::Park
                }
            }
        }
    } else if closed {
        SendAction::Closed
    } else if buffer_len < capacity {
        SendAction::DeliverBuffered
    } else {
        SendAction::Park
    }
}

/// What a `recv` poll should do. Identical for buffered and rendezvous channels — a rendezvous
/// deposit lands in the same one-slot buffer a buffered message would, so draining is uniform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecvAction {
    /// A message is buffered — dequeue it and complete (ready → `some(v)`).
    Deliver,
    /// The channel is closed and drained — complete (ready → `none`).
    ClosedEmpty,
    /// The buffer is empty and the channel is open — park (pending).
    Park,
}

/// The **recv** decision: drain a buffered message; otherwise yield `none` once closed-and-drained,
/// else park.
pub fn poll_recv(buffer_len: usize, closed: bool) -> RecvAction {
    if buffer_len > 0 {
        RecvAction::Deliver
    } else if closed {
        RecvAction::ClosedEmpty
    } else {
        RecvAction::Park
    }
}

/// Register that one **producer hold** ended — a spawned task/isolate that had captured a `Sender`
/// for this channel completed. Returns `true` when the channel should now **auto-close** (the last
/// producer is gone, so receivers should drain then observe `none` instead of blocking forever).
/// Idempotent explicit `close()` stays a separate path; an already-closed channel stays closed.
/// Saturating so a stray extra drop can never wrap. Auto-close is keyed on producer-task lifecycle
/// (not raw sender-value RC) because the enclosing async/top-level scope retains a structural sender
/// until it ends — too late to signal "no more sends"; see the backend `Channel::Local::producers`.
pub fn producer_left(producers: &mut u32) -> bool {
    *producers = producers.saturating_sub(1);
    *producers == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_send_delivers_until_full_then_parks() {
        // Capacity 2: room → deliver-and-complete, full → park, closed → error.
        assert_eq!(
            poll_send(2, 0, false, SendPhase::Fresh),
            SendAction::DeliverBuffered
        );
        assert_eq!(
            poll_send(2, 1, false, SendPhase::Fresh),
            SendAction::DeliverBuffered
        );
        assert_eq!(poll_send(2, 2, false, SendPhase::Fresh), SendAction::Park);
        assert_eq!(poll_send(2, 0, true, SendPhase::Fresh), SendAction::Closed);
    }

    #[test]
    fn rendezvous_send_deposits_then_completes_on_take() {
        // Capacity 0: a fresh send deposits into the empty handoff, then parks (Deposited) until a
        // receiver drains it (buffer back to empty) → complete. A second fresh send parks while the
        // handoff is occupied. A close before depositing is an error; after depositing it does not
        // fail the send (a receiver may still take the parked message).
        assert_eq!(
            poll_send(0, 0, false, SendPhase::Fresh),
            SendAction::Deposit
        );
        assert_eq!(poll_send(0, 1, false, SendPhase::Fresh), SendAction::Park);
        assert_eq!(
            poll_send(0, 1, false, SendPhase::Deposited),
            SendAction::Park
        );
        assert_eq!(
            poll_send(0, 0, false, SendPhase::Deposited),
            SendAction::Complete
        );
        assert_eq!(poll_send(0, 0, true, SendPhase::Fresh), SendAction::Closed);
        assert_eq!(
            poll_send(0, 0, true, SendPhase::Deposited),
            SendAction::Complete
        );
    }

    #[test]
    fn recv_delivers_then_none_on_closed_drained() {
        assert_eq!(poll_recv(1, false), RecvAction::Deliver);
        assert_eq!(poll_recv(1, true), RecvAction::Deliver); // drain before observing closed
        assert_eq!(poll_recv(0, false), RecvAction::Park);
        assert_eq!(poll_recv(0, true), RecvAction::ClosedEmpty);
    }

    #[test]
    fn producer_left_closes_only_on_last() {
        let mut n = 2;
        assert!(!producer_left(&mut n)); // 2 -> 1
        assert!(producer_left(&mut n)); //  1 -> 0, auto-close
        // Saturating: a stray extra drop cannot wrap or re-fire.
        assert!(producer_left(&mut n)); // 0 -> 0, still "at zero"
    }
}
