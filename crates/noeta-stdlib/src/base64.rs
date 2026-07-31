//! The Ring 2 `base64` module — RFC 4648 encoding and decoding over `bytes`, shared by both
//! backends, and its one error story, [`Base64Error`].
//!
//! Base64 is the language's ambient binary-over-text envelope: every LLM provider inlines images
//! and files as base64, MCP resource contents are base64, JWT segments are base64url, and a data
//! URI is base64. Until this module the only way to reach it from Noeta was hex string arithmetic
//! (`bytes.to_hex()` plus a hand-rolled alphabet walk), and *decoding* was not expressible at all —
//! `bytes` had no per-byte read, so a base64 `bytes` value could not be produced in-language. That
//! primitive (`b[i]`, plus `b.slice(start, end?)`) landed alongside this module; the module itself
//! is here so nobody has to write the transform by hand at all.
//!
//! ## Two alphabets, and why both
//!
//! RFC 4648 defines two: §4's **standard** alphabet (`+`, `/`, `=` padding) and §5's **URL-safe**
//! one (`-`, `_`). The module offers both, as four named functions rather than a flag:
//!
//! | Function | Alphabet | Padding it writes |
//! |---|---|---|
//! | [`encode`] / [`decode`] | standard (`+/`) | `=`-padded — RFC 4648 §4 canonical, and what §10's vectors show |
//! | [`encode_url`] / [`decode_url`] | URL-safe (`-_`) | none — what RFC 7515 (JWS/JWT) mandates and what every url-safe API expects |
//!
//! **Why offer URL-safe at all**, when a caller could in principle post-process the standard form
//! with `replace("+", "-")`? Because the substitution trick is right in the easy direction and
//! wrong in the one that matters. A url-safe *decoder* built that way accepts a mixed-alphabet
//! string (`+` and `-` in one token) instead of rejecting it, and re-deriving the padding a decode
//! needs is a `len % 4` calculation people get wrong silently. Verifying a JWT is exactly where a
//! sloppy decoder turns into a security bug, so the strict, alphabet-checked door belongs in the
//! library. The cost is two more registered functions over a crate the toolchain already links.
//!
//! **Why names rather than a `url_safe: bool` / `pad: bool` flag.** A boolean at the call site
//! documents nothing and fails silently: pass the wrong one and the remote party rejects your
//! token, with no local error to point at. The name states the wire format, and the surface reads
//! the way the RFC does.
//!
//! ## What a decode accepts
//!
//! One deliberate asymmetry, stated once here because it is the module's only judgment call:
//! **the alphabet is strict, the padding is not.** Each decoder rejects any character outside its
//! own alphabet — so `decode_url` refuses `+`/`/` and `decode` refuses `-`/`_`, which is what makes
//! them worth having — and each accepts its input padded or unpadded, because a padded and an
//! unpadded token denote the same bytes and real senders differ on which they emit. Non-canonical
//! trailing bits are still rejected (`base64`'s default), so a successful decode round-trips.
//!
//! Decoding is **recoverable** by construction: base64 from a remote party is untrusted input
//! exactly like JSON, so `decode`/`decode_url` return `Result<bytes, Base64Error>` and never abort.

use crate::{ErrorKind, ExternValue, StdError};
use ::base64::Engine;
use ::base64::alphabet;
use ::base64::engine::{GeneralPurpose, GeneralPurposeConfig, general_purpose};
use std::any::Any;
use std::cmp::Ordering;

/// The decode configuration both engines share: **padding-indifferent**, canonical-bits-strict.
/// See the module note — a padded and an unpadded token mean the same bytes, while non-canonical
/// trailing bits would break round-tripping.
const DECODE_CONFIG: GeneralPurposeConfig = GeneralPurposeConfig::new()
    .with_decode_padding_mode(::base64::engine::DecodePaddingMode::Indifferent)
    .with_decode_allow_trailing_bits(false);

/// The standard-alphabet decoder (RFC 4648 §4): accepts `+`/`/`, rejects `-`/`_`.
const STANDARD_DECODER: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, DECODE_CONFIG);

/// The URL-safe-alphabet decoder (RFC 4648 §5): accepts `-`/`_`, rejects `+`/`/`.
const URL_SAFE_DECODER: GeneralPurpose = GeneralPurpose::new(&alphabet::URL_SAFE, DECODE_CONFIG);

