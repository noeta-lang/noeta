//! **The comparator-arm census: the checker's union rule against the runtime comparator, in lockstep.**
//!
//! A union orders when every member orders and every *pairing* of two members is one the runtime
//! comparator can answer. The second half is [`noeta_types::Type::comparator_answers`], a
//! restatement in the type lattice of what `noeta_value::ops::compare_primitive` actually does — and
//! a restatement is a second spelling of one rule, which is the drift this repo has a bug class for.
//!
//! So the table below is not consulted; it is **driven**. Every ordered pair of representative types
//! is put through `.compare()` on a `dyn` receiver — the one door that reaches the comparator with
//! the static refusal out of the way — on **both engines**, and the answer is compared against what
//! the lattice predicate claims. A disagreement fails here, in either direction, and both directions
//! matter for a different reason:
//!
//!   * **Comparator answers, predicate says no** — a union that would work is refused. `number` was
//!     exactly this: `int | float | f32 | … | u64` ordered fine at run time and could not be sorted,
//!     compared or passed to a `Comparable` bound.
//!   * **Predicate says yes, comparator refuses** — worse. The union checks clean and aborts at the
//!     first pairing that meets, which is the failure the union rule exists to prevent.
//!
//! The row set is exhaustive over [`ComparatorArm`] (asserted), and includes the kinds that order
//! through *no* arm — `bytes`, a list, a map, a set, a tuple, an unordered native type, a struct
//! with no derive — because "has no arm" is as much a claim as "has one".

use std::collections::BTreeSet;

use noeta_conformance::reference::reference_run;
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_types::{ComparatorArm, Type};
use noeta_vm::VmBackend;

/// One census row: a type as the lattice spells it, and an expression producing a value of it.
struct Row {
    /// What the row is called in a failure message.
    label: &'static str,
    /// The lattice type, which is what the predicate under test is asked about.
    ty: Type,
    /// A `.noe` expression of that type, evaluated into the `dyn` probe.
    expr: &'static str,
    /// Whether this type orders **at all** — a separate question from which *pairings* have an arm,
    /// and the checker asks it separately too (`Checker::satisfies`, which reads derives and ABI
    /// declarations the type lattice cannot see). Declared here rather than computed, and validated
    /// against the comparator's own diagonal below, so it is data the census checks rather than
    /// data the census trusts.
    orders_alone: bool,
}

fn intn(signed: bool, bits: u8) -> Type {
    Type::IntN { signed, bits }
}

