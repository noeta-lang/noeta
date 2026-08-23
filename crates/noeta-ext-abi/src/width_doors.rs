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
//!
//! # Two questions, two channels
//!
//! A door either **reads** a value or **computes** with one, and the two need different answers
//! from the static type:
//!
//! * *reading* — rendering, joining, ordering, placing — needs the value's **structure**: where
//!   under this type is there an unsigned 64-bit word? That is a
//!   [`crate::render_hint::RenderHint`], and its invariant is that a width under 64 needs no hint,
//!   because every such value fits in an i64 word and already renders, sorts and compares
//!   correctly.
//! * *computing* — folding, wrapping, overflow-reporting, mapping, clamping — needs the numeric
//!   element's **width**: `(signed, bits)`. A `u8` fold wraps at 8 bits, and no hint can say so.
//!
//! [`WidthDisclosure::Compute`] is the second question's classification, and the census walks it
//! with a **boxed** narrow-width list, comparing it against the packed twin of the same list type.
//! Boxed is the only representation where the question is open: a packed buffer carries its element
//! width in its schema.

use crate::ring1::{ListMethod, MapMethod, SetMethod, mask_to_width};
use serde::{Deserialize, Serialize};

/// The **numeric element width** a [`WidthDisclosure::Compute`] door computes at: how many bits the
/// answer wraps in, and how the erased words are read.
///
/// A fixed-width integer is erased to its i64 word, and a *boxed* list carries nothing else — so
/// `[200u8, 100u8].map(fn(x) => x).sum()` folds two ordinary words and has no way to know the total
/// wraps at 8 bits unless the door is told. The checker reads it off the receiver's static type at
/// the call span; both backends hand it to the one shared kernel. A packed list needs none of this:
/// its buffer's schema *is* the width, which is why the two representations agree only once the
/// boxed path has this.
///
/// [`ElemWidth::WORD`] — signed, 64 bits — is what a door with no recorded width computes at, and
/// is exactly the erased word. So `int`, `i64` and every non-numeric element behave as they always
/// did, and a door the checker could not type (a `dyn` receiver, a generic body naming a type
/// parameter) degrades to the untouched path rather than to a wrong number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElemWidth {
    /// How the erased words read: `true` for `int`/`i64`/`i8`/…, `false` for `u64`/`u8`/….
    pub signed: bool,
    /// The declared width in bits — 8, 16, 32 or 64.
    pub bits: u8,
}

impl ElemWidth {
    /// The erased 64-bit word itself: what an untyped, unrecorded or non-fixed-width element
    /// computes at, and the identity for [`ElemWidth::wrap`].
    pub const WORD: ElemWidth = ElemWidth {
        signed: true,
        bits: 64,
    };

    /// An unsigned 64-bit element — the one width whose *reading* differs from the word's, and the
    /// only thing a [`crate::render_hint::RenderHint`] could ever say about a numeric element.
    pub const U64: ElemWidth = ElemWidth {
        signed: false,
        bits: 64,
    };

    /// Reduce a fold's or a map's erased result back into the declared width, exactly as the
    /// language's own `+`/`-`/`*` do — the low `bits` bits are a ring homomorphism under those, so
    /// accumulating at 64 and wrapping once at the end equals accumulating at the width.
    pub fn wrap(self, value: i64) -> i64 {
        mask_to_width(value, self.signed, self.bits)
    }

    /// Add two erased words **in this width**, reporting overflow instead of wrapping — the one
    /// definition of `checked_sum`'s step, and the boxed twin of the packed kernel's native
    /// `checked_add`.
    ///
    /// Below 64 bits both operands are already in range, so their i64 sum is exact and the only
    /// question is whether it still fits — which is why a narrow fold overflows where a 64-bit one
    /// does not (`200u8 + 100u8`). At 64 bits the word has no room to spare and the two *readings*
    /// disagree about which sums overflow at all: `u64::MAX + 2` wraps past zero, while the same
    /// words read signed are `-1 + 2` and overflow nothing.
    pub fn checked_add(self, acc: i64, x: i64) -> Option<i64> {
        if self.bits >= 64 {
            return if self.signed {
                acc.checked_add(x)
            } else {
                (acc as u64).checked_add(x as u64).map(|v| v as i64)
            };
        }
        let sum = acc.wrapping_add(x);
        (self.wrap(sum) == sum).then_some(sum)
    }

    /// Whether an element of this width is **already non-negative**, which decides `abs`: on an
    /// unsigned type it is the identity, and reading the word signed instead folds `u64::MAX` to
    /// `1`. Signed widths take the ordinary wrapping negation, where `i8::MIN.abs()` stays
    /// `i8::MIN` exactly as the packed kernel's `i8::wrapping_abs` leaves it.
    pub fn already_non_negative(self) -> bool {
        !self.signed
    }
}

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
    /// Its answer is **computed from the elements as numbers** — a fold that wraps at the element
    /// width, an overflow report whose overflow point *is* that width, a map or a comparison
    /// against a bound. It needs the element's `(signed, bits)`, which is a different question from
    /// the one a [`crate::render_hint::RenderHint`] answers and needs a different channel.
    ///
    /// The hint says "this word is a `u64`", never "this word is 8 bits wide", and that is not an
    /// omission: its invariant is that a width under 64 needs no hint, which is *true* for reading
    /// a value (every `u8` fits in an i64 word and already renders, sorts and compares correctly)
    /// and *false* for computing with one (`[200u8, 100u8].sum()` is `44`, not `300`). A packed
    /// buffer carries its width in its schema and is exact without either channel; a **boxed**
    /// narrow list — a `map` result, an `iter().collect()` — carries only the erased words, so the
    /// element width has to arrive from the static type at the door.
    Compute,
}

impl WidthDisclosure {
    /// Whether this door owes the hint an answer — the two classifications the census drives a
    /// `u64` through and checks.
    pub fn must_consult_hint(self) -> bool {
        matches!(self, WidthDisclosure::Display | WidthDisclosure::Order)
    }

    /// Whether this door owes the **element width** an answer — the classification the census
    /// drives a boxed narrow-width list through, against its packed twin.
    pub fn must_consult_width(self) -> bool {
        matches!(self, WidthDisclosure::Compute)
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
