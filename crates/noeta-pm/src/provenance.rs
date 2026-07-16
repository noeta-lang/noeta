//! Package **provenance** (package-manager Phase 4, #2): Ed25519-signed attestations that bind a
//! published release to its source commit, so a consumer can verify — independently of trusting the
//! registry's database — that a maintainer attested "this version = this commit".
//!
//! ## Trust model (self-contained; the sigstore evolution is noted)
//!
//! A **scope** (`company`) registers an Ed25519 **public key**; `noeta publish` signs an
//! [`Attestation`] with the scope's **private key**; the registry stores the signature per release
//! and serves the scope's public key. A consumer fetches both and verifies the signature. The key is
//! trusted **on first use** and **pinned in `noeta.lock`** (like an SSH host key): a later registry
//! compromise that serves a different key or a forged signature is then rejected, because it can't
//! produce a signature valid under the *pinned* key. This defends the "compromised registry re-points
//! a version to malicious code" threat (the registry lacks the maintainer's private key) and raises
//! the bar on a stolen publish token (an attacker also needs the signing key).
//!
//! **Limits (honest):** first-use is trust-on-first-use (no out-of-band root yet), and the signing
//! key is a long-lived secret. The evolution is **Sigstore-style keyless signing** — an OIDC identity
//! (e.g. a CI's) → a short-lived cert → a public **transparency log** — which removes the stealable
//! secret and adds public detectability. The [`Attestation`] shape and the verify seam stay the same;
//! only the *trust root* changes (a registered key → an OIDC identity + a CA + a log).

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use semver::Version;

use crate::error::PmError;

/// The signed binding (package-manager Phase 4, #2): a release's identity, version, and the commit
/// SHA it resolves to. Signing this is what makes "version → commit" verifiable and non-repudiable.
#[derive(Debug)]
pub struct Attestation<'a> {
    pub name: &'a str,
    pub version: &'a Version,
    pub sha: &'a str,
}

impl Attestation<'_> {
    /// The exact bytes that are signed and verified — a fixed, domain-separated, deterministic
    /// encoding so signer and verifier always agree.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "noeta-attestation-v1\n{}\n{}\n{}\n",
            self.name, self.version, self.sha
        )
        .into_bytes()
    }
}

/// A generated Ed25519 keypair, hex-encoded (the private key is the 32-byte seed; the public key the
/// 32-byte verifying key). The private half is a secret to guard; the public half is registered.
#[derive(Debug)]
pub struct Keypair {
    pub private_hex: String,
    pub public_hex: String,
}

/// Generate a fresh signing keypair from OS entropy.
pub fn generate_keypair() -> Result<Keypair, PmError> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|err| PmError::Io(format!("cannot read OS entropy: {err}")))?;
    let signing = SigningKey::from_bytes(&seed);
    let public = signing.verifying_key();
    Ok(Keypair {
        private_hex: to_hex(&seed),
        public_hex: to_hex(public.as_bytes()),
    })
}

/// The hex public key corresponding to a hex private key (the seed) — for registering a scope's key.
pub fn public_key_hex(private_hex: &str) -> Result<String, PmError> {
    let seed = from_hex::<32>(private_hex, "private key")?;
    Ok(to_hex(
        SigningKey::from_bytes(&seed).verifying_key().as_bytes(),
    ))
}

/// Sign `attestation` with the hex-encoded private key, returning the hex signature (128 chars).
pub fn sign(attestation: &Attestation, private_hex: &str) -> Result<String, PmError> {
    let seed = from_hex::<32>(private_hex, "private key")?;
    let signing = SigningKey::from_bytes(&seed);
    let signature = signing.sign(&attestation.canonical_bytes());
    Ok(to_hex(&signature.to_bytes()))
}

/// Verify `signature_hex` over `attestation` against the hex public key. `Ok(())` iff the signature
/// is valid — proof the holder of the matching private key attested exactly this (name, version, sha).
pub fn verify(
    attestation: &Attestation,
    signature_hex: &str,
    public_hex: &str,
) -> Result<(), PmError> {
    let public_bytes = from_hex::<32>(public_hex, "public key")?;
    let verifying = VerifyingKey::from_bytes(&public_bytes)
        .map_err(|err| PmError::Trust(format!("invalid public key: {err}")))?;
    let signature_bytes = from_hex::<64>(signature_hex, "signature")?;
    let signature = Signature::from_bytes(&signature_bytes);
    verifying
        .verify_strict(&attestation.canonical_bytes(), &signature)
        .map_err(|_| PmError::Trust("signature does not verify against the public key".to_string()))
}

/// Lowercase hex of `bytes`.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode exactly `N` bytes of hex, tagging errors with `what`. Malformed key/signature material
/// is a [`PmError::Trust`] problem — it can never verify.
fn from_hex<const N: usize>(s: &str, what: &str) -> Result<[u8; N], PmError> {
    if s.len() != N * 2 {
        return Err(PmError::Trust(format!(
            "{what} must be {} hex chars, got {}",
            N * 2,
            s.len()
        )));
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| PmError::Trust(format!("{what} is not valid hex")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att<'a>(name: &'a str, version: &'a Version, sha: &'a str) -> Attestation<'a> {
        Attestation { name, version, sha }
    }

    #[test]
    fn canonical_bytes_are_the_fixed_cross_language_format() {
        // This exact byte layout is what the Cloudflare Worker (`sig_message`) reproduces to verify
        // signatures — any drift here breaks cross-language provenance. Pin it.
        let v = Version::new(1, 0, 0);
        assert_eq!(
            att("signed/pkg", &v, "abc").canonical_bytes(),
            b"noeta-attestation-v1\nsigned/pkg\n1.0.0\nabc\n"
        );
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let kp = generate_keypair().unwrap();
        let v = Version::new(1, 2, 0);
        let a = att("acme/foo", &v, "abc123");
        let sig = sign(&a, &kp.private_hex).unwrap();
        assert_eq!(sig.len(), 128);
        verify(&a, &sig, &kp.public_hex).expect("a genuine signature verifies");
    }

    #[test]
    fn a_tampered_attestation_fails_verification() {
        let kp = generate_keypair().unwrap();
        let v = Version::new(1, 2, 0);
        let sig = sign(&att("acme/foo", &v, "abc123"), &kp.private_hex).unwrap();
        // A different commit SHA under the same signature must not verify (the binding is broken).
        let forged = att("acme/foo", &v, "EVILsha");
        assert!(verify(&forged, &sig, &kp.public_hex).is_err());
        // A different signing key must not verify either.
        let other = generate_keypair().unwrap();
        assert!(verify(&att("acme/foo", &v, "abc123"), &sig, &other.public_hex).is_err());
    }

    #[test]
    fn malformed_key_or_signature_is_an_error() {
        let v = Version::new(1, 0, 0);
        let a = att("a/b", &v, "s");
        assert!(sign(&a, "not-hex").is_err());
        assert!(verify(&a, "abcd", "00").is_err());
    }
}
