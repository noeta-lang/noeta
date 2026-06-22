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
        Err(StdError {
            kind: ErrorKind::Arity,
            message: format!(
                "method `{method}` takes {expected} argument(s) but {} were supplied",
                args.len()
            ),
        })
    }
}

fn want_str<'a>(method: &str, args: &[Arg<'a>], index: usize) -> Result<&'a str, StdError> {
    match args[index] {
        Arg::Str(value) => Ok(value),
        _ => Err(arg_type_error(method, "string")),
    }
}

fn want_int(method: &str, args: &[Arg], index: usize) -> Result<i64, StdError> {
    match args[index] {
        Arg::Int(value) => Ok(value),
        _ => Err(arg_type_error(method, "int")),
    }
}

fn arg_type_error(method: &str, expected: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("method `{method}` expects {} argument", an(expected)),
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
}
