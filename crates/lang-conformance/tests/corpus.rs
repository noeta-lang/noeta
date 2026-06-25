//! Runs the whole conformance corpus under `cargo test`, so the executable spec is a
//! CI gate and not only reachable via `lang test`. Mirrors what `lang test` does.

use std::path::PathBuf;

use lang_conformance::{Stage, run_corpus, run_differential, run_leak_check};

fn corpus_root() -> PathBuf {
    // crates/lang-conformance → workspace root → tests/conformance
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance")
}

#[test]
fn demo_example_and_corpus_case_stay_in_sync() {
    // The §14 acceptance program lives in two places: `examples/orders.lang` (what
    // `lang run` executes) and `tests/conformance/demo/orders.lang` (the corpus case with
    // its `// expect:` assertions). They are byte-identical mirrors; this guards the drift.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let example = std::fs::read_to_string(workspace.join("examples/orders.lang"))
        .expect("examples/orders.lang exists");
    let corpus = std::fs::read_to_string(workspace.join("tests/conformance/demo/orders.lang"))
        .expect("tests/conformance/demo/orders.lang exists");
    assert_eq!(
        example, corpus,
        "examples/orders.lang and tests/conformance/demo/orders.lang have diverged"
    );
}

#[test]
fn conformance_corpus_passes() {
    let root = corpus_root();
    assert!(
        root.is_dir(),
        "conformance corpus not found at {}",
        root.display()
    );

    let report = run_corpus(&root, None, Stage::Eval);
    assert!(
        !report.cases.is_empty(),
        "the conformance corpus is empty — at least the walking-skeleton case should exist"
    );
    assert!(
        report.all_passed(),
        "conformance failures:\n{}",
        report.to_human()
    );
}

/// The leak oracle's **known debt**: `(backend, program)` pairs that leak at clean exit today
/// because of reference cycles no live collector reaps yet (architecture §0/Phase 6). Every entry
/// is a nested-function capture cycle:
///
/// - the tree-walker leaks a closure ↔ its child call-scope (the global drain only breaks the
///   *global* scope's cycle, not one rooted in a nested scope);
/// - the VM leaks the same cycle because its Bacon–Rajan trial-deletion collector is built but
///   **dormant** (never wired).
///
/// Phase 6 fixes both (structural `Weak` parent for eval, wiring the collector for the VM) and
/// removes the matching entries here. The oracle asserts the leak set equals *exactly* this list,
/// so a brand-new leak fails the gate AND a fixed leak forces the allowlist to shrink.
const KNOWN_LEAKS: &[(&str, &str)] = &[
    ("eval", "closures/capture_immutable_error.lang"),
    ("eval", "closures/counter_nested_fn.lang"),
    ("eval", "closures/recursive_nested_fn.lang"),
    ("vm", "closures/recursive_nested_fn.lang"),
];

#[test]
fn leak_oracle_residency_is_zero_except_known_cycles() {
    // The leak oracle (architecture §0): every program must reclaim all of its heap before it
    // returns — residency 0 at clean exit, on *both* backends. A cycle leak or missed release
    // shows up as a positive per-program residual. The only tolerated residuals are the cyclic
    // ones in `KNOWN_LEAKS` (Phase-6 debt); any other leak — or any change to the known set —
    // fails the gate.
    let report = run_leak_check(&corpus_root(), None);
    eprintln!("{}", report.to_human());

    let mut found: Vec<(&str, &str)> = report
        .leaks
        .iter()
        .map(|l| (l.backend, l.name.as_str()))
        .collect();
    found.sort_unstable();
    let mut expected: Vec<(&str, &str)> = KNOWN_LEAKS.to_vec();
    expected.sort_unstable();

    assert_eq!(
        found,
        expected,
        "the leak-oracle set changed.\n  - a NEW pair ⇒ a real leak to fix or a regression\n  \
         - a MISSING pair ⇒ a debt was fixed; remove it from KNOWN_LEAKS\nfull report:\n{}",
        report.to_human()
    );
}

#[test]
fn differential_backends_agree() {
    // The differential oracle: every program the M1 VM can compile must produce a byte-for-
    // byte identical `RunResult` to the M0 tree-walker. Programs outside the VM's current
    // subset are skipped (not failed); this is the climbing coverage gate for Thrust A.
    let report = run_differential(&corpus_root(), None);
    // Print coverage so the agent loop can watch it climb slice by slice.
    eprintln!("{}", report.to_human());
    assert!(
        report.ok(),
        "the VM diverged from the tree-walker:\n{}",
        report.to_human()
    );
    // M1.0 established the spine on the smallest subset; each slice raised this floor until
    // M1.5 reached the **Thrust-A gate**: the VM compiles 100% of the comparable corpus
    // (every parse-clean case) — match/`?`/`??`/constructors completing the feature surface.
    // The tree-walker is now frozen as the pure oracle; this floor must never regress.
    assert_eq!(
        report.skipped,
        0,
        "the VM must compile 100% of the comparable corpus (Thrust-A gate); got:\n{}",
        report.to_human()
    );
}
