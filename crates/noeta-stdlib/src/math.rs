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
//! - `sin`/`cos`/`tan` always yield a float (argument in radians).
//! - The transcendental family (`ln`/`log`/`log2`/`log10`/`exp`, inverse trig, hyperbolics,
//!   `hypot`) is real-valued and always yields a float, like `sqrt`. Out-of-domain inputs
//!   (`ln(-1.0)`, `asin(2.0)`) yield NaN, matching `sqrt(-1.0)` — no new failure mode.

use crate::{Arg, Dispatch, ErrorKind, Output, StdError};

/// The `math` function names, in dispatch order — for tooling that wants the surface.
pub const FUNCTIONS: &[&str] = &[
    "pi", "e", "sqrt", "pow", "abs", "floor", "ceil", "round", "min", "max", "sin", "cos", "tan",
    "asin", "acos", "atan", "atan2", "ln", "log", "log2", "log10", "exp", "hypot", "sinh", "cosh",
    "tanh",
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
        // Trigonometry — real-valued, argument in radians (ints widen). Always a float.
        "sin" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.sin()))
        }
        "cos" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.cos()))
        }
        "tan" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.tan()))
        }
        // Inverse trig — real-valued, result in radians. Out-of-domain (`asin(2.0)`) is NaN.
        "asin" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.asin()))
        }
        "acos" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.acos()))
        }
        "atan" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.atan()))
        }
        // `atan2(y, x)` — the quadrant-aware angle of the vector `(x, y)`, y first like C/Rust.
        "atan2" => {
            want_arity(func, args, 2)?;
            let y = want_float(func, args, 0)?;
            let x = want_float(func, args, 1)?;
            Ok(Output::Float(y.atan2(x)))
        }
        // Logarithms & exponential. `ln` is natural; `log(x, base)` is the arbitrary-base form;
        // `log2`/`log10` use the dedicated (more precise) instructions.
        "ln" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.ln()))
        }
        "log" => {
            want_arity(func, args, 2)?;
            let x = want_float(func, args, 0)?;
            let base = want_float(func, args, 1)?;
            Ok(Output::Float(x.log(base)))
        }
        "log2" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.log2()))
        }
        "log10" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.log10()))
        }
        "exp" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.exp()))
        }
        // `hypot(a, b)` — `sqrt(a² + b²)` without intermediate overflow.
        "hypot" => {
            want_arity(func, args, 2)?;
            let a = want_float(func, args, 0)?;
            let b = want_float(func, args, 1)?;
            Ok(Output::Float(a.hypot(b)))
        }
        // Hyperbolics — real-valued, always a float.
        "sinh" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.sinh()))
        }
        "cosh" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.cosh()))
        }
        "tanh" => {
            want_arity(func, args, 1)?;
            Ok(Output::Float(want_float(func, args, 0)?.tanh()))
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
    fn trig_yields_float() {
        assert_eq!(done("sin", &[Arg::Float(0.0)]), Output::Float(0.0));
        assert_eq!(done("cos", &[Arg::Float(0.0)]), Output::Float(1.0));
        assert_eq!(done("tan", &[Arg::Float(0.0)]), Output::Float(0.0));
        // Ints widen to radians.
        assert_eq!(done("sin", &[Arg::Int(0)]), Output::Float(0.0));
    }

    #[test]
    fn transcendentals_yield_float() {
        assert_eq!(
            done("ln", &[Arg::Float(std::f64::consts::E)]),
            Output::Float(1.0)
        );
        assert_eq!(
            done("log", &[Arg::Float(8.0), Arg::Float(2.0)]),
            Output::Float(3.0)
        );
        assert_eq!(done("log2", &[Arg::Float(8.0)]), Output::Float(3.0));
        assert_eq!(done("log10", &[Arg::Float(1000.0)]), Output::Float(3.0));
        assert_eq!(done("exp", &[Arg::Float(0.0)]), Output::Float(1.0));
        assert_eq!(
            done("hypot", &[Arg::Float(3.0), Arg::Float(4.0)]),
            Output::Float(5.0)
        );
        assert_eq!(done("asin", &[Arg::Float(0.0)]), Output::Float(0.0));
        assert_eq!(done("acos", &[Arg::Float(1.0)]), Output::Float(0.0));
        assert_eq!(done("atan", &[Arg::Float(0.0)]), Output::Float(0.0));
        assert_eq!(
            done("atan2", &[Arg::Float(1.0), Arg::Float(1.0)]),
            Output::Float(std::f64::consts::FRAC_PI_4)
        );
        assert_eq!(done("sinh", &[Arg::Float(0.0)]), Output::Float(0.0));
        assert_eq!(done("cosh", &[Arg::Float(0.0)]), Output::Float(1.0));
        assert_eq!(done("tanh", &[Arg::Float(0.0)]), Output::Float(0.0));
        // Ints widen, as everywhere in the module.
        assert_eq!(done("exp", &[Arg::Int(0)]), Output::Float(1.0));
    }

    #[test]
    fn out_of_domain_yields_nan_like_sqrt() {
        for (func, args) in [
            ("ln", vec![Arg::Float(-1.0)]),
            ("asin", vec![Arg::Float(2.0)]),
            ("acos", vec![Arg::Float(-2.0)]),
        ] {
            match done(func, &args) {
                Output::Float(f) => assert!(f.is_nan(), "{func} should yield NaN"),
                other => panic!("expected Float, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_function_falls_through() {
        assert_eq!(call("cbrt", &[Arg::Float(1.0)]), Dispatch::Unknown);
    }
}
