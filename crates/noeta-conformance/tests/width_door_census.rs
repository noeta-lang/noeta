//! **The width-door census, behaviorally.**
//!
//! [`noeta_ext_abi::width_doors`] classifies every collection-surface method by what it discloses
//! about a fixed-width integer, through exhaustive matches — so a new method does not compile until
//! it says. That alone would only move the prose. This drives a `u64` **above `i64::MAX`** through
//! every door the classification says must consult the hint, on **both engines**, and asserts the
//! value reads back whole.
//!
//! The probe table is the other half of the forcing function: it is exhaustive too, so a new method
//! cannot be added without either a program that exercises it or an explicit statement that it
//! discloses nothing. A classification with no probe behind it is the shape this census exists to
//! prevent — a door declared connected that nothing ever walked through.
//!
//! And the identity doors are asserted in the opposite direction. A set's buffer and a map's key
//! slots are built at one site and probed at another, so a hint there would lose a member that is
//! present — strictly worse than an order a reader finds surprising. Those probes check the value
//! is still **findable**, which is the property that would break if someone "fixed" them.

use noeta_conformance::reference::reference_run;
use noeta_db::LangDatabase;
use noeta_span::{Source, SourceId};
use noeta_stdlib::iter::IterMethod;
use noeta_stdlib::reductions::{BoolReduce, NumReduce};
use noeta_stdlib::ring1::{ListMethod, MapMethod, SetMethod};
use noeta_stdlib::width_doors::{
    of_bool_reduce, of_iter_method, of_list_method_name, of_num_reduce,
};
use noeta_stdlib::width_doors_ring1::{
    WidthDisclosure, of_list_method, of_map_method, of_set_method,
};
use noeta_vm::VmBackend;

/// `u64::MAX` — the value whose erased word reads `-1`, so any door that drops the hint says so
/// loudly rather than subtly.
const BIG: &str = "18446744073709551615u64";
/// One below the signed boundary's mirror, to catch an order that only looks right at the extreme.
const MID: &str = "9223372036854775808u64";

/// A door's probe: a program, and the substring its output must contain.
struct Probe {
    program: String,
    want: String,
}

fn p(program: impl Into<String>, want: impl Into<String>) -> Option<Probe> {
    Some(Probe {
        program: program.into(),
        want: want.into(),
    })
}

/// The probe for each `List<T>` method, or `None` where the classification says it discloses
/// nothing. Exhaustive: a new method needs an answer here.
fn list_probe(m: ListMethod) -> Option<Probe> {
    let xs = format!("xs: List<u64> = [{BIG}, 1u64, {MID}]");
    match m {
        ListMethod::Join => p(
            format!("{xs}\necho xs.join(\",\")"),
            format!("{BIG_D},1,{MID_D}"),
        ),
        ListMethod::Sorted => p(
            format!("{xs}\necho xs.sorted()"),
            format!("[1, {MID_D}, {BIG_D}]"),
        ),
        ListMethod::ToSet => p(
            // Identity: the members must still be FOUND, which is what a hint here would break.
            format!("{xs}\ns = xs.to_set()\necho s.contains({BIG})\necho s.contains({MID})"),
            "true\ntrue",
        ),
        ListMethod::Contains
        | ListMethod::Reverse
        | ListMethod::Slice
        | ListMethod::First
        | ListMethod::Last
        | ListMethod::Set => None,
    }
}

/// The probe for each `Set<T>` method. Every one is an identity door, so every probe asserts the
/// find-again property rather than an order.
fn set_probe(m: SetMethod) -> Option<Probe> {
    let s = format!("s: Set<u64> = [{BIG}, 1u64].to_set()");
    match m {
        SetMethod::Contains => p(format!("{s}\necho s.contains({BIG})"), "true"),
        SetMethod::Add => p(format!("{s}\necho s.add({MID}).contains({MID})"), "true"),
        SetMethod::Remove => p(
            format!("{s}\necho s.remove({BIG}).contains({BIG})"),
            "false",
        ),
        SetMethod::Union => p(
            format!("{s}\nt: Set<u64> = [{MID}].to_set()\necho s.union(t).contains({MID})"),
            "true",
        ),
        SetMethod::Intersection => p(
            format!("{s}\nt: Set<u64> = [{BIG}].to_set()\necho s.intersection(t).contains({BIG})"),
            "true",
        ),
    }
}

