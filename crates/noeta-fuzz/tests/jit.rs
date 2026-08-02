//! The tier-1 JIT against the interpreter, over generated programs.
//!
//! See [`noeta_fuzz::jit_target`] for what this checks and why the interpreter-vs-interpreter
//! differential cannot substitute for it: native code is where miri cannot look, and where a
//! missing retain has nowhere else to show.
//!
//! Runs only with `--features jit`; without it the whole file compiles away, exactly as
//! `noeta-conformance` gates the corpus version.

#![cfg(feature = "jit")]

use noeta_fuzz::jit_target::{self, BASE_SEED};
use noeta_fuzz::run_target::Reach;

const DEEP_STACK: usize = 64 * 1024 * 1024;

/// Programs swept by the gate. Smaller than the other targets': each program is compiled by
/// Cranelift and run twice.
const NONCES: u32 = 750;

/// The floor on programs that reached both tiers.
const MIN_RAN: u32 = 25;

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
fn tier_one_agrees_with_the_interpreter_over_generated_programs() {
    on_deep_stack(|| {
        let mut ran = 0u32;
        for nonce in 0..NONCES {
            let src = jit_target::source(BASE_SEED, nonce);
            match jit_target::jit_check(&src) {
                Ok(Reach::Ran) => ran += 1,
                Ok(_) => {}
                Err(violation) => panic!("nonce {nonce}: {violation}\n--- program ---\n{src}"),
            }
        }
        eprintln!("jit differential: {ran} of {NONCES} generated programs ran on both tiers");
        assert!(
            ran >= MIN_RAN,
            "only {ran} of {NONCES} generated programs reached both tiers (floor {MIN_RAN}) — \
             nothing was compared, which looks exactly like nothing disagreeing."
        );
    });
}
