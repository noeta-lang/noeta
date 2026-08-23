//! **The width-door census**: what each collection surface discloses about a fixed-width integer,
//! and which of them must consult a [`crate::render_hint::RenderHint`] to be right.
//!
//! # The bug class this exists to end
//!
//! A width lives only in the **static type** — every integer width is the same erased 64-bit word
//! at run time. So a `u64` above `i64::MAX` reads back correctly only where something threaded the
//! type's hint to the site that renders or orders it. The mechanism for doing that is factored
//! properly: one [`crate::render_hint::unsigned_digits`], one
//! [`crate::render_hint::unsigned_order`], one `RenderHint`, all here in the lean crate so both
//! backends run the same bodies rather than re-deriving them.
//!
//! What kept breaking was never *duplication*. It was **completeness**. The set of doors that
//! reveal a width is open-ended — `sorted`, `min`/`max`, a set's rendered order, a map's key order,
//! `keys()`, `values()`, iteration, packed key display, packed key order, a nested packed key, and
//! most recently JSON object key order — and each is a separate site in two backends, connected by
//! hand. Every bug in the family has the same shape: *a door existed and nobody connected it*. The
//! JSON one was the nineteenth, and it was found by probing for it rather than by anything failing.
//!
//! The rule that governs them all was prose, checked by nothing:
//!
//! > A hint may be consumed by a walk producing output a program **reads**, never by one **placing**
//! > a value for later retrieval.
//!
//! # What is enforced
//!
//! Every method of every collection surface is classified here through an **exhaustive match**, so
//! a new one does not compile until it says what it discloses — the same forcing function that
//! makes [`crate::ring1::ListMethod`] and its siblings safe to extend across two backends.
//!
//! A classification alone would only move the prose, so `noeta-conformance`'s census drives a `u64`
//! past bit 63 through every [`WidthDisclosure::Display`] and [`WidthDisclosure::Order`] door on
//! **both** engines and asserts the value reads back whole, and through every
//! [`WidthDisclosure::Identity`] door asserting the value stays *findable*. A door classified as
//! disclosing and not actually hinted fails there.

use crate::ring1::{ListMethod, MapMethod, SetMethod};

/// What a surface method lets a program learn about a fixed-width integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthDisclosure {
    /// It produces **output a program reads** — a rendering. It must render under the hint, or a
    /// `u64` past bit 63 shows its signed reinterpretation: different digits, not a coarser answer.
    Display,
    /// It produces an **order a program observes**. It must order under the hint, or the value
    /// sorts by the erased word and lands below everything smaller.
    Order,
    /// It places or probes a value by identity — a set's canonical buffer, a map key's slot.
    ///
    /// **These must never be hinted**, and that is the sharp end of the rule rather than an
    /// omission. A placement is built at one site and probed at another; hinting one side and not
    /// the other loses a member that is present, which is strictly worse than an order a reader
    /// finds surprising. Every key stays findable precisely because this stays a pure function of
    /// the value's own words.
    Identity,
    /// No width can escape through it: nothing is rendered, no observable order is produced, and
    /// any element it hands back carries its own static type to the next door.
    None,
}

impl WidthDisclosure {
    /// Whether this door owes the hint an answer — the two classifications the census drives a
    /// `u64` through and checks.
    pub fn must_consult_hint(self) -> bool {
        matches!(self, WidthDisclosure::Display | WidthDisclosure::Order)
    }
}

/// What a `List<T>` method discloses.
pub fn of_list_method(m: ListMethod) -> WidthDisclosure {
    match m {
        // Renders every element into one string.
        ListMethod::Join => WidthDisclosure::Display,
        // Hands the program an order over the elements themselves.
        ListMethod::Sorted => WidthDisclosure::Order,
        // A set's buffer is a placement, and `to_set` is what builds it.
        ListMethod::ToSet => WidthDisclosure::Identity,
        // `contains` compares for equality, which the erased word answers exactly — two `u64`s are
        // equal iff their words are, whatever the width says.
        ListMethod::Contains => WidthDisclosure::None,
        // Selections and reads: each hands back elements (or a list of them) whose static type
        // still names the width at whatever door consumes them next.
        ListMethod::Reverse
        | ListMethod::Slice
        | ListMethod::First
        | ListMethod::Last
        | ListMethod::Set => WidthDisclosure::None,
    }
}

/// What a `Set<T>` method discloses.
pub fn of_set_method(m: SetMethod) -> WidthDisclosure {
    match m {
        // Every one of these places members into a canonical buffer, or probes one. The buffer's
        // order is identity, never the type's.
        SetMethod::Add | SetMethod::Remove | SetMethod::Union | SetMethod::Intersection => {
            WidthDisclosure::Identity
        }
        SetMethod::Contains => WidthDisclosure::Identity,
    }
}

/// What a `Map<K, V>` method discloses.
pub fn of_map_method(m: MapMethod) -> WidthDisclosure {
    // `keys`/`values` hand the program a sequence, and the sequence's ORDER is observable — this is
    // the pair that has to agree with the rendered map, and did not until the ordering hint reached
    // both.
    match m {
        MapMethod::Keys | MapMethod::Values => WidthDisclosure::Order,
        // Placement and probing. A hint here would let a `dyn`-positioned lookup miss a key that is
        // present.
        MapMethod::Has | MapMethod::Get | MapMethod::GetOr | MapMethod::Set | MapMethod::Remove => {
            WidthDisclosure::Identity
        }
    }
}
