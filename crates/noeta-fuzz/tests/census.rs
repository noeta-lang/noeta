//! The runtime-rejection inventory cannot grow in silence.
//!
//! See [`noeta_fuzz::census`] for what this is and why it exists. In one line: the runtime's
//! static-class rejections are a finite set, each one is the question "can a checked program reach
//! this?", and the answers are worth more than another million fuzzed programs — nine defects came
//! out of asking them by hand, several of which no generated program had reached.
//!
//! This test does not assert that every reason has been *answered*. It asserts that the set has not
//! changed without someone noticing, which is what makes the next answer get asked.

use noeta_fuzz::census;

#[test]
fn the_runtime_rejection_inventory_matches_its_snapshot() {
    let found = census::reasons();
    let recorded = census::snapshot();

    let added: Vec<_> = found.difference(&recorded).collect();
    let removed: Vec<_> = recorded.difference(&found).collect();

    let mut report = String::new();
    if !added.is_empty() {
        report.push_str(
            "\nNEW static-class rejection(s) in the runtime, not in the census snapshot.\n\
             Each is a way a program can be refused at run time on grounds the checker might have \
             settled. Before recording it, answer the question the census exists to ask: can a \
             program `check_all` accepts reach this? If yes, that is a check-vs-run divergence and \
             belongs in `noeta-check`, not here.\n",
        );
        for r in &added {
            report.push_str(&format!("  + {r}\n"));
        }
    }
    if !removed.is_empty() {
        report.push_str(
            "\nRecorded reason(s) the runtime no longer has — delete them from the snapshot.\n",
        );
        for r in &removed {
            report.push_str(&format!("  - {r}\n"));
        }
    }
    assert!(
        report.is_empty(),
        "{report}\nsnapshot: {}\nto refresh after answering: \
         cargo run --release -p noeta-fuzz --example censusdump > crates/noeta-fuzz/census.txt",
        census::snapshot_path().display()
    );
}

/// The scan itself has to keep working. A refactor that renamed the error funnel, or a scanner bug
/// that silently matched nothing, would empty the inventory and leave the drift test passing
/// against an equally empty snapshot — green, and testing nothing. Same anti-vacuity discipline as
/// the parse-rate floor.
#[test]
fn the_census_scan_still_finds_the_runtime() {
    let found = census::reasons();
    assert!(
        found.len() > 60,
        "the census scan found only {} static-class rejection reasons — it found ~91 when written, \
         so either the runtime shed most of them or the scan stopped matching. An inventory that \
         silently empties makes every gate built on it vacuous.",
        found.len()
    );
    // And the two codes that carry the class must both still appear.
    for code in ["TypeMismatch", "UnknownName"] {
        assert!(
            found.iter().any(|r| r.code == code),
            "no {code} rejection found in the runtime — the scan is not reaching it"
        );
    }
}
