//! The **render hint**: how to display a value whose static type contains an unsigned 64-bit
//! integer.
//!
//! A fixed-width integer is erased to the underlying i64 word at runtime ([`crate::BuiltinTy::IntN`]
//! and `noeta_types::Type::IntN` both say so), so nothing the value carries distinguishes `u64` from
//! `i64`. For arithmetic and ordering that is handled by the width-carrying ops the checker records
//! in its width sites; for *rendering* the same information is needed, because a `u64` past bit 63
//! is a negative i64 word and would print as its signed reinterpretation.
//!
//! The signedness is therefore taken from the **static type at the display site** — `echo`, an
//! interpolation hole, and a display-based `~` operand — and travels to both backends as one of
//! these hints, built by the checker and applied by the shared render walk. Widths narrower than 64
//! bits need no hint: every value of one fits in an i64 and already prints correctly.
//!
//! The hint mirrors only the structure display itself walks: a scalar, a list/set's elements, a
//! map's keys and values, positional slots (a tuple's positions, an object's declared fields), and
//! an enum's per-variant payload. It is **sparse** — a branch with no unsigned integer under it is
//! absent, and a type with none anywhere produces no hint at all, so a program that never mentions
//! `u64` carries nothing and renders through the untouched path.

use serde::{Deserialize, Serialize};

/// How to render a value whose static type contains an unsigned 64-bit integer. See the module
/// docs; every variant is a *position* under which such an integer was found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderHint {
    /// The value **is** the erased integer word: render its bits as a `u64`.
    Unsigned,
    /// Every element of a `List`/`Set` carries this hint.
    Elements(Box<RenderHint>),
    /// A `Map`'s keys and/or values carry these hints. At least one side is `Some` — a `Map` with
    /// no unsigned integer on either side produces no hint at all.
    Entries {
        key: Option<Box<RenderHint>>,
        value: Option<Box<RenderHint>>,
    },
    /// Positional slots — a tuple's positions or an object's declared fields — sparse and ascending
    /// by index, holding only the slots that need one.
    Slots(Vec<(u32, RenderHint)>),
    /// An enum's payload slots, keyed by **variant name** (the discriminator the rendered value
    /// carries) and sparse in the same way: only variants with a hinted payload appear. `Option`'s
    /// `some` and `Result`'s `Ok`/`Err` use this form like any other enum.
    Variants(Vec<(String, Vec<(u32, RenderHint)>)>),
}

impl RenderHint {
    /// The hint for slot `index` of a [`RenderHint::Slots`], or `None` for any other shape.
    /// The lists are short (only the hinted slots), so a scan beats a map.
    pub fn slot(&self, index: u32) -> Option<&RenderHint> {
        match self {
            RenderHint::Slots(slots) => slots.iter().find(|(i, _)| *i == index).map(|(_, h)| h),
            _ => None,
        }
    }

    /// The payload slots of `variant` in a [`RenderHint::Variants`], or `None` for any other shape
    /// (or a variant that needs no hint).
    pub fn variant(&self, variant: &str) -> Option<&[(u32, RenderHint)]> {
        match self {
            RenderHint::Variants(variants) => variants
                .iter()
                .find(|(name, _)| name == variant)
                .map(|(_, slots)| slots.as_slice()),
            _ => None,
        }
    }

    /// Build a [`RenderHint::Slots`] from per-slot hints, dropping the slots that need none.
    /// Returns `None` when no slot does — the sparseness rule, in one place.
    pub fn slots(hints: impl IntoIterator<Item = Option<RenderHint>>) -> Option<RenderHint> {
        let slots: Vec<(u32, RenderHint)> = hints
            .into_iter()
            .enumerate()
            .filter_map(|(i, h)| h.map(|h| (i as u32, h)))
            .collect();
        (!slots.is_empty()).then_some(RenderHint::Slots(slots))
    }
}

/// Render an erased integer word as the unsigned value it stands for — the one place the
/// reinterpretation is written, shared by both backends so they cannot spell it differently.
pub fn unsigned_digits(word: i64) -> String {
    (word as u64).to_string()
}

/// A map key's rendered form under an optional hint. An integer key is the only kind a hint can
/// reach; every other key renders through the shared [`noeta_ext_abi::MapKey::render`] contract, so
/// a string key keeps its quoted form. Shared by both backends, whose map entries hold the same
/// [`noeta_ext_abi::MapKey`] even though their values differ.
pub fn map_key_display(key: &noeta_ext_abi::MapKey, hint: Option<&RenderHint>) -> String {
    match (hint, key) {
        (Some(RenderHint::Unsigned), noeta_ext_abi::MapKey::Int(word)) => unsigned_digits(*word),
        _ => key.render(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sparseness rule in one place: `slots` drops the positions that need no hint and folds a
    /// hint-free aggregate to `None`, so a type with no unsigned integer under it produces nothing.
    #[test]
    fn slots_are_sparse_and_an_empty_aggregate_is_no_hint() {
        assert_eq!(RenderHint::slots([None, None, None]), None);
        let hint = RenderHint::slots([None, Some(RenderHint::Unsigned), None]).unwrap();
        assert_eq!(hint, RenderHint::Slots(vec![(1, RenderHint::Unsigned)]));
        assert_eq!(hint.slot(1), Some(&RenderHint::Unsigned));
        assert_eq!(hint.slot(0), None);
        // A `Slots` answers no variant, and a `Variants` no slot — the lookups do not cross.
        assert_eq!(hint.variant("some"), None);
        let variants = RenderHint::Variants(vec![("some".into(), vec![(0, RenderHint::Unsigned)])]);
        assert_eq!(
            variants.variant("some"),
            Some(&[(0, RenderHint::Unsigned)][..])
        );
        assert_eq!(variants.variant("none"), None);
        assert_eq!(variants.slot(0), None);
    }

    /// The reinterpretation itself, at both boundaries: the largest word an `i64` also holds reads
    /// the same either way, and everything past bit 63 is where the two readings part.
    #[test]
    fn an_erased_word_reads_unsigned_past_bit_63() {
        assert_eq!(unsigned_digits(i64::MAX), "9223372036854775807");
        assert_eq!(unsigned_digits(i64::MIN), "9223372036854775808");
        assert_eq!(unsigned_digits(-1), "18446744073709551615");
        assert_eq!(unsigned_digits(255), "255");
    }

    /// Only an integer key is reinterpreted; every other key renders through `MapKey::render`,
    /// hint or no hint.
    #[test]
    fn only_an_integer_map_key_takes_the_hint() {
        let int_key = noeta_ext_abi::MapKey::Int(-1);
        let str_key = noeta_ext_abi::MapKey::Str("a".into());
        assert_eq!(
            map_key_display(&int_key, Some(&RenderHint::Unsigned)),
            "18446744073709551615"
        );
        assert_eq!(map_key_display(&int_key, None), int_key.render());
        assert_eq!(
            map_key_display(&str_key, Some(&RenderHint::Unsigned)),
            str_key.render()
        );
    }
}
