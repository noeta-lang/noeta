//! The CRDT convergence core (p2p P0) — the **data-convergence** layer of the local-first stack
//! (architecture §9.15), split cleanly from the networking layer it will one day pair with.
//!
//! # What lives here (and what does not)
//!
//! This crate owns the **oracle-critical** half of collaborative state and nothing else: the
//! *merge algebra* over primitive replica state. It is **state-based** (a CvRDT / convergent
//! replicated data type): each value carries its whole state, and [`Mergeable::merge`] computes
//! the least upper bound of two states. The three laws every implementation here upholds —
//!
//! 1. **commutativity** — `a.merge(b) == b.merge(a)`,
//! 2. **associativity** — `(a.merge(b)).merge(c) == a.merge(b.merge(c))`, and
//! 3. **idempotence** — `a.merge(a) == a`
//!
//! — are exactly what let independent replicas converge to the same value *without coordination*,
//! regardless of the order or duplication of the updates they exchange. The crate's tests exercise
//! all three (example-based and property-based) over every type.
//!
//! Like [`noeta-reactive`](../noeta_reactive/index.html), this crate is **dependency-free**,
//! `unsafe`-free, holds **no language values**, and does **no I/O**. The `para.crdt` module in
//! `noeta-para-p2p` wraps these types as extern values and both backends run this identical code, so
//! the differential oracle holds by construction — the same guarantee reactivity's shared graph has.
//!
//! # Two determinism disciplines (the differential's proof obligations)
//!
//! 1. **Explicit replica identity.** A counter's state is keyed by a **replica id** — a string the
//!    caller supplies (`counter.increment("A", 1)`). In P0 there is no network and no node identity
//!    capability, so identity is explicit and deterministic; a real node's persisted Ed25519 id
//!    (§9.15.1) becomes the source of that string in a later slice, changing nothing here.
//! 2. **Ordered state.** Every collection is a [`BTreeMap`]/[`BTreeSet`], never a hash map: merge,
//!    equality, and display must be independent of iteration order, and hash-order would make the
//!    differential (and equality itself) nondeterministic.
//!
//! # Value semantics
//!
//! Every type is an **immutable value**: an update (`increment`, `insert`) returns a *new* state
//! rather than mutating in place, matching the language's value semantics and making purity
//! self-evident. This is why a CRDT can be a plain `Send` extern value with content equality — the
//! whole state is in the value, so copies are independent and `a.merge(b)` never touches `a` or `b`.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// A **state-based CRDT**: a join-semilattice whose [`merge`](Mergeable::merge) is commutative,
/// associative, and idempotent. Implementing this type is a *promise* to uphold those three laws —
/// it is what makes a value safe to replicate. In a later slice this Rust trait is the backing for
/// the language-level `Mergeable` bound that `synced_signal` requires of its state (§9.15.1); in P0
/// it is the internal contract the `para.crdt` surface dispatches `merge` through.
///
/// A replicated value must also be **wire-sendable** — its whole state has to cross the [`crate`]-
/// external P2p byte seam to reach a peer — so serialization is part of the contract (p2p P2.0),
/// provided once here over [`postcard`] (compact and deterministic: the same bytes on every host,
/// which the sandbox differential depends on). Concrete types only implement [`Mergeable::merge`].
pub trait Mergeable: Serialize + DeserializeOwned + Sized {
    /// The least upper bound of `self` and `other` — a new state greater than or equal to both
    /// under the lattice order. Never mutates either operand.
    fn merge(&self, other: &Self) -> Self;

    /// The full state serialized for the wire. Deterministic (the state's ordered collections
    /// serialize in a fixed order), so two hosts encode an equal value to equal bytes.
    fn to_bytes(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("CRDT state is always serializable")
    }

    /// Reconstruct state from wire bytes, or `None` if they are malformed / not this type — a
    /// received message is untrusted input, so decoding failure is an ordinary outcome, not a panic.
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

/// A **grow-only counter** (G-Counter): a per-replica map of monotonically increasing counts whose
/// value is their sum. Only *increment* is representable — the state can never move backward, which
/// is what makes the merge (element-wise max) a lattice join. It is the canonical smallest CRDT and
/// the building block of [`PnCounter`].
///
/// Counts saturate at [`u64::MAX`] rather than overflow — a grow-only counter is conceptually
/// unbounded, and saturation keeps every operation total (no panic path across the extern seam).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GCounter {
    /// Replica id → that replica's contributed count. Ordered so merge/equality/display are
    /// iteration-order-independent. An absent replica reads as 0.
    counts: BTreeMap<String, u64>,
}

