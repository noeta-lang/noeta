//! Runs the whole conformance corpus under `cargo test`, so the executable spec is a
//! CI gate and not only reachable via `lang test`. Mirrors what `lang test` does.

use std::path::PathBuf;

use lang_conformance::{Stage, run_corpus, run_differential};

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
    // M1.0 established the spine on the smallest subset; each slice raises this floor until it
    // reaches 100%. M1.4 added the object model — records/classes/enums on shapes, member
    // access, methods, structural update, and opaque `use` stubs. Guard against collapse.
    assert!(
        report.supported() >= 24,
        "expected the VM to compile at least the M1.4 subset, got:\n{}",
        report.to_human()
    );
}
