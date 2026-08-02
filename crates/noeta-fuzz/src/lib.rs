//! Structured input generation for fuzzing the Noeta toolchain.
//!
//! The crate is deliberately small and driver-agnostic: [`generate`] turns a `&[u8]` into a
//! syntactically valid Noeta program, and everything else is a way to point that at some component
//! and assert a property. Today the target is the formatter ([`noeta_fmt::oracle`]); the same
//! generator is what a checker, VM or backend differential would consume, because the expensive
//! part of fuzzing a language is producing inputs that get *past* the parser, and that part is
//! shared.
//!
//! # Why this is not just more corpus
//!
//! The repository already sweeps ~1,200 corpus files through the formatter on every run. Those are
//! real programs, and each has exactly one layout: the one its author wrote and `noeta fmt` has
//! already normalized. What they cannot vary is the *input* layout — collapsed one-line bodies,
//! comments at odd depths, a broken method chain with a comment between links, a header in
//! redundant parentheses, semicolons on some statements and not others. Those are the inputs the
//! formatter has to be a fixed point over, and they are exactly what this generates.
//!
//! # Keeping it honest
//!
//! A structured generator fails silently: if it drifts into emitting unparseable text, the
//! formatter declines every input, every property passes vacuously, and the suite stays green
//! while testing nothing. [`parse_rate`] measures the fraction of generated programs that parse
//! cleanly, and `tests/generator.rs` asserts a floor on it. Any change to [`generate`] that breaks the
//! grammar fails that test rather than quietly hollowing out the suite.

pub mod bundle_target;
pub mod census;
pub mod fmt_target;
pub mod generate;
/// The tier-1 JIT differential. Feature-gated so the default build does not pull Cranelift —
/// the same gating `noeta-conformance` uses for the corpus version of this oracle.
#[cfg(feature = "jit")]
pub mod jit_target;
pub mod leak_target;
pub mod run_target;
pub mod typed;

use noeta_span::{Source, SourceId};

/// Whether `src` lexes and parses with no diagnostics.
///
/// This is the formatter's own admission test: `format_source` declines anything that does not
/// parse cleanly, so a generated program that fails here contributes nothing to any property.
pub fn parses_cleanly(src: &str) -> bool {
    let source = Source::new(SourceId(0), "fuzz.noe", src);
    let lexed = noeta_lexer::lex(&source);
    if !lexed.diagnostics.is_empty() {
        return false;
    }
    noeta_parser::parse(&source, &lexed.tokens)
        .diagnostics
        .is_empty()
}

/// The fraction of `count` generated programs that parse cleanly, in `0.0..=1.0`.
///
/// Seeds are derived from `seed` deterministically, so the measurement is reproducible. This is the
/// generator's own health metric — see the crate docs for why it is asserted rather than merely
/// reported.
pub fn parse_rate(count: u32, seed: u64) -> f64 {
    let mut ok = 0u32;
    for i in 0..count {
        let bytes = seed_bytes(seed, i);
        if parses_cleanly(&generate::program(&bytes)) {
            ok += 1;
        }
    }
    f64::from(ok) / f64::from(count.max(1))
}

/// A deterministic pseudo-random byte buffer for seed `(seed, nonce)`.
///
/// SplitMix64, inlined: the driver only needs bytes that vary, and depending on a PRNG crate for
/// that would be a dependency the fuzzer itself does not need — a libFuzzer target supplies its own
/// bytes and never calls this.
pub fn seed_bytes(seed: u64, nonce: u32) -> Vec<u8> {
    let mut state = seed ^ (u64::from(nonce).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut out = Vec::with_capacity(512);
    for _ in 0..64 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out
}
