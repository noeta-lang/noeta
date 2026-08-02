//! Sweep generated programs through the tier-1 differential.
//!
//! ```text
//! cargo run --release -p noeta-fuzz --features jit --example jitscan -- 20000
//! ```
//!
//! Without `--features jit` this prints why it did nothing rather than silently succeeding.

#[cfg(feature = "jit")]
fn main() {
    use noeta_fuzz::jit_target::{self, BASE_SEED};
    use noeta_fuzz::run_target::Reach;

    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000);
    let (mut ran, mut bad) = (0u32, 0u32);
    for nonce in 0..n {
        let src = jit_target::source(BASE_SEED, nonce);
        match jit_target::jit_check(&src) {
            Ok(Reach::Ran) => ran += 1,
            Ok(_) => {}
            Err(v) => {
                bad += 1;
                if bad <= 8 {
                    println!("nonce {nonce}: {v}");
                }
            }
        }
        if nonce % 1_000 == 999 {
            eprintln!("  … {} of {n}", nonce + 1);
        }
    }
    println!("{ran}/{n} ran on both tiers; {bad} disagreed");
}

#[cfg(not(feature = "jit"))]
fn main() {
    println!("built without `--features jit` — nothing was compared");
}
