//! **The compile-determinism gate**: the same program compiles to the same bytes in a *different
//! process*.
//!
//! # Why this test re-executes itself
//!
//! The bug class it exists to catch is "a `HashSet`/`HashMap` iterated into a serialized table".
//! Rust's default hasher is seeded randomly, but the seed is fixed for the life of a thread — so a
//! test that compiles the same program twice on one thread gets the same wrong order twice and
//! passes. Even compiling on two threads only *sometimes* differs, and would make the gate flaky
//! rather than sound.
//!
//! The only arrangement that reliably observes the divergence is two **processes**. libtest gives
//! us no `main` to intercept, so the test spawns its own binary
//! (`current_exe --exact <this test> --nocapture`) with [`CHILD_ENV`] set; the child sees the
//! variable, prints one `name\tdigest` line per compiled corpus program, and exits. The parent
//! compiles the same corpus itself and compares. Two processes, two hasher seeds, one expected
//! answer.
//!
//! Spawning also defeats every in-process cache (salsa, the startup bytecode cache) for free: the
//! child shares nothing with the parent but the corpus on disk.
//!
//! # Verifying the gate can fail
//!
//! A determinism gate that cannot fail is worse than none. To check it by hand, reintroduce the
//! bug — make any table that reaches [`noeta_bytecode::Module`] collect from a `HashSet` without
//! sorting (`Module::destruct_reachable` was the historical one) — and this test must fail with a
//! long list of diverging cases. It was verified that way when it was written: before the fix,
//! 1046 of 1046 compiled corpus programs diverged; after, zero.

use std::path::PathBuf;
use std::process::Command;

use noeta_conformance::{DeterminismReport, digest_corpus, on_deep_stack};

/// Set on the re-executed child. Its value is unused — presence is the whole protocol.
const CHILD_ENV: &str = "NOETA_DETERMINISM_CHILD";

/// The libtest filter that selects exactly the test below, for the child invocation. Kept next to
/// the function so a rename that misses one is a fast, obvious failure ("child produced no
/// digests") rather than a silently vacuous gate.
const TEST_NAME: &str = "compiled_modules_are_byte_identical_across_processes";

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance")
}

#[test]
fn compiled_modules_are_byte_identical_across_processes() {
    on_deep_stack(|| {
        let root = corpus_root();
        assert!(
            root.is_dir(),
            "conformance corpus not found at {}",
            root.display()
        );

        // The child arm: compile, print, done. No assertions here — the parent owns the verdict.
        if std::env::var_os(CHILD_ENV).is_some() {
            print!("{}", digest_corpus(&root).to_wire());
            return;
        }

        let exe = std::env::current_exe().expect("test binary path");
        let output = Command::new(&exe)
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("re-exec the test binary as the second process");
        assert!(
            output.status.success(),
            "the child process failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let theirs = DeterminismReport::from_wire(&String::from_utf8_lossy(&output.stdout));

        let mine = digest_corpus(&root);
        assert!(
            !mine.digests.is_empty(),
            "no corpus program compiled — the gate would be vacuous"
        );
        assert_eq!(
            theirs.digests.len(),
            mine.digests.len(),
            "the child reported {} digests, this process compiled {} — the child arm did not run \
             the same corpus (is `TEST_NAME` still the name of this test?)",
            theirs.digests.len(),
            mine.digests.len()
        );

        let diverged = mine.diff(&theirs);
        assert!(
            diverged.is_empty(),
            "compilation is not deterministic: {} of {} corpus programs compiled to different \
             bytes in a second process. A `HashSet`/`HashMap` iteration order is reaching the \
             serialized module.\nfirst 20:\n{}",
            diverged.len(),
            mine.digests.len(),
            diverged
                .iter()
                .take(20)
                .map(|(name, a, b)| format!("  {name}: {a} vs {b}\n"))
                .collect::<String>()
        );
    });
}
