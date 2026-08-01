//! **The `ABI_VERSION` changelog gate**: the number and the paragraph that explains it are two
//! hand-maintained halves, and this ties them together.
//!
//! ## Why this and not a digest
//!
//! `noeta-bundle`'s `FORMAT_VERSION` got a structural guard — a canonical `Module` encoded and
//! digested, so a layout change fails until the version *and* the digest move
//! (`noeta-bundle/tests/module_layout_digest.rs`). [`noeta_ext_abi::ABI_VERSION`] looks like the same
//! shape and is not, in one decisive way: **nothing reads it.** It appears exactly once in the tree,
//! at its own declaration. Every extension is compiled from source against the exact toolchain, so
//! an ABI break is a compile error today; the constant is *recorded* for the future
//! dynamically-loaded-extension handshake, not *checked*. There is no artifact to mis-decode, so
//! there is nothing to digest.
//!
//! A digest of the ABI *surface* would also be the wrong instrument here. The constant's own doc
//! says "bump it freely — any change means any change", and the surface spans `registry`, `host`,
//! `ctx`, `stream` and `ring1`; a text digest over all of it would fire on churn that the policy
//! already tells you to bump for, and a gate that fires constantly is one people re-green without
//! reading. The structural half of the ABI's protection already exists and is stronger than a
//! digest: `constraint_fields.rs` parses the declaration types and fails until every new `pub`
//! field is classified.
//!
//! ## What is left, and what this checks
//!
//! What `ABI_VERSION` actually lacks is the tie between the bump and its note. The changelog above
//! it is the whole value of the constant — a version number with no paragraph describing what
//! changed is a digit — and it is exactly as unenforced as `FORMAT_VERSION`'s was. So:
//!
//! - the current version has a `**N** —` paragraph;
//! - no paragraph describes a version the constant has not reached (a note written, the bump
//!   forgotten);
//! - the paragraphs are contiguous from 2, so a bump of two at once cannot hide an unexplained
//!   version in the gap.

use std::path::Path;

fn abi_version_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    let at = src
        .find("pub const ABI_VERSION")
        .expect("`ABI_VERSION` is declared in noeta-ext-abi/src/lib.rs");
    // The doc block is the run of `///` lines immediately above the declaration.
    let head = &src[..at];
    let mut doc: Vec<&str> = Vec::new();
    for line in head.lines().rev() {
        if line.trim_start().starts_with("///") {
            doc.push(line);
        } else if !line.trim().is_empty() {
            break;
        }
    }
    doc.reverse();
    doc.join("\n")
}

/// The versions the changelog explains, in the order they appear.
fn documented_versions(doc: &str) -> Vec<u32> {
    doc.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("/// **")?;
            let (n, tail) = rest.split_once("**")?;
            // `**8** — …` is a changelog head; `**Bump it freely.**` and inline bolds are not.
            tail.trim_start()
                .starts_with('\u{2014}')
                .then(|| n.parse::<u32>().ok())
                .flatten()
        })
        .collect()
}

#[test]
fn the_changelog_explains_exactly_the_current_abi_version() {
    let doc = abi_version_doc();
    let versions = documented_versions(&doc);
    let current = noeta_ext_abi::ABI_VERSION;

    assert!(
        !versions.is_empty(),
        "the `ABI_VERSION` changelog parse found no `**N** —` paragraphs; the doc block's shape \
         changed and this gate is no longer reading it"
    );
    assert!(
        versions.contains(&current),
        "`ABI_VERSION` is {current} but the changelog above it explains only {versions:?}. Every \
         bump owes a paragraph saying what changed in the registration/dispatch contract — that \
         paragraph is the entire value of the constant, since nothing in the tree reads the number \
         itself (it is the handshake a future dynamically-loaded extension will refuse a mismatch \
         with)."
    );
    let ahead: Vec<u32> = versions.iter().copied().filter(|v| *v > current).collect();
    assert!(
        ahead.is_empty(),
        "the changelog explains ABI version(s) {ahead:?} that `ABI_VERSION` ({current}) has not \
         been bumped to — the note landed and the bump did not"
    );

    let expected: Vec<u32> = (2..=current).collect();
    assert_eq!(
        versions, expected,
        "the changelog's versions are not the contiguous run 2..={current} in order. A gap means a \
         bump nobody explained; a repeat or a reorder means the file no longer reads as history."
    );
}