/// Encode `data` with the **standard** alphabet, `=`-padded — RFC 4648 §4, the canonical form and
/// the one §10's test vectors show.
pub fn encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

/// Encode `data` with the **URL-safe** alphabet and no padding — RFC 4648 §5 as RFC 7515 (JWS/JWT)
/// requires it, so the result is safe in a URL path, a query parameter, and a filename.
pub fn encode_url(data: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// Decode `text` as **standard**-alphabet base64, padded or not. A character outside the standard
/// alphabet, a truncated group, or non-canonical trailing bits is a [`Base64Error`] — never an
/// abort, because base64 from a remote party is untrusted input exactly like JSON.
pub fn decode(text: &str) -> Result<Vec<u8>, Base64Error> {
    STANDARD_DECODER
        .decode(text)
        .map_err(|e| Base64Error::from_decode(&e, Base64Alphabet::Standard))
}

/// Decode `text` as **URL-safe**-alphabet base64, padded or not — the `encode_url` inverse and the
/// JWT-segment door. Rejects `+`/`/`, which is the whole reason it is a separate function rather
/// than a character substitution over [`decode`].
pub fn decode_url(text: &str) -> Result<Vec<u8>, Base64Error> {
    URL_SAFE_DECODER
        .decode(text)
        .map_err(|e| Base64Error::from_decode(&e, Base64Alphabet::UrlSafe))
}

// --- the error story -----------------------------------------------------------------------------

/// Which alphabet the failing decode was reading — named in the message so "invalid character" is
/// actionable rather than mysterious when a caller reached for the wrong door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Base64Alphabet {
    Standard,
    UrlSafe,
}

impl Base64Alphabet {
    fn label(self) -> &'static str {
        match self {
            Base64Alphabet::Standard => "standard",
            Base64Alphabet::UrlSafe => "url-safe",
        }
    }
}

/// What went wrong in a base64 decode — the kind axis of a [`Base64Error`], mirroring
/// [`crate::json::JsonErrorKind`]: an enum, not a magic string, so the surface `kind()` renders
/// [`Base64ErrorKind::label`] and every interior consumer matches the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64ErrorKind {
    /// A character outside the decoder's alphabet (including a `-`/`_` handed to the standard door,
    /// or a `+`/`/` handed to the URL-safe one). Carries the offset.
    InvalidCharacter,
    /// The encoded text has a length no base64 encoding can produce (a truncated group).
    InvalidLength,
    /// The final symbol carries bits that decode to nothing — a non-canonical encoding, which would
    /// otherwise decode to bytes that do not re-encode to the same text.
    InvalidLastSymbol,
    /// The `=` padding is malformed (misplaced, or the wrong amount). Note the decoders accept
    /// *absent* padding by design; this is padding that is present and wrong.
    InvalidPadding,
}

impl Base64ErrorKind {
    /// The surface label `Base64Error.kind()` returns.
    pub fn label(self) -> &'static str {
        match self {
            Base64ErrorKind::InvalidCharacter => "invalid_character",
            Base64ErrorKind::InvalidLength => "invalid_length",
            Base64ErrorKind::InvalidLastSymbol => "invalid_last_symbol",
            Base64ErrorKind::InvalidPadding => "invalid_padding",
        }
    }
}

/// `Base64Error`'s registered short name (its `ExtType` in the registry).
pub const BASE64_ERROR_TYPE_NAME: &str = "Base64Error";

/// `Base64Error`'s qualified runtime identity (`{namespace}.{name}` of its `ExtType` registration)
/// — what [`ExternValue::type_identity`] returns, and the `Type::Named` key the checker uses for
/// the `decode`/`decode_url` error arms.
pub const BASE64_ERROR_TYPE_IDENTITY: &str = "std.base64.Base64Error";

/// The one base64 decode error — the [`crate::json::JsonError`] model applied to a flat input.
///
/// Carries the failure kind, the byte **offset** into the encoded text where it was detected (the
/// positional analogue of `JsonError`'s path — a base64 document has no structure to walk), and the
/// composed detail sentence. Pure `Send` data with content equality; user code reaches the parts
/// through registered methods (`message`/`kind`/`offset`), and the value displays as its composed
/// [`Base64Error::message`], which is also what `impl Display`'s `to_string()` and `impl Error`'s
/// `message()` return — both declared on its `ExtType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base64Error {
    /// What went wrong.
    pub kind: Base64ErrorKind,
    /// The 0-based byte offset into the encoded text where the failure was detected. `None` only
    /// for [`Base64ErrorKind::InvalidPadding`], which is a property of the whole token rather than
    /// of one position in it.
    pub offset: Option<u32>,
    /// The detail sentence (`invalid url-safe base64 character '+' at offset 3`).
    pub detail: String,
}

