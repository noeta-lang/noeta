//! The real host's side of a run's cancellation token (interruptible-io) — the fan-out that lets a
//! leaf blocked *outside* the interpreter end its own wait.
//!
//! [`noeta_stdlib::Cancellable`] hands a host two things: the run's flag, which is the only thing
//! allowed to *decide* anything, and a `CancelWake`, which is how a party that is not looking at
//! anything gets roused. A host has more than one such party (a child's output buffer, a streaming
//! body's channel, an HTTP request in flight) and they block on different primitives, so this type
//! sits between them: the host registers **one** hook on the wake, and that hook runs every live
//! party's own interruption.
//!
//! Parties are held weakly. A `CancelWake`'s hook list lives as long as the run, so registering one
//! hook per spawned child would accumulate a closure per child for a program that spawns in a loop;
//! holding the parties here instead means a dead party's entry is dropped the next time anything
//! registers.
//!
//! Ordering is what makes this race-free, and it is worth stating once because every party depends
//! on it. Arming stores the flag *before* registering the hook, and `CancelWake::register` runs a
//! hook whose wake already fired immediately — so a party that comes into existence after the
//! cancellation was requested never parks: it reads [`HostCancel::pending`] and returns before it
//! waits. A party that is *already* waiting is roused by the hook. There is no third case.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// Something inside a real host that can block outside the interpreter, and knows how to end its own
/// wait. The implementation's whole job is to make a blocked party **return**; it decides nothing —
/// the party re-reads [`HostCancel::pending`] and reports [`noeta_stdlib::ErrorKind::Interrupted`]
/// itself.
///
/// A hook must not wait on the party it rouses (the fan-out holds a lock across the call). Taking
/// the party's *own* short-lived lock is expected rather than forbidden, and for a condvar it is
/// required: a `notify_all` that does not synchronize with the waiter's flag check can be delivered
/// into the window between that check and the wait, and lost.
pub(crate) trait CancelParty: Send + Sync {
    /// End this party's current wait, if it has one. Idempotent, and a no-op when nothing is
    /// waiting — a party that is roused spuriously re-checks and carries on.
    fn interrupt(&self);
}

/// One real host's cancellation token: the run's flag, plus every live party to rouse when it is
/// set. Shared (`Arc`) into each party at construction, so a party made after arming is armed by
/// construction and one made before it reads the flag through the same cell.
#[derive(Default)]
pub(crate) struct HostCancel {
    /// The run's cancellation flag, installed once by
    /// [`Cancellable::set_cancel`](noeta_stdlib::Cancellable::set_cancel). Absent on a host nobody
    /// armed — an ordinary `noeta run`, which has no cancellation to observe — and then
    /// [`HostCancel::pending`] is permanently false, which is exactly right: those leaves block as
    /// they always did.
    flag: OnceLock<Arc<AtomicBool>>,
    /// Every party that can block, weakly. Dead entries are pruned at the next registration.
    parties: Mutex<Vec<Weak<dyn CancelParty>>>,
    /// The **async** side of the same fan-out, for a leaf that blocks on a future rather than on a
    /// primitive of its own — a request in flight, which is interrupted by racing it rather than by
    /// rousing anything. Kept here rather than as a party because there is nothing to hold weakly:
    /// a waiter exists only for the duration of one `select!`.
    interrupted: tokio::sync::Notify,
}

impl std::fmt::Debug for HostCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostCancel")
            .field("armed", &self.flag.get().is_some())
            .finish_non_exhaustive()
    }
}

impl HostCancel {
    /// Install the run's flag. Called once, at startup, before any user code runs; a second call is
    /// ignored, because a host serves exactly one run.
    pub(crate) fn arm(&self, flag: Arc<AtomicBool>) {
        let _ = self.flag.set(flag);
    }

    /// Whether this host serves a run that can be cancelled at all. False for an ordinary `noeta
    /// run`, which lets a leaf skip machinery that only exists to be interruptible.
    pub(crate) fn armed(&self) -> bool {
        self.flag.get().is_some()
    }

    /// Whether the run this host serves is being cancelled **right now**. False on an unarmed host,
    /// and false again once the VM has honored the request (it clears the flag), so a destructor
    /// running on the way out is not told to stop by a cancellation that is already spent.
    pub(crate) fn pending(&self) -> bool {
        self.flag
            .get()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// Add a party to rouse, and drop any that have since died.
    pub(crate) fn register(&self, party: &Arc<impl CancelParty + 'static>) {
        let mut parties = self.parties.lock().unwrap_or_else(|e| e.into_inner());
        parties.retain(|p| p.strong_count() > 0);
        parties.push(Arc::downgrade(party) as Weak<dyn CancelParty>);
    }

    /// Rouse every live party — the body of the single hook the host registers on the run's
    /// `CancelWake`.
    pub(crate) fn interrupt_all(&self) {
        self.interrupted.notify_waiters();
        let parties = self.parties.lock().unwrap_or_else(|e| e.into_inner());
        for party in parties.iter() {
            if let Some(party) = party.upgrade() {
                party.interrupt();
            }
        }
    }

    /// A future that resolves when this run is cancelled, for a leaf that races it against its own
    /// work. **`enable()` it before starting that work**, or `notify_waiters` fired in between is
    /// delivered to nobody — the same lost-wakeup shape the condvar parties close by taking their
    /// own lock, in the async spelling.
    pub(crate) fn interrupted(&self) -> tokio::sync::futures::Notified<'_> {
        self.interrupted.notified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A party that records how many times it was roused.
    #[derive(Default)]
    struct Counter(std::sync::atomic::AtomicUsize);

    impl CancelParty for Counter {
        fn interrupt(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Counter {
        fn count(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn an_unarmed_host_never_reports_a_pending_cancellation() {
        let cancel = HostCancel::default();
        assert!(!cancel.pending(), "nobody armed it");
        // And it still fans out without panicking, so a leaf need not ask whether it is armed.
        cancel.interrupt_all();
    }

    #[test]
    fn arming_makes_the_flag_observable_and_the_fan_out_reaches_every_party() {
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = HostCancel::default();
        cancel.arm(Arc::clone(&flag));
        let one = Arc::new(Counter::default());
        let two = Arc::new(Counter::default());
        cancel.register(&one);
        cancel.register(&two);

        assert!(!cancel.pending());
        flag.store(true, Ordering::Relaxed);
        assert!(cancel.pending());

        cancel.interrupt_all();
        assert_eq!((one.count(), two.count()), (1, 1));

        // Honoring the request clears the flag, and a later leaf must then block normally again.
        flag.store(false, Ordering::Relaxed);
        assert!(!cancel.pending());
    }

    #[test]
    fn a_dead_party_is_pruned_rather_than_roused() {
        let cancel = HostCancel::default();
        let gone = Arc::new(Counter::default());
        cancel.register(&gone);
        drop(gone);
        // The fan-out simply skips it…
        cancel.interrupt_all();
        // …and the next registration reclaims its slot, so a program spawning children in a loop
        // does not accumulate one entry per child for the life of the run.
        let live = Arc::new(Counter::default());
        cancel.register(&live);
        assert_eq!(
            cancel.parties.lock().unwrap().len(),
            1,
            "the dead entry was pruned by the registration"
        );
    }
}
