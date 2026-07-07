//! The lazy-iterator method surface (Track I.1a).
//!
//! This enum is shared so a `match` over it is exhaustive in both
//! backends — adding a method will not compile until both handle it. Iterators are reference values
//! whose backing representation differs per backend (each wraps its own list value), so — unlike
//! `FileHandle` — only the method *names* live here; the cursor logic is implemented per backend, with
//! the differential oracle guarding that the two agree.

/// A method callable on an iterator value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IterMethod {
    /// `next()` → `some(elem)` advancing the cursor, `none` at end.
    Next,
    /// `collect()` → a `List` of the remaining elements (drains the iterator).
    Collect,
    /// `take(n)` → an iterator yielding at most `n` more elements (Track I.1b).
    Take,
    /// `drop(n)` → an iterator skipping the next `n` elements, yielding the rest (Track I.1b).
    Drop,
    /// `chain(other)` → an iterator yielding all of `self` then all of `other` (Track I.1b).
    Chain,
    /// `enumerate()` → an iterator of `(index, element)` tuples, indexing from 0 (Track I.1b.2).
    Enumerate,
    /// `zip(other)` → an iterator of `(self_elem, other_elem)` tuples, stopping at the shorter
    /// (Track I.1b.2).
    Zip,
    /// `count()` → the number of remaining elements as an `int` (drains the iterator, Track I.1b).
    Count,
    /// `sum()` → the sum of the remaining numeric elements (drains the iterator, Track I.1b.2).
    Sum,
    /// `map(f)` → an iterator yielding `f(element)` for each element (Track I.1c).
    Map,
    /// `filter(f)` → an iterator yielding the elements for which `f(element)` is true (Track I.1c).
    Filter,
}

impl IterMethod {
    pub fn from_name(name: &str) -> Option<IterMethod> {
        match name {
            "next" => Some(IterMethod::Next),
            "collect" => Some(IterMethod::Collect),
            "take" => Some(IterMethod::Take),
            "drop" => Some(IterMethod::Drop),
            "chain" => Some(IterMethod::Chain),
            "enumerate" => Some(IterMethod::Enumerate),
            "zip" => Some(IterMethod::Zip),
            "count" => Some(IterMethod::Count),
            "sum" => Some(IterMethod::Sum),
            "map" => Some(IterMethod::Map),
            "filter" => Some(IterMethod::Filter),
            _ => None,
        }
    }
}
