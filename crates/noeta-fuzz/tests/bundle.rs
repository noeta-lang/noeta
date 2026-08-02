//! The `.noeb` container, fuzzed.
//!
//! Two properties, and one guard that keeps them from going hollow.
//!
//! **Totality.** No byte string makes a reader panic. `read`, `is_bundle`, `stapled_len` and
//! `extract_stapled` all take bytes straight off disk, and the startup cache reads a bundle on
//! *every* run — so a panic on a half-written cache entry is a crash on a path nobody thinks about.
//! Errors are data; a panic is not.
//!
//! **Round-trip.** `read(write(m))` recovers `m`. The existing tests pin this for one hand-built
//! canonical module; this runs it over modules compiled from generated programs, so the encoder
//! meets shapes, packed schemas and method tables it was not hand-fed.
//!
//! **Depth.** A reader that rejects everything at the magic byte satisfies totality trivially. The
//! corruption strategies therefore keep inputs plausible, and the suite asserts that a real share of
//! them reach the payload or beyond — the same anti-vacuity discipline the program generator's
//! parse-rate floor enforces, applied to a binary format.

use noeta_fuzz::bundle_target::{self, Reach};
use proptest::prelude::*;

/// The base seed this suite walks. A failure reports the nonce, which reproduces the module.
const BASE_SEED: u64 = 0xB0114E;

/// Nonces walked by the round-trip sweep. Most yield no module: the generator is a *syntax*
/// generator, and `module_for` requires a program the checker accepts, which under a tenth of them
/// are. The sweep is sized so the survivors are still a meaningful population.
const NONCES: u32 = 3_000;

/// The floor on modules actually round-tripped. Stated as an absolute rather than a fraction,
/// because the fraction is expected to be small and the thing that matters is that the property
/// runs at all.
const MIN_MODULES: u32 = 100;

/// The first generated module at or after nonce 0 — the seed bundle for the corruption arms.
///
/// Searched rather than pinned at nonce 0: `module_for` requires a program the checker accepts, and
/// which nonce first clears that bar is not a stable fact about the generator.
fn seed_module() -> noeta_bytecode::Module {
    (0..512)
        .find_map(|n| bundle_target::module_for(BASE_SEED, n))
        .expect("some generated program type-checks and compiles")
}

#[test]
fn a_module_survives_the_container() {
    let mut checked = 0u32;
    let mut skipped = 0u32;
    for nonce in 0..NONCES {
        let Some(module) = bundle_target::module_for(BASE_SEED, nonce) else {
            skipped += 1;
            continue;
        };
        let blob = noeta_bundle::write(&module);
        let back = noeta_bundle::read(&blob).unwrap_or_else(|e| {
            panic!("nonce {nonce}: a freshly written bundle did not read: {e}")
        });
        assert_eq!(
            back.encode(),
            module.encode(),
            "nonce {nonce}: the module did not survive write/read"
        );
        checked += 1;
    }
    eprintln!("bundle round-trip: {checked} modules | {skipped} of {NONCES} nonces yielded none");
    // The property is worthless if the survivors dry up, so the floor is asserted rather than
    // assumed — and reported, so a drop shows up as a failure and not as a quietly smaller sweep.
    assert!(
        checked >= MIN_MODULES,
        "only {checked} modules round-tripped (floor {MIN_MODULES}) — something upstream of the \
         container is rejecting programs it used to accept, and this property is barely running."
    );
}

/// Every reader is total over arbitrary bytes. This arm is the shallow one by construction — random
/// bytes almost never carry the magic — and it exists to cover the entry checks themselves.
#[test]
fn the_readers_are_total_over_arbitrary_bytes() {
    for nonce in 0..2_000u32 {
        let bytes = noeta_fuzz::seed_bytes(0xDEADBEEF, nonce);
        for len in [0usize, 1, 4, 7, 16, 64, bytes.len()] {
            let slice = &bytes[..len.min(bytes.len())];
            // Each must return, whatever it returns.
            let _ = noeta_bundle::read(slice);
            let _ = noeta_bundle::is_bundle(slice);
            let _ = noeta_bundle::stapled_len(slice);
            let _ = noeta_bundle::extract_stapled(slice);
        }
    }
}

/// The deep arm: corrupt a *valid* bundle and assert the reader still only ever returns.
///
/// The reach histogram is printed and its tail asserted, because "no panic" is satisfied just as
/// well by rejecting everything at offset 0 — and that would test nothing at all.
#[test]
fn a_corrupted_bundle_is_rejected_rather_than_fatal() {
    let blob = noeta_bundle::write(&seed_module());

    let mut deep = 0u32;
    let mut total = 0u32;
    let mut histogram = std::collections::BTreeMap::<Reach, u32>::new();
    for nonce in 0..4_000u32 {
        let damaged = bundle_target::corrupt(&blob, &noeta_fuzz::seed_bytes(0xC0117, nonce));
        // The point of the arm: this must return, not unwind.
        let _ = noeta_bundle::read(&damaged);
        let _ = noeta_bundle::extract_stapled(&damaged);
        let r = bundle_target::reach(&damaged);
        *histogram.entry(r).or_default() += 1;
        if r >= Reach::Payload {
            deep += 1;
        }
        total += 1;
    }
    eprintln!("bundle corruption reach: {histogram:?}");
    assert!(
        deep * 4 > total,
        "only {deep}/{total} corruptions reached the payload — the corruption strategies are \
         being rejected at the header, so this proves nothing about the decoder: {histogram:?}"
    );
}

/// A stapled executable's trailer carries an attacker-supplied length. Whatever it says, recovering
/// the bundle must return rather than index out of bounds.
#[test]
fn a_stapled_trailer_length_is_never_trusted_into_a_panic() {
    let blob = noeta_bundle::write(&seed_module());
    let image = noeta_bundle::staple(b"fake runtime image bytes", &blob);

    for nonce in 0..2_000u32 {
        let bytes = noeta_fuzz::seed_bytes(0x5747, nonce);
        let mut damaged = image.clone();
        // Rewrite the 8-byte little-endian length in the trailer with anything at all, including
        // lengths far beyond the image.
        let len_at = damaged.len() - noeta_bundle::TRAILER_LEN;
        damaged[len_at..len_at + 8].copy_from_slice(&bytes[..8]);
        let recovered = noeta_bundle::extract_stapled(&damaged);
        // Whatever comes back must be a slice of the image, and reading it must also return.
        if let Some(inner) = recovered {
            assert!(inner.len() <= damaged.len());
            let _ = noeta_bundle::read(inner);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Totality under proptest, which shrinks a failing byte string to something readable.
    #[test]
    fn read_is_total(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = noeta_bundle::read(&bytes);
        let _ = noeta_bundle::is_bundle(&bytes);
        let _ = noeta_bundle::stapled_len(&bytes);
        let _ = noeta_bundle::extract_stapled(&bytes);
    }

    /// And totality over inputs that *start* valid, which is where the decoder actually runs.
    #[test]
    fn read_is_total_over_corrupted_bundles(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let blob = noeta_bundle::write(&seed_module());
        let damaged = bundle_target::corrupt(&blob, &bytes);
        let _ = noeta_bundle::read(&damaged);
        let _ = noeta_bundle::extract_stapled(&damaged);
    }
}
