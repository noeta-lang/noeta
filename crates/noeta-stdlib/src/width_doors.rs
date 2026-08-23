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
        IterMethod::Sum | IterMethod::Product => WidthDisclosure::Order,
        // The same fold, reporting instead of wrapping — so the element width decides not the
        // digits but WHICH SUMS OVERFLOW, and at 64 bits the signed and unsigned readings disagree
        // about that (`u64::MAX + 2` wraps past zero; the same words read signed are `-1 + 2`).
        IterMethod::CheckedSum => WidthDisclosure::Order,
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
