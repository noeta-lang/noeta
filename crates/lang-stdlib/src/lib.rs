//! The layered standard library (M1.10).
//!
//! Ring 1 is the always-present core surface bound to the language's primitive types.
//! Where a Ring 1 operation is expressible over data that is represented *identically*
//! in both runtimes, its semantics live here once and both backends call into it — so
//! the differential oracle (`TreeWalkBackend` ≡ `VmBackend`) holds by construction, not
//! merely by test. Strings are the first such surface: both the M0 tree-walker
//! (`Value::Str(String)`) and the M1 VM (`Payload::Str(String)`) store them as a Rust
//! `String`, so every string method's behavior, arity, and argument typing is defined
//! here and each backend is reduced to thin value↔primitive glue.
//!
//! Collection methods (list/map) manipulate backend-specific value representations and
//! so cannot live here wholesale; they are implemented per backend with the differential
//! as the guard. Determinism is a hard requirement throughout (no wall clock, no
//! hash-order, seeded PRNG) — see `plans/m1/slice-10-stdlib.md`.
//!
//! Ring 2 native modules (`json`, `math`, `fs`, …) are imported with `use std.{name}` and
//! dispatched as `name.func(args)` through the native-extension [`registry`]: each module declares
//! its functions and one shared `dispatch`, and both backends route every call through it (so the
//! differential holds by construction). Each backend only binds the module value and marshals
//! arguments/results across the neutral [`registry::NativeValue`]/[`registry::NativeOut`] seam.

pub mod env;
pub mod executor;
pub mod fs;
pub mod handle;
pub mod host;
pub mod iter;
pub mod json;
pub mod math;
pub mod quat;
pub mod random;
pub mod registry;
pub mod vec3;

pub use executor::{Executor, SandboxExecutor};
pub use handle::{FileHandle, FileHandleMethod, FileMode, Flush, ReadSource};
pub use host::{Host, SandboxHost};
pub use iter::IterMethod;
pub use registry::{
    ExtFn, ExtModule, Extension, NativeOut, NativeValue, RetTy, Scalar, SigType, StdExtension,
    TypeRecipe,
};

/// A backend-agnostic view of an argument value, covering only the primitive shapes the
/// stdlib introspects. Each backend cheaply projects its own `Value` onto this; anything
/// the stdlib never inspects collapses to [`Arg::Other`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arg<'a> {
    Str(&'a str),
    Int(i64),
    Float(f64),
    Bool(bool),
    Other,
}

/// A backend-agnostic result the caller wraps back into its own `Value`. Kept to the
/// shapes Ring 1 string methods actually produce.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Str(String),
    Bool(bool),
    Int(i64),
    /// A float (e.g. `math.sqrt`). The caller wraps it in its float value.
    Float(f64),
    /// A list of strings (e.g. `split`). The caller builds its native list of string values.
    StrList(Vec<String>),
}

/// The category of a stdlib misuse, mapped by each backend onto a `DiagnosticCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Wrong number of arguments.
    Arity,
    /// An argument was the wrong type.
    ArgType,
    /// An index/range argument fell outside the collection's bounds.
    Bounds,
    /// A name that does not exist (e.g. an unknown function on a native module).
    UnknownName,
    /// A Ring 2 IO operation failed (e.g. reading a path absent from the sandbox).
    Io,
}

/// A stdlib misuse error. The `message` is rendered here so both backends report it
/// identically; the `kind` selects the diagnostic code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdError {
    pub kind: ErrorKind,
    pub message: String,
}

/// The outcome of attempting a built-in method dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// Handled — wrap this output into a native value.
    Done(Output),
    /// Not a built-in method of this surface; the caller continues to other dispatch.
    Unknown,
    /// A built-in method of this surface, but misused.
    Err(StdError),
}

