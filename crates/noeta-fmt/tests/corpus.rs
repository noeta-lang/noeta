//! The standing corpus property harness.
//!
//! Over the whole `.noe` corpus (`tests/`, `examples/`) the formatter must, for **every** file,
//! either produce output or decline with a *declared* [`noeta_fmt::FmtError`] — it may never panic —
//! and every file it does format must satisfy every invariant in [`noeta_fmt::oracle`]: idempotence,
//! comment completeness, and comment placement. The safety gate (output re-parses to the same AST
//! modulo spans) is enforced inside `format_source`, so a passing `Ok` is already safe.
//!
//! The invariants themselves live in [`noeta_fmt::oracle`] rather than here, because this file is
//! only one of two input sources for them: `noeta-fuzz` runs the same oracle over *generated*
//! programs. Corpus files are real code in one fixed layout each; generated programs vary layout,
//! nesting and config far beyond what any corpus contains. Sharing the oracle is what keeps the two
//! from drifting into asserting different contracts.

use std::path::{Path, PathBuf};

use noeta_fmt::{
    FmtConfig,
    oracle::{self, Verdict},
};

/// Collect every `.noe` file under the repository's source corpus.
fn corpus_files() -> Vec<PathBuf> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2) // crates/noeta-fmt -> crates -> repo root
        .expect("repo root")
        .to_path_buf();
    let mut files = Vec::new();
    for dir in ["tests", "examples"] {
        collect_noe(&repo_root.join(dir), &mut files);
    }
    files
}

fn collect_noe(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_noe(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

#[test]
fn corpus_is_safe_and_idempotent() {
    // The formatter parses each corpus file, and a deeply-nested case (a reactive `@html` LiveView
    // template) recurses past the default ~2 MiB test-thread stack and aborts the process. Sweep on a
    // 64 MiB worker, the same deep stack the eval corpus and the conformance oracle use
    // (`noeta_conformance::on_deep_stack`, matched to `noeta_parser`'s deep-parse stack).
    const DEEP_STACK: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, || {
                // Every configuration must hold every invariant: the default (source-directed),
                // width-driven wrapping, and import sorting.
                let configs = [
                    ("wrap=false", FmtConfig::default()),
                    (
                        "wrap=true",
                        FmtConfig {
                            wrap: true,
                            line_width: 80,
                            ..FmtConfig::default()
                        },
                    ),
                    (
                        "sort_imports",
                        FmtConfig {
                            sort_imports: true,
                            ..FmtConfig::default()
                        },
                    ),
                ];
                let files = corpus_files();
                assert!(!files.is_empty(), "found no corpus files — wrong root?");

                for (label, config) in configs {
                    let (mut ok, mut parse_err) = (0u32, 0u32);
                    for path in &files {
                        let Ok(text) = std::fs::read_to_string(path) else {
                            continue;
                        };
                        let name = path.to_string_lossy();
                        match oracle::check(&name, &text, &config) {
                            Ok(Verdict::Clean) => ok += 1,
                            // Intentional error-case corpus files do not parse; fmt declines them.
                            Ok(Verdict::Declined) => parse_err += 1,
                            Err(violation) => panic!("[{label}] {name}: {violation}"),
                        }
                    }
                    eprintln!(
                        "fmt corpus [{label}]: {} files | ok+idempotent {ok} | parse-err {parse_err}",
                        files.len()
                    );
                    // The printer is total over parseable programs: every non-error file must format.
                    assert!(
                        ok > 500,
                        "[{label}] expected most corpus files to format, got {ok}"
                    );
                }
            })
            .expect("spawn deep-stack fmt-corpus worker")
            .join()
            .expect("corpus worker panicked");
    });
}
