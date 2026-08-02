//! The checker accepts programs that are well-typed by construction.
//!
//! # The direction nothing else here could test
//!
//! Every other oracle in this crate tests the checker for being too **lenient**: generate a program,
//! see whether a divergence shows up at run time. That works because a leniency bug is visible on a
//! program that is subtly wrong, and the syntax generator makes plenty of those.
//!
//! A **false positive** — the checker rejecting a correct program — cannot be found that way. A
//! rejection only means something if you already know the program is good, and "it parsed" is not
//! good enough. [`noeta_fuzz::typed`] builds from the typing rules upward, so every one of its
//! programs is correct by construction and every rejection is a bug.
//!
//! # What keeps it honest
//!
//! The same thing that keeps the syntax generator honest, and for a sharper reason. A type-directed
//! generator that drifts into emitting ill-typed programs does not fail loudly — it starts producing
//! rejections, and the natural response to a failing test is to *lower the bar*. So the property
//! here is all-or-nothing (`CLEAN_RATE` is 100%, not a floor to be nudged down), and a regression
//! prints the offending program with the checker's own diagnostics, which is what makes fixing the
//! generator the obvious move rather than relaxing the assertion.

use noeta_fuzz::run_target;

/// Programs swept by the gate.
const NONCES: u32 = 3_000;

const SEED: u64 = 0x7_9DED;

const DEEP_STACK: usize = 64 * 1024 * 1024;

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

/// Every generated program parses and type-checks. Not "almost every".
#[test]
fn the_checker_accepts_every_well_typed_generated_program() {
    on_deep_stack(|| {
        for nonce in 0..NONCES {
            let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, nonce));
            assert!(
                noeta_fuzz::parses_cleanly(&src),
                "nonce {nonce}: a program built from the grammar does not parse — that is a \
                 generator bug, and until it is fixed this suite is testing less than it claims.\n\
                 --- program ---\n{src}"
            );
            let diagnostics = run_target::check_diagnostics(&src);
            assert!(
                diagnostics.is_empty(),
                "nonce {nonce}: the checker rejected a program built from its own typing rules.\n\
                 Either this is a checker false positive — the thing this suite exists to find — or \
                 the generator emitted something it had no right to. Read the diagnostic before \
                 assuming the second.\n  {}\n--- program ---\n{src}",
                diagnostics.join("\n  ")
            );
        }
        eprintln!("typed generator: {NONCES}/{NONCES} accepted");
    });
}

/// And they run. A well-typed program that the compiler or the runtime refuses is the same class of
/// finding as a rejection, one stage later — and it is what raises the sample size every other
/// execution oracle here is bounded by (~7% of syntax-generated programs run; these all do).
#[test]
fn well_typed_generated_programs_also_run() {
    on_deep_stack(|| {
        let mut ran = 0u32;
        for nonce in 0..500 {
            let src = noeta_fuzz::typed::program(&noeta_fuzz::seed_bytes(SEED, nonce));
            match run_target::evaluate_total(&src) {
                Ok(run_target::Reach::Ran) => ran += 1,
                Ok(other) => panic!(
                    "nonce {nonce}: a well-typed program only reached {other:?}\n--- program ---\n{src}"
                ),
                Err(violation) => panic!("nonce {nonce}: {violation}\n--- program ---\n{src}"),
            }
        }
        eprintln!("typed generator: {ran}/500 ran, agreeing across both backends");
    });
}
