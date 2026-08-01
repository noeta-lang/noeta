//! The cross-repo fixture pin (audit row 4, item 3) — the half that has to be *un*forgettable.
//!
//! `test_data/wire/` is the canonical registry-protocol fixture set; `noeta-registry` carries a
//! verbatim copy at `test/fixtures/wire/`. Two repos cannot share a file, so the copy is pinned two
//! ways, and this file owns both:
//!
//! 1. **`MANIFEST.sha256` pins the fixtures.** Every fixture hashes to its listed digest, and every
//!    fixture is listed. This catches a local hand-edit.
//! 2. **[`noeta_pm::registry::WIRE_MANIFEST_SHA256`] pins the manifest.** The manifest lives *inside*
//!    the copied directory and travels with it, so (1) alone cannot tell a current copy from a stale
//!    one — each repo hashes its own fixtures against its own manifest and both stay green while the
//!    protocol diverges. The stamp is the one value outside the copied set: the registry repo carries
//!    the identical constant in `src/wire-manifest.ts`, so `cp`-ing fixtures across without
//!    acknowledging the change in source fails the receiving repo's build, by name.
//!
//! Both live in an **integration test with no feature gate** on purpose. The advisory chain, the log
//! verification and the wire round-trips are all behind `registry-http` / `provenance`, which is why
//! roughly 40% of this crate's tests were dead until CI started running `--all-features`. The pin
//! that exists to catch a forgotten step must not itself be behind a step someone can forget, so it
//! uses only `sha2` and `std` from dev-dependencies and runs in every `cargo test -p noeta-pm`.

use sha2::{Digest, Sha256};

fn wire_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/wire")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Every fixture hashes to its `MANIFEST.sha256` entry, and every fixture is listed.
#[test]
fn manifest_pins_the_fixture_bytes() {
    let dir = wire_dir();
    let manifest = std::fs::read_to_string(dir.join("MANIFEST.sha256")).expect("MANIFEST.sha256");
    let mut listed = std::collections::BTreeSet::new();
    for line in manifest.lines() {
        let (hash, name) = line.split_once("  ").expect("sha256sum format");
        listed.insert(name.to_string());
        let bytes = std::fs::read(dir.join(name))
            .unwrap_or_else(|err| panic!("fixture `{name}` listed but unreadable: {err}"));
        assert_eq!(
            sha256_hex(&bytes),
            hash,
            "fixture `{name}` does not match MANIFEST.sha256 — run `scripts/sync-wire-fixtures.sh`, \
             which regenerates the manifest, re-stamps both repos and copies the set across"
        );
    }
    for entry in std::fs::read_dir(&dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".json") {
            assert!(
                listed.contains(&name),
                "fixture `{name}` is not in MANIFEST.sha256 — run `scripts/sync-wire-fixtures.sh`"
            );
        }
    }
}

/// And the manifest itself hashes to the source stamp — the value the registry repo must carry too.
///
/// This is the assertion that makes an un-propagated regeneration loud instead of silent. Failing
/// here means the fixtures changed and the stamp did not; failing in `noeta-registry` means the
/// fixtures were copied across and *its* stamp did not.
#[test]
fn the_source_stamp_pins_the_manifest() {
    let manifest = std::fs::read(wire_dir().join("MANIFEST.sha256")).expect("MANIFEST.sha256");
    let actual = sha256_hex(&manifest);
    assert_eq!(
        actual,
        noeta_pm::registry::WIRE_MANIFEST_SHA256,
        "`MANIFEST.sha256` no longer hashes to `noeta_pm::registry::WIRE_MANIFEST_SHA256`.\n\
         \n\
         The wire fixtures changed. That is a REGISTRY PROTOCOL CHANGE, and it is only half done: \
         the copy in `noeta-registry/test/fixtures/wire/` and the matching stamp in \
         `noeta-registry/src/wire-manifest.ts` have to move with it, or the two repos will speak \
         different bytes while both test suites stay green.\n\
         \n\
         Run `scripts/sync-wire-fixtures.sh` (regenerates, re-stamps BOTH repos, copies the set), \
         then `cargo test -p noeta-pm --all-features` here and `pnpm test` there. Do not paste \
         {actual} into the constant by hand — that fixes this test and nothing else."
    );
}
