//! Find generated programs that make the compiler **panic** rather than return.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example compilescan -- scan 2000
//! cargo run --release -p noeta-fuzz --example compilescan -- min 41
//! ```
//!
//! `noeta_compiler::compile` reports an unsupported construct as `Err(Unsupported)`, so a panic is
//! a different thing entirely — an input shape nobody enumerated. The generator reaches those
//! because it emits syntactically valid programs that are *not* type-correct, which is exactly the
//! space between "the parser accepted it" and "the checker would have rejected it".

use std::panic::{AssertUnwindSafe, catch_unwind};

use noeta_span::{Source, SourceId};

/// Whether compiling `src` panics. Panic output is silenced so a scan is readable.
fn panics(src: &str) -> bool {
    let source = Source::new(SourceId(0), "scan.noe", src);
    let lexed = noeta_lexer::lex(&source);
    if !lexed.diagnostics.is_empty() {
        return false;
    }
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    if !parsed.diagnostics.is_empty() {
        return false;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let out = catch_unwind(AssertUnwindSafe(|| {
        noeta_compiler::compile(&parsed.program)
    }));
    std::panic::set_hook(prev);
    out.is_err()
}

/// Whether the *checker* rejects `src` — the question that decides how reachable a panic is. A
/// program the checker refuses never reaches the compiler through the CLI.
fn checker_rejects(src: &str) -> bool {
    let source = Source::new(SourceId(0), "scan.noe", src);
    let lexed = noeta_lexer::lex(&source);
    let parsed = noeta_parser::parse(&source, &lexed.tokens);
    !noeta_check::check_all(&parsed.program)
        .diagnostics
        .is_empty()
}

fn main() {
    noeta_stdlib::registry::default_seeded();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("scan");
    let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2_000);

    match mode {
        "min" => {
            let src = noeta_fuzz::generate::program(&noeta_fuzz::seed_bytes(0xB0114E, n));
            if !panics(&src) {
                println!("nonce {n} does not panic");
                return;
            }
            let reduced = minimize(&src);
            println!(
                "nonce {n}: reduced {} -> {} lines",
                src.lines().count(),
                reduced.lines().count()
            );
            println!("checker rejects it: {}", checker_rejects(&reduced));
            println!("--- minimal reproducer ---\n{reduced}\n--- end ---");
        }
        _ => {
            let mut hits = Vec::new();
            let mut checker_clean = 0u32;
            let mut clean_and_panics = 0u32;
            for nonce in 0..n {
                let src = noeta_fuzz::generate::program(&noeta_fuzz::seed_bytes(0xB0114E, nonce));
                let clean = !checker_rejects(&src);
                if clean {
                    checker_clean += 1;
                }
                if panics(&src) {
                    hits.push(nonce);
                    if clean {
                        clean_and_panics += 1;
                    }
                }
            }
            println!("{}/{} generated programs panic the compiler", hits.len(), n);
            println!("first: {:?}", &hits[..hits.len().min(12)]);
            println!("{checker_clean}/{n} pass the checker with no diagnostics");
            // The number that decides severity: a panic on a program the checker *accepts* is
            // reachable through `noeta run`; one the checker rejects is not.
            println!("{clean_and_panics}/{n} both pass the checker AND panic the compiler");
        }
    }
}

/// Line-granular delta debugging, keeping the panic. A reduction that stops parsing stops panicking,
/// so it is rejected — the same self-correcting trick the fmt minimizer uses.
fn minimize(src: &str) -> String {
    let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
    let mut chunk = lines.len().max(1);
    while chunk >= 1 {
        let mut i = 0;
        while i < lines.len() {
            let end = (i + chunk).min(lines.len());
            let mut candidate = lines.clone();
            candidate.drain(i..end);
            let text = candidate.join("\n");
            if !text.trim().is_empty() && panics(&text) {
                lines = candidate;
            } else {
                i += 1;
            }
        }
        if chunk == 1 {
            break;
        }
        chunk /= 2;
    }
    lines.join("\n")
}