impl Base64Error {
    /// Classify a `base64` crate decode failure, naming the alphabet the caller's door was reading
    /// so a wrong-door mistake reads as one.
    fn from_decode(error: &::base64::DecodeError, alphabet: Base64Alphabet) -> Base64Error {
        use ::base64::DecodeError;
        let (kind, offset, detail) = match error {
            DecodeError::InvalidByte(at, byte) => (
                Base64ErrorKind::InvalidCharacter,
                Some(*at),
                format!(
                    "invalid {} base64 character {} at offset {at}",
                    alphabet.label(),
                    render_byte(*byte)
                ),
            ),
            DecodeError::InvalidLength(at) => (
                Base64ErrorKind::InvalidLength,
                Some(*at),
                format!("invalid base64 length: a group is truncated at offset {at}"),
            ),
            DecodeError::InvalidLastSymbol(at, byte) => (
                Base64ErrorKind::InvalidLastSymbol,
                Some(*at),
                format!(
                    "non-canonical base64: the final symbol {} at offset {at} has bits that decode \
                     to nothing",
                    render_byte(*byte)
                ),
            ),
            DecodeError::InvalidPadding => (
                Base64ErrorKind::InvalidPadding,
                None,
                "invalid base64 padding".to_string(),
            ),
        };
        Base64Error {
            kind,
            offset: offset.and_then(|at| u32::try_from(at).ok()),
            detail,
        }
    }

    /// The composed human message — `impl Error`'s `message()`. The offset is already inside the
    /// detail sentence (a flat input has no path to prefix), so this IS the detail.
    pub fn message(&self) -> String {
        self.detail.clone()
    }

    /// The **abort** mapping, for any caller that needs the failure as a `StdError` rather than as a
    /// value. The registered `decode`/`decode_url` doors never take this path — they are recoverable
    /// by construction — so it exists for embedders and future non-`Result` doors.
    pub fn into_std_error(self) -> StdError {
        StdError {
            kind: ErrorKind::ArgType,
            message: self.message(),
        }
    }
}

/// Render an offending byte readably: a printable ASCII character as `'x'`, anything else as its
/// hex escape, so a message about a stray newline or a high byte still says something.
fn render_byte(byte: u8) -> String {
    if byte.is_ascii_graphic() {
        format!("'{}'", byte as char)
    } else {
        format!("0x{byte:02x}")
    }
}

