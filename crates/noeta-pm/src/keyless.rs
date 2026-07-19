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

use sha2::{Digest, Sha256};
use sigstore_trust_root::{SIGSTORE_PRODUCTION_TRUSTED_ROOT, TrustedRoot};
use sigstore_types::Sha256Hash;
use sigstore_verify::types::Bundle;
use sigstore_verify::{VerificationPolicy, verify};

use crate::error::PmError;
use crate::provenance::Attestation;
use crate::registry::GitCoords;

/// The in-toto predicate type naming what a Noeta publish attestation *means* — monitors and
/// tooling key on this to recognize (and index) Noeta releases in the public transparency log.
pub const PREDICATE_TYPE: &str = "https://noeta.dev/attestation/publish/v1";

/// The sha256 (hex) of the attestation's [`Attestation::canonical_bytes`] — the **artifact digest**
/// a keyless bundle attests. Both sides compute it from first principles: the publisher when
/// building the in-toto statement, the consumer from the registry-served release facts it is about
/// to trust. The canonical bytes stay the single cross-trust-root truth; DSSE/in-toto is envelope.
pub fn attested_digest(attestation: &Attestation<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(attestation.canonical_bytes());
    let out = hasher.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// The in-toto Statement (v1) a keyless publish signs as its DSSE payload: the subject binds
/// [`attested_digest`] under the release's name, and the predicate carries the human/monitor-facing
/// publish facts (identity, version, commit, and where the tag lives). Deterministic output — the
/// exact bytes are what the ephemeral key signs, so no map-ordering surprises.
pub fn publish_statement(attestation: &Attestation<'_>, coords: &GitCoords) -> String {
    debug_assert_eq!(
        attestation.sha, coords.sha,
        "attestation and coords disagree"
    );
    serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{
            "name": format!("{}@{}", attestation.name, attestation.version),
            "digest": { "sha256": attested_digest(attestation) },
        }],
        "predicateType": PREDICATE_TYPE,
        "predicate": {
            "name": attestation.name,
            "version": attestation.version.to_string(),
            "sha": attestation.sha,
            "url": coords.url,
            "tag": coords.tag,
        },
    })
    .to_string()
}

/// The in-toto predicate type naming a Noeta **advisory** attestation (advisory-intake arc, publisher
/// tier) — the keyless counterpart of a publish attestation, so a monitor can recognize an
/// owner-issued advisory in the public transparency log.
pub const ADVISORY_PREDICATE_TYPE: &str = "https://noeta.dev/attestation/advisory/v1";

/// The sha256 (hex) of an advisory's canonical bytes — the **subject digest** a publisher advisory's
/// keyless bundle attests. Both sides compute it from the advisory's canonical bytes: the scope owner
/// when building the statement, the consumer from the registry-served advisory it is about to trust.
pub fn advisory_attested_digest(canonical_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The in-toto Statement (v1) a publisher advisory keyless-signs: the subject binds
/// [`advisory_attested_digest`] under the advisory id, the predicate carries the advisory's identity.
/// Deterministic — the exact bytes the ephemeral key signs.
pub fn advisory_statement(advisory_id: &str, package: &str, canonical_bytes: &[u8]) -> String {
    serde_json::json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{
            "name": format!("advisory/{advisory_id}"),
            "digest": { "sha256": advisory_attested_digest(canonical_bytes) },
        }],
        "predicateType": ADVISORY_PREDICATE_TYPE,
        "predicate": { "id": advisory_id, "package": package },
    })
    .to_string()
}

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

/// The trust root verification anchors to: a pinned snapshot of sigstore.dev's
/// `trusted_root.json` embedded in the binary (Fulcio CA certificates, Rekor public keys, CT log
/// keys) — the single out-of-band root of trust. `NOETA_SIGSTORE_TRUST_ROOT` (a path to a
/// `trusted_root.json`) overrides it: the operational escape hatch for a Sigstore root rotation
/// that lands before a toolchain update ships (and what hermetic tests point at their minted
/// test root). TUF-based rotation is the surfaced v-next.
pub fn trust_root() -> Result<TrustedRoot, PmError> {
    if let Some(path) = std::env::var_os("NOETA_SIGSTORE_TRUST_ROOT") {
        let text = std::fs::read_to_string(&path).map_err(|err| {
            PmError::Io(format!(
                "cannot read NOETA_SIGSTORE_TRUST_ROOT `{}`: {err}",
                std::path::Path::new(&path).display()
            ))
        })?;
        return TrustedRoot::from_json(&text).map_err(|err| {
            PmError::Trust(format!(
                "NOETA_SIGSTORE_TRUST_ROOT `{}` is not a valid trust root: {err}",
                std::path::Path::new(&path).display()
            ))
        });
    }
    TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT)
        .map_err(|err| PmError::Trust(format!("embedded Sigstore trust root is invalid: {err}")))
}

