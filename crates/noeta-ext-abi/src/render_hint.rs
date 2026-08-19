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
//! One position is not a property of the site: a door inside a **generic** body reads a static type
//! that names a type parameter (`fn wrap<T>(v: T)`, or `self.v` inside `class Holder<T>`), and a
//! type parameter names no width. Erased generics give one compiled body to every instantiation, so
//! the answer lives outside the body. The checker records [`RenderHint::Param`] at that position,
//! the program's type-argument table carries each instantiation's own [`TypeArgHints`], and
//! [`RenderHint::resolve`] splices the two at the door — one hint, one splice, whichever way the
//! instantiation arrived. Two channels deliver it, because a body has one or the other:
//!
//! * a generic `fn`'s own parameters ride the **hidden type-argument slot** that already carries a
//!   forwarded decode recipe, filled by the call;
//! * a generic *type*'s parameters ride the **receiver's reflected tag**, which its construction
//!   site stamped. A method takes no hidden slot — its four name-keyed entry points (a `dyn`
//!   receiver, either handle form, `invoke`) bind positionally, so a leading slot would be read as a
//!   value argument — and it does not need one, because it has a receiver.
//!
//! A parameter neither channel can name resolves to nothing, so the value renders as the erased word
//! — the same answer a `dyn` gets, and for the same reason. That is also what a resolution to
//! *nothing* means at a display door's outermost position: the instantiation is a type that prints
//! through its own `to_string`, and the door has to behave exactly as an unhinted one does, `Display`
//! dispatch included, or a type would render one way at a concrete door and another through a
//! parameter instantiated to it.
//!
//! A hint the checker records for a value a native method **keeps** ([`PushHint`]) is spliced at the
//! binding call rather than at the walk that reads it: the later tick has no frame to read a slot
//! from, and the call that bound the value is the last moment an instantiation is knowable.
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
    /// **Whatever the enclosing generic body's type-argument slot `n` turns out to be** — the one
    /// position whose hint is not a property of the site.
    ///
    /// Inside `fn wrap<T>(v: T)` the door's static type is `T`, which names no width at all: the
    /// signedness lives at the *call*, which is where `T` becomes `u64` or `i64`. Erased generics
    /// give one compiled body to every instantiation, so the answer cannot be baked — it arrives on
    /// the same hidden slot that already delivers `json.try_parse::<T>`'s decode recipe, and
    /// [`RenderHint::resolve`] splices it in at the door. Inside `class Holder<T>`'s methods the
    /// same hint reads the same table through the other channel, the receiver's reflected tag; see
    /// the module docs.
    ///
    /// A `Param` is a **leaf under an ordinary structure**: `fn srt<T>(xs: List<T>)` records
    /// `Elements(Param(0))`, so the shape the walk takes is still static and only the width at the
    /// bottom is dynamic. `n` is a slot ordinal of the enclosing body, in one list: the forwarding
    /// slots `noeta_check::Sites::forwarding_fns` counts, then the receiver-read ones
    /// `noeta_check::Sites::self_render_fns` counts.
    ///
    /// Nothing consumes an unresolved `Param`: every walk here treats it as no hint (the value
    /// renders as the erased word), which is what makes an instantiation the call site could not
    /// name degrade to the untouched path rather than to a wrong number.
    Param(u32),
}

/// The hints of one interned type argument — what [`RenderHint::Param`] resolves to, at each answer
/// a door can ask for.
///
/// Three fields rather than one, for the two reasons the checker builds a hint per stated purpose.
/// An object's slots are numbered differently for a display or ordering walk (every declared field)
/// than for the deep marshal a JSON encoding runs on (`#[Transient]` fields dropped). And a value
/// whose type implements `Display` prints through its own `to_string`, so hinting the *outermost*
/// display position would replace the form the type chose — while ordering never consults
/// `to_string`, and a value nested in a collection or a field prints with `repr`, which does not
/// dispatch `Display` either. So the exemption applies to exactly one position, and
/// [`RenderHint::resolve`] tells the two apart by handing the lookup its `outermost` flag.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeArgHints {
    /// The hint for the **outermost** position of a display door: [`TypeArgHints::order`], or
    /// `None` where the instantiation is a declared type that prints through its own `to_string`.
    pub display: Option<RenderHint>,
    /// The hint for a walk that reads the value's own declared slots and never dispatches
    /// `Display` — every ordering door, and every non-outermost display position. `None` when the
    /// instantiation holds no unsigned 64-bit integer anywhere.
    pub order: Option<RenderHint>,
    /// The hint for the marshalled value a JSON encoding walks.
    pub json: Option<RenderHint>,
}

