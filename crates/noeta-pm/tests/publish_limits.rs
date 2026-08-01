//! The publish limits' boundary, from the client side (audit row 4, item 4).
//!
//! `package.description` is validated twice: by
//! [`noeta_pm::manifest::validate_description`] so a bad blurb fails at `noeta check` instead of at
//! publish, and by the registry Worker's `validateDescription` so the index never stores one. Two
//! implementations of one rule — and they disagreed about the **units**:
//!
//! | | length | control characters |
//! |---|---|---|
//! | client (Rust) | `chars().count()` — Unicode scalar values | `char::is_control` — the whole `Cc` category |
//! | registry (TS) | `.length` — UTF-16 code units | an ASCII-only regex |
//!
//! So an emoji cost two toward a "200 characters" limit on the server and one on the client (a
//! description `noeta check` accepted came back 400 from publish), and U+0085 was client-rejected
//! and server-accepted (the index would store a blurb no publisher could have written). Both sides
//! now use the Rust semantics — code points, and the full `Cc` category — because that is what a
//! human means by "characters" and the client is where the message is actionable.
//!
//! These two tests read the **same fixture files** the Worker's suite feeds to its publish handler,
//! so the boundary is asserted from both ends of the wire against one set of bytes. Ungated, for the
//! same reason as the other cross-repo pins in this crate: a pin behind a feature flag is a pin
//! somebody has to remember.

use noeta_pm::manifest::{MAX_DESCRIPTION, validate_description};

fn fixture_description(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test_data/wire")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
    let value: serde_json::Value = serde_json::from_str(&text).expect(name);
    value["description"]
        .as_str()
        .unwrap_or_else(|| panic!("`{name}` has no string `description`"))
        .to_string()
}

/// Exactly [`MAX_DESCRIPTION`] astral-plane code points is legal — and is *not* 200 UTF-16 units.
#[test]
fn a_description_of_exactly_max_astral_code_points_is_accepted() {
    let desc = fixture_description("publish-request-description-max.json");
    // The fixture IS the boundary. If it stops being both of these, it was regenerated wrong and the
    // assertion below would pass while proving nothing.
    assert_eq!(
        desc.chars().count(),
        MAX_DESCRIPTION,
        "the fixture is no longer exactly MAX_DESCRIPTION code points"
    );
    assert_eq!(
        desc.encode_utf16().count(),
        MAX_DESCRIPTION * 2,
        "the fixture is no longer astral-plane — it must cost two UTF-16 code units per character, \
         or it cannot detect a server counting the wrong unit"
    );
    assert_eq!(
        validate_description(&desc).as_deref(),
        Ok(desc.as_str()),
        "a description of exactly MAX_DESCRIPTION code points was rejected client-side"
    );
}

/// U+0085 (NEL) is a `Cc` control and is illegal — on both sides, now.
#[test]
fn a_description_containing_u0085_is_rejected() {
    let desc = fixture_description("publish-request-description-control.json");
    assert!(
        desc.contains('\u{85}'),
        "the fixture no longer contains U+0085 — it cannot detect a server whose control class \
         shrank back to ASCII"
    );
    assert!(
        desc.chars().count() <= MAX_DESCRIPTION,
        "the fixture must fail on the control character, not on length"
    );
    assert!(
        validate_description(&desc).is_err(),
        "U+0085 was accepted client-side — `char::is_control` covers U+0080–U+009F, and the registry \
         now rejects the same range, so this is a real divergence"
    );
}
