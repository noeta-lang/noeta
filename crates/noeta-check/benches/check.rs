//! Checker throughput over the real conformance corpus (audit-3 Finding 12 gate).
//!
//! The checker runs per keystroke under the salsa graph (LSP), so its allocation behavior is an
//! IDE-latency lever. This bench parses every cleanly-parsing `tests/conformance/**/*.noe` once
//! up front, then times `check_all` over the whole parsed corpus — checker cost only, no lexing,
//! parsing, linking, or IO in the measured loop. Any change to environment lookup, symbol
//! tables, or `Type` cloning shows up here against real programs rather than synthetic ones.

use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use noeta_ast::Program;
use noeta_lexer::lex;
use noeta_parser::parse;
use noeta_span::{Source, SourceId};

/// Every `.noe` file under `tests/conformance`, parsed. Files with parse diagnostics are kept
/// too — the checker must be robust over recovered ASTs and the negative half of the corpus is
/// real checker input — but a file that fails to *read* is a corpus error and panics.
fn parsed_corpus() -> Vec<Program> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/conformance");
    let mut files: Vec<PathBuf> = walk(&root);
    files.sort();
    assert!(
        files.len() > 500,
        "conformance corpus unexpectedly small ({} files) — wrong root?",
        files.len()
    );
    files
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let source = Source::new(SourceId::FIRST, path.to_string_lossy(), &text);
            let lexed = lex(&source);
            parse(&source, &lexed.tokens).program
        })
        .collect()
}

fn walk(dir: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
    out
}

fn bench_check(c: &mut Criterion) {
    let corpus = parsed_corpus();
    let total: usize = corpus.len();
    let mut group = c.benchmark_group("check");
    // One iteration = one full checker pass over every conformance program.
    group.sample_size(20);
    group.bench_function(format!("conformance_corpus_{total}_files"), |b| {
        b.iter(|| {
            let mut diags = 0usize;
            for program in &corpus {
                diags += noeta_check::check(black_box(program)).len();
            }
            black_box(diags)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_check);
criterion_main!(benches);
