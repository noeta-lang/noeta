//! Every operator against every operand shape, exhaustively.
//!
//! # Why a matrix and not more cases
//!
//! The printer's binding-power table (`prec`/`binop_prec`) exists so an operand is parenthesized
//! exactly when the parser would otherwise re-associate it. Three defects in that table have been
//! found one at a time — `x is T` filed as self-delimiting, an arrow closure filed the same way,
//! and a resugared conditional likewise — each by a user or a fuzzer hitting the one shape that
//! exposed it, and each fixed with a hand-written case beside the last one.
//!
//! Auditing the two tables settled the operator half: `binop_prec` and the parser's pratt entries
//! agree on all sixteen binary operators, `is` sits at the comparison tier in both, and prefix and
//! postfix match. The half that cannot be settled by reading is the catch-all — `_ => u8::MAX`,
//! "this form never needs parentheses as an operand" — which is where all three defects lived.
//!
//! So this enumerates it instead. Every operand shape the language has, in both operand positions
//! of every binary operator, plus the prefix and postfix contexts: 700-odd programs asserted
//! against `noeta_fmt::oracle`, whose safety gate already refuses any output that does not re-parse
//! to the same AST. A new expression form that is not self-delimiting fails here the first time
//! someone adds it to `OPERANDS`, and the table's catch-all stops being a silent default.

use noeta_fmt::FmtConfig;
use noeta_fmt::oracle::{self, Verdict};

/// Every binary operator, by its source spelling. Precedence is irrelevant here — the point is to
/// put each operand shape on both sides of each of them.
const BINARY_OPS: &[&str] = &[
    "*", "/", "%", "+", "-", "<<", ">>", "&", "^", "|", "~", "<", "<=", ">", ">=", "==", "!=",
    "===", "!==", "&&", "||", "??", "|>",
];

/// Every expression shape that can stand as an operand, written so it parses in any position.
///
/// The four at the end are the ones the table gets wrong when it gets anything wrong: a form whose
/// body runs to the end of the enclosing expression rather than closing itself.
const OPERANDS: &[&str] = &[
    // Atoms and self-delimiting forms.
    "a",
    "1",
    "1.5",
    "true",
    "\"s\"",
    "[1, 2]",
    "(1, 2)",
    "#{1}",
    "a.b",
    "a.b()",
    "a[0]",
    "a.0",
    "a?",
    "-a",
    "!a",
    "a.as<int>()",
    "fn(x) { return x }",
    "match a { _ => 1 }",
    // Operator forms — an operand that is itself an operator expression.
    "a + b",
    "a * b",
    "a == b",
    "a && b",
    "a || b",
    "a ?? b",
    "a |> b",
    "a ~ b",
    "a .. b",
    // The forms that extend rightward, and are therefore the whole reason the table exists.
    "a is int",
    "fn(x) => x",
    "if a then 1 else 2",
];

/// Assert the formatter is a fixed point on `src` under the default configuration.
///
/// `oracle::check` enforces the safety gate — output must re-parse to the same AST modulo spans —
/// plus idempotence and comment preservation, so a wrongly-parenthesized operand fails here as
/// either a `Declined` verdict (the formatter refused its own output) or a violation.
fn holds(src: &str) {
    let config = FmtConfig::default();
    match oracle::check("precedence_matrix.noe", src, &config) {
        Ok(Verdict::Clean) => {}
        Ok(Verdict::Declined) => panic!(
            "the formatter declined a well-formed program — it could not print this shape back \
             into itself:\n{src}"
        ),
        Err(violation) => panic!("{violation}\n--- source ---\n{src}"),
    }
}

#[test]
fn every_operand_shape_survives_every_binary_operator() {
    let mut checked = 0usize;
    for op in BINARY_OPS {
        for operand in OPERANDS {
            // Left and right operand positions are not symmetric: a rightward-extending form is
            // harmless on the right of an operator and re-associates on the left, and a form that
            // needs a receiver is the other way round.
            holds(&format!("x = {operand} {op} z\n"));
            holds(&format!("x = z {op} {operand}\n"));
            checked += 2;
        }
    }
    eprintln!("precedence matrix: {checked} operand/operator programs");
}

/// The prefix and postfix contexts, which re-associate differently from the infix ones: `!` takes
/// the tightest thing to its right, and a postfix chain takes the tightest thing to its left.
#[test]
fn every_operand_shape_survives_the_prefix_and_postfix_contexts() {
    for operand in OPERANDS {
        holds(&format!("x = !({operand})\n"));
        holds(&format!("x = -({operand})\n"));
        holds(&format!("x = ({operand}).f()\n"));
        holds(&format!("x = ({operand})[0]\n"));
        holds(&format!("x = ({operand}) is int\n"));
        holds(&format!("x = ({operand})?\n"));
    }
}

/// An operand nested two deep, so a table entry that is wrong only under an *enclosing* operator
/// still has somewhere to show it.
#[test]
fn every_operand_shape_survives_being_nested_twice() {
    for operand in OPERANDS {
        holds(&format!("x = ({operand} + 1) * 2\n"));
        holds(&format!("x = 2 * (1 + {operand})\n"));
        holds(&format!("x = [{operand}, 1]\n"));
        holds(&format!("x = f({operand}, 1)\n"));
    }
}
