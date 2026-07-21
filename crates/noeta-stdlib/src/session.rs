//! `std.session` — signed, stateless sessions carried in a cookie.
//!
//! The state rides on the request rather than in the server, which is not a stylistic choice: a
//! `noeta serve --parallel N` gives every worker its own host and its own retained arena, so the
//! obvious in-memory implementation — a `Cell<Map<…>>` the handler captures — is correct at
//! `--parallel 1` and silently fragments above it. A session written on worker 2 is invisible to
//! the others, and requests bounce between them. The bug appears only under the flag you reach for
//! in production and presents as random logouts. A signed cookie has no such failure mode: every
//! worker can read it and none had to have written it.
//!
//! It also needs no framework hook — read a `Request`, write a `Response` — so it composes with a
//! bare `server.serve` handler and with any router built on top.
//!
//! ## The token
//!
//! ```text
//! base64url(payload) "." base64url(hmac_sha256(key, base64url(payload)))
//! ```
//!
//! where `payload` is `{"d": {…}, "exp": <unix seconds>}`. The data nests under `d` so there is no
//! reserved key name a caller could collide with.
//!
//! Three properties are load-bearing, and each is pinned by a test rather than only a comment:
//!
//! * **The MAC is verified before the payload is parsed.** Attacker-controlled bytes never reach
//!   the JSON parser unauthenticated. This ordering is the whole security argument.
//! * **`exp` is mandatory.** With no store there is nothing to revoke against, so an unbounded
//!   token is valid forever and a stolen one is stolen for good. Expiry is a parameter, not an
//!   option.
//! * **Verification tries every key; signing uses the first.** Rotation that logs everyone out is
//!   rotation nobody performs.

use std::any::Any;
use std::cmp::Ordering;
use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use noeta_ext_abi::ExternValue;
use noeta_ext_abi::{ErrorKind, StdError};

pub const KEYRING_TYPE_NAME: &str = "Keyring";
pub const KEYRING_TYPE_IDENTITY: &str = "std.session.Keyring";
pub const SESSION_TYPE_NAME: &str = "Session";
pub const SESSION_TYPE_IDENTITY: &str = "std.session.Session";

/// The cookie a session rides in.
///
/// Fixed rather than configurable: two names would have to agree across `open` and `attach`, and a
/// mismatch fails as "the user is silently logged out", which is exactly the class of bug this
/// module exists to avoid. An application that needs a different name — or a different carrier
/// entirely, such as an `Authorization` header — uses [`encode`]/[`decode`] directly.
pub const COOKIE_NAME: &str = "session";

/// The largest token we will emit, in bytes.
///
/// Browsers cap a cookie at about 4 KB and silently drop anything larger, so the ceiling is real
/// whether or not we enforce it. Erroring is the only honest option: truncating loses state, and
/// emitting an over-long cookie loses the whole session at the browser with nothing in the logs.
/// Hitting this is the signal to move to a server-side store.
pub const MAX_TOKEN_BYTES: usize = 4096;

/// The signing keys, newest first.
///
/// A list rather than one secret so a key can be rotated without invalidating every live session:
/// signing uses the first, verification accepts any. Drop the old key once its sessions have aged
/// past their expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyring {
    pub secrets: Vec<Vec<u8>>,
}

impl Keyring {
    pub fn new(secrets: Vec<Vec<u8>>) -> Result<Keyring, StdError> {
        if secrets.is_empty() {
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: "a session keyring needs at least one secret".to_string(),
            });
        }
        if let Some(short) = secrets.iter().find(|s| s.len() < 16) {
            // Not a style rule: an HMAC key shorter than the hash's block is a weak key, and a
            // session secret is exactly the thing people paste a short placeholder into.
            return Err(StdError {
                kind: ErrorKind::ArgType,
                message: format!(
                    "a session secret must be at least 16 bytes, got {}. Generate one with \
                     `crypto.random_bytes(32).to_hex()` and load it from the environment.",
                    short.len()
                ),
            });
        }
        Ok(Keyring { secrets })
    }
}

/// Session data plus whether it changed.
///
/// Copy-modify like `Response` and `Cookie`: `set` returns a new `Session`. That keeps one mutation
/// story across the whole HTTP surface, and it is what makes `dirty` trustworthy — the flag is set
/// exactly where a builder ran, never by an aliased handle mutating underneath.
///
/// A `BTreeMap` so the encoding is deterministic: the same data always produces the same token,
/// which makes the tests meaningful and the differential oracle happy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Session {
    pub data: BTreeMap<String, String>,
    pub dirty: bool,
}