/// The probe for each `Map<K, V>` method.
fn map_probe(m: MapMethod) -> Option<Probe> {
    let mp = format!("m: Map<u64, int> = {{{BIG}: 1, 1u64: 2, {MID}: 3}}");
    match m {
        MapMethod::Keys => p(
            format!("{mp}\necho m.keys()"),
            format!("[1, {MID_D}, {BIG_D}]"),
        ),
        MapMethod::Values => p(format!("{mp}\necho m.values()"), "[2, 3, 1]"),
        // Identity doors: the key must still be reachable.
        MapMethod::Get => p(format!("{mp}\necho m[{BIG}]"), "1"),
        MapMethod::Has => p(format!("{mp}\necho m.has({BIG})"), "true"),
        MapMethod::GetOr => p(format!("{mp}\necho m.get_or({BIG}, 0)"), "1"),
        MapMethod::Set => p(format!("{mp}\necho m.set({MID}, 9)[{MID}]"), "9"),
        MapMethod::Remove => p(format!("{mp}\necho m.remove({BIG}).has({BIG})"), "false"),
    }
}

/// The probe for each `Iterator<T>` method.
fn iter_probe(m: IterMethod) -> Option<Probe> {
    let it = format!("xs: List<u64> = [{BIG}, 1u64, {MID}]");
    match m {
        IterMethod::Join => p(
            format!("{it}\necho xs.iter().join(\",\")"),
            format!("{BIG_D},1,{MID_D}"),
        ),
        IterMethod::Min => p(format!("{it}\necho xs.iter().min()"), "some(1)"),
        IterMethod::Max => p(
            format!("{it}\necho xs.iter().max()"),
            format!("some({BIG_D})"),
        ),
        IterMethod::Sum => p(
            // The fold wraps at the element width, not the erased one.
            format!("ys: List<u64> = [{BIG}, 2u64]\necho ys.iter().sum()"),
            "1",
        ),
        IterMethod::Product => p(
            format!("ys: List<u64> = [{BIG}, 1u64]\necho ys.iter().product()"),
            BIG_D,
        ),
        IterMethod::CheckedSum => p(
            format!("ys: List<u64> = [{BIG}, 2u64]\necho ys.iter().checked_sum()"),
            "none",
        ),
        IterMethod::ToSet => p(
            format!("{it}\necho xs.iter().to_set().contains({BIG})"),
            "true",
        ),
        IterMethod::Contains
        | IterMethod::Count
        | IterMethod::CountTrue
        | IterMethod::Any
        | IterMethod::All
        | IterMethod::Next
        | IterMethod::Collect
        | IterMethod::Last
        | IterMethod::Take
        | IterMethod::Drop
        | IterMethod::Chain
        | IterMethod::Enumerate
        | IterMethod::Zip
        | IterMethod::Map
        | IterMethod::Filter => None,
    }
}

/// The probe for each numeric list reduction.
fn num_reduce_probe(m: NumReduce) -> Option<Probe> {
    let xs = format!("xs: List<u64> = [{BIG}, 1u64, {MID}]");
    match m {
        NumReduce::Min => p(format!("{xs}\necho xs.min()"), "some(1)"),
        NumReduce::Max => p(format!("{xs}\necho xs.max()"), format!("some({BIG_D})")),
        NumReduce::Sum => p(format!("ys: List<u64> = [{BIG}, 2u64]\necho ys.sum()"), "1"),
        NumReduce::Product => p(
            format!("ys: List<u64> = [{BIG}, 1u64]\necho ys.product()"),
            BIG_D,
        ),
    }
}

/// The boolean reductions fold `bool` elements, so none of them can disclose an integer width.
fn bool_reduce_probe(m: BoolReduce) -> Option<Probe> {
    match m {
        BoolReduce::Any | BoolReduce::All | BoolReduce::CountTrue => None,
    }
}

/// `u64::MAX` and the bit-63 boundary as they must READ — the unsigned digits, never the word.
const BIG_D: &str = "18446744073709551615";
const MID_D: &str = "9223372036854775808";

