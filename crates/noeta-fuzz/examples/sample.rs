//! Print generated programs and the generator's parse rate — the development loop for `noeta_fuzz::generate`.
//!
//! ```text
//! cargo run -p noeta-fuzz --example sample            # rate over 2000 programs
//! cargo run -p noeta-fuzz --example sample -- show 3  # print 3 programs
//! cargo run -p noeta-fuzz --example sample -- bad 5   # print 5 that fail to parse, with diagnostics
//! ```

use noeta_span::{Source, SourceId};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("rate");
    let n: u32 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(if mode == "rate" { 2000 } else { 3 });

    match mode {
        "show" => {
            for i in 0..n {
                let src = noeta_fuzz::generate::program(&noeta_fuzz::seed_bytes(0xC0FFEE, i));
                let ok = noeta_fuzz::parses_cleanly(&src);
                println!("===== seed {i} | parses: {ok} =====\n{src}");
            }
        }
        "bad" => {
            let mut shown = 0;
            let mut i = 0u32;
            while shown < n && i < 200_000 {
                let src = noeta_fuzz::generate::program(&noeta_fuzz::seed_bytes(0xC0FFEE, i));
                if !noeta_fuzz::parses_cleanly(&src) {
                    let source = Source::new(SourceId(0), "fuzz.noe", src.as_str());
                    let lexed = noeta_lexer::lex(&source);
                    let parsed = noeta_parser::parse(&source, &lexed.tokens);
                    println!("===== seed {i} =====");
                    // Only the first diagnostic matters: everything after it is recovery noise.
                    // Print the source line it points at, which is what actually identifies the
                    // construct the generator got wrong.
                    if let Some(d) = lexed
                        .diagnostics
                        .iter()
                        .chain(parsed.diagnostics.iter())
                        .next()
                    {
                        let at = (d.span.start as usize).min(src.len());
                        let line_start = src[..at].rfind('\n').map_or(0, |p| p + 1);
                        let line_end = src[at..].find('\n').map_or(src.len(), |p| at + p);
                        let line_no = src[..at].matches('\n').count() + 1;
                        println!("  {:?}: {}", d.code, d.message);
                        println!("  line {line_no}: {}", &src[line_start..line_end]);
                        println!("  {}^", " ".repeat(9 + at - line_start));
                    }
                    shown += 1;
                }
                i += 1;
            }
            println!("scanned {i} programs");
        }
        // Parse whatever is on stdin and report its diagnostics — the quickest way to settle a
        // question about what the grammar actually accepts while tuning the generator.
        "parse" => {
            let src = std::io::read_to_string(std::io::stdin()).expect("read stdin");
            let source = Source::new(SourceId(0), "stdin.noe", src.as_str());
            let lexed = noeta_lexer::lex(&source);
            let parsed = noeta_parser::parse(&source, &lexed.tokens);
            let diags: Vec<_> = lexed
                .diagnostics
                .iter()
                .chain(parsed.diagnostics.iter())
                .collect();
            if diags.is_empty() {
                println!("OK");
            } else {
                for d in diags {
                    println!(
                        "{:?} @{}..{}: {}",
                        d.code, d.span.start, d.span.end, d.message
                    );
                }
            }
        }
        _ => {
            let rate = noeta_fuzz::parse_rate(n, 0xC0FFEE);
            println!("parse rate over {n} programs: {:.2}%", rate * 100.0);
        }
    }
}
