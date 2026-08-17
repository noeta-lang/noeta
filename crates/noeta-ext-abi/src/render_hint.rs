//! The **render hint**: the structural map of the unsigned 64-bit integers under a static type —
//! where to read an erased word unsigned when writing a value out, and where to read one unsigned
//! when ordering one.
//!
//! A fixed-width integer is erased to the underlying i64 word at runtime (`noeta_ast::BuiltinTy::IntN`
//! and `noeta_types::Type::IntN` both say so), so nothing the value carries distinguishes `u64` from
//! `i64`. For arithmetic that is handled by the width-carrying ops the checker records in its width
//! sites; for writing a value out, and for the ordering the collections compute, the same
//! information is needed, because a `u64` past bit 63 is a negative i64 word and would otherwise
//! appear as — and sort as — its signed reinterpretation.
//!
//! The signedness is therefore taken from the **static type at the door** and travels to both
//! backends as one of these hints, built by the checker and applied by a shared walk. Widths
//! narrower than 64 bits need no hint: every value of one fits in an i64 and is already correct.
//!
//! There are three kinds of door, and they take the same hint:
//!
//! * **Display** — `echo`, an interpolation hole, a display-based `~` operand — applied by each
//!   backend to its own value model.
//! * **JSON** — the `json.stringify` argument, a derived `to_json()` or `inspect()` receiver —
//!   applied by [`json_stringify`] here, over the one neutral `NativeValue` tree both backends
//!   marshal into. This half is a *data* encoding rather than a rendering: the number is not merely
//!   displayed wrong, it is written wrong to an API response or a persisted record.
//! * **Ordering** — `.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, a rendered set or map,
//!   and a `for` over a set or map — applied by each backend's comparator against its own value
//!   model, through [`unsigned_order`] and [`map_key_order`].
//!
//! The hint mirrors only the structure those walks take: a scalar, a list/set's elements, a map's
//! keys and values, positional slots (a tuple's positions, an object's fields), and an enum's
//! per-variant payload. It is **sparse** — a branch with no unsigned integer under it is absent, and
//! a type with none anywhere produces no hint at all, so a program that never mentions `u64` carries
//! nothing and goes through the untouched path.
//!
//! The walks do not all number an object's slots the same way, which is why the checker builds each
//! hint for a stated purpose (`HintPurpose`): a display and an ordering walk both see the object's
//! own declared fields, while the deep marshal a JSON encoding runs on drops the `#[Transient]`
//! ones, so its slot numbers count only the fields that survive.
//!
//! **A hint never reaches an identity order.** A set's canonical buffer and a
//! [`crate::MapKey`]'s [`Ord`] place elements for binary search, `BTreeMap` lookup, hashing
//! and the deterministic destructor sort — they are built at one site and probed at another, so they
//! must stay a pure function of the erased word, or a value laundered through `dyn` would probe with
//! a different order than it was stored under and miss a member that is present. Those orders are
//! never observed directly: both backends produce the order a program *sees* at the door, under the
//! hint. What a declared type carries in its own runtime description — a `u64` **field**, via
//! `noeta_object::Shape::unsigned_slots` — is a different mechanism and does reach the identity
//! order, precisely because it is a property of the data rather than of the call.

use serde::{Deserialize, Serialize};

