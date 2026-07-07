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