impl Session {
    pub fn with(&self, name: &str, value: &str) -> Session {
        let mut next = self.clone();
        next.data.insert(name.to_string(), value.to_string());
        next.dirty = true;
        next
    }

    pub fn without(&self, name: &str) -> Session {
        let mut next = self.clone();
        // Only dirty if something actually went — otherwise a speculative `remove` on every request
        // would re-emit the cookie forever and quietly extend its own expiry.
        if next.data.remove(name).is_some() {
            next.dirty = true;
        }
        next
    }

    pub fn cleared(&self) -> Session {
        if self.data.is_empty() {
            return self.clone();
        }
        Session {
            data: BTreeMap::new(),
            dirty: true,
        }
    }
}

/// Build the payload JSON: `{"d": {…}, "exp": N}`. The data nests under `d` so no key name is
/// reserved. Written against `serde_json::Value` rather than a derived struct to keep this crate
/// free of a `serde` derive dependency for one shape.
fn payload_json(data: &BTreeMap<String, String>, exp: u64) -> serde_json::Value {
    let d: serde_json::Map<String, serde_json::Value> = data
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    serde_json::json!({ "d": d, "exp": exp })
}

/// The inverse of [`payload_json`], total: any shape that is not exactly what we emit is `None`.
/// A non-string value is rejected rather than coerced — the data is ours, so anything else means
/// the token was not produced by this module.
fn payload_parse(json: &[u8]) -> Option<(BTreeMap<String, String>, u64)> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let exp = value.get("exp")?.as_u64()?;
    let mut data = BTreeMap::new();
    for (k, v) in value.get("d")?.as_object()? {
        data.insert(k.clone(), v.as_str()?.to_string());
    }
    Some((data, exp))
}

/// Sign `data`, valid for `max_age` seconds from `now_ms`.
pub fn encode(
    data: &BTreeMap<String, String>,
    keys: &Keyring,
    max_age: i64,
    now_ms: u64,
) -> Result<String, StdError> {
    if max_age <= 0 {
        return Err(StdError {
            kind: ErrorKind::ArgType,
            message: format!("a session `max_age` must be positive, got {max_age}"),
        });
    }
    let payload = payload_json(data, now_ms / 1000 + max_age as u64);
    let json = serde_json::to_vec(&payload).map_err(|e| StdError {
        kind: ErrorKind::ArgType,
        message: format!("a session payload could not be encoded: {e}"),
    })?;
    let body = B64.encode(json);
    let tag = crate::crypto::hmac_sha256(&keys.secrets[0], body.as_bytes());
    let token = format!("{body}.{}", B64.encode(tag));
    if token.len() > MAX_TOKEN_BYTES {
        return Err(StdError {
            kind: ErrorKind::ArgType,
            message: format!(
                "this session is {} bytes, over the {MAX_TOKEN_BYTES}-byte cookie limit — a \
                 browser would drop it silently. Store less in the session, or move to a \
                 server-side store keyed by a small id.",
                token.len()
            ),
        });
    }
    Ok(token)
}

/// Verify and decode a token, or `None`.
///
/// Every rejection — bad shape, bad signature, expired — answers `None`. The caller has exactly one
/// correct response to all three (treat the request as unauthenticated), and distinguishing them
/// would tell an attacker which of their guesses was closer.
pub fn decode(token: &str, keys: &Keyring, now_ms: u64) -> Option<BTreeMap<String, String>> {
    let (body, tag_b64) = token.split_once('.')?;
    let tag = B64.decode(tag_b64).ok()?;

    // Authenticate FIRST. Nothing below this line may run on unverified input — `verify_slice` is
    // constant-time, so a wrong tag also leaks nothing through timing.
    if !keys
        .secrets
        .iter()
        .any(|secret| crate::crypto::hmac_sha256_verify(secret, body.as_bytes(), &tag))
    {
        return None;
    }

    let json = B64.decode(body).ok()?;
    let (data, exp) = payload_parse(&json)?;
    if exp <= now_ms / 1000 {
        return None;
    }
    Some(data)
}

