//! **Precedence parentheses.** Parentheses are not in the AST, so the printer re-derives them from
//! a binding-power table that must mirror the parser's Pratt table exactly. Where the two disagree,
//! the printer emits text that parses back to a *different* program.
//!
//! The corpus harness cannot localize this class. Its safety gate does catch it — `format_source`
//! re-parses its own output and refuses to return text whose AST differs — but the symptom is a
//! whole file declining to format with `internal safety check failed`, pointing at no construct in
//! particular. These tests name the construct.
//!
//! The case that prompted the file: `is` is postfix in *shape*, so the printer's table filed it with
//! call/member/index at binding power 14 ("never needs parenthesizing as an operand"). The parser
//! registers it at the **comparison** tier, bp 5. So `!(d is int)` printed as `!d is int`, which
//! reads back as `(!d) is int`, and every file containing `if !(x is T)` refused to format.

use noeta_fmt::{FmtConfig, format_source};

/// Formatting `src` yields exactly `src` — it is already canonically formatted, parentheses
/// included. `format_source`'s own safety gate additionally guarantees the output re-parses to the
/// same AST, so a passing assertion means the parentheses are both *necessary* and *sufficient*.
fn preserved(src: &str) {
    let out = format_source("precedence.noe", src, &FmtConfig::default()).expect("formats");
    assert_eq!(out, src, "fmt must round-trip the parenthesized form");
}

/// Formatting succeeds and produces `want`.
fn formats_to(src: &str, want: &str) {
    let out = format_source("precedence.noe", src, &FmtConfig::default()).expect("formats");
    assert_eq!(out, want);
}

// --- `x is T` binds at the comparison tier, not as a tight postfix ---

/// The regression. `!` binds tighter than `is`, so the parentheses are load-bearing: without them
/// the negation would apply to `d` and the type test to the result.
#[test]
fn negated_type_test_keeps_its_parentheses() {
    preserved("d: dyn = 42\nif !(d is int) {\n    echo \"not an int\"\n}\n");
}

/// The same shape one step further along: a type test used as a *receiver*. `.` binds at 14 and
/// `is` at 5, so dropping these parentheses would attach the member access to `int`.
#[test]
fn a_type_test_receiver_keeps_its_parentheses() {
    preserved("d: dyn = 42\nflag = (d is int).to_string()\necho flag\n");
}

/// And the counter-case, which is what makes the fix a fix rather than a blanket parenthesization:
/// `&&` binds *looser* than `is` (3 vs 5), so these parentheses are not needed and must not appear.
/// A printer that "fixed" the bug by wrapping every type test would pass the safety gate and still
/// be wrong — it would rewrite the author's code on every run.
#[test]
fn a_type_test_under_a_logical_operator_needs_no_parentheses() {
    preserved("d: dyn = 42\nif d is int && d is string {\n    echo \"never\"\n}\n");
    formats_to(
        "d: dyn = 42\nif (d is int) && (d is string) {\n    echo \"never\"\n}\n",
        "d: dyn = 42\nif d is int && d is string {\n    echo \"never\"\n}\n",
    );
}

/// An operand that binds *tighter* than `is` needs no parentheses either — `+` is 11, `is` is 5, so
/// `a + b is int` already means `(a + b) is int`. Pinned because the type test prints its own
/// operand at receiver strength, which is safe but could tempt someone into "simplifying" it.
#[test]
fn a_tighter_operand_of_a_type_test_is_not_re_parenthesized() {
    preserved("a = 1\nb = 2\nflag = (a + b) is int\necho flag\n");
}
