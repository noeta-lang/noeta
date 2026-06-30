//! The lazy-iterator method surface (Track I.1a).
//!
//! Like [`crate::FileHandleMethod`], this enum is shared so a `match` over it is exhaustive in both
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
    /// `count()` → the number of remaining elements as an `int` (drains the iterator, Track I.1b).
    Count,
}

impl IterMethod {
    pub fn from_name(name: &str) -> Option<IterMethod> {
        match name {
            "next" => Some(IterMethod::Next),
            "collect" => Some(IterMethod::Collect),
            "take" => Some(IterMethod::Take),
            "drop" => Some(IterMethod::Drop),
            "chain" => Some(IterMethod::Chain),
            "count" => Some(IterMethod::Count),
            _ => None,
        }
    }
}
