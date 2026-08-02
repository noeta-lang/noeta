//! Measure the type-directed generator's health, and show what it makes.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example typedscan -- rate 5000
//! cargo run --release -p noeta-fuzz --example typedscan -- show 3
//! cargo run --release -p noeta-fuzz --example typedscan -- bad 5000
//! ```
//!
//! `rate` is the number that matters: the fraction of generated programs the checker accepts. A
//! type-directed generator that drifts into emitting ill-typed programs still looks like it works,
//! and every false-positive test built on it quietly becomes vacuous — so `bad` prints the first
//! few rejections with their diagnostics, which is how the rate gets fixed rather than lowered.

use noeta_fuzz::run_target;

const SEED: u64 = 0x7_9DED;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("rate");
    let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2_000);

    match mode {
        "show" => {
            let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, n));
            println!("{src}");
            println!("--- verdict: {:?}", run_target::evaluate_total(&src));
            for d in run_target::check_diagnostics(&src) {
                println!("  check: {d}");
            }
        }
        "bad" => {
            let mut shown = 0;
            for nonce in 0..n {
                let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, nonce));
                let diags = run_target::check_diagnostics(&src);
                if !diags.is_empty() {
                    println!("--- nonce {nonce}");
                    for d in diags.iter().take(3) {
                        println!("  {d}");
                    }
                    shown += 1;
                    if shown >= 12 {
                        break;
                    }
                }
            }
            if shown == 0 {
                println!("no rejections in {n} programs");
            }
        }
        // The prize: every well-typed program RUNS, so the backend differential, the compile
        // invariant and the static-error oracle all see 100% of the sweep instead of the ~7% the
        // syntax generator yields.
        "run" => {
            let (mut ran, mut bad) = (0u32, 0u32);
            for nonce in 0..n {
                let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, nonce));
                match run_target::evaluate_total(&src) {
                    Ok(run_target::Reach::Ran) => ran += 1,
                    Ok(other) => {
                        bad += 1;
                        if bad <= 5 {
                            println!("nonce {nonce}: only reached {other:?}");
                        }
                    }
                    Err(v) => {
                        bad += 1;
                        if bad <= 5 {
                            println!("nonce {nonce}: {v}");
                        }
                    }
                }
                if nonce % 2_000 == 1_999 {
                    eprintln!("  … {} of {n}", nonce + 1);
                }
            }
            println!("{ran}/{n} ran and agreed across both backends; {bad} did not");
        }
        _ => {
            let (mut clean, mut unparsed) = (0u32, 0u32);
            for nonce in 0..n {
                let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, nonce));
                if !noeta_fuzz::parses_cleanly(&src) {
                    unparsed += 1;
                    continue;
                }
                if run_target::check_diagnostics(&src).is_empty() {
                    clean += 1;
                }
            }
            println!(
                "{clean}/{n} check clean ({:.1}%), {unparsed} did not parse",
                clean as f64 / n as f64 * 100.0
            );
        }
    }
}