impl ExternValue for Keyring {
    fn type_identity(&self) -> &'static str {
        KEYRING_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Keyring>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        // Never the secrets — a keyring interpolated into a log line or a panic message must not
        // hand over the ability to forge every session.
        write!(out, "<keyring {} key(s)>", self.secrets.len())
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

impl ExternValue for Session {
    fn type_identity(&self) -> &'static str {
        SESSION_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Session>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        // Key names but never values: a session's values are credentials often enough that
        // interpolating one should not leak it.
        write!(out, "<session {} entry(s)>", self.data.len())
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

    fn keys() -> Keyring {
        Keyring::new(vec![b"0123456789abcdef0123456789abcdef".to_vec()]).expect("valid")
    }

    fn data(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const NOW: u64 = 1_000_000_000_000;

    #[test]
    fn round_trips() {
        let d = data(&[("user", "42"), ("role", "admin")]);
        let token = encode(&d, &keys(), 3600, NOW).expect("encodes");
        assert_eq!(decode(&token, &keys(), NOW), Some(d));
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let token = encode(&data(&[("role", "user")]), &keys(), 3600, NOW).expect("encodes");
        let (body, tag) = token.split_once('.').expect("two parts");
        // Re-sign nothing — just swap the payload for one claiming admin, the attack this exists
        // to stop.
        let forged_body = B64.encode(
            serde_json::to_vec(&payload_json(
                &data(&[("role", "admin")]),
                NOW / 1000 + 3600,
            ))
            .expect("encodes"),
        );
        assert_ne!(forged_body, body);
        assert_eq!(decode(&format!("{forged_body}.{tag}"), &keys(), NOW), None);
    }

    #[test]
    fn an_expired_token_is_rejected() {
        let token = encode(&data(&[("user", "42")]), &keys(), 60, NOW).expect("encodes");
        assert!(decode(&token, &keys(), NOW + 59_000).is_some());
        assert_eq!(decode(&token, &keys(), NOW + 61_000), None);
    }

    #[test]
    fn a_rotated_key_still_verifies_old_sessions() {
        let old = keys();
        let token = encode(&data(&[("user", "42")]), &old, 3600, NOW).expect("encodes");
        let rotated = Keyring::new(vec![
            b"NEWNEWNEWNEWNEWNEWNEWNEWNEWNEWNE".to_vec(),
            old.secrets[0].clone(),
        ])
        .expect("valid");
        // The old session survives the rotation…
        assert!(decode(&token, &rotated, NOW).is_some());
        // …but new tokens are signed with the new key, so dropping the old one later is safe.
        let fresh = encode(&data(&[("user", "7")]), &rotated, 3600, NOW).expect("encodes");
        let new_only = Keyring::new(vec![rotated.secrets[0].clone()]).expect("valid");
        assert!(decode(&fresh, &new_only, NOW).is_some());
        assert_eq!(decode(&token, &new_only, NOW), None);
    }

    #[test]
    fn garbage_never_reaches_the_json_parser() {
        // Valid base64 of valid JSON, but unsigned: it must die at the MAC, not at the parser.
        let body = B64.encode(
            serde_json::to_vec(&payload_json(
                &data(&[("role", "admin")]),
                NOW / 1000 + 3600,
            ))
            .expect("encodes"),
        );
        assert_eq!(
            decode(&format!("{body}.{}", B64.encode([0u8; 32])), &keys(), NOW),
            None
        );
        // And the structurally broken cases are all `None`, never a panic.
        for bad in ["", ".", "a.b", "no-dot", "!!!.!!!"] {
            assert_eq!(decode(bad, &keys(), NOW), None, "{bad:?}");
        }
    }

    #[test]
    fn an_oversized_session_errors_rather_than_being_dropped_by_the_browser() {
        let big = data(&[("blob", &"x".repeat(MAX_TOKEN_BYTES))]);
        let err = encode(&big, &keys(), 3600, NOW).expect_err("over the cookie limit");
        assert!(err.message.contains("server-side store"), "{}", err.message);
    }

    #[test]
    fn a_weak_or_absent_secret_is_refused() {
        assert!(Keyring::new(vec![]).is_err());
        assert!(Keyring::new(vec![b"short".to_vec()]).is_err());
    }

    #[test]
    fn dirty_tracks_real_change_only() {
        let s = Session::default();
        assert!(!s.dirty);
        assert!(s.with("a", "1").dirty);
        // Removing an absent key changes nothing, so it must not re-emit the cookie and silently
        // extend its expiry.
        assert!(!s.without("absent").dirty);
        assert!(!s.cleared().dirty);
        assert!(s.with("a", "1").without("a").dirty);
    }
}
