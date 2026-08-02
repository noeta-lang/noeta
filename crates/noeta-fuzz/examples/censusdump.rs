//! Print the runtime-rejection inventory, for refreshing `crates/noeta-fuzz/census.txt`.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --example censusdump > crates/noeta-fuzz/census.txt
//! ```
//!
//! Refresh it only after answering the question a new entry poses — see `noeta_fuzz::census`.

fn main() {
    println!(
        "# The runtime's static-class rejection reasons, derived from `noeta-vm` / `noeta-eval` /"
    );
    println!("# `noeta-value` by `noeta_fuzz::census::reasons`. One line per distinct reason:");
    println!("# <DiagnosticCode>\\t<message template, format holes collapsed to `{{}}`>.");
    println!("#");
    println!(
        "# This is an INVENTORY, not a verdict list. Each line is a way the runtime can refuse"
    );
    println!("# a program on grounds the checker might have settled instead; the census exists so");
    println!("# that a NEW one cannot appear without someone asking whether a checked program can");
    println!(
        "# reach it. Roughly forty have been probed by hand so far — nine were divergences and"
    );
    println!("# are now static errors in `noeta-check` — and the rest are recorded, not cleared.");
    println!("#");
    println!("# Regenerate with:");
    println!(
        "#   cargo run --release -p noeta-fuzz --example censusdump > crates/noeta-fuzz/census.txt"
    );
    for reason in noeta_fuzz::census::reasons() {
        println!("{reason}");
    }
}
