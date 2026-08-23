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
//! **The computing doors are walked differently, because they answer a different question.** A
//! [`WidthDisclosure::Compute`] door — a numeric fold, `checked_sum`, a bulk array op — derives its
//! answer from the elements *as numbers*, at the element's own width, and no hint can state a width
//! below 64. Those probes run the same door twice over the same numbers, once on a **packed** list
//! and once on the **boxed** twin `.map(fn(x) => x)` produces, and pin the FULL output exactly. The
//! packed side reads its width off its buffer's schema and is the reference; the boxed side is the
//! representation that carries nothing but the erased words. A `-1` tripwire would be useless here —
//! a boxed `[200u8, 100u8].sum()` answering `300` instead of `44` looks entirely plausible — which
//! is why these assert the whole answer rather than the absence of a tell.
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
    NAME_DISPATCHED_LIST_METHODS, of_bool_reduce, of_iter_method, of_list_method_name,
    of_num_reduce,
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

/// A door's probe: a program, and what its output must be. A disclosing door checks `want` as a
/// substring (the value has to read back whole, wherever in the output it lands); a **computing**
/// door checks it as the entire output, because a wrong width produces a plausible-looking number
/// rather than a tell.
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

/// The **boxed** twin of the list `expr` names: `map` with the identity changes nothing about the
/// values and everything about the representation, which is exactly the contrast a computing door
/// has to survive.
fn boxed(expr: &str) -> String {
    format!("{expr}.map(fn(x) => x)")
}

