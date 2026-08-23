//! The width-door census for the surfaces declared in this crate — the lazy iterator and the two
//! list reductions. See [`noeta_ext_abi::width_doors`] for the rule, the bug class, and the
//! classification of the ring-1 collection methods.

use crate::iter::IterMethod;
use crate::reductions::{BoolReduce, NumReduce};
use noeta_ext_abi::width_doors::WidthDisclosure;

/// What an `Iterator<T>` method discloses.
pub fn of_iter_method(m: IterMethod) -> WidthDisclosure {
    match m {
        // Renders the remaining elements into one string.
        IterMethod::Join => WidthDisclosure::Display,
        // Hands the program an extremum chosen under an order. Each drains into the eager list
        // reduction, which is where that order is computed.
        IterMethod::Min | IterMethod::Max => WidthDisclosure::Order,
        // Folds the remaining elements arithmetically, wrapping at the ELEMENT width rather than
        // the erased one — `[200u8, 100u8].iter().sum()` is `44`.
        IterMethod::Sum | IterMethod::Product => WidthDisclosure::Compute,
        // The same fold, reporting instead of wrapping — so the element width decides not the
        // digits but WHICH SUMS OVERFLOW. At 8 bits `200 + 100` overflows where at 64 it does not,
        // and at 64 the signed and unsigned readings disagree about it in both directions
        // (`u64::MAX + 2` wraps past zero; the same words read signed are `-1 + 2`).
        IterMethod::CheckedSum => WidthDisclosure::Compute,
        // Builds a set's canonical buffer.
        IterMethod::ToSet => WidthDisclosure::Identity,
        // Equality, which the erased word answers exactly.
        IterMethod::Contains => WidthDisclosure::None,
        // Counting, and boolean folds over `bool` elements — no integer width is involved in
        // either answer.
        IterMethod::Count | IterMethod::CountTrue | IterMethod::Any | IterMethod::All => {
            WidthDisclosure::None
        }
        // Element and sequence producers: each hands elements onward, and the static type at the
        // door that consumes them is what names the width there.
        IterMethod::Next
        | IterMethod::Collect
        | IterMethod::Last
        | IterMethod::Take
        | IterMethod::Drop
        | IterMethod::Chain
        | IterMethod::Enumerate
        | IterMethod::Zip
        | IterMethod::Map
        | IterMethod::Filter => WidthDisclosure::None,
    }
}

/// What a numeric list reduction discloses.
///
/// The split runs between the two that hand back an **element** and the two that compute a **new
/// number**. `min`/`max` produce an order a program observes and are right at every width below 64
/// under either reading, so they are ordering doors; `sum`/`product` wrap at the element width, so
/// `[200u8, 100u8].sum()` is `44` and no hint can say so.
pub fn of_num_reduce(m: NumReduce) -> WidthDisclosure {
    match m {
        // The ordering reductions pick under an order the program then sees.
        NumReduce::Min | NumReduce::Max => WidthDisclosure::Order,
        // The arithmetic folds wrap at the ELEMENT width rather than the erased one.
        NumReduce::Sum | NumReduce::Product => WidthDisclosure::Compute,
    }
}

/// What a boolean list reduction discloses. None of them can: every one folds `bool` elements.
pub fn of_bool_reduce(m: BoolReduce) -> WidthDisclosure {
    match m {
        BoolReduce::Any | BoolReduce::All | BoolReduce::CountTrue => WidthDisclosure::None,
    }
}

/// The `List<T>` methods that reach their implementation by **name** rather than through a surface
/// enum, and what each discloses.
///
/// This is the hole a census over enums cannot see. `ListMethod`/`NumReduce`/`BoolReduce` between
/// them cover twenty of the checker's twenty-seven `List<T>` methods; the rest are matched on their
/// name at the dispatch site (`is_bulk_method`, and `checked_sum`'s own special case), so no
/// exhaustive match ever forces them to be classified — and a door nothing forces is a door that
/// reads the erased word until someone goes looking. Two of the rows below (`abs`, `clamp`) were
/// found that way, by classifying the surface rather than by anything failing.
///
/// The completeness check lives in `noeta-conformance`'s census, which reads the checker's own
/// `list_method` surface and asserts every name is either a member of an enum or a row here — and
/// walks each row that discloses (a `u64` past bit 63) or computes (a **boxed** narrow-width list,
/// against its packed twin), so a classification here is a claim the census has to make good on
/// rather than a label.
pub const NAME_DISPATCHED_LIST_METHODS: &[(&str, WidthDisclosure)] = &[
    // The overflow-reporting fold: whether it reports at all depends on the element width.
    ("checked_sum", WidthDisclosure::Compute),
    // The bulk array ops. All four produce numbers out of numbers at the element's width: `scale`
    // and `neg` wrap there the same way `+` does, `abs` folds around it (`i8::MIN.abs()` stays
    // `i8::MIN`, and an unsigned element is already non-negative), and `clamp` compares against
    // bounds the checker types as the element type. A packed receiver reads all of that off its
    // buffer's schema; a boxed one has to be told.
    ("scale", WidthDisclosure::Compute),
    ("neg", WidthDisclosure::Compute),
    ("abs", WidthDisclosure::Compute),
    ("clamp", WidthDisclosure::Compute),
    // Sequence and count producers: each hands elements onward carrying their own static type, and
    // a length is not a width.
    ("len", WidthDisclosure::None),
    ("iter", WidthDisclosure::None),
    ("enumerate", WidthDisclosure::None),
    ("map", WidthDisclosure::None),
    ("filter", WidthDisclosure::None),
    // `to_bytes` writes a packed buffer's raw words, and a packed list carries its element width in
    // its schema rather than reading it off a hint — the buffer IS the width's home.
    ("to_bytes", WidthDisclosure::None),
];

/// What the `List<T>` method `name` discloses, whether it is dispatched through an enum or by name.
pub fn of_list_method_name(name: &str) -> Option<WidthDisclosure> {
    if let Some(m) = crate::ListMethod::from_name(name) {
        return Some(noeta_ext_abi::width_doors::of_list_method(m));
    }
    if let Some(m) = NumReduce::from_name(name) {
        return Some(of_num_reduce(m));
    }
    if let Some(m) = BoolReduce::from_name(name) {
        return Some(of_bool_reduce(m));
    }
    NAME_DISPATCHED_LIST_METHODS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| *d)
}