impl GCounter {
    /// The empty counter — value 0, no replicas recorded.
    pub fn new() -> GCounter {
        GCounter::default()
    }

    /// A copy of this counter with `replica`'s count raised by `by` (saturating). The only update:
    /// grow-only means there is no decrement, which is what keeps merge a monotone join.
    pub fn increment(&self, replica: &str, by: u64) -> GCounter {
        let mut counts = self.counts.clone();
        let slot = counts.entry(replica.to_string()).or_insert(0);
        *slot = slot.saturating_add(by);
        GCounter { counts }
    }

    /// The counter's value: the saturating sum of every replica's count.
    pub fn value(&self) -> u64 {
        self.counts
            .values()
            .fold(0u64, |acc, &c| acc.saturating_add(c))
    }
}

impl Mergeable for GCounter {
    /// Element-wise maximum over the union of replica ids — the join of two grow-only counters.
    /// Commutative and associative because `max` is; idempotent because `max(c, c) == c`.
    fn merge(&self, other: &GCounter) -> GCounter {
        let mut counts = self.counts.clone();
        for (replica, &count) in &other.counts {
            let slot = counts.entry(replica.clone()).or_insert(0);
            *slot = (*slot).max(count);
        }
        GCounter { counts }
    }
}

/// A **PN-Counter** (positive-negative): two [`GCounter`]s — one accumulating increments, one
/// accumulating decrements — whose value is `positive - negative`. This is the standard way to get
/// a decrementable counter while keeping each half grow-only (and therefore a lattice), since a
/// single counter that could go down would not have a monotone merge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PnCounter {
    positive: GCounter,
    negative: GCounter,
}

impl PnCounter {
    /// The empty counter — value 0.
    pub fn new() -> PnCounter {
        PnCounter::default()
    }

    /// A copy with `replica`'s positive contribution raised by `by`.
    pub fn increment(&self, replica: &str, by: u64) -> PnCounter {
        PnCounter {
            positive: self.positive.increment(replica, by),
            negative: self.negative.clone(),
        }
    }

    /// A copy with `replica`'s negative contribution raised by `by`.
    pub fn decrement(&self, replica: &str, by: u64) -> PnCounter {
        PnCounter {
            positive: self.positive.clone(),
            negative: self.negative.increment(replica, by),
        }
    }

    /// The counter's value: total increments minus total decrements. The subtraction is done in
    /// [`i128`] and clamped to [`i64`] so a lopsided pair cannot wrap (the extern surface returns an
    /// `int`).
    pub fn value(&self) -> i64 {
        let net = i128::from(self.positive.value()) - i128::from(self.negative.value());
        net.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    }
}

impl Mergeable for PnCounter {
    /// Merge each half independently — a product of two lattices is a lattice, so the three laws
    /// follow from [`GCounter`]'s.
    fn merge(&self, other: &PnCounter) -> PnCounter {
        PnCounter {
            positive: self.positive.merge(&other.positive),
            negative: self.negative.merge(&other.negative),
        }
    }
}

/// A **grow-only set** (G-Set) of strings: elements can only be added, and merge is set union. The
/// simplest non-counter CRDT — a different lattice shape (union rather than per-key max) that proves
/// the convergence machinery generalizes past counters. Removal is deliberately absent: it needs an
/// add/remove CRDT (OR-Set) whose per-element tags belong with the value-carrying types in a later
/// slice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GSet {
    /// Members, ordered — so merge/equality/`members()` are iteration-order-independent.
    members: BTreeSet<String>,
}

impl GSet {
    /// The empty set.
    pub fn new() -> GSet {
        GSet::default()
    }

    /// A copy with `element` added (a no-op if already present — sets are idempotent under insert).
    pub fn insert(&self, element: &str) -> GSet {
        let mut members = self.members.clone();
        members.insert(element.to_string());
        GSet { members }
    }

    /// Whether `element` is a member.
    pub fn contains(&self, element: &str) -> bool {
        self.members.contains(element)
    }