/// The Ring 1 string methods, in dispatch order. Used by name resolution / tooling that
/// wants to know the surface without exercising it.
pub const STRING_METHODS: &[&str] = &[
    "upper",
    "lower",
    "trim",
    "contains",
    "starts_with",
    "ends_with",
    "split",
    "replace",
    "repeat",
];

/// Dispatch a built-in string method. `recv` is the receiver string; `args` are the
/// already-projected call arguments. Returns [`Dispatch::Unknown`] when `method` is not
/// part of the string surface so the caller can fall through to its other dispatch.
pub fn string_method(recv: &str, method: &str, args: &[Arg]) -> Dispatch {
    if !STRING_METHODS.contains(&method) {
        return Dispatch::Unknown;
    }
    match string_method_inner(recv, method, args) {
        Ok(output) => Dispatch::Done(output),
        Err(error) => Dispatch::Err(error),
    }
}

fn string_method_inner(recv: &str, method: &str, args: &[Arg]) -> Result<Output, StdError> {
    match method {
        "upper" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.to_uppercase()))
        }
        "lower" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.to_lowercase()))
        }
        "trim" => {
            want_arity(method, args, 0)?;
            Ok(Output::Str(recv.trim().to_string()))
        }
        "contains" => {
            want_arity(method, args, 1)?;
            Ok(Output::Bool(recv.contains(want_str(method, args, 0)?)))
        }
        "starts_with" => {
            want_arity(method, args, 1)?;
            Ok(Output::Bool(recv.starts_with(want_str(method, args, 0)?)))
        }
        "ends_with" => {
            want_arity(method, args, 1)?;
            Ok(Output::Bool(recv.ends_with(want_str(method, args, 0)?)))
        }
        "split" => {
            want_arity(method, args, 1)?;
            Ok(Output::StrList(split(recv, want_str(method, args, 0)?)))
        }
        "replace" => {
            want_arity(method, args, 2)?;
            let from = want_str(method, args, 0)?;
            let to = want_str(method, args, 1)?;
            Ok(Output::Str(recv.replace(from, to)))
        }
        "repeat" => {
            want_arity(method, args, 1)?;
            let count = want_int(method, args, 0)?;
            Ok(Output::Str(recv.repeat(count.max(0) as usize)))
        }
        // STRING_METHODS gates entry, so every listed method is handled above.
        _ => unreachable!("unlisted string method `{method}`"),
    }
}

/// Split `recv` on `sep`. An empty separator yields the Unicode scalar characters (the
/// useful, surprise-free reading), avoiding Rust's leading/trailing empty fields.
fn split(recv: &str, sep: &str) -> Vec<String> {
    if sep.is_empty() {
        recv.chars().map(|c| c.to_string()).collect()
    } else {
        recv.split(sep).map(|part| part.to_string()).collect()
    }
}

fn want_arity(method: &str, args: &[Arg], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(method, expected, args.len()))
    }
}

fn want_str<'a>(method: &str, args: &[Arg<'a>], index: usize) -> Result<&'a str, StdError> {
    match args[index] {
        Arg::Str(value) => Ok(value),
        _ => Err(type_error(method, "string")),
    }
}

fn want_int(method: &str, args: &[Arg], index: usize) -> Result<i64, StdError> {
    match args[index] {
        Arg::Int(value) => Ok(value),
        _ => Err(type_error(method, "int")),
    }
}

/// Build the canonical "wrong number of arguments" error. Public so the collection methods
/// (implemented per backend over their own value types) report misuse with text identical to
/// the string surface — keeping both backends' diagnostics in lockstep.
pub fn arity_error(method: &str, expected: usize, got: usize) -> StdError {
    StdError {
        kind: ErrorKind::Arity,
        message: format!("method `{method}` takes {expected} argument(s) but {got} were supplied"),
    }
}

/// Build the canonical "wrong argument type" error. `expected` is the type noun (`"string"`,
/// `"int"`, `"list of strings"`, ...); the article is chosen for readability. Public for the
/// same reason as [`arity_error`].
pub fn type_error(method: &str, expected: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("method `{method}` expects {} argument", an(expected)),
    }
}

