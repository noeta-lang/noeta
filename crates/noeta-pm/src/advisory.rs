//! Client-side security-advisory verification (namespace-protection #1, advisory feed) — the consumer
//! half of the registry's signed advisory database (`noeta-registry/src/advisory.ts`). `noeta audit`
//! fetches the feed, verifies each advisory's Ed25519 signature against a **pinned** advisory key, and
//! flags any resolved dependency whose version falls in an advisory's affected range.
//!
//! Two trust layers, mirroring the server:
//!   * per-advisory signatures — verified here, so a network MITM or a compromised mirror can neither
//!     inject a fake advisory (red-flagging a healthy package) nor tamper with a real one;
//!   * a signed feed head — `{ count, digest }`, so a client that pinned an earlier head detects a
//!     *dropped* advisory (the recomputed digest wouldn't match, and a shrunken count is a rollback).
//!
//! Gated on `registry-http` (serde, to deserialize the feed) + `provenance` (Ed25519 + SHA-256). The
//! canonical byte formats MUST match the server's exactly.

use ed25519_dalek::{Signature, VerifyingKey};
use semver::{Version, VersionReq};
use sha2::{Digest, Sha256};

use crate::error::PmError;
use crate::transparency::hex_to_array;

const ADVISORY_PREFIX: &str = "noeta-advisory-v1";
const FEED_PREFIX: &str = "noeta-advisory-feed-v1";

/// One security advisory as served by the registry's advisory feed. Field order/shape mirrors the
/// server's wire form (`toWire`); `details`/`url` default to empty, `patched` is optional.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Advisory {
    /// Advisory id, e.g. `NOETA-2026-0001`.
    pub id: String,
    /// Affected package identity `company/package`.
    pub package: String,
    /// Affected versions, a SemVer requirement (e.g. `">=1.0.0, <1.2.3"`).
    pub ranges: String,
    /// First fixed version(s), informational.
    #[serde(default)]
    pub patched: Option<String>,
    /// `low` | `medium` | `high` | `critical`.
    pub severity: String,
    /// One-line headline (newline-free).
    pub summary: String,
    /// Longer description (may be multi-line); folded into the signature as a SHA-256 digest.
    #[serde(default)]
    pub details: String,
    /// Link to the full advisory.
    #[serde(default)]
    pub url: String,
    /// `true` once retracted (a false alarm) — kept, not deleted, so the feed count is monotonic.
    pub withdrawn: bool,
    /// Monotonic feed cursor (for delta sync).
    pub seq: u64,
    /// Hex Ed25519 signature over [`Self::canonical_bytes`].
    pub signature: String,
    /// The index of this advisory's leaf in the transparency log (advisory-log binding). `None` if the
    /// registry doesn't log advisories — then its issuance is only signature-attested, not publicly
    /// logged.
    #[serde(default)]
    pub log_index: Option<u64>,
}

impl Advisory {
    /// The exact bytes the registry signed — reproduced identically so the signature verifies. MUST
    /// match the server's `canonicalBytes`. `details` is folded in as a SHA-256 digest (it may be
    /// multi-line); `state` binds the withdrawn flag so an advisory can't be silently un-retracted
    /// under the same signature.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let details_hash = hex_encode(&Sha256::digest(self.details.as_bytes()));
        let state = if self.withdrawn {
            "withdrawn"
        } else {
            "active"
        };
        format!(
            "{ADVISORY_PREFIX}\n{}\n{}\n{}\n{}\n{state}\n{}\n{details_hash}\n{}\n",
            self.id, self.package, self.ranges, self.severity, self.summary, self.url,
        )
        .into_bytes()
    }

    /// Verify this advisory's signature against the pinned advisory public key (hex). `Ok(false)` = a
    /// well-formed but non-verifying signature (tampered or wrong key); `Err` = malformed input.
    pub fn verify(&self, public_hex: &str) -> Result<bool, PmError> {
        verify_ed25519(public_hex, &self.canonical_bytes(), &self.signature)
    }

    /// Whether a live release at `version` is covered by this advisory's affected range. A malformed
    /// range matches nothing (fail-open on parse — a bad advisory shouldn't fabricate a hit).
    pub fn affects(&self, version: &Version) -> bool {
        VersionReq::parse(&self.ranges).is_ok_and(|req| req.matches(version))
    }

    /// A withdrawn advisory is a retracted false alarm — it never flags a dependency.
    pub fn is_active(&self) -> bool {
        !self.withdrawn
    }
}

/// The signed feed-head bytes `noeta-advisory-feed-v1\n{count}\n{digest}\n` — MUST match the server.
pub fn feed_head_bytes(count: usize, digest_hex: &str) -> Vec<u8> {
    format!("{FEED_PREFIX}\n{count}\n{digest_hex}\n").into_bytes()
}

