//! Triage the formatter fuzzer: scan seeds, group failures by family, minimize one.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example triage -- scan 3000     # histogram by family
//! cargo run --release -p noeta-fuzz --example triage -- first Safety  # first seed of a family
//! cargo run --release -p noeta-fuzz --example triage -- min 92        # minimal reproducer
//! ```

use std::collections::BTreeMap;

use noeta_fmt::oracle::{self, Verdict};
use noeta_fuzz::fmt_target::{self, Class};

/// The base seed the fuzz suite sweeps. Kept in sync with `tests/fmt.rs` so a seed reported by the
/// test reproduces here unchanged.
const BASE: u64 = 0xF0217A;

/// The formatter recurses to the nesting depth of its input; generated programs can exceed the
/// default thread stack.
fn on_deep_stack<R: Send>(body: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, body)
            .expect("spawn deep-stack worker")
            .join()
            .expect("deep-stack worker panicked")
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("scan");
    on_deep_stack(|| match mode {
        "min" => {
            let seed: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            minimize_seed(seed);
        }
        // Run the oracle over stdin using the config `seed` denotes — for hand-reducing a case
        // past what line-granular minimization can reach.
        "stdin" => {
            let seed: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            let (_, config) = fmt_target::case(BASE, seed);
            let src = std::io::read_to_string(std::io::stdin()).expect("read stdin");
            println!("[{}]", fmt_target::describe(&config));
            match oracle::check("stdin.noe", &src, &config) {
                Ok(v) => println!("{v:?}"),
                Err(v) => println!("{v}"),
            }
        }
        // Print what the formatter makes of stdin under the default config — for pinning the exact
        // canonical form a regression test should assert.
        "out" => {
            let src = std::io::read_to_string(std::io::stdin()).expect("read stdin");
            match noeta_fmt::format_source("out.noe", &src, &noeta_fmt::FmtConfig::default()) {
                Ok(out) => print!("{out}"),
                Err(e) => println!("ERROR: {e}"),
            }
        }
        "first" => {
            let want = args.get(1).map(String::as_str).unwrap_or("Safety");
            first_of(
                want,
                args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5_000),
            );
        }
        _ => {
            let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3_000);
            scan(n);
        }
    });
}

/// Count failures by family over `n` seeds, and report the lowest seed of each — the one to
/// minimize, since a lower seed tends to mean a shorter program.
fn scan(n: u32) {
    let mut counts: BTreeMap<Class, (u32, u32)> = BTreeMap::new();
    let (mut clean, mut declined) = (0u32, 0u32);
    for i in 0..n {
        let (src, config) = fmt_target::case(BASE, i);
        match oracle::check("scan.noe", &src, &config) {
            Ok(Verdict::Clean) => clean += 1,
            Ok(Verdict::Declined) => declined += 1,
            Err(v) => {
                let entry = counts.entry(Class::of(&v)).or_insert((0, i));
                entry.0 += 1;
            }
        }
    }
    println!("scanned {n} seeds: {clean} clean, {declined} declined");
    if counts.is_empty() {
        println!("no violations");
        return;
    }
    println!("{:<18} {:>6}  first seed", "family", "count");
    for (class, (count, first)) in &counts {
        println!("{class:<18?} {count:>6}  {first}");
    }
}

/// Print the first seed whose failure is of family `want`.
fn first_of(want: &str, limit: u32) {
    for i in 0..limit {
        let (src, config) = fmt_target::case(BASE, i);
        if let Err(v) = oracle::check("scan.noe", &src, &config)
            && format!("{:?}", Class::of(&v)) == want
        {
            println!("seed {i} [{}]", fmt_target::describe(&config));
            println!("{v}");
            println!("--- input ({} lines) ---\n{src}", src.lines().count());
            return;
        }
    }
    println!("no {want} failure in the first {limit} seeds");
}

/// Minimize the failure at `seed` and print the reproducer.
fn minimize_seed(seed: u32) {
    let (src, config) = fmt_target::case(BASE, seed);
    let Err(v) = oracle::check("min.noe", &src, &config) else {
        println!("seed {seed} does not fail");
        return;
    };
    let class = Class::of(&v);
    let before = src.lines().count();
    let reduced = fmt_target::minimize(&src, &config, class);
    println!("seed {seed} [{}]", fmt_target::describe(&config));
    println!("family: {class:?}");
    println!(
        "reduced {before} lines -> {} lines",
        reduced.lines().count()
    );
    println!("--- minimal reproducer ---\n{reduced}\n--- end ---");
    match oracle::check("min.noe", &reduced, &config) {
        Err(v) => println!("violation: {v}"),
        Ok(verdict) => println!("UNEXPECTED: reduced case is {verdict:?}"),
    }
}