/// Build the canonical "cannot order" error for a method (`sorted`, `to_set`) over values that
/// are not mutually orderable (mixed kinds, or a non-orderable element). Maps to `E0007` like
/// other type misuse. Both `sorted` and set construction require a single orderable element type
/// so the result has a deterministic canonical order.
pub fn unorderable_error(method: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "method `{method}` requires values of a single orderable type (int, float, or string)"
        ),
    }
}

/// Build the canonical "slice out of bounds" error for `slice(start, end)` on a list of
/// length `len`. Public so both backends render the bounds error identically (→ `IndexOutOfBounds`).
pub fn slice_bounds_error(start: i64, end: i64, len: usize) -> StdError {
    StdError {
        kind: ErrorKind::Bounds,
        message: format!("slice [{start}..{end}] is out of bounds for list of length {len}"),
    }
}

/// The `vec` module's scalar Vec3 function names (P-PACK Phase 4.1), in surface order. A "Vec3" is
/// any `@packed`-or-plain struct value with exactly three `f32` fields; structural, so a user names
/// the type. `dot`/`length` return an `f32`; the rest return a Vec3 of the same shape as the input.
pub const VEC_SCALAR_FUNCTIONS: &[&str] = &[
    "add",
    "sub",
    "scale",
    "dot",
    "cross",
    "length",
    "normalize",
    "distance",
    "lerp",
    "reflect",
    "clamp",
    "min",
    "max",
    "abs",
];

/// Build the canonical "no such function on a native module" error (→ `E0005`).
pub fn no_function_error(module: &str, func: &str) -> StdError {
    StdError {
        kind: ErrorKind::UnknownName,
        message: format!("module `{module}` has no function `{func}`"),
    }
}

/// Build the canonical "invalid JSON" error for `json.parse` (→ `E0007`).
pub fn invalid_json_error(detail: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("invalid JSON: {detail}"),
    }
}

/// Format an `f64` for display and serialization: a whole finite value keeps one decimal place
/// (`2.0`, not `2`), everything else uses the shortest round-tripping form. Both backends' `display`
/// and the shared JSON serializer call this, so numbers render identically everywhere — the
/// single source the two duplicated copies used to be.
pub fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

/// Format an `f32` at f32 precision (the shortest round-tripping f32 decimal) — the `f32` analog of
/// [`format_float`], so e.g. `0.1f32` shows `0.1`, not the f64-widened `0.10000000149…`.
pub fn format_f32(f: f32) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        f.to_string()
    }
}

/// "a string" / "an int" — pick the article so messages read naturally.
fn an(noun: &str) -> String {
    let article = match noun.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    };
    format!("{article} {noun}")
}

/// The Ring 1 list methods that manipulate backend-specific values, so each backend implements
/// the value work itself. They are enumerated here (rather than matched as strings in each
/// backend) so a `match` over [`ListMethod`] is exhaustive: adding a method will not compile
/// until *both* backends handle it — the differential's static guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMethod {
    /// `reverse()` → a new list with the elements in reverse order.
    Reverse,
    /// `contains(x)` → `bool`, by structural value equality.
    Contains,
    /// `join(sep)` → a string of the elements' display forms separated by `sep`.
    Join,
    /// `sorted()` → a new list sorted by the primitive ordering (homogeneous numbers or
    /// strings); a non-orderable or mixed-kind element is an error.
    Sorted,
    /// `slice(start, end)` → the sublist `[start, end)`; out-of-range bounds are an error.
    Slice,
    /// `first()` → `some(head)` if the list is non-empty, else `none`.
    First,
    /// `last()` → `some(tail)` if the list is non-empty, else `none`.
    Last,
    /// `to_set()` → a `Set` of the list's elements (sorted + de-duplicated); a non-orderable or
    /// mixed-kind element is an error.
    ToSet,
    /// `set(index, value)` → a **new** list with `index` replaced by `value` (value semantics; the
    /// receiver is unchanged). An out-of-range index is an error (E0016), as for index reads — `set`
    /// replaces an existing position, it does not append. The reuse pass may make this an in-place
    /// overwrite when the receiver is uniquely owned. This is the target of the `xs[i] = v` sugar.
    Set,
}