/// `Base64Error` IS a user-facing extern type — pure, host-free, content-equal, not key-capable
/// (the `JsonError` model). It displays as its composed message, so `echo`/interpolation of an
/// `Err(e)` payload reads naturally in both backends by construction.
impl ExternValue for Base64Error {
    fn type_identity(&self) -> &'static str {
        BASE64_ERROR_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Base64Error>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.message())
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10's test vectors — the normative ones, in the RFC's own order.
    const RFC_4648_VECTORS: &[(&str, &str)] = &[
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ];

    #[test]
    fn rfc_4648_vectors_encode() {
        for (plain, encoded) in RFC_4648_VECTORS {
            assert_eq!(encode(plain.as_bytes()), *encoded, "encode({plain:?})");
        }
    }

    #[test]
    fn rfc_4648_vectors_decode() {
        for (plain, encoded) in RFC_4648_VECTORS {
            assert_eq!(
                decode(encoded).unwrap(),
                plain.as_bytes(),
                "decode({encoded:?})"
            );
        }
    }

    #[test]
    fn rfc_4648_vectors_round_trip_url_safe_unpadded() {
        // The URL-safe door writes no padding, so the vectors lose their `=` tails; the bytes are
        // unchanged (`foobar`'s alphabet has no `+`/`/` in it, which is the point of the next test).
        for (plain, encoded) in RFC_4648_VECTORS {
            let url = encode_url(plain.as_bytes());
            assert_eq!(url, encoded.trim_end_matches('='), "encode_url({plain:?})");
            assert_eq!(decode_url(&url).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn the_two_alphabets_differ_exactly_where_the_rfc_says() {
        // 0xFB 0xFF 0xBE exercises both substituted characters: standard `+/`, url-safe `-_`.
        let data = [0xfb, 0xff, 0xbe];
        assert_eq!(encode(&data), "+/++");
        assert_eq!(encode_url(&data), "-_--");
        assert_eq!(decode("+/++").unwrap(), data);
        assert_eq!(decode_url("-_--").unwrap(), data);
        // And each door REJECTS the other's alphabet — the reason url-safe is a real function and
        // not a character substitution the caller could do afterwards.
        assert_eq!(
            decode("-_--").unwrap_err().kind,
            Base64ErrorKind::InvalidCharacter
        );
        assert_eq!(
            decode_url("+/++").unwrap_err().kind,
            Base64ErrorKind::InvalidCharacter
        );
        assert!(
            decode_url("+/++")
                .unwrap_err()
                .message()
                .contains("url-safe"),
            "the message names the alphabet the door was reading"
        );
    }

    #[test]
    fn high_bytes_round_trip_through_both_doors() {
        // Every byte value, so no 8→6-bit regrouping edge is untested.
        let all: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&all)).unwrap(), all);
        assert_eq!(decode_url(&encode_url(&all)).unwrap(), all);
        // A lone high byte, which a signed per-byte read would mangle.
        assert_eq!(encode(&[0xff]), "/w==");
        assert_eq!(decode("/w==").unwrap(), vec![0xff]);
    }

    #[test]
    fn multi_byte_utf8_round_trips_as_its_encoded_bytes() {
        // base64 is byte-oriented: multi-byte text must survive as its UTF-8 encoding, not as chars.
        for text in ["héllo", "日本語", "🎉 emoji", "naïve café"] {
            let bytes = text.as_bytes();
            assert_eq!(decode(&encode(bytes)).unwrap(), bytes, "{text}");
            assert_eq!(decode_url(&encode_url(bytes)).unwrap(), bytes, "{text}");
            // The encoded length is driven by the BYTE count, not the character count.
            assert_eq!(encode(bytes).len(), bytes.len().div_ceil(3) * 4);
        }
        assert_eq!(encode("héllo".as_bytes()), "aMOpbGxv");
    }

    #[test]
    fn padding_is_accepted_either_way_but_the_alphabet_is_not() {
        // The module's one judgment call, pinned: padding indifferent, alphabet strict.
        assert_eq!(decode("Zg==").unwrap(), b"f");
        assert_eq!(decode("Zg").unwrap(), b"f", "unpadded standard is accepted");
        assert_eq!(decode_url("Zg==").unwrap(), b"f", "padded url-safe is too");
        assert_eq!(decode_url("Zg").unwrap(), b"f");
    }

    #[test]
    fn invalid_input_is_a_recoverable_classified_error() {
        // A character in neither alphabet.
        let err = decode("Zm9v!!").unwrap_err();
        assert_eq!(err.kind, Base64ErrorKind::InvalidCharacter);
        assert_eq!(err.offset, Some(4));
        assert!(err.message().contains("'!'"), "{}", err.message());
        assert_eq!(err.kind.label(), "invalid_character");

        // A truncated group: one leftover symbol can never be a whole byte.
        let err = decode("Zm9vY").unwrap_err();
        assert_eq!(err.kind, Base64ErrorKind::InvalidLength);

        // Non-canonical trailing bits — decodes to bytes that would not re-encode to this text.
        let err = decode("Zh==").unwrap_err();
        assert_eq!(err.kind, Base64ErrorKind::InvalidLastSymbol);
        assert!(err.message().contains("non-canonical"), "{}", err.message());

        // A non-graphic offender still renders readably rather than as a raw control byte.
        let err = decode("Zm9v\u{7}A==").unwrap_err();
        assert_eq!(err.kind, Base64ErrorKind::InvalidCharacter);
        assert!(err.message().contains("0x07"), "{}", err.message());
    }

    #[test]
    fn the_error_is_a_content_equal_extern_that_displays_as_its_message() {
        let err = decode("!!").unwrap_err();
        assert_eq!(
            (&err as &dyn ExternValue).display_string(),
            err.message(),
            "displays as its composed message"
        );
        assert_eq!(err.type_identity(), BASE64_ERROR_TYPE_IDENTITY);
        assert!(err.eq_value(&decode("!!").unwrap_err()));
        assert!(!err.eq_value(&decode("Zm9vY").unwrap_err()));
        assert_eq!(err.clone().into_std_error().kind, ErrorKind::ArgType);
    }
}
