//! Generated programs give their memory back.
//!
//! See [`noeta_fuzz::leak_target`] for why this needs an oracle of its own: a leak produces no
//! wrong answer, so no comparison-based property — not the backend differential, not the checker
//! oracle — can see it.
//!
//! The corpus leak oracle in `noeta-conformance` is the gate for programs somebody wrote. This is
//! the one for object graphs nobody thought of.

use noeta_fuzz::leak_target::{self, BASE_SEED};
use noeta_fuzz::run_target::Reach;
use proptest::prelude::*;

/// The front end recurses over nesting the default ~2 MiB test-thread stack cannot hold.
const DEEP_STACK: usize = 64 * 1024 * 1024;

/// Programs swept by the gate. A floor, not a claim — `leakscan` goes as deep as you like.
const NONCES: u32 = 2_000;

/// The floor on programs that actually ran. Every assertion here is conditioned on a program that
/// executes and allocates, so a sweep whose `Ran` column collapses proved nothing — the same
/// anti-vacuity discipline as the parse rate and the corruption-reach histogram.
const MIN_RAN: u32 = 60;

fn on_deep_stack<R: Send>(body: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, body)
            .expect("spawn deep-stack worker")
            .join()
            .expect("deep-stack worker panicked")
    })
}

#[test]
fn generated_programs_reclaim_everything_they_allocate() {
    // One worker thread for the whole sweep: both live-object counters are thread-local, so the
    // runs and their measurements have to stay on the thread that made the allocations.
    on_deep_stack(|| {
        let mut ran = 0u32;
        for nonce in 0..NONCES {
            let src = leak_target::source(BASE_SEED, nonce);
            match leak_target::leak_check(&src) {
                Ok(Reach::Ran) => ran += 1,
                Ok(_) => {}
                Err(violation) => panic!(
                    "nonce {nonce}: {violation}\n\
                     replay: cargo run --release -p noeta-fuzz --example leakscan -- min {nonce}\n\
                     --- program ---\n{src}"
                ),
            }
        }
        eprintln!("leak oracle: {ran} of {NONCES} generated programs ran and reclaimed fully");
        assert!(
            ran >= MIN_RAN,
            "only {ran} of {NONCES} generated programs ran (floor {MIN_RAN}) — nothing allocated, \
             so nothing was measured, and this sweep proved nothing about reclamation."
        );
    });
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, max_shrink_iters: 4096, ..ProptestConfig::default() })]

    // Shrinking over the driver bytes, so a leaking program reduces toward a paste-ready case.
    #[test]
    fn reclamation_holds_under_shrinking(bytes in prop::collection::vec(any::<u8>(), 0..384)) {
        let src = noeta_fuzz::generate::program_with(
            &bytes,
            &noeta_fuzz::generate::GenOptions::terminating(),
        );
        let verdict = on_deep_stack(|| leak_target::leak_check(&src));
        prop_assert!(verdict.is_ok(), "{}\n--- program ---\n{src}", verdict.unwrap_err());
    }
}
