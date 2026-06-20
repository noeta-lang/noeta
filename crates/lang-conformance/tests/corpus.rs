//! Runs the whole conformance corpus under `cargo test`, so the executable spec is a
//! CI gate and not only reachable via `lang test`. Mirrors what `lang test` does.

use std::path::PathBuf;

use lang_conformance::{Stage, run_corpus};

fn corpus_root() -> PathBuf {
    // crates/lang-conformance → workspace root → tests/conformance
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance")
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