impl TypeArgHints {
    /// Whether this instantiation needs no hint at any door — the overwhelmingly common case, and
    /// the one a resolution can answer without walking anything.
    pub fn is_empty(&self) -> bool {
        self.display.is_none() && self.order.is_none() && self.json.is_none()
    }

    /// The hint a **display** door reads at `outermost`: the `Display` exemption applies to the
    /// outermost position only.
    pub fn at_display(&self, outermost: bool) -> Option<RenderHint> {
        if outermost {
            self.display.clone()
        } else {
            self.order.clone()
        }
    }
}

/// The slot value that names **no instantiation** — what lowering emits for a render slot the call
/// site could not resolve ([`crate::HiddenArg::Erased`]), and what [`resolve_hint`] reads as "no
/// hint". Negative on purpose: every real index into the type-argument table is non-negative, so no
/// table can grow into it.
pub const NO_TYPE_ARG: i64 = -1;

/// Which door is resolving a hint — the axis on which the three per-instantiation answers differ.
///
/// An exhaustive choice rather than a pair of flags: the display door is the only one that exempts
/// its outermost position (a declared type prints through its own `to_string`), and the JSON door is
/// the only one that reads the marshalled numbering, so a fourth door added later has to say which
/// of those it is.
///
/// Serializable and hashable because a door whose hint is spliced by a **preceding op** carries the
/// answer forward keyed by `(site, door)`: one site can hold a hint for more than one door, and the
/// two resolutions are different hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HintDoor {
    /// `echo`, an interpolation hole, a display-based `~` operand.
    Display,
    /// `.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, a rendered set or map, a `for` over
    /// one — every order a program observes.
    Order,
    /// The `json.stringify` argument, a derived `to_json()` or `inspect()` receiver.
    Json,
}

/// How a call inside a generic body fills a render slot naming an instantiation the body **built**
/// out of its own type parameters — `wrap([v])` inside `fn built<T>(v: T)`, which instantiates
/// `wrap` at `List<T>`.
///
/// Nothing in `built`'s parameter types names `List<T>`, so no slot of `built` carries it and no
/// caller of `built` could have interned it: the instantiation exists only because this body
/// constructed it. What the body *does* hold is `T`, on a slot its callers filled — so the answer is
/// arithmetic on that: `List<u64>` is `Elements` of whatever `T` turned out to be, and the shape
/// around the leaf is static even though the leaf is not.
///
/// The composition is that arithmetic, precomputed. Its [`Self::leaves`] name the enclosing body's
/// slots the built type reads, and [`Self::cases`] maps each combination of values those slots can
/// hold onto the table entry the combination composes to. The values are the same
/// [`TypeArgHints`]-table indices every other render slot carries, so the callee is handed an
/// ordinary slot value and resolves it through the ordinary splice — the composition is spent at
/// the call and nothing downstream knows it happened.
///
/// A combination with no case is [`NO_TYPE_ARG`], which is every combination composing to no hint at
/// all — a `List<int>` needs none — and every one a bounded enumeration did not reach. Degrading is
/// the rule for a render slot, so an instantiation this cannot name reads the erased word rather
/// than refusing the program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HintComposition {
    /// The enclosing body's render-slot ordinals the built instantiation reads, ascending — the
    /// slots lowering emits reads of, in the order [`HintCase::leaves`] is written.
    pub leaves: Box<[u32]>,
    /// One row per combination of leaf values the enumeration reached, in the order it found them.
    /// Lowering carries these to the op that performs the lookup; a combination absent here is
    /// [`NO_TYPE_ARG`].
    pub cases: Box<[HintCase]>,
}