    /// The number of members.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The members in sorted order.
    pub fn members(&self) -> Vec<String> {
        self.members.iter().cloned().collect()
    }
}

impl Mergeable for GSet {
    /// Set union — commutative, associative, and idempotent by definition of union.
    fn merge(&self, other: &GSet) -> GSet {
        let mut members = self.members.clone();
        members.extend(other.members.iter().cloned());
        GSet { members }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- GCounter -----------------------------------------------------------------------------

    #[test]
    fn gcounter_sums_across_replicas_and_is_grow_only() {
        let c = GCounter::new()
            .increment("A", 3)
            .increment("B", 4)
            .increment("A", 5);
        assert_eq!(c.value(), 12); // A=8, B=4
        // Grow-only: an older state never reappears; each increment yields a fresh, larger value.
        assert_eq!(GCounter::new().increment("A", 1).value(), 1);
    }

    #[test]
    fn gcounter_merge_takes_the_per_replica_max() {
        let a = GCounter::new().increment("A", 5).increment("B", 1);
        let b = GCounter::new().increment("A", 2).increment("B", 9);
        // max(A: 5,2)=5, max(B: 1,9)=9 → 14, either order.
        assert_eq!(a.merge(&b).value(), 14);
        assert_eq!(a.merge(&b), b.merge(&a));
    }

    #[test]
    fn gcounter_increment_saturates() {
        let c = GCounter::new().increment("A", u64::MAX).increment("A", 10);
        assert_eq!(c.value(), u64::MAX);
    }

    // --- PnCounter ----------------------------------------------------------------------------

    #[test]
    fn pncounter_nets_increments_against_decrements() {
        let c = PnCounter::new()
            .increment("A", 10)
            .decrement("A", 3)
            .decrement("B", 2);
        assert_eq!(c.value(), 5);
    }

    #[test]
    fn pncounter_can_go_negative() {
        let c = PnCounter::new().decrement("A", 7).increment("B", 2);
        assert_eq!(c.value(), -5);
    }

    #[test]
    fn pncounter_merge_is_order_independent() {
        let a = PnCounter::new().increment("A", 5).decrement("A", 1);
        let b = PnCounter::new().increment("A", 2).decrement("B", 4);
        assert_eq!(a.merge(&b), b.merge(&a));
        // A.pos=max(5,2)=5, A.neg=max(1,0)=1, B.neg=4 → 5 - 5 = 0.
        assert_eq!(a.merge(&b).value(), 0);
    }

    // --- GSet ---------------------------------------------------------------------------------

    // --- Wire serialization (p2p P2.0) --------------------------------------------------------

    #[test]
    fn every_crdt_round_trips_through_the_wire() {
        let g = GCounter::new().increment("A", 7).increment("B", 3);
        assert_eq!(GCounter::from_bytes(&g.to_bytes()), Some(g.clone()));

        let p = PnCounter::new().increment("A", 5).decrement("B", 2);
        assert_eq!(PnCounter::from_bytes(&p.to_bytes()), Some(p.clone()));

        let s = GSet::new().insert("x").insert("y");
        assert_eq!(GSet::from_bytes(&s.to_bytes()), Some(s.clone()));
    }

    #[test]
    fn a_decoded_peer_state_merges_like_the_original() {
        // The wire round-trip must preserve merge behavior: decoding B's state and merging it is
        // identical to merging B directly — the property the synced signal relies on.
        let a = GCounter::new().increment("A", 4);
        let b = GCounter::new().increment("B", 6);
        let decoded_b = GCounter::from_bytes(&b.to_bytes()).unwrap();
        assert_eq!(a.merge(&decoded_b), a.merge(&b));
        assert_eq!(a.merge(&decoded_b).value(), 10);
    }

    #[test]
    fn malformed_or_cross_type_bytes_decode_to_none_not_a_panic() {
        // Garbage bytes are untrusted input → None, never a panic.
        assert_eq!(GCounter::from_bytes(&[0xFF, 0xFF, 0xFF, 0xFF]), None);
    }

    #[test]
    fn gset_insert_is_idempotent_and_merge_is_union() {
        let a = GSet::new().insert("x").insert("y").insert("x");
        assert_eq!(a.len(), 2);
        assert!(a.contains("x"));
        let b = GSet::new().insert("y").insert("z");
        let m = a.merge(&b);
        assert_eq!(m.members(), vec!["x", "y", "z"]);
        assert_eq!(a.merge(&b), b.merge(&a));
    }

    // --- The three lattice laws, property-based over every type -------------------------------
    //
    // Random operation logs are folded into three independent replicas that each see a different
    // subset/order of the operations, then reconciled. Convergence is the assertion that the merge
    // of all three is identical no matter how they are combined — commutativity + associativity +
    // idempotence, exercised at once, which is precisely the "converge without coordination"
    // guarantee the whole local-first design rests on.

    /// One update: which replica, whether it is an increment (`true`) or decrement, and by how much.
    fn op_strategy() -> impl Strategy<Value = (String, bool, u64)> {
        (
            prop::sample::select(vec!["A", "B", "C"]).prop_map(String::from),
            any::<bool>(),
            0u64..1000,
        )
    }

    proptest! {
        /// GCounter: the three laws (over arbitrary states — the algebra holds regardless of how
        /// the states arose) plus convergence-to-full-sum (which additionally respects the CRDT
        /// contract that a replica id is only ever incremented on *its own* node, so each replica's
        /// total lives on exactly one node and per-replica `max` recovers the true sum).
        #[test]
        fn gcounter_laws(
            ops in prop::collection::vec((prop::sample::select(vec!["A","B","C"]).prop_map(String::from), 0u64..1000), 0..40),
        ) {
            // Three nodes, each owning the replica id of the same name — the valid usage: a node
            // only bumps its own replica entry.
            let node = |owner: &str| {
                ops.iter().filter(|(r, _)| r == owner)
                    .fold(GCounter::new(), |c, (r, by)| c.increment(r, *by))
            };
            let a = node("A");
            let b = node("B");
            let c = node("C");

            prop_assert_eq!(a.merge(&b), b.merge(&a));                              // commutative
            prop_assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)));          // associative
            prop_assert_eq!(a.merge(&a), a.clone());                               // idempotent
            // Convergence: every node that has seen all three states agrees, whatever the path...
            let via_ab = a.merge(&b).merge(&c);
            let via_ca = c.merge(&a).merge(&b);
            prop_assert_eq!(&via_ab, &via_ca);
            // ...and re-merging a stale copy (duplicate/out-of-order delivery) changes nothing.
            prop_assert_eq!(via_ab.merge(&a), via_ab.clone());
            // The converged value is the full sum (every op counted once).
            let full = ops.iter().fold(GCounter::new(), |acc, (r, by)| acc.increment(r, *by));
            prop_assert_eq!(via_ab.value(), full.value());
        }

        /// PnCounter: laws + convergence over mixed inc/dec logs.
        #[test]
        fn pncounter_laws(ops in prop::collection::vec(op_strategy(), 0..40)) {
            let apply = |c: PnCounter, (r, inc, by): &(String, bool, u64)| {
                if *inc { c.increment(r, *by) } else { c.decrement(r, *by) }
            };
            let a = ops.iter().step_by(2).fold(PnCounter::new(), apply);
            let b = ops.iter().skip(1).step_by(2).fold(PnCounter::new(), apply);
            let c = ops.iter().step_by(3).fold(PnCounter::new(), apply);

            prop_assert_eq!(a.merge(&b), b.merge(&a));
            prop_assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)));
            prop_assert_eq!(a.merge(&a), a.clone());
        }

        /// GSet: union laws + convergence over arbitrary element logs.
        #[test]
        fn gset_laws(elems in prop::collection::vec("[a-z]{1,3}", 0..40)) {
            let a = elems.iter().step_by(2).fold(GSet::new(), |s, e| s.insert(e));
            let b = elems.iter().skip(1).step_by(2).fold(GSet::new(), |s, e| s.insert(e));
            let c = elems.iter().step_by(3).fold(GSet::new(), |s, e| s.insert(e));

            prop_assert_eq!(a.merge(&b), b.merge(&a));
            prop_assert_eq!(a.merge(&b).merge(&c), a.merge(&b.merge(&c)));
            prop_assert_eq!(a.merge(&a), a.clone());
            // Convergence to the full element set.
            let full: BTreeSet<String> = elems.iter().cloned().collect();
            prop_assert_eq!(a.merge(&b).merge(&c).members(), full.into_iter().collect::<Vec<_>>());
        }
    }
}
