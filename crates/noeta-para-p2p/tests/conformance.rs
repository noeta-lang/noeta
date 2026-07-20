//! The `para.p2p` conformance harness — the executable spec for the extracted p2p/local-first
//! stack, the para-namespace twin of the main `tests/conformance` corpus.
//!
//! The `crdt`/`p2p`/`synced` fixtures moved out of the default corpus when the surface left `std`
//! (they now import `para.*`, which the default std-only registry cannot resolve). So this harness
//! installs `std ∪ ParaP2pExtension` into the process registry first, then runs the moved fixtures
//! through the same engine — the differential oracle (tree-walker ≡ VM) included, so extraction
//! preserved behavior byte-for-byte on both backends.

use std::path::PathBuf;

use noeta_conformance::{Stage, on_deep_stack, run_corpus, run_differential};

fn para_corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance")
}

/// Install `std ∪ para.p2p` into this test process's global registry exactly once. `install_with_
/// extras` panics on a double install and libtest may run tests concurrently, so all para
/// conformance runs live in one `#[test]` that installs before any harness call.
fn install_std_and_para() {
    noeta_stdlib::registry::install_with_extras(&[&noeta_para_p2p::ParaP2pExtension]);
}

#[test]
fn para_conformance_passes_and_backends_agree() {
    on_deep_stack(|| {
        install_std_and_para();
        let root = para_corpus_root();
        assert!(
            root.is_dir(),
            "para conformance corpus not found at {}",
            root.display()
        );

        // 1. Expectation spec: every fixture's `// expect:` header holds under check + eval.
        let report = run_corpus(&root, None, Stage::Eval);
        assert!(
            !report.cases.is_empty(),
            "the para conformance corpus is empty"
        );
        assert!(
            report.all_passed(),
            "para conformance failures:\n{}",
            report.to_human()
        );

        // 2. Differential oracle: the VM must match the tree-walker byte-for-byte, and compile the
        //    whole comparable subset — extraction must not have perturbed either backend.
        let diff = run_differential(&root, None);
        eprintln!("{}", diff.to_human());
        assert!(
            diff.ok(),
            "the VM diverged from the tree-walker on para fixtures:\n{}",
            diff.to_human()
        );
        assert_eq!(
            diff.not_run.unsupported,
            0,
            "the VM must compile 100% of the comparable para corpus; got:\n{}",
            diff.to_human()
        );
    });
}