impl ListMethod {
    pub fn from_name(name: &str) -> Option<ListMethod> {
        match name {
            "reverse" => Some(ListMethod::Reverse),
            "contains" => Some(ListMethod::Contains),
            "join" => Some(ListMethod::Join),
            "sorted" => Some(ListMethod::Sorted),
            "slice" => Some(ListMethod::Slice),
            "first" => Some(ListMethod::First),
            "last" => Some(ListMethod::Last),
            "to_set" => Some(ListMethod::ToSet),
            "set" => Some(ListMethod::Set),
            _ => None,
        }
    }
}

/// The Ring 1 set methods. Value-specific (each backend implements them), enumerated here so the
/// dispatch `match` is exhaustive in both backends — see [`ListMethod`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetMethod {
    /// `contains(x)` → `bool`, whether `x` is a member (by structural equality).
    Contains,
    /// `union(other)` → a new set with every element of `self` or `other`.
    Union,
    /// `intersection(other)` → a new set with the elements in both `self` and `other`.
    Intersection,
    /// `add(x)` → a **new** set with `x` added (a no-op copy if already present). Value semantics; the
    /// single-element companion to `union`. Same in-place-reuse treatment as the other updates.
    Add,
    /// `remove(x)` → a **new** set without `x` (a no-op copy if absent). Value semantics.
    Remove,
}

impl SetMethod {
    pub fn from_name(name: &str) -> Option<SetMethod> {
        match name {
            "contains" => Some(SetMethod::Contains),
            "union" => Some(SetMethod::Union),
            "intersection" => Some(SetMethod::Intersection),
            "add" => Some(SetMethod::Add),
            "remove" => Some(SetMethod::Remove),
            _ => None,
        }
    }
}

/// The Ring 1 map methods. See [`ListMethod`] for why these are an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapMethod {
    /// `keys()` → a list of the map's keys (sorted, since maps iterate in key order).
    Keys,
    /// `values()` → a list of the map's values, in key order.
    Values,
    /// `has(key)` → `bool`, whether `key` is present.
    Has,
    /// `set(key, value)` → a **new** map with `key` mapped to `value` (added or overwritten). Value
    /// semantics — the receiver is unchanged; the reuse pass may make this an in-place mutation when
    /// the receiver is uniquely owned (the map analog of the list `~` self-append).
    Set,
    /// `remove(key)` → a **new** map without `key` (a no-op copy if `key` is absent). Value semantics,
    /// same in-place-reuse treatment as [`MapMethod::Set`].
    Remove,
}

