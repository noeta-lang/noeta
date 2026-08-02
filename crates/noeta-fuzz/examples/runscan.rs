//! Sweep generated programs through the execution oracle and triage what it finds.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example runscan -- scan 2000
//! cargo run --release -p noeta-fuzz --example runscan -- show 41
//! cargo run --release -p noeta-fuzz --example runscan -- min 41
//! ```
//!
//! `scan` prints the reach histogram — how many programs parsed, checked, and ran — alongside the
//! violations, deduplicated by class. The histogram is the part to read first: every invariant is
//! conditioned on a program that *runs*, so a sweep with a thin `Ran` column found nothing because
//! it tested nothing, and that looks identical to a sweep that found nothing because everything
//! works.

use noeta_fuzz::run_target::{self, Reach};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("scan");
    let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2_000);
    let seed = run_target::BASE_SEED;

    match mode {
        "show" => {
            let src = run_target::source(seed, n);
            println!("{src}");
            println!("--- verdict: {:?}", run_target::evaluate_total(&src));
        }
        // Judge a program supplied on stdin — how a hand-reduced candidate gets confirmed.
        "stdin" => {
            let mut src = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut src).expect("read stdin");
            println!("{:?}", run_target::evaluate_total(&src));
            for d in run_target::check_diagnostics(&src) {
                println!("  check: {d}");
            }
        }
        "min" => {
            let src = run_target::source(seed, n);
            let Err(v) = run_target::evaluate_total(&src) else {
                println!("nonce {n} does not violate anything");
                return;
            };
            let target = run_target::class(&v);
            let reduced = run_target::minimize(&src, &target);
            println!(
                "nonce {n} [{target}]: reduced {} -> {} lines\n{}",
                src.lines().count(),
                reduced.lines().count(),
                v
            );
            println!("--- minimal reproducer ---\n{reduced}\n--- end ---");
        }
        _ => {
            let mut histogram = std::collections::BTreeMap::<String, u32>::new();
            let mut findings = std::collections::BTreeMap::<String, (u32, u32, String)>::new();
            let mut dynamic = 0u32;
            for nonce in 0..n {
                let src = run_target::source(seed, nonce);
                if run_target::uses_dynamic_typing(&src) {
                    dynamic += 1;
                }
                match run_target::evaluate_total(&src) {
                    Ok(reach) => *histogram.entry(format!("{reach:?}")).or_default() += 1,
                    Err(v) => {
                        *histogram.entry("VIOLATION".to_string()).or_default() += 1;
                        let entry = findings.entry(run_target::class(&v)).or_insert((
                            0,
                            nonce,
                            v.to_string(),
                        ));
                        entry.0 += 1;
                    }
                }
                if nonce % 500 == 499 {
                    eprintln!("  … {} of {n}", nonce + 1);
                }
            }
            println!("reach over {n} programs: {histogram:?}");
            println!("{dynamic} program(s) reached a dynamically-typed construct (want 0)");
            let ran = histogram
                .get(&format!("{:?}", Reach::Ran))
                .copied()
                .unwrap_or(0);
            println!("{ran}/{n} ran, so that many programs actually exercised the invariants",);
            if findings.is_empty() {
                println!("no violations");
            }
            for (class, (count, first, sample)) in &findings {
                println!("\n[{class}] {count} hit(s), first at nonce {first}\n  {sample}");
            }
        }
    }
}
