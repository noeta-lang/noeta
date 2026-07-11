//! **Keyless provenance** (package-manager Phase 5): Sigstore bundles in place of registered keys.
//!
//! The [`crate::provenance::Attestation`] shape — the signed "version → commit" binding — is
//! unchanged; what changes is the trust root. Instead of "signed by the scope's registered Ed25519
//! key (TOFU-pinned in `noeta.lock`)", a release carries a **Sigstore bundle**: a DSSE envelope
//! signed by an **ephemeral** key, a short-lived **Fulcio certificate** binding that key to a
//! verified **OIDC identity** (e.g. a GitHub Actions workflow), and a **Rekor transparency-log**
//! inclusion proof with a signed checkpoint. There is no long-lived secret to steal, and every
//! release is publicly attributable: anyone can monitor the log for "a release of my package
//! signed by an identity that isn't mine" — even for packages they never installed, and even
//! against a compromised registry operator, because the CA and log are operated by OpenSSF, not
//! by the registry.
//!
//! Verification is **fully offline** from the stored bundle (the registry serves it next to the
//! release): certificate chain → identity policy → SCT → inclusion proof + checkpoint signature →
//! integrated-time within certificate validity → DSSE signature over the payload → payload binds
//! the artifact. No network, no Rekor round-trip; the trust root is a pinned snapshot of
//! sigstore.dev's `trusted_root.json` embedded at build time (TUF-based rotation is a surfaced
//! deferral).
//!
//! Keyless is a **second trust root, not a replacement**: the Ed25519 path (`provenance`) stays
//! for publishers without an OIDC identity. Per-scope trust and the downgrade-protection rules
//! live in the lockfile/graph layer, not here — this module only answers "does this bundle prove
//! that identity X attested these bytes?".

use sigstore_trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot};
use sigstore_types::Sha256Hash;
use sigstore_verify::types::Bundle;
use sigstore_verify::{VerificationPolicy, verify};

/// The consumer-side pin: which OIDC identity is allowed to sign for a scope. Both parts match
/// exactly — the `issuer` is the OIDC provider (e.g. `https://token.actions.githubusercontent.com`),
/// the `identity` the certificate's SAN (for GitHub Actions, the workflow ref).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPolicy {
    pub issuer: String,
    pub identity: String,
}

/// What a successfully verified bundle proves: the identity Fulcio certified for the ephemeral
/// signing key, and when the log recorded the signature. This is what gets TOFU-pinned.
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    pub issuer: String,
    pub identity: String,
    /// Unix seconds at which Rekor integrated the entry (the signature's established time).
    pub integrated_time: Option<i64>,
}

/// The pinned production trust root embedded in the binary — sigstore.dev's Fulcio CA
/// certificates, Rekor public keys, and CT log keys. The single out-of-band root of trust.
pub fn production_trust_root() -> Result<TrustedRoot, String> {
    TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)
        .map_err(|err| format!("embedded Sigstore trust root is invalid: {err}"))
}

/// Verify a Sigstore bundle offline against the embedded production trust root: the bundle's DSSE
/// payload must attest exactly `artifact_sha256` (hex), and — when a policy is given — the signing
/// certificate's identity must match it. `None` policy = first-use (the caller pins what returns).
pub fn verify_bundle(
    bundle_json: &str,
    artifact_sha256: &str,
    policy: Option<&IdentityPolicy>,
) -> Result<VerifiedIdentity, String> {
    let root = production_trust_root()?;
    verify_bundle_with_root(bundle_json, artifact_sha256, policy, &root)
}