/// SHA-256 (hex) over the id-sorted concatenation of every advisory's canonical bytes — reproduced
/// from the served feed so a served head digest that doesn't match the served advisories (a withheld
/// entry) is caught. MUST match the server's `feedDigest`.
pub fn feed_digest(advisories: &[Advisory]) -> String {
    let mut sorted: Vec<&Advisory> = advisories.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let mut hasher = Sha256::new();
    for a in sorted {
        hasher.update(a.canonical_bytes());
    }
    hex_encode(&hasher.finalize())
}

/// Verify the feed head's Ed25519 signature over `{count}\n{digest}` against the pinned advisory key.
pub fn verify_feed_head(
    public_hex: &str,
    count: usize,
    digest_hex: &str,
    signature_hex: &str,
) -> Result<bool, PmError> {
    verify_ed25519(
        public_hex,
        &feed_head_bytes(count, digest_hex),
        signature_hex,
    )
}

fn verify_ed25519(public_hex: &str, message: &[u8], signature_hex: &str) -> Result<bool, PmError> {
    let pk: [u8; 32] = hex_to_array(public_hex)
        .ok_or_else(|| PmError::Trust("advisory public key is not 32 hex bytes".to_string()))?;
    let key = VerifyingKey::from_bytes(&pk)
        .map_err(|err| PmError::Trust(format!("bad advisory public key: {err}")))?;
    let sig: [u8; 64] = hex_to_array(signature_hex)
        .ok_or_else(|| PmError::Trust("advisory signature is not 64 hex bytes".to_string()))?;
    let signature = Signature::from_bytes(&sig);
    Ok(key.verify_strict(message, &signature).is_ok())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[9u8; 32])
    }
    fn public_hex(sk: &SigningKey) -> String {
        hex_encode(sk.verifying_key().as_bytes())
    }

    fn advisory(sk: &SigningKey, withdrawn: bool) -> Advisory {
        let mut a = Advisory {
            id: "NOETA-2026-0001".into(),
            package: "acme/http".into(),
            ranges: ">=1.0.0, <1.2.3".into(),
            patched: Some("1.2.3".into()),
            severity: "high".into(),
            summary: "request smuggling".into(),
            details: "multi\nline\ndetails".into(),
            url: "https://example.com/1".into(),
            withdrawn,
            seq: 0,
            signature: String::new(),
            log_index: None,
        };
        a.signature = hex_encode(&sk.sign(&a.canonical_bytes()).to_bytes());
        a
    }

    #[test]
    fn signature_round_trips_and_tamper_is_caught() {
        let sk = key();
        let pk = public_hex(&sk);
        let a = advisory(&sk, false);
        assert!(a.verify(&pk).unwrap());

        // Narrowing the affected range (hiding who is vulnerable) breaks the signature.
        let mut tampered = a.clone();
        tampered.ranges = ">=1.0.0, <1.0.1".into();
        assert!(!tampered.verify(&pk).unwrap());

        // Flipping the withdrawn flag (silently retracting) breaks it too.
        let mut unretracted = advisory(&sk, true);
        unretracted.withdrawn = false;
        assert!(!unretracted.verify(&pk).unwrap());
    }

    #[test]
    fn affects_matches_the_version_range() {
        let sk = key();
        let a = advisory(&sk, false);
        assert!(a.affects(&Version::new(1, 0, 0)));
        assert!(a.affects(&Version::new(1, 2, 2)));
        assert!(!a.affects(&Version::new(1, 2, 3))); // patched
        assert!(!a.affects(&Version::new(0, 9, 0)));
        // A withdrawn advisory is inert regardless of range.
        assert!(!advisory(&sk, true).is_active());
    }

    #[test]
    fn feed_head_binds_the_digest_of_the_whole_set() {
        let sk = key();
        let pk = public_hex(&sk);
        let a1 = advisory(&sk, false);
        let mut a2 = advisory(&sk, false);
        a2.id = "NOETA-2026-0002".into();
        a2.package = "acme/tls".into();
        a2.signature = hex_encode(&sk.sign(&a2.canonical_bytes()).to_bytes());
        let set = vec![a1.clone(), a2.clone()];

        let digest = feed_digest(&set);
        // Order-independent (server sorts by id).
        assert_eq!(digest, feed_digest(&[a2.clone(), a1.clone()]));
        let sig = hex_encode(&sk.sign(&feed_head_bytes(2, &digest)).to_bytes());
        assert!(verify_feed_head(&pk, 2, &digest, &sig).unwrap());
        // Dropping an advisory changes the digest, so the old signature no longer matches the new set.
        let dropped = feed_digest(&[a1]);
        assert!(!verify_feed_head(&pk, 2, &dropped, &sig).unwrap());
    }

    #[test]
    fn malformed_key_is_an_error_not_a_panic() {
        let a = advisory(&key(), false);
        assert!(a.verify("zz").is_err());
    }
}
