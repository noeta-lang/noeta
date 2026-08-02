//! Sweep generated programs through the leak oracle and triage what it finds.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example leakscan -- scan 20000
//! cargo run --release -p noeta-fuzz --example leakscan -- min 41
//! ```

use noeta_fuzz::leak_target::{self, BASE_SEED};
use noeta_fuzz::run_target::Reach;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("scan");
    let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5_000);

    match mode {
        "min" => {
            let src = leak_target::source(BASE_SEED, n);
            if !leak_target::still_leaks(&src) {
                println!("nonce {n} reclaims fully");
                return;
            }
            let reduced = leak_target::minimize(&src);
            println!(
                "nonce {n}: reduced {} -> {} lines",
                src.lines().count(),
                reduced.lines().count()
            );
            if let Err(v) = leak_target::leak_check(&reduced) {
                println!("{v}");
            }
            println!("--- minimal reproducer ---\n{reduced}\n--- end ---");
        }
        _ => {
            let (mut ran, mut leaked) = (0u32, 0u32);
            let mut first: Vec<u32> = Vec::new();
            for nonce in 0..n {
                let src = leak_target::source(BASE_SEED, nonce);
                match leak_target::leak_check(&src) {
                    Ok(Reach::Ran) => ran += 1,
                    Ok(_) => {}
                    Err(v) => {
                        leaked += 1;
                        if first.len() < 8 {
                            first.push(nonce);
                            println!("nonce {nonce}: {v}");
                        }
                    }
                }
                if nonce % 1_000 == 999 {
                    eprintln!("  … {} of {n}", nonce + 1);
                }
            }
            println!("{ran}/{n} ran and were measured; {leaked} leaked");
            if !first.is_empty() {
                println!("first leaking nonces: {first:?}");
            }
        }
    }
}
