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
        // Hands the program an extremum chosen under an order.
        IterMethod::Min | IterMethod::Max => WidthDisclosure::Order,
        // Folds the remaining elements arithmetically; the total's own static type carries the
        // width to whatever renders it, and the FOLD itself wraps at the element width rather than
        // the erased one.
        IterMethod::Sum | IterMethod::Product | IterMethod::CheckedSum => WidthDisclosure::Order,
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
pub fn of_num_reduce(m: NumReduce) -> WidthDisclosure {
    match m {
        // The ordering reductions pick under an order the program then sees.
        NumReduce::Min | NumReduce::Max => WidthDisclosure::Order,
        // The arithmetic folds wrap at the ELEMENT width rather than the erased one, which is the
        // same dependence on the static type by a different name.
        NumReduce::Sum | NumReduce::Product => WidthDisclosure::Order,
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
/// exhaustive match ever forces them to be classified. Two of them were wrong when this table was
/// written, and neither was reachable from anything that would have said so.
///
/// The completeness check lives in `noeta-conformance`'s census, which reads the checker's own
/// `list_method` surface and asserts every name is either a member of an enum or a row here.
pub const NAME_DISPATCHED_LIST_METHODS: &[(&str, WidthDisclosure)] = &[
    // The overflow-reporting fold: whether it reports at all depends on the element width.
    ("checked_sum", WidthDisclosure::Order),
    // The bulk array ops. `scale` and `neg` only compute — they wrap at the element width the same
    // way `+` does, and never compare — so no width can leak through them. `abs` and `clamp`
    // COMPARE (against zero, and against the bounds), and a comparison on the erased word is
    // exactly where this family's bugs live.
    ("scale", WidthDisclosure::None),
    ("neg", WidthDisclosure::None),
    ("abs", WidthDisclosure::Order),
    ("clamp", WidthDisclosure::Order),
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
