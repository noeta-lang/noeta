//! The standing corpus property harness.
//!
//! Over the whole `.noe` corpus (`tests/`, `examples/`) the formatter must, for **every** file,
//! either produce output or decline with a *declared* [`FmtError`] — it may never panic — and every
//! file it does format must be **idempotent** (`format(format(x)) == format(x)`). The safety gate
//! (output re-parses to the same AST modulo spans) is enforced inside `format_source`, so a passing
//! `Ok` is already safe.
//!
//! In F0 the printer covers a tiny subset, so most files land in `Unsupported`; that count shrinks to
//! zero as F3 completes. The harness prints a coverage summary each run so progress is visible. It is
//! deliberately non-flaky: it asserts only invariants that must hold at every slice, not a coverage
//! floor that would fail early slices.

use std::path::{Path, PathBuf};

use noeta_fmt::{FmtConfig, FmtError, format_source};

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
    let config = FmtConfig::default();
    let files = corpus_files();
    assert!(!files.is_empty(), "found no corpus files — wrong root?");

    let (mut ok, mut parse_err) = (0u32, 0u32);

    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let name = path.to_string_lossy();
        match format_source(&name, &text, &config) {
            Ok(once) => {
                ok += 1;
                // Idempotency: formatting the output again must be a fixed point.
                let twice = format_source(&name, &once, &config)
                    .unwrap_or_else(|e| panic!("{name}: re-format failed: {e}"));
                assert_eq!(once, twice, "{name}: formatting is not idempotent");
            }
            // Intentional error-case corpus files do not parse; the formatter declines them.
            Err(FmtError::Parse(_)) => parse_err += 1,
            // A safety failure is always a printer bug — surface it loudly.
            Err(FmtError::Safety(why)) => panic!("{name}: SAFETY GATE tripped: {why}"),
        }
    }

    eprintln!(
        "fmt corpus: {} files | ok+idempotent {ok} | parse-err {parse_err}",
        files.len()
    );
    // The printer is total over parseable programs: every non-error file must format.
    assert!(ok > 500, "expected most corpus files to format, got {ok}");
}