/// How to write out a value whose static type contains an unsigned 64-bit integer. See the module
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

    /// The element hint of a [`RenderHint::Elements`] — a list's or set's per-element positions —
    /// or `None` for any other shape.
    pub fn elements(&self) -> Option<&RenderHint> {
        match self {
            RenderHint::Elements(inner) => Some(inner),
            _ => None,
        }
    }

    /// The key hint of a [`RenderHint::Entries`] — what a map's key order reads — or `None` for any
    /// other shape (or a map whose keys need none).
    pub fn entry_key(&self) -> Option<&RenderHint> {
        match self {
            RenderHint::Entries { key, .. } => key.as_deref(),
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

/// Order two erased integer words as the unsigned values they stand for — the ordering twin of
/// [`unsigned_digits`], and likewise the only place the reinterpretation is spelled.
pub fn unsigned_order(a: i64, b: i64) -> std::cmp::Ordering {
    (a as u64).cmp(&(b as u64))
}

/// The **observed** order of two map keys under an optional key hint: an integer key reads unsigned
/// where the hint says so, a packed key's slots read unsigned per the hint's slot list, and every
/// other pairing falls back to the key's own [`Ord`] — the identity order.
///
/// Used only where a program *sees* the order (a rendered map, `keys()`, `values()`, iteration).
/// Storage and lookup keep using [`Ord`]: see the module docs for why the two must not be the same
/// function.
pub fn map_key_order(
    a: &crate::MapKey,
    b: &crate::MapKey,
    hint: Option<&RenderHint>,
) -> std::cmp::Ordering {
    use crate::MapKey;
    match (hint, a, b) {
        (Some(RenderHint::Unsigned), MapKey::Int(x), MapKey::Int(y)) => unsigned_order(*x, *y),
        (Some(hint @ RenderHint::Slots(_)), MapKey::Packed(x), MapKey::Packed(y)) => x
            .type_name
            .cmp(&y.type_name)
            .then_with(|| packed_fields_order(&x.fields, &y.fields, hint)),
        _ => a.cmp(b),
    }
}

/// The slot-wise observed order of two packed keys of the same type: a slot the hint marks
/// [`RenderHint::Unsigned`] reads its word unsigned, every other slot keeps the derived
/// [`Ord`] a packed field carries.
fn packed_fields_order(
    a: &[crate::PackedKeyField],
    b: &[crate::PackedKeyField],
    hint: &RenderHint,
) -> std::cmp::Ordering {
    use crate::PackedKeyField;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let ord = match (hint.slot(i as u32), x, y) {
            (Some(RenderHint::Unsigned), PackedKeyField::Int(p), PackedKeyField::Int(q)) => {
                unsigned_order(*p, *q)
            }
            _ => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// A map key's rendered form under an optional hint. An integer key is the only kind a hint can
/// reach; every other key renders through the shared [`crate::MapKey::render`] contract, so
/// a string key keeps its quoted form. Shared by both backends, whose map entries hold the same
/// [`crate::MapKey`] even though their values differ.
pub fn map_key_display(key: &crate::MapKey, hint: Option<&RenderHint>) -> String {
    match (hint, key) {
        (Some(RenderHint::Unsigned), crate::MapKey::Int(word)) => unsigned_digits(*word),
        _ => key.render(),
    }
}

/// Serialize a deeply-marshalled [`crate::NativeValue`] to JSON **under a hint** — the JSON
/// twin of the display walk, and the one serializer both backends reach for a hinted door.
///
/// Without a hint this *is* [`crate::json_text::stringify`], byte for byte: the walk
/// delegates the moment a branch has none, so only the hinted spine is re-walked here and every
/// unhinted subtree is produced by the single shared engine. With one, an erased word at a
/// [`RenderHint::Unsigned`] position is written as the unsigned value it stands for, at any depth —
/// a list element, a map key or value, a tuple or object slot, an enum payload.
///
/// The hint describes the *static type*; the tree describes the marshalled value, and the two
/// differ in exactly one place: an `Option` marshals **through** its payload (`some(x)` is `x`,
/// `none` is null), while its hint is the ordinary [`RenderHint::Variants`] every enum gets. That
/// is why a `Variants` hint over a non-variant value takes the `some` payload's hint — the last
/// arm below.
pub fn json_stringify(value: &crate::NativeValue, hint: Option<&RenderHint>) -> String {
    use crate::json_text::{json_string, stringify};
    use crate::{NativeValue, Scalar};
    let Some(hint) = hint else {
        return stringify(value);
    };
    match (hint, value) {
        // The reinterpretation itself: the erased word read as the `u64` the type says it is.
        (RenderHint::Unsigned, NativeValue::Scalar(Scalar::Int(word))) => unsigned_digits(*word),
        // A list or set — every element carries the same hint.
        (RenderHint::Elements(inner), NativeValue::List(items)) => {
            json_array(items.iter().map(|item| json_stringify(item, Some(inner))))
        }
        // A map: keys and values take their own hints. A JSON object key is text by definition, so
        // the marshal has already rendered it (`MapKey::as_native_str`); a hinted key is therefore
        // read back as the i64 word that text was rendered from — the exact inverse of the one
        // `to_string` that produced it — and re-rendered unsigned.
        (RenderHint::Entries { key, value: val }, NativeValue::Map(entries)) => {
            json_object(entries.iter().map(|(k, v)| {
                (
                    json_map_key(k, key.as_deref()),
                    json_stringify(v, val.as_deref()),
                )
            }))
        }
        // Positional slots against a tuple, which marshals as a JSON array.
        (RenderHint::Slots(_), NativeValue::List(items)) => json_array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| json_stringify(item, hint.slot(i as u32))),
        ),
        // Positional slots against an object — a declared struct/class marshals as a JSON object in
        // serialized field order, which is the order the checker numbered the slots in.
        (RenderHint::Slots(_), NativeValue::Map(entries)) => json_object(
            entries
                .iter()
                .enumerate()
                .map(|(i, (k, v))| (json_string(k), json_stringify(v, hint.slot(i as u32)))),
        ),
        (RenderHint::Slots(_), NativeValue::Instance { fields, .. }) => json_object(
            fields
                .iter()
                .enumerate()
                .map(|(i, (k, v))| (json_string(k), json_stringify(v, hint.slot(i as u32)))),
        ),
        // An enum value: the payload-free form is its case name, the payload-carrying one the
        // `{"Variant":[fields]}` shape, each field under its own slot hint.
        (
            RenderHint::Variants(_),
            NativeValue::Variant {
                variant, fields, ..
            },
        ) => {
            if fields.is_empty() {
                return json_string(variant);
            }
            let slots = hint.variant(variant).unwrap_or(&[]);
            let parts = json_array(fields.iter().enumerate().map(|(i, field)| {
                let slot = slots.iter().find(|(j, _)| *j == i as u32).map(|(_, h)| h);
                json_stringify(field, slot)
            }));
            format!("{{{}:{}}}", json_string(variant), parts)
        }
        // An `Option` reaches here: it marshalled through its payload, so the value is the payload
        // itself (or unit for `none`) while the hint is still the enum's. Apply the `some` payload's
        // hint to it; `none` is a unit, which the delegation below writes as `null`.
        (RenderHint::Variants(_), _) => {
            match hint.variant("some").and_then(|slots| {
                slots
                    .iter()
                    .find(|(i, _)| *i == 0)
                    .map(|(_, payload)| payload)
            }) {
                Some(payload) => json_stringify(value, Some(payload)),
                None => stringify(value),
            }
        }
        // The hint describes a position this value does not occupy (a `dyn` that came back a
        // different shape, a hint-free branch): the shared engine answers, unchanged.
        _ => stringify(value),
    }
}

/// One JSON array from already-serialized elements — the array syntax written once for
/// [`json_stringify`]'s three array-shaped arms.
fn json_array(parts: impl IntoIterator<Item = String>) -> String {
    let parts: Vec<String> = parts.into_iter().collect();
    format!("[{}]", parts.join(","))
}

/// One JSON object from already-serialized `(quoted key, value)` pairs — the object syntax written
/// once for [`json_stringify`]'s three object-shaped arms.
fn json_object(entries: impl IntoIterator<Item = (String, String)>) -> String {
    let parts: Vec<String> = entries
        .into_iter()
        .map(|(key, value)| format!("{key}:{value}"))
        .collect();
    format!("{{{}}}", parts.join(","))
}

/// A marshalled map key as its **quoted JSON key**, under the key's own hint. See the
/// [`RenderHint::Entries`] arm of [`json_stringify`]: the key arrives as the text
/// [`crate::MapKey::as_native_str`] produced, so an [`RenderHint::Unsigned`] key is read
/// back as that i64 word and re-rendered. A key whose text is not one is left alone.
fn json_map_key(key: &str, hint: Option<&RenderHint>) -> String {
    match hint {
        Some(RenderHint::Unsigned) => match key.parse::<i64>() {
            Ok(word) => crate::json_text::json_string(&unsigned_digits(word)),
            Err(_) => crate::json_text::json_string(key),
        },
        _ => crate::json_text::json_string(key),
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

    /// The ordering reinterpretation at both boundaries, and its asymmetry with the signed one:
    /// past bit 63 the two readings order oppositely, which is the whole ordering defect.
    #[test]
    fn an_erased_word_orders_unsigned_past_bit_63() {
        use std::cmp::Ordering;
        assert_eq!(unsigned_order(1, -1), Ordering::Less);
        assert_eq!(1i64.cmp(&-1), Ordering::Greater);
        assert_eq!(unsigned_order(i64::MAX, i64::MIN), Ordering::Less);
        assert_eq!(unsigned_order(255, 255), Ordering::Equal);
    }

    /// A map key's OBSERVED order: an integer key reads unsigned under the hint and by its own
    /// `Ord` without one, a packed key reads its hinted slots unsigned, and a key kind the hint
    /// does not describe keeps the identity order either way.
    #[test]
    fn a_map_key_orders_unsigned_only_where_the_hint_says_so() {
        use crate::{MapKey, PackedKeyField};
        use std::cmp::Ordering;
        let (max, one) = (MapKey::Int(-1), MapKey::Int(1));
        assert_eq!(
            map_key_order(&max, &one, Some(&RenderHint::Unsigned)),
            Ordering::Greater
        );
        assert_eq!(map_key_order(&max, &one, None), Ordering::Less);
        // A string key is untouched by an (impossible, defensive) integer hint.
        let (a, b) = (MapKey::from("a"), MapKey::from("b"));
        assert_eq!(
            map_key_order(&a, &b, Some(&RenderHint::Unsigned)),
            Ordering::Less
        );
        // A packed key: slot 0 hinted unsigned, slot 1 left signed.
        let key = |at: i64, lane: i64| {
            MapKey::packed(
                "Tick",
                vec![PackedKeyField::Int(at), PackedKeyField::Int(lane)],
            )
        };
        let slots = RenderHint::Slots(vec![(0, RenderHint::Unsigned)]);
        assert_eq!(
            map_key_order(&key(-1, 0), &key(1, 0), Some(&slots)),
            Ordering::Greater
        );
        assert_eq!(map_key_order(&key(-1, 0), &key(1, 0), None), Ordering::Less);
        // The unhinted slot still orders signed, and only after the hinted one ties.
        assert_eq!(
            map_key_order(&key(1, -1), &key(1, 0), Some(&slots)),
            Ordering::Less
        );
    }

    /// Only an integer key is reinterpreted; every other key renders through `MapKey::render`,
    /// hint or no hint.
    #[test]
    fn only_an_integer_map_key_takes_the_hint() {
        let int_key = crate::MapKey::Int(-1);
        let str_key = crate::MapKey::Str("a".into());
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

    // --- the hinted JSON walk -------------------------------------------------------------------

    use crate::{NativeValue, Scalar};

    fn int(word: i64) -> NativeValue {
        NativeValue::Scalar(Scalar::Int(word))
    }

    /// No hint is the shared engine, byte for byte — the delegation that keeps one serializer.
    #[test]
    fn an_unhinted_walk_is_the_shared_serializer() {
        let value = NativeValue::List(vec![int(-1), NativeValue::Str("a".into())]);
        assert_eq!(
            json_stringify(&value, None),
            crate::json_text::stringify(&value)
        );
        // And so is a hint that describes a position this value does not occupy.
        assert_eq!(
            json_stringify(&value, Some(&RenderHint::Unsigned)),
            crate::json_text::stringify(&value)
        );
    }

    /// The three boundaries, bare: the largest word an i64 also holds reads the same either way,
    /// and everything past bit 63 is where the two readings part.
    #[test]
    fn a_hinted_scalar_writes_its_unsigned_digits() {
        for (word, text) in [
            (i64::MAX, "9223372036854775807"),
            (i64::MIN, "9223372036854775808"),
            (-1, "18446744073709551615"),
        ] {
            assert_eq!(
                json_stringify(&int(word), Some(&RenderHint::Unsigned)),
                text
            );
        }
    }

    /// Every nesting position the hint models, against the tree the marshal actually produces.
    #[test]
    fn a_hint_reaches_every_nested_position() {
        // A list's elements.
        let list = NativeValue::List(vec![int(-1), int(1)]);
        assert_eq!(
            json_stringify(
                &list,
                Some(&RenderHint::Elements(Box::new(RenderHint::Unsigned)))
            ),
            "[18446744073709551615,1]"
        );
        // A map's values, and its keys — which arrive as the text the marshal rendered.
        let map = NativeValue::Map(vec![("-1".into(), int(-1))]);
        assert_eq!(
            json_stringify(
                &map,
                Some(&RenderHint::Entries {
                    key: Some(Box::new(RenderHint::Unsigned)),
                    value: Some(Box::new(RenderHint::Unsigned)),
                })
            ),
            "{\"18446744073709551615\":18446744073709551615}"
        );
        // A string-keyed map keeps its key whatever the value hint says.
        assert_eq!(
            json_stringify(
                &NativeValue::Map(vec![("v".into(), int(-1))]),
                Some(&RenderHint::Entries {
                    key: None,
                    value: Some(Box::new(RenderHint::Unsigned)),
                })
            ),
            "{\"v\":18446744073709551615}"
        );
        // A tuple's slots (a JSON array) and an object's slots (a JSON object), both positional.
        let slots = RenderHint::Slots(vec![(0, RenderHint::Unsigned)]);
        assert_eq!(
            json_stringify(
                &NativeValue::List(vec![int(-1), NativeValue::Str("t".into())]),
                Some(&slots)
            ),
            "[18446744073709551615,\"t\"]"
        );
        assert_eq!(
            json_stringify(
                &NativeValue::Map(vec![
                    ("reading".into(), int(-1)),
                    ("label".into(), NativeValue::Str("m".into())),
                ]),
                Some(&slots)
            ),
            "{\"reading\":18446744073709551615,\"label\":\"m\"}"
        );
    }

    /// An enum's payload takes its variant's slot hint; a payload-free case is still its name.
    #[test]
    fn an_enum_payload_takes_its_variants_slot() {
        let hint = RenderHint::Variants(vec![("Raw".into(), vec![(0, RenderHint::Unsigned)])]);
        let raw = NativeValue::Variant {
            enum_name: "Reading".into(),
            variant: "Raw".into(),
            variant_index: 0,
            fields: vec![int(-1)],
        };
        assert_eq!(
            json_stringify(&raw, Some(&hint)),
            "{\"Raw\":[18446744073709551615]}"
        );
        let missing = NativeValue::Variant {
            enum_name: "Reading".into(),
            variant: "Missing".into(),
            variant_index: 1,
            fields: Vec::new(),
        };
        assert_eq!(json_stringify(&missing, Some(&hint)), "\"Missing\"");
    }

    /// The one place hint and tree disagree: an `Option` marshals through its payload, so the
    /// `some` slot's hint applies to the bare value and `none` is still `null`.
    #[test]
    fn an_option_takes_its_some_payloads_hint_through_the_flattening() {
        let hint = RenderHint::Variants(vec![("some".into(), vec![(0, RenderHint::Unsigned)])]);
        assert_eq!(
            json_stringify(&int(-1), Some(&hint)),
            "18446744073709551615"
        );
        assert_eq!(json_stringify(&NativeValue::Unit, Some(&hint)), "null");
    }
}
