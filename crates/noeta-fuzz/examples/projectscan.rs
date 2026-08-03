//! Sweep generated **projects** through the check-vs-run differential and triage what it finds.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example projectscan -- scan 2000
//! cargo run --release -p noeta-fuzz --example projectscan -- show 41
//! ```
//!
//! `scan` prints the reach table — how many projects each side accepted, per layout — alongside the
//! violations, deduplicated by class. The table is the part to read first: the invariant is a
//! boolean equality, so a sweep in which every project came out `Rejected` proved one implication
//! and skipped the other, and that looks identical to a sweep that found nothing because everything
//! works.
//!
//! `show <nonce>` materializes one project, prints its layout and files, and reports both sides'
//! verdicts — the replay a failing sweep names.

use std::collections::BTreeMap;

use noeta_fuzz::project_target::{self as target, Reach};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("scan");
    let n: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(360);
    let seed = target::BASE_SEED;

    match mode {
        "show" => {
            let dir = noeta_test_temp::TempDir::new("projectscan-show");
            let layout = target::materialize(&dir, seed, n);
            println!("nonce {n}: {layout:?} at {}", dir.display());
            for entry in target::entries(&dir) {
                let text = std::fs::read_to_string(&entry).unwrap_or_default();
                println!("--- {} ---\n{text}", entry.display());
            }
            println!("check: {:?}", target::check_project(&dir));
            println!("run:   {:?}", target::run_project(&dir));
            println!("verdict: {:?}", target::evaluate_total(&dir));
        }
        _ => {
            // Per-layout accepted/rejected counts. Which layouts can reach `Accepted` at all is the
            // fact a reader needs: a layout that is structurally always refused (an unspellable
            // module name) proves the agreement only on the refusing side.
            let mut table: BTreeMap<String, (u32, u32)> = BTreeMap::new();
            let mut findings: BTreeMap<String, (u32, u32, String)> = BTreeMap::new();
            let mut tiers = 0u32;
            for nonce in 0..n {
                let dir = noeta_test_temp::TempDir::new("projectscan");
                let layout = target::materialize(&dir, seed, nonce);
                let key = format!("{layout:?}");
                match target::evaluate_total(&dir) {
                    Ok(evaluated) => {
                        if !evaluated.tiers_checked.is_empty() {
                            tiers += 1;
                        }
                        let row = table.entry(key).or_default();
                        match evaluated.reach {
                            Reach::Accepted => row.0 += 1,
                            Reach::Rejected => row.1 += 1,
                        }
                    }
                    Err(violation) => {
                        let entry = findings.entry(target::class(&violation)).or_insert((
                            0,
                            nonce,
                            format!("[{key}] {violation}"),
                        ));
                        entry.0 += 1;
                    }
                }
                if nonce % 50 == 49 {
                    eprintln!("  … {} of {n}", nonce + 1);
                }
            }
            println!("reach over {n} projects (layout: accepted / rejected)");
            let mut accepted = 0u32;
            let mut rejected = 0u32;
            for layout in target::LAYOUTS {
                let key = format!("{layout:?}");
                let (a, r) = table.get(&key).copied().unwrap_or((0, 0));
                accepted += a;
                rejected += r;
                println!("  {key:<20} {a:>5} / {r:>5}");
            }
            println!("  {:<20} {accepted:>5} / {rejected:>5}", "TOTAL");
            println!("{tiers} project(s) swept a code-tier shape (want 0)");
            if findings.is_empty() {
                println!("no violations");
            }
            for (class, (count, first, sample)) in &findings {
                println!("\n[{class}] {count} hit(s), first at nonce {first}\n  {sample}");
            }
        }
    }
}