/// One row of a [`HintComposition`]: what the built instantiation composes to when the body's leaf
/// slots hold exactly these values.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HintCase {
    /// The leaf slots' runtime values, positionally matching [`HintComposition::leaves`]. Each is a
    /// [`TypeArgHints`]-table index or [`NO_TYPE_ARG`].
    pub leaves: Box<[i64]>,
    /// The table index whose hints the composed instantiation carries.
    pub composed: i64,
}

/// The table index `cases` yields for the leaf slots' runtime values — the one lookup both backends
/// run, so a composed instantiation cannot resolve one way compiled and another interpreted.
/// [`NO_TYPE_ARG`] for a combination no case names, which is how a composition degrades.
///
/// Each leaf is **canonicalized against `table` first**: an entry with no hint of its own is the
/// same nothing [`NO_TYPE_ARG`] is, and the composed answer cannot tell them apart. That is what
/// keeps the case list to one row per combination of the program's *hint-carrying* instantiations —
/// a handful at most — instead of one per combination of every entry the table holds. `fn f<K, V>`
/// called at `K = string` still has a real table entry on its key slot, and it composes exactly as
/// an unnamed one does.
///
/// A linear scan over that handful, at a call that would otherwise render the erased word.
pub fn compose_type_arg(cases: &[HintCase], table: &[TypeArgHints], leaves: &[i64]) -> i64 {
    let canonical = |v: &i64| -> i64 {
        match usize::try_from(*v).ok().and_then(|i| table.get(i)) {
            Some(hints) if !hints.is_empty() => *v,
            _ => NO_TYPE_ARG,
        }
    };
    cases
        .iter()
        .find(|c| {
            c.leaves.len() == leaves.len()
                && c.leaves.iter().zip(leaves).all(|(k, v)| *k == canonical(v))
        })
        .map_or(NO_TYPE_ARG, |c| c.composed)
}

/// Splice a door's hint against the enclosing frame's **render slots** — the one resolution both
/// backends run, so a generic door cannot render one way compiled and another interpreted.
///
/// `slots` holds the frame's hidden type-argument slot values in slot order, and `table` is the
/// program's [`TypeArgHints`] projection. A slot that is out of range, or holds [`NO_TYPE_ARG`],
/// contributes no hint — so a call that could not name its instantiation degrades to reading the
/// erased word rather than to a wrong number.
///
/// Borrowed unchanged for every hint with no parameter under it, which is every door outside a
/// generic body.
pub fn resolve_hint<'a>(
    hint: &'a RenderHint,
    slots: &[i64],
    table: &[TypeArgHints],
    door: HintDoor,
) -> Option<std::borrow::Cow<'a, RenderHint>> {
    hint.resolve(&|n: u32, outermost: bool| {
        let entry = slots
            .get(n as usize)
            .filter(|v| **v >= 0)
            .and_then(|v| table.get(*v as usize))?;
        match door {
            HintDoor::Display => entry.at_display(outermost),
            HintDoor::Order => entry.order.clone(),
            HintDoor::Json => entry.json.clone(),
        }
    })
}

impl RenderHint {
    /// Whether a [`RenderHint::Param`] appears anywhere under this hint — the test that keeps
    /// [`RenderHint::resolve`] off the path of every hint built from a concrete type.
    pub fn has_param(&self) -> bool {
        match self {
            RenderHint::Unsigned => false,
            RenderHint::Param(_) => true,
            RenderHint::Elements(inner) => inner.has_param(),
            RenderHint::Entries { key, value } => {
                key.as_deref().is_some_and(RenderHint::has_param)
                    || value.as_deref().is_some_and(RenderHint::has_param)
            }
            RenderHint::Slots(slots) => slots.iter().any(|(_, h)| h.has_param()),
            RenderHint::Variants(variants) => variants
                .iter()
                .any(|(_, slots)| slots.iter().any(|(_, h)| h.has_param())),
        }
    }