impl MapMethod {
    pub fn from_name(name: &str) -> Option<MapMethod> {
        match name {
            "keys" => Some(MapMethod::Keys),
            "values" => Some(MapMethod::Values),
            "has" => Some(MapMethod::Has),
            "set" => Some(MapMethod::Set),
            "remove" => Some(MapMethod::Remove),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(recv: &str, method: &str, args: &[Arg]) -> Output {
        match string_method(recv, method, args) {
            Dispatch::Done(output) => output,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn case_transforms_and_trim() {
        assert_eq!(done("Hi", "upper", &[]), Output::Str("HI".into()));
        assert_eq!(done("Hi", "lower", &[]), Output::Str("hi".into()));
        assert_eq!(done("  x  ", "trim", &[]), Output::Str("x".into()));
    }

    #[test]
    fn predicates_return_bool() {
        assert_eq!(
            done("hello", "contains", &[Arg::Str("ell")]),
            Output::Bool(true)
        );
        assert_eq!(
            done("hello", "starts_with", &[Arg::Str("he")]),
            Output::Bool(true)
        );
        assert_eq!(
            done("hello", "ends_with", &[Arg::Str("lo")]),
            Output::Bool(true)
        );
        assert_eq!(
            done("hello", "contains", &[Arg::Str("z")]),
            Output::Bool(false)
        );
    }

    #[test]
    fn split_on_separator_and_on_empty() {
        assert_eq!(
            done("a,b,c", "split", &[Arg::Str(",")]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            done("abc", "split", &[Arg::Str("")]),
            Output::StrList(vec!["a".into(), "b".into(), "c".into()])
        );
    }

    #[test]
    fn replace_and_repeat() {
        assert_eq!(
            done("a.b.c", "replace", &[Arg::Str("."), Arg::Str("/")]),
            Output::Str("a/b/c".into())
        );
        assert_eq!(
            done("ab", "repeat", &[Arg::Int(3)]),
            Output::Str("ababab".into())
        );
        // A negative count clamps to an empty string rather than panicking.
        assert_eq!(
            done("ab", "repeat", &[Arg::Int(-1)]),
            Output::Str(String::new())
        );
    }

    #[test]
    fn unicode_is_handled_per_scalar() {
        assert_eq!(done("naïve", "upper", &[]), Output::Str("NAÏVE".into()));
        assert_eq!(
            done("héllo", "split", &[Arg::Str("")]),
            Output::StrList(vec![
                "h".into(),
                "é".into(),
                "l".into(),
                "l".into(),
                "o".into()
            ])
        );
    }

    #[test]
    fn unknown_method_falls_through() {
        assert_eq!(string_method("x", "frobnicate", &[]), Dispatch::Unknown);
    }

    #[test]
    fn arity_mismatch_is_an_error() {
        match string_method("x", "upper", &[Arg::Str("y")]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::Arity),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn wrong_argument_type_is_an_error() {
        match string_method("x", "contains", &[Arg::Int(1)]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::ArgType),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn collection_method_names_resolve() {
        assert_eq!(ListMethod::from_name("reverse"), Some(ListMethod::Reverse));
        assert_eq!(
            ListMethod::from_name("contains"),
            Some(ListMethod::Contains)
        );
        assert_eq!(ListMethod::from_name("join"), Some(ListMethod::Join));
        assert_eq!(ListMethod::from_name("sorted"), Some(ListMethod::Sorted));
        assert_eq!(ListMethod::from_name("slice"), Some(ListMethod::Slice));
        assert_eq!(ListMethod::from_name("first"), Some(ListMethod::First));
        assert_eq!(ListMethod::from_name("last"), Some(ListMethod::Last));
        assert_eq!(ListMethod::from_name("to_set"), Some(ListMethod::ToSet));
        assert_eq!(ListMethod::from_name("nope"), None);
        assert_eq!(MapMethod::from_name("keys"), Some(MapMethod::Keys));
        assert_eq!(MapMethod::from_name("values"), Some(MapMethod::Values));
        assert_eq!(MapMethod::from_name("has"), Some(MapMethod::Has));
        assert_eq!(MapMethod::from_name("nope"), None);
        assert_eq!(SetMethod::from_name("contains"), Some(SetMethod::Contains));
        assert_eq!(SetMethod::from_name("union"), Some(SetMethod::Union));
        assert_eq!(
            SetMethod::from_name("intersection"),
            Some(SetMethod::Intersection)
        );
        assert_eq!(SetMethod::from_name("nope"), None);
    }

    #[test]
    fn error_builders_render_canonically() {
        assert_eq!(
            arity_error("reverse", 0, 2).message,
            "method `reverse` takes 0 argument(s) but 2 were supplied"
        );
        assert_eq!(
            type_error("has", "string").message,
            "method `has` expects a string argument"
        );
    }
}