/// Verify a Sigstore bundle offline against the embedded production trust root: the bundle's DSSE
/// payload must attest exactly `artifact_sha256` (hex), and — when a policy is given — the signing
/// certificate's identity must match it. `None` policy = first-use (the caller pins what returns).
pub fn verify_bundle(
    bundle_json: &str,
    artifact_sha256: &str,
    policy: Option<&IdentityPolicy>,
) -> Result<VerifiedIdentity, PmError> {
    let root = trust_root()?;
    verify_bundle_with_root(bundle_json, artifact_sha256, policy, &root)
}

/// An OIDC identity token detected from the CI environment (GitHub Actions, GitLab CI,
/// Buildkite, …) — the ambient credential a keyless `noeta publish` signs under. `None` when the
/// environment carries none (publish then falls back to the key path or unsigned).
pub struct AmbientIdentity(sigstore_oidc::IdentityToken);

impl std::fmt::Debug for AmbientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The raw token is a credential — never in debug output.
        f.debug_struct("AmbientIdentity")
            .field("issuer", &self.issuer())
            .field("identity", &self.identity())
            .finish_non_exhaustive()
    }
}

impl AmbientIdentity {
    /// The human-facing identity the token asserts (what Fulcio will certify).
    pub fn identity(&self) -> &str {
        self.0.identity()
    }

    /// The OIDC issuer (e.g. `https://token.actions.githubusercontent.com`).
    pub fn issuer(&self) -> &str {
        self.0.issuer()
    }

    /// Wrap a raw OIDC JWT handed over directly (a CI system that injects the token itself,
    /// or a test fixture). The token's *authenticity* is Fulcio's to judge, not ours.
    pub fn from_jwt(jwt: &str) -> Result<AmbientIdentity, PmError> {
        sigstore_oidc::IdentityToken::from_jwt(jwt)
            .map(AmbientIdentity)
            .map_err(|err| PmError::Auth(format!("malformed OIDC token: {err}")))
    }
}

/// Detect an ambient OIDC identity (CI). Errors only on a *broken* ambient environment (e.g. the
/// token endpoint refused) — a plain "not in CI" is `Ok(None)`.
pub fn ambient_identity() -> Result<Option<AmbientIdentity>, PmError> {
    let runtime = publish_runtime()?;
    let token = runtime
        .block_on(sigstore_oidc::IdentityToken::detect_ambient())
        .map_err(|err| PmError::Auth(format!("ambient OIDC detection failed: {err}")))?;
    Ok(token.map(AmbientIdentity))
}

/// Acquire an OIDC identity **interactively** (K6): the OAuth 2.0 authorization-code flow with
/// PKCE against Sigstore's public OAuth frontend (`oauth2.sigstore.dev` — Dex fronting
/// GitHub/Google/Microsoft logins; the certified identity is the **email**). Opens the browser
/// and waits on a local redirect server; a headless environment (or `force_oob`) falls back to
/// print-the-URL / paste-the-code. `NOETA_OIDC_URL` overrides the provider (tests, private
/// deployments).
pub fn interactive_identity(force_oob: bool) -> Result<AmbientIdentity, PmError> {
    interactive_identity_with(sigstore_oidc::oauth::DefaultAuthCallback, force_oob)
}

/// [`interactive_identity`] with a custom UX callback — the seam hermetic tests drive the flow
/// through programmatically (no stdin, no browser).
pub fn interactive_identity_with(
    callback: impl sigstore_oidc::oauth::AuthCallback,
    force_oob: bool,
) -> Result<AmbientIdentity, PmError> {
    let oidc_url = std::env::var("NOETA_OIDC_URL").ok();
    interactive_identity_at(oidc_url.as_deref(), callback, force_oob)
}