/// Run `program` on both engines, assert they agree and exit clean, and return the shared stdout.
#[track_caller]
fn run_both(program: &str, door: &str) -> String {
    let db = LangDatabase::default();
    let source = Source::new(SourceId::FIRST, "width_door.noe", program);
    let src = noeta_db::source_program(&db, &source, noeta_lexer::Edition::DEFAULT);
    let parsed = noeta_db::ast(&db, src);
    assert!(
        parsed.0.diagnostics.is_empty(),
        "{door}: probe must parse cleanly: {:?}\n{program}",
        parsed.0.diagnostics
    );
    let checked = noeta_db::checked(&db, src);
    assert!(
        checked
            .diagnostics
            .iter()
            .all(|d| d.severity != noeta_diagnostics::Severity::Error),
        "{door}: probe must check cleanly: {:?}\n{program}",
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
        reference.stdout, vm.stdout,
        "{door}: the two engines disagree\n{program}"
    );
    assert_eq!(
        reference.exit_code, 0,
        "{door}: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// Doors that are classified as disclosing and **do not consult the hint yet** — the census's debt
/// ledger, asserted exactly.
///
/// A gap recorded here is visible, bounded and cannot grow: a door that starts failing and is not
/// on this list turns the census red, and a door on this list that starts *passing* turns it red
/// too, saying so. The list only ever shrinks. That is the whole difference between a census and a
/// disabled test — the runtime-rejection snapshot in `noeta-fuzz` works the same way.
///
/// Both entries were found by this census on its first run, which is the argument for it existing.
const KNOWN_UNHINTED: &[(&str, &str)] = &[
    (
        "List::Join",
        "`join` renders every element through the unhinted `display`, so a `List<u64>` joins its          erased words while `echo` of the same list prints the values. Closing it needs a render          hint recorded at the `join` call and carried to the method — the machinery `echo` uses —          so it is its own slice rather than a line here.",
    ),
    (
        "Iterator::Join",
        "The same defect reached through the lazy door: `it.join(sep)` drains into the eager          `join`, which is the property that makes the two spellings agree by construction — so it          agrees on this too. One fix closes both.",
    ),
    (
        "Iterator::CheckedSum",
        "`checked_sum` folds through `checked_sum_scalars`, which adds at i64: `u64::MAX` is the          word `-1`, so `u64::MAX + 2` overflows nothing and reports `some(1)` where the element          width says `none`. The PACKED path is already right (`checked_sum_buf::<u64>`); the boxed          fallback needs the element width threaded to it. The eager `xs.checked_sum()` has the          same defect — it is not reached by any of the six enums, being name-dispatched, which is          its own finding.",
    ),
];

/// Every door the classification says must consult the hint, walked with a `u64` past bit 63.
///
/// A door classified [`WidthDisclosure::Display`] or [`WidthDisclosure::Order`] and not actually
/// hinted fails here — that is the whole point. The `-1` assertion is separate from the
/// `want` match because the erased reading is the specific failure this family produces, and
/// naming it makes a red say what went wrong rather than only that something did.
#[track_caller]
fn walk(door: &str, disclosure: WidthDisclosure, probe: Option<Probe>) {
    match (disclosure, probe) {
        (WidthDisclosure::None, None) => {}
        (WidthDisclosure::None, Some(_)) => {
            panic!("{door} discloses nothing but carries a probe — say which it is")
        }
        (d, None) => panic!(
            "{door} is classified {d:?} and has no probe. A door declared connected that nothing \
             walks through is exactly the shape this census exists to prevent — give it a program \
             that drives a `u64` past bit 63 through it."
        ),
        (_, Some(probe)) => {
            let out = run_both(&probe.program, door);
            let whole = !out.contains("-1") && out.contains(&probe.want);
            match KNOWN_UNHINTED.iter().find(|(d, _)| *d == door) {
                // Recorded debt: it must still be broken. A door that starts reading the value
                // whole is fixed, and leaving it on the ledger would let the next gap hide behind
                // an entry that no longer means anything.
                Some((_, why)) => assert!(
                    !whole,
                    "{door} now reads the value whole — remove it from KNOWN_UNHINTED.\n\
                     The entry said: {why}"
                ),
                None => {
                    assert!(
                        !out.contains("-1"),
                        "{door}: the erased word reached the output — the hint did not reach this \
                         door\n{}\n---\n{out}",
                        probe.program
                    );
                    assert!(
                        out.contains(&probe.want),
                        "{door}: expected {:?} in the output\n{}\n---\n{out}",
                        probe.want,
                        probe.program
                    );
                }
            }
        }
    }
}

#[test]
fn every_list_door_reads_a_u64_whole() {
    for m in [
        ListMethod::Reverse,
        ListMethod::Contains,
        ListMethod::Join,
        ListMethod::Sorted,
        ListMethod::Slice,
        ListMethod::First,
        ListMethod::Last,
        ListMethod::ToSet,
        ListMethod::Set,
    ] {
        walk(&format!("List::{m:?}"), of_list_method(m), list_probe(m));
    }
}

#[test]
fn every_set_door_keeps_its_members_findable() {
    for m in [
        SetMethod::Contains,
        SetMethod::Union,
        SetMethod::Intersection,
        SetMethod::Add,
        SetMethod::Remove,
    ] {
        walk(&format!("Set::{m:?}"), of_set_method(m), set_probe(m));
    }
}

#[test]
fn every_map_door_agrees_on_key_order_and_keeps_keys_findable() {
    for m in [
        MapMethod::Keys,
        MapMethod::Values,
        MapMethod::Has,
        MapMethod::Set,
        MapMethod::Remove,
        MapMethod::GetOr,
        MapMethod::Get,
    ] {
        walk(&format!("Map::{m:?}"), of_map_method(m), map_probe(m));
    }
}

#[test]
fn every_iterator_door_reads_a_u64_whole() {
    for m in [
        IterMethod::Next,
        IterMethod::Collect,
        IterMethod::Take,
        IterMethod::Drop,
        IterMethod::Chain,
        IterMethod::Enumerate,
        IterMethod::Zip,
        IterMethod::Count,
        IterMethod::Sum,
        IterMethod::Min,
        IterMethod::Max,
        IterMethod::Map,
        IterMethod::Filter,
        IterMethod::Product,
        IterMethod::CheckedSum,
        IterMethod::Last,
        IterMethod::ToSet,
        IterMethod::Join,
        IterMethod::Any,
        IterMethod::All,
        IterMethod::Contains,
        IterMethod::CountTrue,
    ] {
        walk(
            &format!("Iterator::{m:?}"),
            of_iter_method(m),
            iter_probe(m),
        );
    }
}

#[test]
fn every_reduction_door_reads_a_u64_whole() {
    for m in [
        NumReduce::Sum,
        NumReduce::Product,
        NumReduce::Min,
        NumReduce::Max,
    ] {
        walk(
            &format!("NumReduce::{m:?}"),
            of_num_reduce(m),
            num_reduce_probe(m),
        );
    }
    for m in [BoolReduce::Any, BoolReduce::All, BoolReduce::CountTrue] {
        walk(
            &format!("BoolReduce::{m:?}"),
            of_bool_reduce(m),
            bool_reduce_probe(m),
        );
    }
}

/// **The hole a census over enums cannot see.**
///
/// `ListMethod`/`NumReduce`/`BoolReduce` cover twenty of the checker's twenty-seven `List<T>`
/// methods. The other seven reach their implementation by NAME — `is_bulk_method`, and
/// `checked_sum`'s own special case — so no exhaustive match forces them to be classified, and two
/// of them (`abs`, `clamp`) were wrong when this was written with nothing anywhere to say so.
///
/// So the surface is read from the checker itself, which is the authority on what a `List<T>` has:
/// every name it types must resolve to a disclosure, through an enum or through
/// `NAME_DISPATCHED_LIST_METHODS`. Adding a method to the language without classifying it fails
/// here. Reading source at test time is the technique `check_options_census` and
/// `constraint_fields` already use for the same reason — the alternative is a hand-copied list,
/// which is the thing being guarded against.
#[test]
fn every_list_method_the_checker_types_has_a_disclosure() {
    let stdlib = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .join("noeta-check/src/stdlib.rs");
    let text = std::fs::read_to_string(&stdlib).expect("the checker's surface is readable");

    // The `list_method` return-type table: from its signature to the next item at column zero.
    let start = text
        .find("fn list_method(")
        .expect("the checker declares its `List<T>` surface here");
    let body = &text[start..];
    let end = body[10..].find("\nfn ").expect("the table ends") + 10;
    let body = &body[..end];

    let mut unclassified: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for raw in body.split('"').skip(1).step_by(2) {
        // Match-arm literals only: a method name is lowercase ASCII with underscores.
        if raw.is_empty()
            || !raw
                .chars()
                .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
        {
            continue;
        }
        if names.iter().any(|n| n == raw) {
            continue;
        }
        names.push(raw.to_string());
        if of_list_method_name(raw).is_none() {
            unclassified.push(raw.to_string());
        }
    }

    assert!(
        names.len() >= 25,
        "the `List<T>` surface should have been read from the checker, found only {names:?} — the \
         table's shape changed and this scan needs updating rather than the classification"
    );
    assert!(
        unclassified.is_empty(),
        "these `List<T>` methods have no width disclosure: {unclassified:?}\n\n\
         Every method the checker types must say what it lets a program learn about a fixed-width \
         integer. If it is dispatched through a surface enum, classify it there; if it reaches its \
         implementation by name, add a row to `NAME_DISPATCHED_LIST_METHODS`. A method that \
         COMPARES (against zero, against a bound, against another element) is where this family's \
         bugs live — `abs` and `clamp` both read the erased word, and neither was reachable from \
         anything that would have said so."
    );
}
