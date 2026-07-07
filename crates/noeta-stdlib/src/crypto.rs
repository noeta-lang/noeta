//! `std.crypto` — content digests and keyed digests (crypto arc C2), plus the bcrypt password
//! helpers (C4). The math is RustCrypto's (`sha1`/`sha2`/`md-5`/`hmac`) and the `bcrypt` crate —
//! we never hand-roll primitives. Everything here is pure: effectful inputs (the bcrypt salt)
//! arrive as arguments, drawn from the Host `Entropy` capability by the registry dispatch.
//!
//! `sha1` and `md5` ship for interop (UUID v5, legacy checksums, cache keys) and are documented
//! as not collision-resistant — they are not an integrity story.

use crate::{ErrorKind, StdError};
use hmac::{Hmac, Mac};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

pub fn sha512(data: &[u8]) -> Vec<u8> {
    Sha512::digest(data).to_vec()
}

pub fn sha1(data: &[u8]) -> Vec<u8> {
    Sha1::digest(data).to_vec()
}

pub fn md5(data: &[u8]) -> Vec<u8> {
    Md5::digest(data).to_vec()
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn hmac_sha512(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha512>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Verify an HMAC-SHA256 tag in constant time (crypto arc C7). `bytes ==` short-circuits on the
/// first differing byte — fine for content digests, a timing oracle for auth tags — so tag
/// comparison gets a purpose-named function on the `hmac` crate's own constant-time
/// `Mac::verify_slice`. A truncated, tampered, or wrong-key tag is `false`, never an error.
pub fn hmac_sha256_verify(key: &[u8], data: &[u8], tag: &[u8]) -> bool {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.verify_slice(tag).is_ok()
}

/// The HMAC-SHA512 twin of [`hmac_sha256_verify`].
pub fn hmac_sha512_verify(key: &[u8], data: &[u8], tag: &[u8]) -> bool {
    let mut mac = <Hmac<Sha512>>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.verify_slice(tag).is_ok()
}

/// Constant-time equality for secrets that are NOT HMAC tags (session tokens, API keys, stored
/// digests) — `subtle`'s `ct_eq`, which examines every byte regardless of where the first
/// difference sits. Different lengths are `false` (length is not treated as secret, the
/// standard contract).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

/// bcrypt password hash (crypto arc C4). The 16-byte salt arrives as an ARGUMENT — drawn from
/// the Host `Entropy` capability by the registry dispatch, never self-generated — so the sandbox
/// stays deterministic (exact-string pinnable) and the real host gets OS entropy. The result is
/// the standard `$2b$…` modular-crypt string, self-describing for `bcrypt_verify`.
pub fn bcrypt_hash(password: &str, cost: i64, salt: [u8; 16]) -> Result<String, StdError> {
    // bcrypt's defined cost range; out-of-range would panic deep in the blowfish setup.
    if !(4..=31).contains(&cost) {
        return Err(StdError {
            kind: ErrorKind::ArgType,
            message: format!("`crypto.bcrypt_hash` cost must be in 4..=31, got {cost}"),
        });
    }
    bcrypt::hash_with_salt(password, cost as u32, salt)
        .map(|parts| parts.to_string())
        .map_err(|e| StdError {
            kind: ErrorKind::ArgType,
            // The one reachable failure: a password over 72 bytes (bcrypt's hard input limit —
            // the crate errors rather than silently truncating).
            message: format!("`crypto.bcrypt_hash`: {e}"),
        })
}

/// Verify a password against a bcrypt hash. A wrong password is `false`; a string that is not a
/// bcrypt hash at all is an error (a malformed hash is a program bug, not a failed login).
pub fn bcrypt_verify(password: &str, hash: &str) -> Result<bool, StdError> {
    bcrypt::verify(password, hash).map_err(|_| StdError {
        kind: ErrorKind::ArgType,
        message: "`crypto.bcrypt_verify`: the hash argument is not a bcrypt hash".to_string(),
    })
}

/// The registered extern-type name of the incremental hasher.
pub const HASHER_TYPE_NAME: &str = "Hasher";

/// An incremental digest (crypto arc C3) — ONE extern type over an algorithm enum, so adding an
/// algorithm never adds a type. The third extern-seam client, and the first in the
/// **mutable + host-free** corner of the {pure, mutable} × {host-free, effectful} matrix:
/// `update` mutates the receiver through the shared cell (reference semantics, like
/// `FileHandle`) but never touches the Host.
#[derive(Clone)]
pub enum Hasher {
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Hasher {
    /// The algorithm label (display, errors).
    pub fn algorithm(&self) -> &'static str {
        match self {
            Hasher::Sha256(_) => "sha256",
            Hasher::Sha512(_) => "sha512",
        }
    }

    /// Absorb more input — the mutating method.
    pub fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    /// The digest of everything absorbed so far — **non-destructive** (finalizes a clone of the
    /// state), so a hasher can report interim digests and keep accepting updates. Deterministic
    /// and least surprising; there is no "consumed hasher" error state to model.
    pub fn digest(&self) -> Vec<u8> {
        match self {
            Hasher::Sha256(h) => h.clone().finalize().to_vec(),
            Hasher::Sha512(h) => h.clone().finalize().to_vec(),
        }
    }
}

impl std::fmt::Debug for Hasher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Hasher({})", self.algorithm())
    }
}

