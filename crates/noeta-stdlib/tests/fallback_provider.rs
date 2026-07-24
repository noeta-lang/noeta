//! The registry's **fallback provider** contract (see `noeta_stdlib::registry::
//! register_default_provider`): linking this crate registers `std_units` as the default provider at
//! load time, but registers *only* — it must not install.
//!
//! Both halves matter, and they pull in opposite directions:
//!
//! * If registration did not happen, a binary that never seeds explicitly panics on its first
//!   front-end lookup. That was the old behaviour, and because an assembling binary seeds while a
//!   *test* binary does not, a crate's tests passed only when some sibling test happened to run
//!   first through the lazily-seeding facade — a scheduling race, not a guarantee.
//! * If registration *installed*, every binary composing its own extension set (`install_with_extras`
//!   in `noeta-conformance`'s `extern_identity`/`typed_call_seam`, `noeta-cli`'s doc samples,
//!   an out-of-tree package's conformance) would find std already installed and panic in `install`.
//!
//! This is its own integration target, so it owns a fresh process and can observe the
//! pre-first-lookup state — which is exactly what a `#[cfg(test)]` unit test inside a shared test
//! binary cannot do, whatever order its siblings run in.

/// Nothing is installed until the first lookup, and the first lookup then succeeds on its own —
/// with no `default_seeded()`/`install*` call anywhere in this file.
///
/// **The linkage caveat this test also pins.** Load-time registration lives in `noeta-stdlib`'s
/// object, and a linker only pulls an rlib's object into a binary that references *something* from
/// it. A binary that names no `noeta-stdlib` item at all therefore gets no registration and still
/// panics on first lookup — no worse than before this seam existed, but not the "linking is enough"
/// guarantee it looks like. The reference below is deliberately an inert one (`std_units` as a
/// value, never called) rather than a seeding call: it stands in for the ordinary consumer
/// reference every real front-end test binary already has, and proves that *merely touching* the
/// crate — not seeding through it, and in no particular order — is what the fallback needs.
#[test]
fn registration_does_not_install_but_the_first_lookup_seeds() {
    let _ = noeta_stdlib::registry::std_units;

    // Load-time registration has already run. It must have installed nothing: this is the window a
    // composing binary uses to call `install`/`install_with_extras` and win the `OnceLock`.
    assert!(
        noeta_ext_abi::registry::default_registry().is_none(),
        "registering the fallback provider must not install it — a binary composing its own \
         extension set installs before its first lookup and must not find std already there"
    );

    // The lookup the front-end crates make. Unseeded and with no provider this panics; with the
    // provider registered it installs the std units and answers. No call site had to remember.
    let registry = noeta_ext_abi::registry::single_registry_process();
    assert!(
        matches!(
            registry.classify_use(&["std".to_string()], "math"),
            noeta_ext_abi::registry::UseKind::Module(_)
        ),
        "the lazily-installed default must carry the std units — `use std.math` must classify as \
         a module, not as an unknown target"
    );

    // And it is now genuinely installed, so later lookups share this one registry.
    assert!(noeta_ext_abi::registry::default_registry().is_some());
}