/// The census. Every [`ComparatorArm`] class is represented, plus the no-arm kinds.
fn rows() -> Vec<Row> {
    vec![
        // The numeric tower — the comparator's one cross-type arm, and the reason `number` orders.
        Row {
            label: "int",
            ty: Type::Int,
            expr: "1",
            orders_alone: true,
        },
        Row {
            label: "float",
            ty: Type::Float,
            expr: "2.5",
            orders_alone: true,
        },
        Row {
            label: "f32",
            ty: Type::F32,
            expr: "1.5f32",
            orders_alone: true,
        },
        Row {
            label: "f64",
            ty: Type::F64,
            expr: "3.0f64",
            orders_alone: true,
        },
        Row {
            label: "u8",
            ty: intn(false, 8),
            expr: "2u8",
            orders_alone: true,
        },
        Row {
            label: "i16",
            ty: intn(true, 16),
            expr: "3i16",
            orders_alone: true,
        },
        Row {
            label: "u64",
            ty: intn(false, 64),
            expr: "4u64",
            orders_alone: true,
        },
        // Same-type arms: two of these order only against their own type.
        Row {
            label: "string",
            ty: Type::String,
            expr: "\"a\"",
            orders_alone: true,
        },
        Row {
            label: "bool",
            ty: Type::Bool,
            expr: "true",
            orders_alone: true,
        },
        Row {
            label: "Uuid",
            ty: Type::Named("Uuid".to_string(), Vec::new()),
            expr: "id.uuid_v7()",
            orders_alone: true,
        },
        Row {
            label: "Instant",
            ty: Type::Named("Instant".to_string(), Vec::new()),
            expr: "datetime.from_unix_ms(0)",
            orders_alone: true,
        },
        Row {
            label: "Derived",
            ty: Type::Named("Derived".to_string(), Vec::new()),
            expr: "Derived { n: 1 }",
            orders_alone: true,
        },
        Row {
            label: "Other",
            ty: Type::Named("Other".to_string(), Vec::new()),
            expr: "Other { n: 1 }",
            orders_alone: true,
        },
        // The structural arm: a prelude container, ordered by variant index then payload.
        Row {
            label: "?int",
            ty: Type::Option(Box::new(Type::Int)),
            expr: "some(1)",
            orders_alone: true,
        },
        Row {
            label: "?float",
            ty: Type::Option(Box::new(Type::Float)),
            expr: "opt_float()",
            orders_alone: true,
        },
        Row {
            label: "?string",
            ty: Type::Option(Box::new(Type::String)),
            expr: "opt_string()",
            orders_alone: true,
        },
        Row {
            label: "Result<int, string>",
            ty: Type::Result(Box::new(Type::Int), Box::new(Type::String)),
            expr: "res_int()",
            orders_alone: true,
        },
        // No arm at all: nothing here orders against anything, itself included.
        Row {
            label: "bytes",
            ty: Type::Bytes,
            expr: "\"a\".to_bytes()",
            orders_alone: false,
        },
        Row {
            label: "List<int>",
            ty: Type::List(Box::new(Type::Int)),
            expr: "[1]",
            orders_alone: false,
        },
        Row {
            label: "Map<string, int>",
            ty: Type::Map(Box::new(Type::String), Box::new(Type::Int)),
            expr: "{\"a\": 1}",
            orders_alone: false,
        },
        Row {
            label: "Set<int>",
            ty: Type::Set(Box::new(Type::Int)),
            expr: "[1].to_set()",
            orders_alone: false,
        },
        Row {
            label: "(int, int)",
            ty: Type::Tuple(vec![Type::Int, Type::Int]),
            expr: "(1, 2)",
            orders_alone: false,
        },
        Row {
            label: "Duration",
            ty: Type::Named("Duration".to_string(), Vec::new()),
            expr: "datetime.from_unix_ms(0).diff(datetime.from_unix_ms(1))",
            orders_alone: false,
        },
        Row {
            label: "Plain",
            ty: Type::Named("Plain".to_string(), Vec::new()),
            expr: "Plain { n: 1 }",
            orders_alone: false,
        },
    ]
}

/// The declarations every probe shares. `cmp` takes `dyn` on both sides deliberately: the static
/// ordering rule is what is under test, so the probe must not be gated by it — a `dyn` receiver is
/// documented to defer to the runtime, which is the comparator this census is reading.
const PRELUDE: &str = "use std.{id, datetime}\n\
     @derive(Comparable)\n\
     struct Derived { n: int }\n\
     @derive(Comparable)\n\
     struct Other { n: int }\n\
     struct Plain { n: int }\n\
     fn opt_float(): ?float { return some(2.5) }\n\
     fn opt_string(): ?string { return some(\"a\") }\n\
     fn res_int(): Result<int, string> { return Ok(1) }\n\
     fn cmp(a: dyn, b: dyn): Ordering { return a.compare(b) }\n";

/// Whether the **runtime** comparator answers this pairing, agreed by both engines.
///
/// A pairing with no arm aborts, which is a non-zero exit and an `E0007` — not a panic, so the
/// probe reads the exit code rather than catching anything.
#[track_caller]
fn runtime_answers(left: &str, right: &str, pair: &str) -> bool {
    let program = format!("{PRELUDE}echo cmp({left}, {right})\n");
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "comparator_arm.noe", &program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "{pair}: probe must parse cleanly: {:?}\n{program}",
        parsed.0.diagnostics
    );
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked
            .diagnostics
            .iter()
            .all(|d| d.severity != noeta_diagnostics::Severity::Error),
        "{pair}: probe must CHECK cleanly — a `dyn` receiver defers, so a static error here means \
         the probe stopped reaching the comparator: {:?}\n{program}",
        checked.diagnostics
    );
    let reference = reference_run(&parsed.0.program, checked.sites.clone());
    let module = noeta_db::bytecode(&db, src)
        .0
        .as_ref()
        .expect("probe compiles")
        .clone();
    let vm = VmBackend::new().run_module(&module);
    assert_eq!(
        reference.exit_code == 0,
        vm.exit_code == 0,
        "{pair}: the two engines disagree about whether the comparator answers\n{program}"
    );
    assert_eq!(
        reference.stdout, vm.stdout,
        "{pair}: the two engines disagree\n{program}"
    );
    reference.exit_code == 0
}

