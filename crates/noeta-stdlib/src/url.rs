//! `std.http.url` — percent-encoding and percent-decoding (RFC 3986).
//!
//! This is native for one reason: **the transformation is over bytes, not characters.** A
//! multi-byte character encodes to one `%XX` per UTF-8 byte, and decoding has to reassemble those
//! bytes before they are a character again. Noeta's string surface is Unicode-scalar-based, so a
//! `.noe` implementation can encode ASCII correctly and silently mangle everything else — which is
//! exactly the bug class a query parameter carrying a name, a city, or an emoji hits first.
//!
//! It lives beside `http.client`/`http.server` because every consumer of it is HTTP: building a
//! query string, reading one back off a URL, escaping a path segment. `Request.query(name)` already
//! decodes in the host, so a *served* request never needs this — it is what a program reaches for
//! when it parses a URL itself (a test harness driving a router with `"/pets?status=for%20sale"`,
//! a client assembling a query, a redirect target being taken apart).
//!
//! **One codec, not two.** The implementation is [`noeta_ext_abi::net`]'s, which is also what the
//! request path already decodes through (`Request.query`, `server.parse_form`) — so what a program
//! decodes by hand and what the host decoded for it can never disagree. This module is the *name*:
//! a URL codec is not a server's concern (a client needs it just as much), so it addresses under
//! `http.url` rather than hanging off `http.server`.

/// Percent-encode one component, leaving the RFC 3986 unreserved set alone.
///
/// The unreserved set is `A-Z a-z 0-9 - _ . ~`; everything else becomes `%XX` per UTF-8 byte,
/// uppercase-hex (RFC 3986 §2.1 prefers uppercase, and a decoder accepts either). A space is
/// `%20`, not `+`: both decode to a space in a query, and `%20` is the form that is *also* correct
/// inside a path segment.
///
/// This encodes a **component** — one name or one value — so `&`, `=`, `?`, `#` and `/` are all
/// escaped. Encoding a whole query string or a whole path with it would escape the separators that
/// give it structure; encode the pieces, then join them.
pub fn encode(value: &str) -> String {
    noeta_ext_abi::net::percent_encode(value)
}

/// Percent-decode one component: every `%XX` back to its byte.
///
/// The exact inverse of [`encode`], and **only** that: a `+` stays a `+`. The rule that a plus
/// means a space belongs to `application/x-www-form-urlencoded` — a *query string*'s encoding — not
/// to URLs, where a plus in a path segment is a literal plus. A query parser does that substitution
/// itself, before decoding (`%2B` then still arrives as a literal `+`, which is the point); that is
/// exactly what `server.parse_form` and `Request.query` do, through the same codec. This matches
/// `decodeURIComponent` and Python's `unquote`, both of which keep the form rule in a separate
/// function for the same reason.
///
/// Decoding is total — it always answers a string. Two inputs a stricter decoder would reject are
/// passed through verbatim instead, because both occur in the wild and neither is worth failing a
/// request over:
///
/// * a `%` that does not begin a valid pair (`100% sure`) stays a literal `%`;
/// * bytes that do not form valid UTF-8 are replaced (`U+FFFD`) rather than dropped, so the result
///   is still a string and the damage is visible.
pub fn decode(value: &str) -> String {
    noeta_ext_abi::net::percent_decode(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unreserved_set_is_left_alone() {
        assert_eq!(encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
    }

    #[test]
    fn every_character_that_would_forge_a_parameter_is_escaped() {
        // These are the ones that turn one parameter into two, or a query into a fragment — the
        // whole reason a component is encoded rather than concatenated.
        assert_eq!(encode("a&b=c?d#e"), "a%26b%3Dc%3Fd%23e");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("100%"), "100%25");
        assert_eq!(encode("a/b"), "a%2Fb");
    }

    #[test]
    fn encoding_is_over_bytes_not_characters() {
        // One `%XX` per UTF-8 byte. Encoding per `char` would emit something no server decodes.
        assert_eq!(encode("é"), "%C3%A9");
        assert_eq!(encode("日"), "%E6%97%A5");
    }

    #[test]
    fn decoding_reassembles_multi_byte_characters() {
        // The property a string-only decoder cannot have: the two escapes are ONE character.
        assert_eq!(decode("%C3%A9"), "é");
        assert_eq!(decode("%E6%97%A5"), "日");
        assert_eq!(decode("%C3%A9").len(), "é".len());
    }

    #[test]
    fn decode_inverts_encode_for_every_shape() {
        for original in ["a b", "a&b=c?d#e", "100%", "é日", "", "plain"] {
            assert_eq!(decode(&encode(original)), original, "{original}");
        }
    }

    #[test]
    fn a_plus_stays_a_plus_because_that_rule_belongs_to_form_encoding() {
        // In a path segment a `+` is a literal plus. A query parser substitutes it for a space
        // BEFORE decoding, which is why this function must not: doing both would turn every `%2B`
        // into a space too. `server.parse_form` is the flavor that does.
        assert_eq!(decode("a+b"), "a+b");
        assert_eq!(decode(&encode("a+b")), "a+b");
        assert_eq!(decode("a%2Bb"), "a+b");
    }

    #[test]
    fn a_stray_percent_is_data_rather_than_a_decode_failure() {
        // Real query strings carry these. Failing the whole request over one is worse than reading
        // it as what it plainly is.
        assert_eq!(decode("100% sure"), "100% sure");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("%A"), "%A");
    }

    #[test]
    fn invalid_utf8_is_replaced_rather_than_dropped() {
        // `%FF` is not a valid UTF-8 lead byte; the answer is still a string, and the damage shows.
        assert_eq!(decode("%FF"), "\u{FFFD}");
    }
}