/// The two `u8` values every computing probe runs, and the packed list holding them. `200 + 100`
/// leaves the range of a `u8` and `200 * 100` leaves it several times over, so every fold below
/// answers differently at 8 bits than at 64 — a probe whose numbers were small would agree either
/// way and prove nothing.
const NARROW: &str = "xs: List<u8> = [200u8, 100u8]";

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
        // The fold wraps at the ELEMENT width, and a `map` adapter is where the runtime has nothing
        // left to trace: the backing buffer is gone and only the terminal's `Iterator<u8>` receiver
        // still names the width. The 64-bit line beside it is the other end of the same channel.
        IterMethod::Sum => p(
            format!(
                "{NARROW}\necho xs.iter().sum()\necho xs.iter().map(fn(x) => x).sum()\n\
                 ys: List<u64> = [{BIG}, 2u64]\necho ys.iter().sum()"
            ),
            "44\n44\n1",
        ),
        IterMethod::Product => p(
            format!(
                "{NARROW}\necho xs.iter().product()\necho xs.iter().map(fn(x) => x).product()\n\
                 ys: List<u64> = [{BIG}, 1u64]\necho ys.iter().product()"
            ),
            format!("32\n32\n{BIG_D}"),
        ),
        // Reporting instead of wrapping: `200 + 100` leaves a `u8`, and `u64::MAX + 2` leaves a
        // `u64`, while the same words read signed (`-1 + 2`) overflow nothing.
        IterMethod::CheckedSum => p(
            format!(
                "{NARROW}\necho xs.iter().checked_sum()\n\
                 echo xs.iter().map(fn(x) => x).checked_sum()\n\
                 zs: List<u8> = [200u8, 55u8]\necho zs.iter().map(fn(x) => x).checked_sum()\n\
                 ys: List<u64> = [{BIG}, 2u64]\necho ys.iter().checked_sum()"
            ),
            "none\nnone\nsome(255)\nnone",
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

/// The probe for each **name-dispatched** `List<T>` method — the rows of
/// [`NAME_DISPATCHED_LIST_METHODS`], which no enum reaches and which therefore had a classification
/// and nothing walking through it.
///
/// Matched on the name rather than on a variant, so this cannot be exhaustive the way its five
/// siblings are; the completeness that replaces the compiler's is the `_ =>` arm, which refuses an
/// unknown name outright. Adding a row to the table without a probe here fails at that arm, and
/// adding one classified [`WidthDisclosure::Order`] or `Display` with `None` fails in [`walk`].
fn name_dispatched_probe(name: &str) -> Option<Probe> {
    let us = format!("us: List<u64> = [{BIG}, {MID}, 1u64]");
    match name {
        // The overflow-reporting fold, in its EAGER spelling — the one the enums cannot reach.
        // `200u8 + 100u8` leaves the range of a `u8` where the same words at 64 bits carry it
        // whole, and `u64::MAX + 2` wraps past zero where the same words read signed are `-1 + 2`
        // and overflow nothing. Both boundaries, because getting one right is not getting the
        // other right.
        "checked_sum" => p(
            format!(
                "{NARROW}\necho xs.checked_sum()\necho {}.checked_sum()\n\
                 zs: List<u8> = [200u8, 55u8]\necho {}.checked_sum()\n\
                 ys: List<u64> = [{BIG}, 2u64]\necho ys.checked_sum()",
                boxed("xs"),
                boxed("zs")
            ),
            "none\nnone\nsome(255)\nnone",
        ),
        // `scale` multiplies and `neg` negates, and both wrap at the element width: `200u8 * 3` is
        // `88` and `(200u8).neg()` is `56`. At 64 bits neither reading changes the bits, which is
        // why the u64 line is absent here and present for the two that compare.
        "scale" => p(
            format!("{NARROW}\necho xs.scale(3)\necho {}.scale(3)", boxed("xs")),
            "[88, 44]\n[88, 44]",
        ),
        "neg" => p(
            format!("{NARROW}\necho xs.neg()\necho {}.neg()", boxed("xs")),
            "[56, 156]\n[56, 156]",
        ),
        // `abs` compares against zero AND wraps: an unsigned element is already non-negative (read
        // signed, `u64::MAX` and the bit-63 boundary are `-1` and `i64::MIN`, and `wrapping_abs`
        // folds the first to `1`), while `(-128i8).abs()` stays `-128` because 128 is not an `i8`.
        "abs" => p(
            format!(
                "ss: List<i8> = [-128i8, 100i8]\necho ss.abs()\necho {}.abs()\n\
                 {us}\necho us.abs()",
                boxed("ss")
            ),
            format!("[-128, 100]\n[-128, 100]\n[{BIG_D}, {MID_D}, 1]"),
        ),
        // Both boundaries sit far ABOVE the high bound, so both clamp down to it. Read signed they
        // are negative and clamp UP to the low bound instead — the same wrong answer for two
        // different values, which is why the control element is in the list. The narrow pair needs
        // no wrap (the result is one of three in-range inputs) and is here so the door that does
        // not move is walked beside the ones that do.
        "clamp" => p(
            format!(
                "{NARROW}\necho xs.clamp(0u8, 150u8)\necho {}.clamp(0u8, 150u8)\n\
                 {us}\necho us.clamp(0u64, 100u64)",
                boxed("xs")
            ),
            "[150, 100]\n[150, 100]\n[100, 100, 1]",
        ),
        // Classified as letting nothing through: each hands elements onward carrying their own
        // static type, and a length is not a width.
        "len" | "iter" | "enumerate" | "map" | "filter" | "to_bytes" => None,
        other => panic!(
            "`{other}` is a name-dispatched `List<T>` method with no probe. Give it a program that \
             drives a `u64` past bit 63, or a boxed narrow-width list, through it — or say here \
             that it lets nothing through."
        ),
    }
}

/// The probe for each numeric list reduction.
fn num_reduce_probe(m: NumReduce) -> Option<Probe> {
    let xs = format!("xs: List<u64> = [{BIG}, 1u64, {MID}]");
    match m {
        NumReduce::Min => p(format!("{xs}\necho xs.min()"), "some(1)"),
        NumReduce::Max => p(format!("{xs}\necho xs.max()"), format!("some({BIG_D})")),
        // The arithmetic folds wrap at the ELEMENT width. `[200u8, 100u8]` is `44` and its product
        // `32`, in both representations; at 64 bits the same fold wraps past zero.
        NumReduce::Sum => p(
            format!(
                "{NARROW}\necho xs.sum()\necho {}.sum()\n\
                 ys: List<u64> = [{BIG}, 2u64]\necho ys.sum()",
                boxed("xs")
            ),
            "44\n44\n1",
        ),
        NumReduce::Product => p(
            format!(
                "{NARROW}\necho xs.product()\necho {}.product()\n\
                 ys: List<u64> = [{BIG}, 1u64]\necho ys.product()",
                boxed("xs")
            ),
            format!("32\n32\n{BIG_D}"),
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
/// **Currently empty, and that is the point.** Every entry this census opened with —
/// `List::Join`, `Iterator::Join`, `Iterator::CheckedSum` — has been closed, so the ledger holds
/// nothing and the census asserts every disclosing door reads a `u64` whole with no exceptions.
///
/// The mechanism stays because the next gap will want it: a door that must consult the hint and
/// does not can be recorded here with a reason, visible and bounded, instead of the census being
/// weakened or switched off. It is asserted exactly in both directions — a door failing without an
/// entry is red, and an entry whose door starts passing is red and says to remove it — so the list
/// can only ever shrink.
const KNOWN_UNHINTED: &[(&str, &str)] = &[];

/// Every door the classification says needs an answer from the static type, walked: a
/// [`WidthDisclosure::Display`]/`Order`/`Identity` door with a `u64` past bit 63, a
/// [`WidthDisclosure::Compute`] door with a **boxed** narrow-width list beside its packed twin.
///
/// A door classified as needing one and not actually getting it fails here — that is the whole
/// point. The two kinds fail differently, which is why they are asserted differently. A dropped
/// *hint* leaks the erased word, so `-1` in the output is a tell worth naming on its own; a dropped
/// *width* produces `300` where the answer is `44`, which looks like an answer. So a computing door
/// is pinned to its whole output and nothing less.
#[track_caller]
fn walk(door: &str, disclosure: WidthDisclosure, probe: Option<Probe>) {
    match (disclosure, probe) {
        (WidthDisclosure::None, None) => {}
        (WidthDisclosure::None, Some(_)) => {
            panic!("{door} lets no width through but carries a probe — say which it is")
        }
        (d, None) => panic!(
            "{door} is classified {d:?} and has no probe. A door declared connected that nothing \
             walks through is exactly the shape this census exists to prevent — give it a program \
             that drives a `u64` past bit 63, or a boxed narrow-width list, through it."
        ),
        (WidthDisclosure::Compute, Some(probe)) => {
            let out = run_both(&probe.program, door);
            assert_eq!(
                out.trim_end(),
                probe.want.trim_end(),
                "{door}: the answer is computed at the ELEMENT width, and this door did not get \
                 it. The packed line reads the width off its buffer's schema; the boxed line — the \
                 `.map(fn(x) => x)` twin — carries only the erased words and needs the checker's \
                 `elem_width_sites` entry to reach the same answer.\n{}",
                probe.program
            );
        }
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

/// The name-dispatched half of the `List<T>` surface, walked with the same `u64` past bit 63.
///
/// The five tests above iterate an enum, so the compiler is what makes them complete. These methods
/// reach their implementation by name — there is no enum to iterate — so the table itself is the
/// iteration, and [`name_dispatched_probe`]'s refusal arm is what keeps a new row from arriving
/// without a program behind it. `abs` and `clamp` are the reason this exists: both were classified
/// as disclosing an order, and being reachable from no enum, both stayed classified and unwalked.
#[test]
fn every_name_dispatched_list_door_reads_a_u64_whole() {
    for &(name, disclosure) in NAME_DISPATCHED_LIST_METHODS {
        walk(
            &format!("List::{name} (name-dispatched)"),
            disclosure,
            name_dispatched_probe(name),
        );
    }
}

/// **The hole a census over enums cannot see.**
///
/// `ListMethod`/`NumReduce`/`BoolReduce` cover twenty of the checker's twenty-seven `List<T>`
/// methods. The other seven reach their implementation by NAME — `is_bulk_method`, and
/// `checked_sum`'s own special case — so no exhaustive match forces them to be classified, and two
/// of them (`abs`, `clamp`) read the erased word with nothing anywhere to say so.
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
         implementation by name, add a row to `NAME_DISPATCHED_LIST_METHODS` and a probe to \
         `name_dispatched_probe`. A method that COMPARES (against zero, against a bound, against \
         another element) is where this family's bugs live — `abs` and `clamp` both compare, and \
         neither was reachable from anything that would have said so."
    );
}
