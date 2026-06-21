//! Runs the whole conformance corpus under `cargo test`, so the executable spec is a
//! CI gate and not only reachable via `lang test`. Mirrors what `lang test` does.

use std::path::PathBuf;

use lang_conformance::{Stage, run_corpus};

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