/// [`verify_bundle`] against an explicit trust root — the seam tests use to substitute a fixture
/// root, and the future TUF-refreshed root would flow through.
pub fn verify_bundle_with_root(
    bundle_json: &str,
    artifact_sha256: &str,
    policy: Option<&IdentityPolicy>,
    root: &TrustedRoot,
) -> Result<VerifiedIdentity, String> {
    let bundle = Bundle::from_json(bundle_json)
        .map_err(|err| format!("malformed Sigstore bundle: {err}"))?;
    let digest = Sha256Hash::from_hex(artifact_sha256)
        .map_err(|err| format!("malformed artifact digest: {err}"))?;

    let mut verification = VerificationPolicy::default();
    if let Some(pin) = policy {
        verification = verification
            .require_identity(pin.identity.clone())
            .require_issuer(pin.issuer.clone());
    }

    let result = verify(digest, &bundle, &verification, root)
        .map_err(|err| format!("keyless verification failed: {err}"))?;

    // Fulcio certificates always carry both; their absence means the bundle proved nothing an
    // identity pin could hold on to, so fail closed rather than pin an empty identity.
    let (Some(identity), Some(issuer)) = (result.identity, result.issuer) else {
        return Err("keyless verification failed: certificate carries no identity/issuer".into());
    };
    Ok(VerifiedIdentity {
        issuer,
        identity,
        integrated_time: result.integrated_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real GitHub-Actions-signed DSSE bundle (in-toto statement over a conda package), from
    /// prefix-dev/sigstore-rust's test data (Apache-2.0). Its certificate chains to the embedded
    /// production trust root, so it exercises the exact path a published Noeta release will.
    const GHA_BUNDLE: &str = include_str!("../test_data/gha_dsse_bundle.json");
    /// The identity Fulcio certified in that bundle's certificate.
    const GHA_IDENTITY: &str =
        "https://github.com/wolfv/sigstore-test/.github/workflows/action.yaml@refs/heads/main";
    const GHA_ISSUER: &str = "https://token.actions.githubusercontent.com";
    /// sha256 of the artifact the bundle's in-toto statement attests (its first subject).
    const GHA_SUBJECT_SHA256: &str =
        "59ed81ee7a2485c47588ebdbad14764bf722c93438b43fe953a651747bc62ad7";

    #[test]
    fn a_real_bundle_verifies_offline_and_yields_its_identity() {
        let verified = verify_bundle(GHA_BUNDLE, GHA_SUBJECT_SHA256, None).unwrap();
        assert_eq!(verified.identity, GHA_IDENTITY);
        assert_eq!(verified.issuer, GHA_ISSUER);
        assert_eq!(verified.integrated_time, Some(1738060096));
    }

    #[test]
    fn a_matching_identity_pin_passes() {
        let pin = IdentityPolicy {
            issuer: GHA_ISSUER.into(),
            identity: GHA_IDENTITY.into(),
        };
        let verified = verify_bundle(GHA_BUNDLE, GHA_SUBJECT_SHA256, Some(&pin)).unwrap();
        assert_eq!(verified.identity, pin.identity);
    }

    #[test]
    fn a_mismatched_identity_pin_is_rejected() {
        let pin = IdentityPolicy {
            issuer: GHA_ISSUER.into(),
            identity: "https://github.com/evil/repo/.github/workflows/x.yaml@refs/heads/main"
                .into(),
        };
        let err = verify_bundle(GHA_BUNDLE, GHA_SUBJECT_SHA256, Some(&pin)).unwrap_err();
        assert!(err.contains("identity mismatch"), "{err}");
    }

    #[test]
    fn a_mismatched_issuer_pin_is_rejected() {
        let pin = IdentityPolicy {
            issuer: "https://accounts.google.com".into(),
            identity: GHA_IDENTITY.into(),
        };
        let err = verify_bundle(GHA_BUNDLE, GHA_SUBJECT_SHA256, Some(&pin)).unwrap_err();
        assert!(err.contains("issuer mismatch"), "{err}");
    }

    #[test]
    fn an_attestation_over_different_bytes_is_rejected() {
        let other = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_bundle(GHA_BUNDLE, other, None).unwrap_err();
        assert!(err.contains("does not match any subject"), "{err}");
    }

    #[test]
    fn a_tampered_signature_is_rejected() {
        // Corrupt the DSSE signature (flip its first byte): the payload is untouched and still
        // attests the right artifact, but the ephemeral key's signature no longer verifies.
        let mut bundle: serde_json::Value = serde_json::from_str(GHA_BUNDLE).unwrap();
        let sig = bundle["dsseEnvelope"]["signatures"][0]["sig"]
            .as_str()
            .unwrap();
        let flipped = if sig.starts_with('M') {
            sig.replacen('M', "N", 1)
        } else {
            sig.replacen(&sig[..1], "M", 1)
        };
        bundle["dsseEnvelope"]["signatures"][0]["sig"] = flipped.into();
        let err = verify_bundle(&bundle.to_string(), GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(err.contains("failed"), "{err}");
    }

    #[test]
    fn malformed_inputs_are_errors_not_panics() {
        assert!(verify_bundle("not json", GHA_SUBJECT_SHA256, None).is_err());
        assert!(verify_bundle(GHA_BUNDLE, "not hex", None).is_err());
        assert!(verify_bundle(GHA_BUNDLE, "abcd", None).is_err()); // wrong length
    }
}