impl crate::ExternValue for Hasher {
    fn type_name(&self) -> &'static str {
        HASHER_TYPE_NAME
    }

    /// Equal iff same algorithm and same absorbed content — observable as "the current digests
    /// agree" (the only well-defined equality over opaque hash states).
    fn eq_value(&self, other: &dyn crate::ExternValue) -> bool {
        match other.as_any().downcast_ref::<Hasher>() {
            Some(o) => self.algorithm() == o.algorithm() && self.digest() == o.digest(),
            None => false,
        }
    }

    /// Unordered (and not `key_capable`: `update` mutates).
    fn cmp_value(&self, _other: &dyn crate::ExternValue) -> Option<std::cmp::Ordering> {
        None
    }

    fn hash_value(&self) -> u64 {
        0 // not key-capable; never used as a map key
    }

    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<{} hasher>", self.algorithm())
    }

    fn clone_box(&self) -> Box<dyn crate::ExternValue> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes_to_hex;

    /// NIST FIPS 180 "abc" vectors + the empty string — the canonical published answers.
    #[test]
    fn digest_vectors() {
        assert_eq!(
            bytes_to_hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            bytes_to_hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            bytes_to_hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            bytes_to_hex(&md5(b"abc")),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        assert_eq!(sha512(b"abc").len(), 64);
    }

    /// The incremental hasher matches the one-shot digest at every point, and `digest()` is
    /// non-destructive — an interim read never perturbs later updates.
    #[test]
    fn hasher_incremental_matches_one_shot() {
        let mut h = Hasher::Sha256(Default::default());
        h.update(b"ab");
        assert_eq!(h.digest(), sha256(b"ab"));
        h.update(b"c");
        assert_eq!(h.digest(), sha256(b"abc"));

        use crate::ExternValue;
        let other_algo = Hasher::Sha512(Default::default());
        assert!(!h.eq_value(&other_algo)); // different algorithm
        assert_eq!((&h as &dyn ExternValue).display_string(), "<sha256 hasher>");
    }

    /// The verify twins accept exactly the tag their hash side produced; tampered data, a wrong
    /// key, and a truncated tag are all `false` (never an error).
    #[test]
    fn hmac_verify_twins_and_constant_time_eq() {
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert!(hmac_sha256_verify(
            b"Jefe",
            b"what do ya want for nothing?",
            &tag
        ));
        assert!(!hmac_sha256_verify(b"Jefe", b"tampered", &tag));
        assert!(!hmac_sha256_verify(
            b"wrong",
            b"what do ya want for nothing?",
            &tag
        ));
        assert!(!hmac_sha256_verify(
            b"Jefe",
            b"what do ya want for nothing?",
            &tag[..16]
        ));
        let tag512 = hmac_sha512(b"Jefe", b"what do ya want for nothing?");
        assert!(hmac_sha512_verify(
            b"Jefe",
            b"what do ya want for nothing?",
            &tag512
        ));
        assert!(!hmac_sha512_verify(
            b"Jefe",
            b"what do ya want for nothing?",
            &tag
        ));

        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(constant_time_eq(b"", b""));
    }

    /// bcrypt against a published vector (the openwall/John the Ripper "U*U" case) plus the
    /// argument-validation edges. `hash` itself is pinned by conformance (sandbox-seeded salt).
    #[test]
    fn bcrypt_verifies_published_vector_and_validates_args() {
        let known = "$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW";
        assert_eq!(bcrypt_verify("U*U", known), Ok(true));
        assert_eq!(bcrypt_verify("U*U*", known), Ok(false));
        assert!(bcrypt_verify("x", "not a hash").is_err());
        assert!(bcrypt_hash("pw", 3, [0; 16]).is_err());
        assert!(bcrypt_hash("pw", 32, [0; 16]).is_err());
        // A fixed salt gives a fixed hash — the determinism the sandbox pin relies on.
        let a = bcrypt_hash("pw", 4, [7; 16]).unwrap();
        assert_eq!(a, bcrypt_hash("pw", 4, [7; 16]).unwrap());
        assert!(a.starts_with("$2b$04$"));
        assert_eq!(bcrypt_verify("pw", &a), Ok(true));
    }

    /// RFC 4231 test case 2 (the short-key "Jefe" case).
    #[test]
    fn hmac_vectors() {
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            bytes_to_hex(&tag),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hmac_sha512(b"Jefe", b"what do ya want for nothing?").len(),
            64
        );
    }
}