/// **The census.** Every ordered pair, both directions, driven through the comparator and compared
/// against the checker's union rule.
///
/// The rule has two halves and the census asserts their conjunction, because that is what
/// `Checker::union_orders` evaluates: **every member orders alone**, and **every pairing has an
/// arm**. Keeping them apart is what makes a failure legible — `Duration` against `Duration` has an
/// arm and still does not order, and reading that as a pair-predicate bug would send the reader to
/// the wrong function.
#[test]
fn the_union_rule_matches_the_runtime_comparator() {
    let rows = rows();
    let mut disagreements = Vec::new();
    for a in &rows {
        for b in &rows {
            let pair = format!("`{}` against `{}`", a.label, b.label);
            let claimed =
                a.orders_alone && b.orders_alone && Type::comparator_answers(&a.ty, &b.ty);
            let actual = runtime_answers(a.expr, b.expr, &pair);
            if claimed != actual {
                disagreements.push(format!(
                    "{pair}: the rule says {claimed} (orders alone: {} and {}; pairing: {}), the \
                     comparator says {actual}",
                    a.orders_alone,
                    b.orders_alone,
                    Type::comparator_answers(&a.ty, &b.ty)
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "the union ordering rule and the runtime comparator have drifted apart — a `true` the \
         comparator refuses is a union that checks clean and aborts, a `false` it answers is a \
         union that works and is refused:\n{}",
        disagreements.join("\n")
    );
}

/// **`orders_alone` is the comparator's own diagonal**, not a claim the census takes on trust.
///
/// A type orders alone exactly when it can be compared against itself, so the declared column is
/// checked against the one probe that answers the question directly. Without this the column could
/// drift into whatever made the matrix pass.
#[test]
fn the_declared_orders_alone_column_is_the_comparators_diagonal() {
    for row in rows() {
        let pair = format!("`{}` against itself", row.label);
        assert_eq!(
            row.orders_alone,
            runtime_answers(row.expr, row.expr, &pair),
            "{pair}: the declared `orders_alone` disagrees with the comparator"
        );
    }
}

/// **Every arm class is represented, and the no-arm kinds are too.**
///
/// The match is exhaustive, so a new [`ComparatorArm`] variant does not compile until it is named
/// here — and naming it without adding a row fails, which is the shape that keeps a class from being
/// declared and never walked.
#[test]
fn the_census_covers_every_arm_class() {
    let rows = rows();
    let seen: BTreeSet<&'static str> = rows
        .iter()
        .map(|r| match r.ty.comparator_arm() {
            Some(ComparatorArm::Numeric) => "numeric",
            Some(ComparatorArm::SameType) => "same-type",
            Some(ComparatorArm::Structural) => "structural",
            None => "no-arm",
        })
        .collect();
    let want: BTreeSet<&'static str> = ["numeric", "same-type", "structural", "no-arm"]
        .into_iter()
        .collect();
    assert_eq!(
        seen, want,
        "every comparator-arm class needs at least one census row — a class with no row behind it \
         is a claim nothing checks"
    );
}

/// **Two instantiations of one generic nominal type are refused as a pair, deliberately.**
///
/// The runtime answers this one: `Box<int>` against `Box<float>` compares field-wise and returns an
/// ordering, so the census's equality would flag it. It is excluded from the row set and pinned here
/// instead, because the reason is not something the type lattice can see — a `Box<T>` may carry a
/// hand-written `impl Comparable` whose `compare` declares a `Box<T>` parameter, and a `Box<float>`
/// arriving at `T = int` breaks the signature its author wrote. So the pairing stays refused.
///
/// Written as an assertion rather than a comment so that teaching the checker to descend into a
/// generic's arguments has to come here and say so.
#[test]
fn two_instantiations_of_one_generic_are_conservatively_unpaired() {
    let box_int = Type::Named("Box".to_string(), vec![Type::Int]);
    let box_float = Type::Named("Box".to_string(), vec![Type::Float]);
    assert!(
        !Type::comparator_answers(&box_int, &box_float),
        "`Box<int>` and `Box<float>` must stay an unpaired pairing"
    );
    assert!(
        Type::comparator_answers(&box_int, &box_int),
        "one instantiation against itself is the ordinary same-type arm"
    );
}