/// [`interactive_identity_with`] against an explicit provider (`None` = Sigstore's public
/// OAuth) — bypasses the process-global env override, like [`publish_bundle_at`].
pub fn interactive_identity_at(
    oidc_url: Option<&str>,
    callback: impl sigstore_oidc::oauth::AuthCallback,
    force_oob: bool,
) -> Result<AmbientIdentity, PmError> {
    let config = match oidc_url {
        Some(url) => sigstore_oidc::oauth::OAuthConfig::from_oidc_url(url),
        None => sigstore_oidc::oauth::OAuthConfig::sigstore(),
    };
    let client = sigstore_oidc::oauth::OAuthClient::new(config);
    let options = sigstore_oidc::oauth::AuthOptions { force_oob };
    let runtime = publish_runtime()?;
    let token = runtime
        .block_on(client.auth_with_options(callback, options))
        .map_err(|err| PmError::Auth(format!("interactive sign-in failed: {err}")))?;
    Ok(AmbientIdentity(token))
}

/// Where a keyless publish sends its requests. Production sigstore.dev by default;
/// `NOETA_FULCIO_URL` / `NOETA_REKOR_URL` override both together (tests, or a private Sigstore
/// deployment — set both or neither).
fn signing_config() -> Result<sigstore_sign::SigningConfig, PmError> {
    let overrides = (
        std::env::var("NOETA_FULCIO_URL").ok(),
        std::env::var("NOETA_REKOR_URL").ok(),
    );
    Ok(match overrides {
        (Some(fulcio), Some(rekor)) => sigstore_sign::SigningConfig {
            fulcio_url: fulcio,
            rekor_url: rekor,
            // No TSA: the transparency log's integrated time is the signature's time source.
            tsa_url: None,
            signing_scheme: sigstore_sign::crypto::SigningScheme::EcdsaP256Sha256,
            rekor_api_version: sigstore_sign::rekor::RekorApiVersion::V1,
            oidc_url: None,
        },
        (None, None) => {
            // Prefer the v1 Rekor API: its entries carry the integrated time the consumer's
            // cert-validity check anchors on, and it is what the verify path is proven against.
            let mut config = sigstore_sign::SigningConfig::production()
                .with_rekor_version(sigstore_sign::rekor::RekorApiVersion::V1);
            config.tsa_url = None;
            config
        }
        _ => {
            return Err(PmError::Trust(
                "set both NOETA_FULCIO_URL and NOETA_REKOR_URL, or neither (production)"
                    .to_string(),
            ));
        }
    })
}

/// Keyless-sign `statement` (the in-toto Statement from [`publish_statement`]) under the ambient
/// identity: ephemeral P-256 key → Fulcio certificate → DSSE envelope → Rekor entry → the
/// Sigstore bundle (JSON) the registry stores. The ephemeral private key exists only inside this
/// call — nothing survives to steal. Endpoints: production sigstore.dev, or the
/// `NOETA_FULCIO_URL`/`NOETA_REKOR_URL` override pair.
pub fn publish_bundle(statement: &[u8], identity: AmbientIdentity) -> Result<String, PmError> {
    publish_bundle_with_config(statement, identity, signing_config()?)
}

/// [`publish_bundle`] against explicit endpoints — the seam hermetic tests drive with their mock
/// Fulcio/Rekor, bypassing the process-global env override.
pub fn publish_bundle_at(
    statement: &[u8],
    identity: AmbientIdentity,
    fulcio_url: &str,
    rekor_url: &str,
) -> Result<String, PmError> {
    publish_bundle_with_config(
        statement,
        identity,
        sigstore_sign::SigningConfig {
            fulcio_url: fulcio_url.to_string(),
            rekor_url: rekor_url.to_string(),
            tsa_url: None,
            signing_scheme: sigstore_sign::crypto::SigningScheme::EcdsaP256Sha256,
            rekor_api_version: sigstore_sign::rekor::RekorApiVersion::V1,
            oidc_url: None,
        },
    )
}

fn publish_bundle_with_config(
    statement: &[u8],
    identity: AmbientIdentity,
    config: sigstore_sign::SigningConfig,
) -> Result<String, PmError> {
    let context = sigstore_sign::SigningContext::with_config(config);
    let signer = context.signer(identity.0);
    let runtime = publish_runtime()?;
    let bundle = runtime
        .block_on(signer.sign_raw_statement(statement))
        .map_err(|err| PmError::Trust(format!("keyless signing failed: {err}")))?;
    bundle
        .to_json()
        .map_err(|err| PmError::Trust(format!("cannot serialize the Sigstore bundle: {err}")))
}

/// A small current-thread runtime for the async sigstore clients — publish is a CLI-only,
/// one-shot flow, so a scoped runtime (the `reqwest::blocking` pattern) beats threading an
/// executor through the package manager.
fn publish_runtime() -> Result<tokio::runtime::Runtime, PmError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| PmError::Io(format!("cannot start the signing runtime: {err}")))
}

