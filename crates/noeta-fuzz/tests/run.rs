//! The execution pipeline, fuzzed: what the checker promises, held against what running does.
//!
//! Three invariants, each already written down in the tree rather than invented here — a checked
//! program compiles, a checked program does not fail *statically* at run time, and the two backends
//! agree. [`noeta_fuzz::run_target`] documents each and why it is the project's own claim.
//!
//! # Why this is a test and not a flake
//!
//! The sweep walks a fixed seed range through a deterministic generator, so it is an ordinary
//! reproducible test that happens to have been written by a generator. A failure names the nonce,
//! and `cargo run --release -p noeta-fuzz --example runscan -- min <nonce>` replays and minimizes
//! it. The proptest arm adds shrinking of the *driver bytes* on top, and fewer bytes generate a
//! smaller program.
//!
//! # The two floors, and why they are assertions
//!
//! Every invariant here is conditioned on a program that **runs**. Most generated programs do not:
//! this is a syntax generator, not a type-directed one, so the checker rejects around nine in ten —
//! legitimately, since it is entitled to. That makes a thin `Ran` column indistinguishable from a
//! clean sweep, so [`MIN_RAN`] is asserted rather than reported.
//!
//! The second floor is subtler. The static-versus-dynamic classification of a runtime diagnostic is
//! only sharp because the generator emits no `dyn` and no reflection — a `dyn` member access really
//! is typed at run time, and calling that a divergence would be wrong. That is a *precondition*, so
//! it is asserted too. An oracle whose precondition quietly stops holding is one that quietly stops
//! finding anything.
//!
//! # Coverage, stated rather than assumed
//!
//! `NONCES` is what runs in the gate. It is not the limit of the technique: `runscan scan 40000`
//! runs the identical oracle over as many as you care to wait for, and is what should be run after
//! touching the checker, the compiler, or either backend. Four defects came out of this target's
//! first sweeps — a hoisted global visible to the statement that binds it, a type name accepted in
//! callee position, a `for` tuple pattern over a non-tuple element, and a duplicate declaration that
//! panicked the compiler.

use noeta_fuzz::run_target::{self, BASE_SEED, Reach};
use proptest::prelude::*;

/// The front end recurses over nesting the default ~2 MiB test-thread stack cannot hold, exactly as
/// in the formatter suite.
const DEEP_STACK: usize = 64 * 1024 * 1024;

/// Programs swept by the gate. See the module docs on why this is a floor, not a claim.
const NONCES: u32 = 2_000;

/// The floor on programs that actually executed. Measured at ~8% of generated programs, so this is
/// set well below that: it is here to catch the sweep going hollow, not to pin a rate.
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
fn the_checker_and_the_run_path_agree_over_generated_programs() {
    on_deep_stack(|| {
        let mut ran = 0u32;
        let mut rejected = 0u32;
        for nonce in 0..NONCES {
            let src = run_target::source(BASE_SEED, nonce);
            match run_target::evaluate_total(&src) {
                Ok(Reach::Ran) => ran += 1,
                Ok(_) => rejected += 1,
                Err(violation) => panic!(
                    "nonce {nonce}: {violation}\n\
                     replay: cargo run --release -p noeta-fuzz --example runscan -- min {nonce}\n\
                     --- program ---\n{src}"
                ),
            }
        }
        eprintln!("run oracle: {ran} programs ran, {rejected} of {NONCES} were rejected earlier");
        assert!(
            ran >= MIN_RAN,
            "only {ran} of {NONCES} generated programs ran (floor {MIN_RAN}) — every invariant \
             here is conditioned on a program that executes, so this sweep proved almost nothing. \
             Something upstream is rejecting programs it used to accept."
        );
    });
}

/// The oracle's precondition, asserted rather than assumed: no generated program reaches a
/// construct whose typing is *supposed* to happen at run time.
///
/// If a future generator arm starts emitting `dyn`, a runtime `TypeMismatch` stops being evidence
/// of anything and the sweep above turns into noise — or, worse, into silence, once someone widens
/// [`run_target::STATIC_AT_RUNTIME`] to compensate. This fails first instead.
#[test]
fn the_generated_language_stays_statically_typed() {
    let offenders: Vec<u32> = (0..NONCES)
        .filter(|&nonce| run_target::uses_dynamic_typing(&run_target::source(BASE_SEED, nonce)))
        .collect();
    assert!(
        offenders.is_empty(),
        "{} generated program(s) reach a dynamically-typed construct (first: {:?}) — a runtime \
         type error is then legitimate, and the static-versus-dynamic classification the run \
         oracle rests on is no longer sharp.",
        offenders.len(),
        &offenders[..offenders.len().min(5)]
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, max_shrink_iters: 4096, ..ProptestConfig::default() })]

    // A shrinking driver over the same oracle. On a failure proptest minimizes the driver bytes,
    // and a shorter buffer winds generation down into a smaller program (the entropy contract), so
    // the printed counterexample is close to a paste-ready regression case.
    #[test]
    fn the_run_oracle_holds_under_shrinking(bytes in prop::collection::vec(any::<u8>(), 0..384)) {
        let src = noeta_fuzz::generate::program_with(
            &bytes,
            &noeta_fuzz::generate::GenOptions::terminating(),
        );
        let verdict = on_deep_stack(|| run_target::evaluate_total(&src));
        prop_assert!(
            verdict.is_ok(),
            "{}\n--- program ---\n{src}",
            verdict.unwrap_err()
        );
    }
}
