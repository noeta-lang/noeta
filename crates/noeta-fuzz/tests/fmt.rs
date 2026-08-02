//! The formatter, fuzzed: [`noeta_fmt::oracle`] asserted over *generated* programs.
//!
//! The corpus harness in `noeta-fmt` runs the same oracle over ~1,200 real files. This runs it over
//! programs nobody wrote, in layouts nobody would write — collapsed one-line bodies, comments
//! between the links of a broken method chain, redundant header parentheses, semicolons on some
//! statements and not others — and across the whole configuration space rather than the three
//! configs the corpus fixes. Six printer defects came out of it, none reachable from the corpus.
//!
//! # Why this is a test and not a flake
//!
//! Both properties here are seeded and reproducible. The sweep walks a fixed range of seeds, so it
//! is an ordinary deterministic test that happens to have been written by a generator: it cannot
//! flake, and a failure names the exact seed to replay with
//! `cargo run --release -p noeta-fuzz --example triage -- min <seed>`. The proptest driver adds
//! shrinking on top — on a failure it minimizes the *driver bytes*, and fewer bytes generate a
//! smaller program, so the counterexample it prints is close to a paste-ready regression case.
//!
//! # Coverage, stated rather than assumed
//!
//! `SEEDS` is what runs in the gate, chosen so this stays comparable to the corpus sweep it sits
//! beside rather than dominating it. It is **not** the limit of the technique: `triage scan 50000`
//! runs the identical oracle over as many seeds as you care to wait for, and is what should be run
//! after touching the printer. Saying so here matters — a bounded sweep that does not admit its
//! bound reads as "the formatter is correct" when it means "the formatter survived 3,000 programs".

use noeta_fmt::oracle::{self, Verdict};
use noeta_fuzz::fmt_target::{self, BASE_SEED};
use noeta_fuzz::generate;
use proptest::prelude::*;

/// The formatter parses deeply-nested input recursively, and a generated program can nest further
/// than the default ~2 MiB test-thread stack allows. The corpus harness runs on the same 64 MiB
/// worker for the same reason.
const DEEP_STACK: usize = 64 * 1024 * 1024;

/// Seeds swept by the gate. See the module docs on why this number is a floor, not a claim.
const SEEDS: u32 = 3_000;

/// Run `body` on a worker with a stack deep enough for the formatter's recursion.
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

/// The workhorse: a deterministic sweep of seeds, each formatted under a config derived from the
/// same bytes.
#[test]
fn fmt_invariants_hold_over_generated_programs() {
    on_deep_stack(|| {
        let mut checked = 0u32;
        let mut declined = 0u32;
        for seed in 0..SEEDS {
            let (src, config) = fmt_target::case(BASE_SEED, seed);
            match oracle::check("generated.noe", &src, &config) {
                Ok(Verdict::Clean) => checked += 1,
                Ok(Verdict::Declined) => declined += 1,
                Err(violation) => panic!(
                    "seed {seed} [{}] violated a formatter invariant:\n{violation}\n\
                     reproduce: cargo run --release -p noeta-fuzz --example triage -- min {seed}\n\
                     --- input ---\n{src}\n--- end input ---",
                    fmt_target::describe(&config)
                ),
            }
        }
        eprintln!("fmt fuzz: {checked} checked | {declined} declined (unparseable)");
        // The sweep must actually exercise the formatter, not decline its way to green. The
        // generator's own parse-rate floor lives in `tests/generator.rs`; this is the same guard
        // stated where the property is consumed.
        assert!(
            checked > SEEDS * 9 / 10,
            "only {checked}/{SEEDS} generated programs reached the formatter — \
             the invariants above are passing vacuously"
        );
    });
}

// The same properties under `proptest`, which adds shrinking. The sweep above is better at
// *finding* violations (its programs are larger and its configs more varied); this one is better at
// *reporting* them, because it minimizes the driver bytes before printing a counterexample.
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        // Shrinking a byte buffer is cheap — the generator is total and bounded by its own node
        // budget — so allow it to run far enough to reach a genuinely small program.
        max_shrink_iters: 4_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn fmt_invariants_hold_under_shrinking(bytes in prop::collection::vec(any::<u8>(), 0..1_024)) {
        let src = generate::program(&bytes);
        let config = fmt_target::config_from(&bytes);
        let outcome = on_deep_stack(|| oracle::check("proptest.noe", &src, &config));
        prop_assert!(
            outcome.is_ok(),
            "[{}] violated a formatter invariant:\n{}\n--- input ---\n{src}\n--- end input ---",
            fmt_target::describe(&config),
            outcome.expect_err("checked is_ok above"),
        );
    }
}
