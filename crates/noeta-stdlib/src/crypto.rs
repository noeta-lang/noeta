//! `std.crypto` — content digests and keyed digests (crypto arc C2), plus the bcrypt password
//! helpers (C4). The math is RustCrypto's (`sha1`/`sha2`/`md-5`/`hmac`) and the `bcrypt` crate —
//! we never hand-roll primitives. Everything here is pure: effectful inputs (the bcrypt salt)
//! arrive as arguments, drawn from the Host `Entropy` capability by the registry dispatch.
//!
//! `sha1` and `md5` ship for interop (UUID v5, legacy checksums, cache keys) and are documented
//! as not collision-resistant — they are not an integrity story.

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
