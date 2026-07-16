//! Shared **argument extraction** for dispatch bodies (audit-2 F8) — the `want_*` guard family,
//! written once over both marshalling generations.
//!
//! Every dispatch re-validates what its declared signature already states (the checker gates
//! arity/types statically; this layer is deliberately defensive — a `dyn` launder or a bug must
//! surface as a diagnostic, not UB). Before this module, that guard family was copy-pasted per
//! seam and per module (`Arg`-based in `ring1`/`math`, `NativeValue`-based in the std registry /
//! `datetime` / `tracing`), so the canonical error text lived in five places. [`ArgView`] is the
//! two-method projection that lets one generic implementation serve both; the error text is
//! byte-identical to every copy it replaces (the differential pins it).
//!
//! Module-specific extractors (a `string|bytes` digest input, an extern downcast, a number that
//! widens int→float) stay next to their modules — only the exact duplicates live here.

use crate::{StdError, arity_error, type_error};

/// The narrow projection [`want_int`]/[`want_str`] need: how an argument view exposes the two
/// primitive shapes the shared guards read. Implemented by both marshalling generations —
/// [`crate::ring1::Arg`] (the legacy zero-alloc string seam) and [`crate::NativeValue`] (the
/// registry seam).
pub trait ArgView {
    /// The argument's string content, if it is a string.
    fn view_str(&self) -> Option<&str>;
    /// The argument's integer value, if it is an int.
    fn view_int(&self) -> Option<i64>;
}

impl ArgView for crate::ring1::Arg<'_> {
    fn view_str(&self) -> Option<&str> {
        match self {
            crate::ring1::Arg::Str(s) => Some(s),
            _ => None,
        }
    }
    fn view_int(&self) -> Option<i64> {
        match self {
            crate::ring1::Arg::Int(n) => Some(*n),
            _ => None,
        }
    }
}

impl ArgView for crate::NativeValue {
    fn view_str(&self) -> Option<&str> {
        match self {
            crate::NativeValue::Str(s) => Some(s),
            _ => None,
        }
    }
    fn view_int(&self) -> Option<i64> {
        match self {
            crate::NativeValue::Scalar(crate::Scalar::Int(n)) => Some(*n),
            _ => None,
        }
    }
}

/// Exact-arity guard — the canonical "wrong number of arguments" gate every dispatch opens with.
pub fn want_arity<T>(func: &str, args: &[T], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(func, expected, args.len()))
    }
}

/// Accept `min..=max` arguments — a function with trailing-optional parameters (http arc H4 /
/// the core methods' optional-arg analogue). The checker already gates the range, so this is the
/// defensive twin of [`want_arity`]; on violation it reports `max` as the expected count.
pub fn want_arity_range<T>(func: &str, args: &[T], min: usize, max: usize) -> Result<(), StdError> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(arity_error(func, max, args.len()))
    }
}

/// A required string argument at `index`.
pub fn want_str<'a, T: ArgView>(
    func: &str,
    args: &'a [T],
    index: usize,
) -> Result<&'a str, StdError> {
    args.get(index)
        .and_then(ArgView::view_str)
        .ok_or_else(|| type_error(func, "string"))
}

/// A required int argument at `index`.
pub fn want_int<T: ArgView>(func: &str, args: &[T], index: usize) -> Result<i64, StdError> {
    args.get(index)
        .and_then(ArgView::view_int)
        .ok_or_else(|| type_error(func, "int"))
}

/// An **optional** int argument at `index`: `None` when absent, the value when present, a type
/// error when present-but-not-an-int. The reader for a trailing-optional parameter.
pub fn opt_int<T: ArgView>(func: &str, args: &[T], index: usize) -> Result<Option<i64>, StdError> {
    match args.get(index) {
        None => Ok(None),
        Some(a) => match a.view_int() {
            Some(n) => Ok(Some(n)),
            None => Err(type_error(func, "int")),
        },
    }
}

/// An **optional** string argument at `index` — the string twin of [`opt_int`].
pub fn opt_str<'a, T: ArgView>(
    func: &str,
    args: &'a [T],
    index: usize,
) -> Result<Option<&'a str>, StdError> {
    match args.get(index) {
        None => Ok(None),
        Some(a) => match a.view_str() {
            Some(s) => Ok(Some(s)),
            None => Err(type_error(func, "string")),
        },
    }
}
