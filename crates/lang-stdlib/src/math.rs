//! The `math` Ring 2 module: pure scalar math. Imported with `use std.{math}` and called
//! `math.sqrt(2.0)`, `math.pow(2, 10)`, etc.
//!
//! Every function is a pure mapping over numeric arguments and lives here once, so both
//! backends compute bit-identically — the differential oracle holds by construction. The
//! single tricky decision is int-vs-float result typing, resolved per function to match the
//! useful reading:
//!
//! - `sqrt`/`pow`/`pi`/`e` always yield a float (they are real-valued).
//! - `floor`/`ceil`/`round` yield an *int* — you floor to get an index, not a float.
//! - `abs`/`min`/`max` *preserve* their argument kind (int in → int out) so integer code
//!   stays integer.

use crate::{Arg, Dispatch, ErrorKind, Output, StdError};

/// The `math` function names, in dispatch order — for tooling that wants the surface.
pub const FUNCTIONS: &[&str] = &[
    "pi", "e", "sqrt", "pow", "abs", "floor", "ceil", "round", "min", "max",
];

/// Dispatch a `math` module function. Returns [`Dispatch::Unknown`] when `func` is not part of
/// the surface so the caller renders the canonical "no such function" error.
pub fn call(func: &str, args: &[Arg]) -> Dispatch {
    if !FUNCTIONS.contains(&func) {
        return Dispatch::Unknown;
    }
    match call_inner(func, args) {
        Ok(output) => Dispatch::Done(output),
        Err(error) => Dispatch::Err(error),
    }
}

fn call_inner(func: &str, args: &[Arg]) -> Result<Output, StdError> {
    match func {
        "pi" => {
            want_arity(func, args, 0)?;
            Ok(Output::Float(std::f64::consts::PI))
        }
        "e" => {
            want_arity(func, args, 0)?;
            Ok(Output::Float(std::f64::consts::E))
        }
        "sqrt" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.sqrt()))
        }
        "pow" => {
            want_arity(func, args, 2)?;
            let base = want_float(func, args, 0)?;
            let exp = want_float(func, args, 1)?;
            Ok(Output::Float(base.powf(exp)))
        }
        "abs" => {
            want_arity(func, args, 1)?;
            Ok(match want_number(func, args, 0)? {
                Number::Int(i) => Output::Int(i.wrapping_abs()),
                Number::Float(f) => Output::Float(f.abs()),
            })
        }
        "floor" => {
            want_arity(func, args, 1)?;
            Ok(Output::Int(want_float(func, args, 0)?.floor() as i64))
        }
        "ceil" => {
            want_arity(func, args, 1)?;
            Ok(Output::Int(want_float(func, args, 0)?.ceil() as i64))
        }
        "round" => {
            want_arity(func, args, 1)?;
            // Round half away from zero (Rust's `f64::round`) — deterministic across backends.
            Ok(Output::Int(want_float(func, args, 0)?.round() as i64))
        }
        "min" => {
            want_arity(func, args, 2)?;
            Ok(pick(func, args, Ordering::Min)?)
        }
        "max" => {
            want_arity(func, args, 2)?;
            Ok(pick(func, args, Ordering::Max)?)
        }
        // FUNCTIONS gates entry, so every listed function is handled above.
        _ => unreachable!("unlisted math function `{func}`"),
    }
}

/// A numeric argument, preserving whether the caller passed an int or a float so kind-preserving
/// functions (`abs`/`min`/`max`) can return the same kind they received.
#[derive(Clone, Copy)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn as_float(self) -> f64 {
        match self {
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

enum Ordering {
    Min,
    Max,
}

/// `min`/`max` of two arguments, preserving int kind only when *both* arguments are ints (a
/// mixed pair promotes to float, matching how arithmetic coerces). Ties return the first
/// argument, which is irrelevant to the numeric result but keeps the rule total.
fn pick(func: &str, args: &[Arg], which: Ordering) -> Result<Output, StdError> {
    let a = want_number(func, args, 0)?;
    let b = want_number(func, args, 1)?;
    let take_a = match which {
        Ordering::Min => a.as_float() <= b.as_float(),
        Ordering::Max => a.as_float() >= b.as_float(),
    };
    let chosen = if take_a { a } else { b };
    Ok(match (a, b, chosen) {
        (Number::Int(_), Number::Int(_), Number::Int(i)) => Output::Int(i),
        _ => Output::Float(chosen.as_float()),
    })
}

fn want_arity(func: &str, args: &[Arg], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(crate::arity_error(func, expected, args.len()))
    }
}

/// Read a numeric argument as a float, accepting both int and float (ints widen, as arithmetic
/// does). A non-number is a type error.
fn want_float(func: &str, args: &[Arg], index: usize) -> Result<f64, StdError> {
    Ok(want_number(func, args, index)?.as_float())
}

fn want_number(func: &str, args: &[Arg], index: usize) -> Result<Number, StdError> {
    match args[index] {
        Arg::Int(i) => Ok(Number::Int(i)),
        Arg::Float(f) => Ok(Number::Float(f)),
        _ => Err(StdError {
            kind: ErrorKind::ArgType,
            message: format!("function `{func}` expects a number argument"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(func: &str, args: &[Arg]) -> Output {
        match call(func, args) {
            Dispatch::Done(output) => output,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn constants_and_roots() {
        assert_eq!(done("pi", &[]), Output::Float(std::f64::consts::PI));
        assert_eq!(done("sqrt", &[Arg::Float(9.0)]), Output::Float(3.0));
        // Ints widen to floats for real-valued functions.
        assert_eq!(done("sqrt", &[Arg::Int(16)]), Output::Float(4.0));
        assert_eq!(
            done("pow", &[Arg::Int(2), Arg::Int(10)]),
            Output::Float(1024.0)
        );
    }

    #[test]
    fn rounding_yields_int() {
        assert_eq!(done("floor", &[Arg::Float(2.9)]), Output::Int(2));
        assert_eq!(done("ceil", &[Arg::Float(2.1)]), Output::Int(3));
        assert_eq!(done("round", &[Arg::Float(2.5)]), Output::Int(3));
        assert_eq!(done("round", &[Arg::Float(-2.5)]), Output::Int(-3));
    }

    #[test]
    fn kind_preserving_abs_min_max() {
        assert_eq!(done("abs", &[Arg::Int(-7)]), Output::Int(7));
        assert_eq!(done("abs", &[Arg::Float(-7.5)]), Output::Float(7.5));
        assert_eq!(done("min", &[Arg::Int(3), Arg::Int(8)]), Output::Int(3));
        assert_eq!(done("max", &[Arg::Int(3), Arg::Int(8)]), Output::Int(8));
        // A mixed pair promotes to float.
        assert_eq!(
            done("max", &[Arg::Int(3), Arg::Float(2.5)]),
            Output::Float(3.0)
        );
    }

    #[test]
    fn wrong_type_is_an_error() {
        match call("sqrt", &[Arg::Str("x")]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::ArgType),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn arity_mismatch_is_an_error() {
        match call("pow", &[Arg::Int(2)]) {
            Dispatch::Err(error) => assert_eq!(error.kind, ErrorKind::Arity),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn unknown_function_falls_through() {
        assert_eq!(call("tan", &[Arg::Float(1.0)]), Dispatch::Unknown);
    }
}
