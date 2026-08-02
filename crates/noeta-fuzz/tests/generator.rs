//! The generator's own health check.
//!
//! Every property this crate asserts is conditional on the generated program actually parsing: if
//! it does not, the component under test declines it and the property holds trivially. A generator
//! that drifted into emitting garbage would therefore turn the whole suite green while testing
//! nothing — the specific way a structured fuzzer fails silently.
//!
//! So the parse rate is asserted, not merely reported. The floor is set well under the rate the
//! generator actually achieves (100% at the time of writing) so that ordinary grammar evolution
//! does not fail the build, while a real regression does.

/// The rate must stay high enough that the properties are not vacuous.
#[test]
fn generated_programs_parse() {
    let rate = noeta_fuzz::parse_rate(2_000, 0x5EED);
    assert!(
        rate > 0.95,
        "generator parse rate fell to {:.1}% — properties over generated programs are passing \
         vacuously. Run `cargo run -p noeta-fuzz --example sample -- bad 20` to see what stopped \
         parsing.",
        rate * 100.0
    );
}

/// Generation is **total**: it returns for any driver bytes, including none at all, and what it
/// returns always parses. Exhaustion winds generation down rather than truncating it mid-token —
/// the property the whole entropy contract rests on.
#[test]
fn generation_is_total_for_any_input() {
    for bytes in [
        vec![],
        vec![0u8],
        vec![0xFF; 1],
        vec![0xFF; 3],
        vec![0x55; 17],
        (0..=255u8).collect(),
    ] {
        let src = noeta_fuzz::generate::program(&bytes);
        assert!(
            noeta_fuzz::parses_cleanly(&src),
            "generated program does not parse for {} driver byte(s):\n{src}",
            bytes.len()
        );
    }
}

/// The same bytes always produce the same program. Reproduction from a reported seed depends on it,
/// and so does the deterministic sweep being a test rather than a coin flip.
#[test]
fn generation_is_deterministic() {
    for nonce in 0..64 {
        let bytes = noeta_fuzz::seed_bytes(0xD37, nonce);
        assert_eq!(
            noeta_fuzz::generate::program(&bytes),
            noeta_fuzz::generate::program(&bytes),
            "generation differed between two runs on the same bytes"
        );
    }
}
