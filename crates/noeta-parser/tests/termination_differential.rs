//! The statement-termination migration differential (audit-3 Finding 7).
//!
//! Newline termination used to be the union of two differently-parameterized algorithms: the
//! lexer's synthetic-`;` pass (`insert_terminators`, absolute bracket depth, statement-ending
//! gate) and a second offsets scan the parser peeked (`newline_terminator_offsets`, brace-relative
//! depth, no gate). The converged path is one scan — `noeta_lexer::newline_boundaries` — whose
//! hard entries the parser weaves into its input as zero-width `;` and whose offsets it uses as
//! soft terminators.
//!
//! Over the **whole `.noe` corpus** this harness proves the convergence changed nothing, twice
//! over:
//!
//! 1. **Decision equality** — the new scan's hard boundaries sit exactly where the legacy pass
//!    synthesized a `;`, and its offset set equals the legacy soft scan's over the legacy stream.
//! 2. **Parse equality** — parsing the clean stream (new pipeline: weaving + soft set) produces a
//!    structurally identical `Program` and identical diagnostics to parsing the legacy
//!    pre-terminated stream (which reproduces the old pipeline bit-for-bit: the legacy `;` are
//!    already in place, so nothing is woven, and the soft set is unchanged by zero-width tokens).
//!
//! Delete together with the legacy functions once the differential has covered a release.

use std::path::{Path, PathBuf};

use noeta_span::{Source, SourceId};

/// Every `.noe` file in the repository: the conformance suite, examples, in-crate fixtures
/// (`crates/`), and first-party packages.
fn corpus_files() -> Vec<PathBuf> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2) // crates/noeta-parser -> crates -> repo root
        .expect("repo root")
        .to_path_buf();
    let mut files = Vec::new();
    for dir in ["tests", "examples", "crates", "packages"] {
        collect_noe(&repo_root.join(dir), &mut files);
    }
    files.sort();
    files
}

fn collect_noe(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_noe(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

#[test]
fn termination_convergence_is_a_no_op_over_the_corpus() {
    // A deeply nested corpus file (a reactive `@html` LiveView tree) parses fine — `parse` spawns
    // its own deep-stack worker — but *comparing* the resulting deep ASTs recurses on this thread,
    // so run the whole differential on a 64 MiB worker, like the fmt corpus harness.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run_differential)
        .expect("spawn deep-stack differential worker")
        .join()
        .expect("differential worker panicked");
}

fn run_differential() {
    let files = corpus_files();
    assert!(!files.is_empty(), "found no corpus files — wrong root?");

    let mut checked = 0usize;
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // unreadable fixture; nothing to compare
        };
        let source = Source::new(SourceId::FIRST, path.display().to_string(), text);
        let lexed = noeta_lexer::lex(&source);
        let clean = lexed.tokens;
        let legacy = noeta_lexer::insert_terminators(&source, clean.clone());

        // 1a. Hard boundaries == the legacy synthetic-`;` insertion points (identified as the
        //     start of the token following each zero-width `;`).
        let boundaries = noeta_lexer::newline_boundaries(&source, &clean);
        let hard: Vec<u32> = boundaries
            .iter()
            .filter(|b| b.hard)
            .map(|b| b.offset)
            .collect();
        let synthesized: Vec<u32> = legacy
            .iter()
            .zip(legacy.iter().skip(1))
            .filter(|(t, _)| {
                t.kind == noeta_lexer::TokenKind::Semicolon && t.span.start == t.span.end
            })
            .map(|(_, next)| next.span.start)
            .collect();
        assert_eq!(
            hard,
            synthesized,
            "hard boundaries diverged from legacy synthetic `;` in {}",
            path.display()
        );

        // 1b. The full boundary offset set == the legacy soft scan over the legacy stream.
        let offsets: Vec<u32> = boundaries.iter().map(|b| b.offset).collect();
        assert_eq!(
            offsets,
            noeta_lexer::newline_terminator_offsets(&source, &legacy),
            "soft terminator offsets diverged from the legacy scan in {}",
            path.display()
        );

        // 2. Old and new termination parse identically: same AST (spans included), same
        //    diagnostics, valid and invalid files alike.
        let new = noeta_parser::parse(&source, &clean);
        let old = noeta_parser::parse(&source, &legacy);
        assert_eq!(
            new.program,
            old.program,
            "parse diverged between old and new termination in {}",
            path.display()
        );
        assert_eq!(
            format!("{:?}", new.diagnostics),
            format!("{:?}", old.diagnostics),
            "diagnostics diverged between old and new termination in {}",
            path.display()
        );
        checked += 1;
    }

    println!("termination differential: {checked} corpus files, old == new on every one");
    assert!(checked > 500, "corpus unexpectedly small: {checked} files");
}