/// [`verify_bundle`] against an explicit trust root — the seam tests use to substitute a fixture
/// root, and the future TUF-refreshed root would flow through.
pub fn verify_bundle_with_root(
    bundle_json: &str,
    artifact_sha256: &str,
    policy: Option<&IdentityPolicy>,
    root: &TrustedRoot,
) -> Result<VerifiedIdentity, PmError> {
    let bundle = Bundle::from_json(bundle_json)
        .map_err(|err| PmError::Trust(format!("malformed Sigstore bundle: {err}")))?;
    let digest = Sha256Hash::from_hex(artifact_sha256)
        .map_err(|err| PmError::Trust(format!("malformed artifact digest: {err}")))?;

    let mut verification = VerificationPolicy::default();
    if let Some(pin) = policy {
        verification = verification
            .require_identity(pin.identity.clone())
            .require_issuer(pin.issuer.clone());
    }

    let result = verify(digest, &bundle, &verification, root)
        .map_err(|err| PmError::Trust(format!("keyless verification failed: {err}")))?;

    // Fulcio certificates always carry both; their absence means the bundle proved nothing an
    // identity pin could hold on to, so fail closed rather than pin an empty identity.
    let (Some(identity), Some(issuer)) = (result.identity, result.issuer) else {
        return Err(PmError::Trust(
            "keyless verification failed: certificate carries no identity/issuer".to_string(),
        ));
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
        assert!(err.message().contains("identity mismatch"), "{err}");
    }

    #[test]
    fn a_mismatched_issuer_pin_is_rejected() {
        let pin = IdentityPolicy {
            issuer: "https://accounts.google.com".into(),
            identity: GHA_IDENTITY.into(),
        };
        let err = verify_bundle(GHA_BUNDLE, GHA_SUBJECT_SHA256, Some(&pin)).unwrap_err();
        assert!(err.message().contains("issuer mismatch"), "{err}");
    }

    #[test]
    fn an_attestation_over_different_bytes_is_rejected() {
        let other = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_bundle(GHA_BUNDLE, other, None).unwrap_err();
        assert!(
            err.message().contains("does not match any subject"),
            "{err}"
        );
    }

    /// Parse the real bundle, apply one structural mutation, re-serialize. Each K2 tamper test
    /// corrupts exactly one verification property, so a pass proves that property is *actually
    /// checked* (a verifier that skipped it would accept the mutant).
    fn mutated(mutate: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut bundle: serde_json::Value = serde_json::from_str(GHA_BUNDLE).unwrap();
        mutate(&mut bundle);
        bundle.to_string()
    }

    /// Flip one base64 character (M↔N) somewhere inside `s` — keeps it decodable, changes bytes.
    fn flip(s: &str) -> String {
        if let Some(i) = s.find(['M', 'N']) {
            let mut out = s.to_string();
            let flipped = if &s[i..=i] == "M" { "N" } else { "M" };
            out.replace_range(i..=i, flipped);
            out
        } else {
            panic!("no flippable char in {s}");
        }
    }

    #[test]
    fn a_tampered_certificate_is_rejected() {
        let bundle = mutated(|b| {
            let cert = b["verificationMaterial"]["certificate"]["rawBytes"]
                .as_str()
                .unwrap()
                .to_string();
            b["verificationMaterial"]["certificate"]["rawBytes"] = flip(&cert).into();
        });
        let err = verify_bundle(&bundle, GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(
            err.message().contains("keyless verification failed"),
            "{err}"
        );
    }

    #[test]
    fn a_tampered_inclusion_proof_is_rejected() {
        let bundle = mutated(|b| {
            let entry = &mut b["verificationMaterial"]["tlogEntries"][0];
            let h = entry["inclusionProof"]["hashes"][0]
                .as_str()
                .unwrap()
                .to_string();
            entry["inclusionProof"]["hashes"][0] = flip(&h).into();
        });
        let err = verify_bundle(&bundle, GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(
            err.message().to_lowercase().contains("inclusion")
                || err.message().to_lowercase().contains("proof"),
            "{err}"
        );
    }

    #[test]
    fn a_tampered_checkpoint_is_rejected() {
        // The checkpoint is the log's signed tree head. Corrupt its signature line: an attacker
        // who can forge inclusion in an unsigned tree could otherwise serve a private fork of
        // the log — checkpoint verification is what makes "in the log" mean something.
        let bundle = mutated(|b| {
            let entry = &mut b["verificationMaterial"]["tlogEntries"][0];
            let env = entry["inclusionProof"]["checkpoint"]["envelope"]
                .as_str()
                .unwrap()
                .to_string();
            // Flip inside the signature line (after "— rekor.sigstore.dev "), not the body.
            let sig_at = env.rfind("wNI9aj").unwrap();
            let (head, sig) = env.split_at(sig_at);
            entry["inclusionProof"]["checkpoint"]["envelope"] =
                format!("{head}{}", flip(sig)).into();
        });
        let err = verify_bundle(&bundle, GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(err.message().to_lowercase().contains("checkpoint"), "{err}");
    }

    #[test]
    fn a_missing_checkpoint_is_rejected() {
        let bundle = mutated(|b| {
            let proof = &mut b["verificationMaterial"]["tlogEntries"][0]["inclusionProof"];
            proof["checkpoint"] = serde_json::json!({ "envelope": "" });
        });
        let err = verify_bundle(&bundle, GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(
            err.message().to_lowercase().contains("checkpoint")
                || err.message().contains("validation failed"),
            "{err}"
        );
    }

    #[test]
    fn a_bundle_with_no_log_entry_is_rejected() {
        // No transparency-log entry = no public detectability = not keyless-verified. The
        // structural validator fails closed before any crypto runs.
        let bundle = mutated(|b| {
            b["verificationMaterial"]["tlogEntries"] = serde_json::json!([]);
        });
        let err = verify_bundle(&bundle, GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(err.message().contains("must have inclusion proof"), "{err}");
    }

    #[test]
    fn a_tampered_integrated_time_is_rejected() {
        // integratedTime is when the log recorded the signature; the inclusion promise (SET)
        // signs it. Moving it (e.g. to sneak inside a revoked cert's validity) must fail.
        let bundle = mutated(|b| {
            b["verificationMaterial"]["tlogEntries"][0]["integratedTime"] =
                "1838060096".to_string().into();
        });
        let err = verify_bundle(&bundle, GHA_SUBJECT_SHA256, None).unwrap_err();
        assert!(
            err.message().contains("keyless verification failed"),
            "{err}"
        );
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
        assert!(err.message().contains("failed"), "{err}");
    }

    #[test]
    fn the_publish_statement_binds_the_canonical_attestation_bytes() {
        let version = semver::Version::new(1, 2, 0);
        let attestation = Attestation {
            name: "acme/imgfx",
            version: &version,
            sha: "a3f9c2d1",
        };
        let coords = GitCoords {
            url: "https://github.com/acme/imgfx".to_string(),
            tag: "v1.2.0".to_string(),
            sha: "a3f9c2d1".to_string(),
        };

        // The subject digest IS sha256(canonical_bytes) — the pinned cross-format contract. The
        // fixed hex below (= sha256("noeta-attestation-v1\nacme/imgfx\n1.2.0\na3f9c2d1\n"),
        // cross-checked out-of-band) guards the whole chain (canonical bytes → sha256 →
        // statement): a drift in either the attestation format or the digest fails loudly here.
        let digest = attested_digest(&attestation);
        assert_eq!(
            digest,
            "6407bd48413706f3aee3c266551929b45b6826320cbf524a174d3ad6c883d98a"
        );

        let statement: serde_json::Value =
            serde_json::from_str(&publish_statement(&attestation, &coords)).unwrap();
        assert_eq!(statement["_type"], "https://in-toto.io/Statement/v1");
        assert_eq!(statement["subject"][0]["name"], "acme/imgfx@1.2.0");
        assert_eq!(statement["subject"][0]["digest"]["sha256"], digest.as_str());
        assert_eq!(statement["predicateType"], PREDICATE_TYPE);
        assert_eq!(statement["predicate"]["sha"], "a3f9c2d1");
        assert_eq!(
            statement["predicate"]["url"],
            "https://github.com/acme/imgfx"
        );
        assert_eq!(statement["predicate"]["tag"], "v1.2.0");

        // Deterministic: the same attestation yields byte-identical statements (what gets signed).
        assert_eq!(
            publish_statement(&attestation, &coords),
            publish_statement(&attestation, &coords)
        );
    }

    #[test]
    fn malformed_inputs_are_errors_not_panics() {
        assert!(verify_bundle("not json", GHA_SUBJECT_SHA256, None).is_err());
        assert!(verify_bundle(GHA_BUNDLE, "not hex", None).is_err());
        assert!(verify_bundle(GHA_BUNDLE, "abcd", None).is_err()); // wrong length
    }
}