    /// Every slot ordinal a [`RenderHint::Param`] under this hint names, ascending and deduplicated
    /// — the leaves a [`HintComposition`] over this hint has to read, and the order its case keys
    /// are written in.
    pub fn param_slots(&self) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect_param_slots(&mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn collect_param_slots(&self, out: &mut Vec<u32>) {
        match self {
            RenderHint::Unsigned => {}
            RenderHint::Param(n) => out.push(*n),
            RenderHint::Elements(inner) => inner.collect_param_slots(out),
            RenderHint::Entries { key, value } => {
                for h in [key, value].into_iter().flatten() {
                    h.collect_param_slots(out);
                }
            }
            RenderHint::Slots(slots) => {
                for (_, h) in slots {
                    h.collect_param_slots(out);
                }
            }
            RenderHint::Variants(variants) => {
                for (_, slots) in variants {
                    for (_, h) in slots {
                        h.collect_param_slots(out);
                    }
                }
            }
        }
    }

    /// Splice each [`RenderHint::Param`] with the hint `slot` gives for that ordinal, producing a
    /// hint with no parameter left in it — the one resolution walk, shared by both backends so a
    /// generic door cannot render one way compiled and another interpreted.
    ///
    /// Sparseness is re-established as it goes, exactly as the checker's builder establishes it:
    /// a branch whose only unsigned position was a parameter that turned out to need no hint
    /// (`wrap<T>` at `T = int`) collapses to `None` rather than surviving as an empty aggregate,
    /// so the door takes the untouched path instead of walking a hint that describes nothing.
    ///
    /// Borrowed unchanged when there is no parameter under it, which is every hint built from a
    /// concrete static type.
    ///
    /// `slot` is handed the ordinal **and** whether the parameter sits at the hint's outermost
    /// position — the one place a display door's `Display` exemption applies. A door with no such
    /// exemption ignores the flag.
    pub fn resolve<'a>(
        &'a self,
        slot: &dyn Fn(u32, bool) -> Option<RenderHint>,
    ) -> Option<std::borrow::Cow<'a, RenderHint>> {
        use std::borrow::Cow;
        if !self.has_param() {
            return Some(Cow::Borrowed(self));
        }
        self.substitute(slot, true).map(Cow::Owned)
    }

    /// [`RenderHint::resolve`]'s recursive half, always producing an owned hint. `outermost` is
    /// true only for the hint the door itself holds; every recursion is a nested position.
    fn substitute(
        &self,
        slot: &dyn Fn(u32, bool) -> Option<RenderHint>,
        outermost: bool,
    ) -> Option<RenderHint> {
        let nested = |h: &RenderHint| h.substitute(slot, false);
        match self {
            RenderHint::Unsigned => Some(RenderHint::Unsigned),
            RenderHint::Param(n) => slot(*n, outermost),
            RenderHint::Elements(inner) => Some(RenderHint::Elements(Box::new(nested(inner)?))),
            RenderHint::Entries { key, value } => {
                let key = key.as_deref().and_then(nested).map(Box::new);
                let value = value.as_deref().and_then(nested).map(Box::new);
                (key.is_some() || value.is_some()).then_some(RenderHint::Entries { key, value })
            }
            RenderHint::Slots(slots) => {
                let slots: Vec<(u32, RenderHint)> = slots
                    .iter()
                    .filter_map(|(i, h)| nested(h).map(|h| (*i, h)))
                    .collect();
                (!slots.is_empty()).then_some(RenderHint::Slots(slots))
            }
            RenderHint::Variants(variants) => {
                let variants: Vec<(String, Vec<(u32, RenderHint)>)> = variants
                    .iter()
                    .filter_map(|(name, slots)| {
                        let slots: Vec<(u32, RenderHint)> = slots
                            .iter()
                            .filter_map(|(i, h)| nested(h).map(|h| (*i, h)))
                            .collect();
                        (!slots.is_empty()).then(|| (name.clone(), slots))
                    })
                    .collect();
                (!variants.is_empty()).then_some(RenderHint::Variants(variants))
            }
        }
    }

    /// The hint for slot `index` of a [`RenderHint::Slots`], or `None` for any other shape.
    /// The lists are short (only the hinted slots), so a scan beats a map.
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
/// [`RenderHint::Unsigned`] reads its word unsigned, a slot holding a nested packed struct descends
/// into the nested hint, and every other slot keeps the derived [`Ord`] a packed field carries.
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
            (
                Some(nested @ RenderHint::Slots(_)),
                PackedKeyField::Struct(pn, p),
                PackedKeyField::Struct(qn, q),
            ) => pn.cmp(qn).then_with(|| packed_fields_order(p, q, nested)),
            _ => x.cmp(y),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// A map key's rendered form under an optional hint. Two kinds of key hold an erased integer word
