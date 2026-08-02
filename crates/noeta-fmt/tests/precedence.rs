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

// --- an arrow closure's body is greedy, so it is not self-delimiting ---
//
// Found by `noeta-fuzz`, which generates parenthesized closures in operand positions the corpus
// never contained. The printer's table filed every `Expr::Closure` under "atoms and self-delimiting
// forms never need parentheses as an operand" — true of `fn(x) { … }`, false of `fn(x) => …`, whose
// body runs to the end of the expression.

/// The regression. Without the parentheses this reads back as `fn() => (1 + 3)` — a closure with a
/// different body, not an addition — so they are load-bearing.
#[test]
fn an_arrow_closure_operand_keeps_its_parentheses() {
    preserved("acc = (fn() => 1) + 3\necho acc\n");
}

/// The same shape as a receiver: `.` binds at 14, so dropping these would make `1.len()` the
/// closure's body.
#[test]
fn an_arrow_closure_receiver_keeps_its_parentheses() {
    preserved("acc = (fn(x) => x).len()\necho acc\n");
}

/// A **block**-bodied closure genuinely is self-delimiting — it ends at its `}` — so it must not be
/// parenthesized. This is the counter-case that makes the fix a fix rather than a blanket
/// parenthesization of every closure.
#[test]
fn a_block_closure_operand_is_not_parenthesized() {
    preserved("acc = fn(x) {\n    return 1\n} + 3\necho acc\n");
}

/// Nor is a closure parenthesized where it is not an operand at all — a binding's right-hand side
/// and a call argument are both positions where nothing can follow the body.
#[test]
fn a_closure_outside_an_operand_position_is_not_parenthesized() {
    preserved("acc = fn(x) => x + 1\necho acc\n");
    preserved("xs = [1, 2]\nacc = xs.map(fn(n) => n * 2)\necho acc\n");
}

/// The one place the fix parenthesizes more than strictly necessary: as the *last* operand of a
/// pipeline nothing follows the closure, so the parentheses are redundant — but "is anything to my
/// right?" is not decidable from binding power alone, and the alternative (a precedence high enough
/// to drop them here) would drop them on the *left* of `??` too, where they hold the parse together.
/// Pinned so the behavior is a decision on record rather than a surprise.
#[test]
fn a_piped_closure_is_conservatively_parenthesized() {
    formats_to(
        "xs = [1, 2]\nacc = xs |> fn(n) => n\necho acc\n",
        "xs = [1, 2]\nacc = xs |> (fn(n) => n)\necho acc\n",
    );
}