/// and so are the two a hint can reach: a bare integer key, and a `@packed` struct key, whose
/// declared fields are words in flat storage. Every other key renders through the shared
/// [`crate::MapKey::render`] contract, so a string key keeps its quoted form. Shared by both
/// backends, whose map entries hold the same [`crate::MapKey`] even though their values differ.
///
/// The arms mirror [`map_key_order`]'s exactly — same key kinds, same hint shapes, same slot
/// numbering — because a rendered map and its key order are two views of the same entries and a
/// program that reads both must see one answer. Rendering is *all* this changes: the key's own
/// [`Ord`], which places it for lookup, is never consulted here and never takes a hint.
pub fn map_key_display(key: &crate::MapKey, hint: Option<&RenderHint>) -> String {
    match (hint, key) {
        (Some(RenderHint::Unsigned), crate::MapKey::Int(word)) => unsigned_digits(*word),
        (Some(hint @ RenderHint::Slots(_)), crate::MapKey::Packed(p)) => {
            crate::map_key::packed_names::display_hinted(&p.type_name, &p.fields, Some(hint))
        }
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
/// A [`RenderHint`] a native type **kept** — obtained from
/// [`NativeCtx::push_hint`](crate::ctx::NativeCtx::push_hint) at the site that bound a value, and
/// stored beside that value to serialize it on a later tick.
///
/// It is **already resolved**: a hint built inside a generic body names a type parameter, and the
/// splice happens at the binding call, whose frame is the last thing that knows the instantiation.
/// So what is stored here describes a width, never a parameter, and the later tick has nothing left
/// to look up.
///
/// The rule a hint lives under is that it may be consumed by a walk producing **output a program
/// reads**, and never by one **placing a value for later retrieval** — a hint that reached
/// [`map_key_order`], a set's canonical buffer, or any other ordering could make a lookup miss a
/// key that is present. For a hint consumed at its call site that rule is kept by the lowering,
/// which hands each site only the walk it asked for. A kept hint has no call site left, so the
/// same rule has to be carried by the type.
///
/// That is the whole reason this wrapper exists rather than storing the [`RenderHint`] directly.
/// Outside this crate a `PushHint` can be constructed, cloned and stored, and handed to
/// [`json_stringify_pushed`] — and nothing else. There is no accessor, no `Deref` and no `From`
/// back to the hint it wraps, so no amount of determination gets one into an ordering walk. The
/// alternative was a comment asking extensions not to, which is not a mechanism.
///
/// A kept hint serializes, and that is all it does:
///
/// ```
/// use noeta_ext_abi::{NativeValue, PushHint, RenderHint, Scalar, json_stringify_pushed};
/// let kept = PushHint::new(RenderHint::Unsigned);
/// let word = NativeValue::Scalar(Scalar::Int(-1));
/// assert_eq!(json_stringify_pushed(&word, Some(&kept)), "18446744073709551615");
/// assert_eq!(json_stringify_pushed(&word, None), "-1");
/// ```
///
/// It cannot be unwrapped, which is what keeps it out of every ordering walk:
///
/// ```compile_fail,E0599
/// use noeta_ext_abi::{PushHint, RenderHint};
/// let kept = PushHint::new(RenderHint::Unsigned);
/// // `render_hint` is crate-private: there is no route from a kept hint back to a `RenderHint`,
/// // so `map_key_order` and the other placement walks are unreachable from here.
/// let _escaped: &RenderHint = kept.render_hint();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushHint(RenderHint);

impl PushHint {
    /// Wrap a hint the caller already holds. Construction is deliberately public and harmless:
    /// the hazard is *unwrapping* one back into a `RenderHint`, so a caller that already has a
    /// hint gains nothing by wrapping it. Both backends build one here when a call site declares
    /// a kept argument.
    pub fn new(hint: RenderHint) -> Self {
        Self(hint)
    }

    /// The wrapped hint, readable only inside this crate — see the type docs.
    pub(crate) fn render_hint(&self) -> &RenderHint {
        &self.0
    }
}

/// [`json_stringify`] for a value serialized under a **kept** hint, and the only walk a
/// [`PushHint`] can reach.
///
/// Byte for byte identical to `json_stringify` given the same hint — it is the one door, not a
/// second implementation.
pub fn json_stringify_pushed(value: &crate::NativeValue, hint: Option<&PushHint>) -> String {
    json_stringify(value, hint.map(PushHint::render_hint))
}

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

    /// A hint built from a concrete type has no parameter under it and resolves to **itself**,
    /// borrowed — the fast path every non-generic door takes.
    #[test]
    fn a_concrete_hint_resolves_to_itself_without_copying() {
        let hint = RenderHint::Elements(Box::new(RenderHint::Unsigned));
        assert!(!hint.has_param());
        let none = |_: u32, _: bool| None;
        let resolved = hint.resolve(&none).unwrap();
        assert!(matches!(resolved, std::borrow::Cow::Borrowed(_)));
        assert_eq!(*resolved, hint);
    }

    /// The splice itself, at every position a parameter can sit under: the structure is the site's
    /// and the leaf is the call's.
    #[test]
    fn a_parameter_takes_the_instantiations_hint() {
        let unsigned = |n: u32, _: bool| (n == 0).then_some(RenderHint::Unsigned);
        // The bare parameter — `fn wrap<T>(v: T)`.
        assert_eq!(
            RenderHint::Param(0).resolve(&unsigned).as_deref(),
            Some(&RenderHint::Unsigned)
        );
        // Under a list — `fn srt<T>(xs: List<T>)`.
        assert_eq!(
            RenderHint::Elements(Box::new(RenderHint::Param(0)))
                .resolve(&unsigned)
                .as_deref(),
            Some(&RenderHint::Elements(Box::new(RenderHint::Unsigned)))
        );
        // A slot and a variant payload, each beside a position that was already concrete.
        let slots = RenderHint::Slots(vec![(0, RenderHint::Param(0)), (2, RenderHint::Unsigned)]);
        assert_eq!(
            slots.resolve(&unsigned).as_deref(),
            Some(&RenderHint::Slots(vec![
                (0, RenderHint::Unsigned),
                (2, RenderHint::Unsigned)
            ]))
        );
        let variants = RenderHint::Variants(vec![("some".into(), vec![(0, RenderHint::Param(0))])]);
        assert_eq!(
            variants.resolve(&unsigned).as_deref(),
            Some(&RenderHint::Variants(vec![(
                "some".into(),
                vec![(0, RenderHint::Unsigned)]
            )]))
        );
        // An instantiation that is itself a composite splices whole (`T = List<u64>`).
        let nested = |_: u32, _: bool| Some(RenderHint::Elements(Box::new(RenderHint::Unsigned)));
        assert_eq!(
            RenderHint::Param(0).resolve(&nested).as_deref(),
            Some(&RenderHint::Elements(Box::new(RenderHint::Unsigned)))
        );
    }

    /// An instantiation with no unsigned integer in it (`wrap<T>` at `T = int`) collapses every
    /// branch that held only that parameter — the sparseness rule, re-established by the splice.
    /// A branch with a concrete position beside it survives, holding only that position.
    #[test]
    fn a_hintless_instantiation_collapses_its_branch() {
        let none = |_: u32, _: bool| None;
        assert_eq!(RenderHint::Param(0).resolve(&none), None);
        assert_eq!(
            RenderHint::Elements(Box::new(RenderHint::Param(0))).resolve(&none),
            None
        );
        assert_eq!(
            RenderHint::Entries {
                key: Some(Box::new(RenderHint::Param(0))),
                value: None,
            }
            .resolve(&none),
            None
        );
        assert_eq!(
            RenderHint::Slots(vec![(0, RenderHint::Param(0))]).resolve(&none),
            None
        );
        assert_eq!(
            RenderHint::Variants(vec![("some".into(), vec![(0, RenderHint::Param(0))])])
                .resolve(&none),
            None
        );
        assert_eq!(
            RenderHint::Slots(vec![(0, RenderHint::Param(0)), (1, RenderHint::Unsigned)])
                .resolve(&none)
                .as_deref(),
            Some(&RenderHint::Slots(vec![(1, RenderHint::Unsigned)]))
        );
    }

    /// The lookup is told which position it is answering for, so a display door can exempt the one
    /// place a declared type's own `to_string` decides the form — and only that place.
    #[test]
    fn only_the_outermost_position_is_told_it_is_outermost() {
        let hints = TypeArgHints {
            display: None,
            order: Some(RenderHint::Unsigned),
            json: Some(RenderHint::Unsigned),
        };
        let at = |_: u32, outermost: bool| hints.at_display(outermost);
        // The whole value is the parameter: the exemption applies and nothing is hinted.
        assert_eq!(RenderHint::Param(0).resolve(&at), None);
        // The parameter sits under a list, which renders its elements structurally.
        assert_eq!(
            RenderHint::Elements(Box::new(RenderHint::Param(0)))
                .resolve(&at)
                .as_deref(),
            Some(&RenderHint::Elements(Box::new(RenderHint::Unsigned)))
        );
    }

    /// An **unresolved** parameter reaching a walk is no hint at all: the erased word, which is the
    /// answer a `dyn` gets. Nothing renders a guess.
    #[test]
    fn an_unresolved_parameter_renders_the_erased_word() {
        assert_eq!(json_stringify(&int(-1), Some(&RenderHint::Param(0))), "-1");
        assert_eq!(
            map_key_display(&crate::MapKey::Int(-1), Some(&RenderHint::Param(0))),
            "-1"
        );
        assert_eq!(
            map_key_order(
                &crate::MapKey::Int(-1),
                &crate::MapKey::Int(1),
                Some(&RenderHint::Param(0))
            ),
            std::cmp::Ordering::Less
        );
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
        // A NESTED packed struct descends into its own slot hint rather than falling back to the
        // derived `Ord` — the position a flat slot walk misses.
        let nested = |at: i64| {
            MapKey::packed(
                "Outer",
                vec![PackedKeyField::Struct(
                    "Inner".into(),
                    vec![PackedKeyField::Int(at)].into_boxed_slice(),
                )],
            )
        };
        let outer = RenderHint::Slots(vec![(
            0,
            RenderHint::Slots(vec![(0, RenderHint::Unsigned)]),
        )]);
        assert_eq!(
            map_key_order(&nested(-1), &nested(1), Some(&outer)),
            Ordering::Greater
        );
        assert_eq!(map_key_order(&nested(-1), &nested(1), None), Ordering::Less);
    }

    /// An integer key and a `@packed` struct key are the two kinds holding an erased word, so they
    /// are the two a hint reaches; every other key renders through `MapKey::render`, hint or no
    /// hint. The packed arm reads the same slot numbering `map_key_order`'s does, at every depth.
    #[test]
    fn an_integer_and_a_packed_map_key_take_the_hint() {
        use crate::{MapKey, PackedKeyField};
        let int_key = MapKey::Int(-1);
        let str_key = MapKey::Str("a".into());
        assert_eq!(
            map_key_display(&int_key, Some(&RenderHint::Unsigned)),
            "18446744073709551615"
        );
        assert_eq!(map_key_display(&int_key, None), int_key.render());
        assert_eq!(
            map_key_display(&str_key, Some(&RenderHint::Unsigned)),
            str_key.render()
        );
        // A packed key: slot 0 hinted unsigned, slot 1 left signed — and unhinted it is `render()`.
        let packed = MapKey::packed(
            "HintedTick",
            vec![PackedKeyField::Int(-1), PackedKeyField::Int(-1)],
        );
        let slots = RenderHint::Slots(vec![(0, RenderHint::Unsigned)]);
        assert_eq!(
            map_key_display(&packed, Some(&slots)),
            "HintedTick {18446744073709551615, -1}"
        );
        assert_eq!(map_key_display(&packed, None), packed.render());
        // A nested packed struct takes the nested hint at its slot.
        let outer = MapKey::packed(
            "HintedOuter",
            vec![PackedKeyField::Struct(
                "HintedInner".into(),
                vec![PackedKeyField::Int(-1)].into_boxed_slice(),
            )],
        );
        let outer_hint = RenderHint::Slots(vec![(
            0,
            RenderHint::Slots(vec![(0, RenderHint::Unsigned)]),
        )]);
        assert_eq!(
            map_key_display(&outer, Some(&outer_hint)),
            "HintedOuter {HintedInner {18446744073709551615}}"
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
