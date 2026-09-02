//! **Hermetic Sigstore test fixtures** (the `keyless-test-fixtures` feature).
//!
//! A complete in-process Sigstore: a test **CA** (Fulcio), a test **CT log**, and a test
//! **Rekor log**, each a deterministic P-256 key this module holds the private half of. The
//! Fulcio handler mints real certificates (Fulcio profile: empty subject, SAN URI identity,
//! issuer extension, code-signing EKU, **embedded SCT** signed by the CT key); the Rekor handler
//! mints real log entries (RFC 6962 leaf hash, size-1 inclusion proof, **signed checkpoint**).
//! [`TestSigstore::trusted_root_json`] emits the matching `trusted_root.json`.
//!
//! The point: the publish → resolve loop runs end-to-end with **no network and no weakened
//! verification** — bundles minted here verify under the *default* policy (chain, SCT,
//! inclusion proof, checkpoint), so the tests prove the same path production takes. The types
//! and encodings deliberately mirror `sigstore-verify`'s own internals (same `x509-cert`,
//! `tls_codec`, `const-oid` versions) so mint and verify cannot drift apart.
//!
//! Never enabled by a shipping binary — dev-dependency graphs only.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{DerSignature, SigningKey};
use p256::pkcs8::EncodePublicKey as _;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tls_codec::{SerializeBytes, TlsByteVecU16, TlsByteVecU24, TlsSerializeBytes, TlsSize};
use x509_cert::certificate::{Certificate, TbsCertificate, Version};
use x509_cert::der::asn1::{BitString, Ia5String, OctetString, Utf8StringRef};
use x509_cert::der::{Decode, Encode};
use x509_cert::ext::Extension;
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{
    BasicConstraints, ExtendedKeyUsage, KeyUsage, KeyUsages, SubjectAltName,
};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use x509_cert::time::{Time, Validity};

/// Fulcio's OIDC-issuer certificate extension (1.3.6.1.4.1.57264.1.1), a DER UTF8String.
const FULCIO_ISSUER_OID: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.3.6.1.4.1.57264.1.1");
/// ecdsa-with-SHA256 (the signature algorithm of every certificate minted here).
const ECDSA_WITH_SHA256: const_oid::ObjectIdentifier =
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");

/// The checkpoint origin line of the test Rekor log.
const REKOR_ORIGIN: &str = "noeta-test-rekor";

/// A deterministic in-process Sigstore (test CA + CT log + Rekor log). The private keys are
/// fixed so every run mints against the same root; times are taken from the real clock because
/// verification compares the log's integrated time against the certificate validity and *now*.
#[derive(Debug)]
pub struct TestSigstore {
    ca_key: SigningKey,
    ct_key: SigningKey,
    rekor_key: SigningKey,
    ca_cert_der: Vec<u8>,
    /// The OIDC issuer the minted certificates claim.
    pub issuer: String,
    /// The identity (SAN URI) the minted certificates certify.
    pub identity: String,
}

impl TestSigstore {
    /// A test Sigstore whose Fulcio will certify exactly `issuer`/`identity`.
    pub fn new(issuer: &str, identity: &str) -> TestSigstore {
        let ca_key = SigningKey::from_bytes(&[7u8; 32].into()).expect("fixed CA key");
        let ct_key = SigningKey::from_bytes(&[11u8; 32].into()).expect("fixed CT key");
        let rekor_key = SigningKey::from_bytes(&[13u8; 32].into()).expect("fixed Rekor key");
        let ca_cert_der = mint_ca(&ca_key);
        TestSigstore {
            ca_key,
            ct_key,
            rekor_key,
            ca_cert_der,
            issuer: issuer.to_string(),
            identity: identity.to_string(),
        }
    }

    /// The `trusted_root.json` binding this test Sigstore's public halves — what
    /// `NOETA_SIGSTORE_TRUST_ROOT` points at in tests.
    pub fn trusted_root_json(&self) -> String {
        let log = |key: &SigningKey, url: &str| {
            let spki = spki_der(key);
            serde_json::json!({
                "baseUrl": url,
                "hashAlgorithm": "SHA2_256",
                "publicKey": {
                    "rawBytes": B64.encode(&spki),
                    "keyDetails": "PKIX_ECDSA_P256_SHA_256",
                    "validFor": { "start": "2020-01-01T00:00:00Z" }
                },
                "logId": { "keyId": B64.encode(sha256(&spki)) }
            })
        };
        serde_json::json!({
            "mediaType": "application/vnd.dev.sigstore.trustedroot+json;version=0.1",
            "tlogs": [log(&self.rekor_key, "https://rekor.noeta.test")],
            "certificateAuthorities": [{
                "subject": { "organization": "noeta-test" },
                "uri": "https://fulcio.noeta.test",
                "certChain": { "certificates": [{ "rawBytes": B64.encode(&self.ca_cert_der) }] },
                "validFor": { "start": "2020-01-01T00:00:00Z" }
            }],
            "ctlogs": [log(&self.ct_key, "https://ctlog.noeta.test")]
        })
        .to_string()
    }

    /// An (unsigned) OIDC JWT asserting this fixture's issuer/identity. Fulcio is what vouches
    /// for token authenticity in the real flow, and here the mock Fulcio is ours — the signature
    /// is never checked by the client. An email identity is asserted as the `email` claim
    /// (Dex-style interactive login); anything else as the subject (CI-style).
    pub fn fake_jwt(&self) -> String {
        let b64url = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let header = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let now = unix_now();
        let mut claims = serde_json::json!({
            "iss": self.issuer,
            "sub": self.identity,
            "aud": "sigstore",
            "iat": now - 30,
            "exp": now + 600,
        });
        if self.identity.contains('@') {
            claims["email"] = self.identity.clone().into();
            claims["email_verified"] = true.into();
        }
        format!(
            "{header}.{}.unsigned",
            b64url(claims.to_string().as_bytes())
        )
    }

    /// The mock **GitHub Actions token endpoint** response wrapping [`TestSigstore::fake_jwt`].
    pub fn github_token_response(&self) -> String {
        serde_json::json!({ "value": self.fake_jwt() }).to_string()
    }

    /// The mock **OIDC provider** for the interactive login: the authorization-code flow with
    /// **real PKCE enforcement**, stateless — the issued code encodes the challenge + state, and
    /// the token exchange recomputes `b64url(sha256(verifier))` against it. `GET /auth` plays
    /// the login page (returning the code as JSON where a browser would display it);
    /// `POST /token` exchanges it for an ID token.
    pub fn handle_oidc(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        if method == "GET" && path.contains("/auth") {
            let query: std::collections::BTreeMap<String, String> = path
                .split_once('?')
                .map(|(_, q)| q)
                .unwrap_or_default()
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let (Some(challenge), Some(state)) = (query.get("code_challenge"), query.get("state"))
            else {
                return (
                    400,
                    r#"{"error":"missing code_challenge/state"}"#.to_string(),
                );
            };
            if query.get("code_challenge_method").map(String::as_str) != Some("S256") {
                return (400, r#"{"error":"only S256 supported"}"#.to_string());
            }
            return (
                200,
                serde_json::json!({ "code": format!("{challenge}.{state}") }).to_string(),
            );
        }
        if method == "POST" && path.contains("/token") {
            let form: std::collections::BTreeMap<String, String> = body
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.to_string(), urldecode(v)))
                .collect();
            let (Some(code), Some(verifier)) = (form.get("code"), form.get("code_verifier")) else {
                return (400, r#"{"error":"missing code/code_verifier"}"#.to_string());
            };
            let Some((challenge, _state)) = code.split_once('.') else {
                return (400, r#"{"error":"malformed code"}"#.to_string());
            };
            let recomputed = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha256(verifier.as_bytes()));
            if recomputed != *challenge {
                return (400, r#"{"error":"PKCE verification failed"}"#.to_string());
            }
            return (
                200,
                serde_json::json!({
                    "access_token": "mock-access-token",
                    "token_type": "Bearer",
                    "id_token": self.fake_jwt(),
                })
                .to_string(),
            );
        }
        (404, r#"{"error":"unknown oidc endpoint"}"#.to_string())
    }

    /// The mock **Fulcio**: `POST /api/v2/signingCert` → a leaf certificate (Fulcio profile,
    /// embedded SCT) for the requester's public key, chained to the test CA.
    pub fn handle_fulcio(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        if method != "POST" || !path.ends_with("/signingCert") {
            return (404, r#"{"error":"unknown fulcio endpoint"}"#.to_string());
        }
        let request: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(err) => return (400, format!("{{\"error\":\"bad request: {err}\"}}")),
        };
        let Some(pem) = request["publicKeyRequest"]["publicKey"]["content"].as_str() else {
            return (400, r#"{"error":"missing public key"}"#.to_string());
        };
        let Ok(spki) = pem::parse_pem_body(pem) else {
            return (400, r#"{"error":"unparseable public key PEM"}"#.to_string());
        };
        let leaf_der = self.mint_leaf(&spki);
        let response = serde_json::json!({
            "signedCertificateEmbeddedSct": {
                "chain": {
                    "certificates": [to_pem("CERTIFICATE", &leaf_der),
                                     to_pem("CERTIFICATE", &self.ca_cert_der)]
                }
            }
        });
        (200, response.to_string())
    }

    /// The mock **Rekor** (v1): `POST /api/v1/log/entries` → a size-1 log holding exactly this
    /// entry: canonicalized body, RFC 6962 leaf hash as the root, empty inclusion path, and a
    /// checkpoint signed by the test log key. No inclusion promise (SET) — the inclusion proof +
    /// checkpoint path is the one consumers rely on offline.
    pub fn handle_rekor(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        if method != "POST" || !path.ends_with("/log/entries") {
            return (404, r#"{"error":"unknown rekor endpoint"}"#.to_string());
        }
        let proposed: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(err) => return (400, format!("{{\"error\":\"bad entry: {err}\"}}")),
        };
        // Rekor stores the *processed* entry, not the proposal: the envelope's hashes are
        // computed server-side and the signatures are lifted out of the envelope next to their
        // verifiers. The consumer's consistency check (CVE-2022-36056) re-derives the payload
        // hash and signature/verifier pairs from the bundle and compares against this body.
        let envelope_json = proposed["spec"]["proposedContent"]["envelope"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let verifier_b64 = proposed["spec"]["proposedContent"]["verifiers"][0]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let envelope: serde_json::Value = serde_json::from_str(&envelope_json).unwrap_or_default();
        let payload_b64 = envelope["payload"].as_str().unwrap_or_default();
        let Ok(payload) = B64.decode(payload_b64) else {
            return (400, r#"{"error":"bad envelope payload"}"#.to_string());
        };
        let sig_b64 = envelope["signatures"][0]["sig"]
            .as_str()
            .unwrap_or_default();
        let stored = serde_json::json!({
            "apiVersion": "0.0.1",
            "kind": "dsse",
            "spec": {
                "envelopeHash": { "algorithm": "sha256", "value": hex(&sha256(envelope_json.as_bytes())) },
                "payloadHash": { "algorithm": "sha256", "value": hex(&sha256(&payload)) },
                "signatures": [{ "signature": sig_b64, "verifier": verifier_b64 }]
            }
        });
        // The canonicalized body is what the log commits to: leaf hash, Merkle root, checkpoint
        // and the bundle's `canonicalizedBody` all derive from these exact bytes.
        let canonical = stored.to_string();
        let leaf = rfc6962_leaf_hash(canonical.as_bytes());
        let spki = spki_der(&self.rekor_key);
        let note = format!("{REKOR_ORIGIN}\n1\n{}\n", B64.encode(&leaf));
        let sig: DerSignature = self.rekor_key.sign(note.as_bytes());
        let mut hint_and_sig = sha256(&spki)[..4].to_vec();
        hint_and_sig.extend_from_slice(sig.as_bytes());
        let checkpoint = format!("{note}\n— {REKOR_ORIGIN} {}\n", B64.encode(hint_and_sig));
        let integrated_time = unix_now() - 60;
        // The SET (inclusion promise): the log's signature over the RFC 8785 canonical JSON of
        // the entry coordinates. With these key names and simple values, sorted-key compact
        // JSON (what serde_json's BTreeMap emits) IS the JCS form the verifier reconstructs.
        // This is what lets a consumer trust `integratedTime` — a v1 bundle's signature time.
        let body_b64 = B64.encode(canonical.as_bytes());
        let set_payload = serde_json::json!({
            "body": body_b64,
            "integratedTime": integrated_time,
            "logID": hex(&sha256(&spki)),
            "logIndex": 0,
        })
        .to_string();
        let set_sig: DerSignature = self.rekor_key.sign(set_payload.as_bytes());
        let response = serde_json::json!({
            "e4a0e5b1c1a94f5c8d5e1f2a3b4c5d6e": {
                "body": body_b64,
                "integratedTime": integrated_time,
                "logID": hex(&sha256(&spki)),
                "logIndex": 0,
                "verification": {
                    "inclusionProof": {
                        "checkpoint": checkpoint,
                        "hashes": [],
                        "logIndex": 0,
                        "rootHash": hex(&leaf),
                        "treeSize": 1
                    },
                    "signedEntryTimestamp": B64.encode(set_sig.as_bytes())
                }
            }
        });
        (201, response.to_string())
    }

    /// Mint a Fulcio-profile leaf for `spki` (the requester's ephemeral public key): empty
    /// subject, critical SAN URI = identity, OIDC-issuer extension, digital-signature KU,
    /// code-signing EKU, and an **embedded SCT** — the CT log's signature over the
    /// RFC 6962 precertificate (the TBS *without* the SCT extension).
    fn mint_leaf(&self, spki: &[u8]) -> Vec<u8> {
        let now = SystemTime::now();
        let validity = Validity {
            not_before: Time::try_from(now - Duration::from_secs(3600)).expect("time"),
            not_after: Time::try_from(now + Duration::from_secs(3600)).expect("time"),
        };
        let ca_cert = Certificate::from_der(&self.ca_cert_der).expect("own CA parses");
        let algorithm = AlgorithmIdentifierOwned {
            oid: ECDSA_WITH_SHA256,
            parameters: None,
        };

        // The Fulcio-profile extensions (sans SCT).
        let ku = KeyUsage(KeyUsages::DigitalSignature.into());
        let eku = ExtendedKeyUsage(vec![const_oid::db::rfc5280::ID_KP_CODE_SIGNING]);
        // Fulcio's SAN kind follows the identity: an email (interactive Dex login) is an
        // rfc822Name, a workflow/CI identity a URI.
        let san_name = if self.identity.contains('@') {
            GeneralName::Rfc822Name(Ia5String::new(&self.identity).expect("identity is IA5"))
        } else {
            GeneralName::UniformResourceIdentifier(
                Ia5String::new(&self.identity).expect("identity is IA5"),
            )
        };
        let san = SubjectAltName(vec![san_name]);
        let issuer_ext = Extension {
            extn_id: FULCIO_ISSUER_OID,
            critical: false,
            extn_value: OctetString::new(
                Utf8StringRef::new(&self.issuer)
                    .expect("issuer is UTF-8")
                    .to_der()
                    .expect("issuer encodes"),
            )
            .expect("issuer wraps"),
        };
        let mut extensions = vec![
            typed_ext(const_oid::db::rfc5280::ID_CE_KEY_USAGE, true, &ku),
            typed_ext(const_oid::db::rfc5280::ID_CE_EXT_KEY_USAGE, false, &eku),
            typed_ext(const_oid::db::rfc5280::ID_CE_SUBJECT_ALT_NAME, true, &san),
            issuer_ext,
        ];

        let mut tbs = TbsCertificate {
            version: Version::V3,
            serial_number: SerialNumber::from(2u32),
            signature: algorithm.clone(),
            issuer: ca_cert.tbs_certificate.subject.clone(),
            validity,
            subject: Name::default(), // Fulcio leaves the subject empty; the SAN is the identity
            subject_public_key_info: SubjectPublicKeyInfoOwned::from_der(spki)
                .expect("requester SPKI parses"),
            issuer_unique_id: None,
            subject_unique_id: None,
            extensions: Some(extensions.clone()),
        };

        // RFC 6962: the CT log signs the *precertificate* — this TBS before the SCT extension
        // exists. The verifier reconstructs exactly these bytes by stripping the SCT extension.
        let precert_tbs = tbs.to_der().expect("precert TBS encodes");
        let issuer_key_hash = sha256(
            &ca_cert
                .tbs_certificate
                .subject_public_key_info
                .to_der()
                .expect("CA SPKI"),
        );
        let timestamp_ms = (unix_now() as u64) * 1000;
        let signed = CtSignedData {
            version: 0, // v1
            signature_type: 0,
            timestamp: timestamp_ms,
            signed_entry: CtSignedEntry::PrecertEntry(CtPreCert {
                issuer_key_hash: issuer_key_hash.try_into().expect("32 bytes"),
                tbs_certificate: precert_tbs.as_slice().into(),
            }),
            extensions: TlsByteVecU16::new(Vec::new()),
        };
        let ct_message = signed.tls_serialize().expect("SCT data serializes");
        let ct_sig: DerSignature = self.ct_key.sign(&ct_message);
        let sct_list = encode_sct_list(
            &sha256(&spki_der(&self.ct_key)),
            timestamp_ms,
            ct_sig.as_bytes(),
        );
        extensions.push(Extension {
            extn_id: const_oid::db::rfc6962::CT_PRECERT_SCTS,
            critical: false,
            extn_value: OctetString::new(
                OctetString::new(sct_list)
                    .expect("SCT list wraps")
                    .to_der()
                    .expect("SCT list encodes"),
            )
            .expect("SCT ext wraps"),
        });
        tbs.extensions = Some(extensions);

        // CA-sign the final TBS.
        let tbs_der = tbs.to_der().expect("final TBS encodes");
        let ca_sig: DerSignature = self.ca_key.sign(&tbs_der);
        Certificate {
            tbs_certificate: tbs,
            signature_algorithm: algorithm,
            signature: BitString::from_bytes(ca_sig.as_bytes()).expect("signature fits"),
        }
        .to_der()
        .expect("certificate encodes")
    }
}

/// Self-signed test CA: basicConstraints CA (critical) + keyCertSign, minted with the same
/// manual TBS machinery as the leaf (the one code path, no builder trait gymnastics).
fn mint_ca(ca_key: &SigningKey) -> Vec<u8> {
    let subject: Name = "CN=noeta test CA,O=noeta-test".parse().expect("CA name");
    let validity = Validity {
        not_before: Time::try_from(SystemTime::now() - Duration::from_secs(3600)).expect("time"),
        not_after: Time::try_from(SystemTime::now() + Duration::from_secs(30 * 24 * 3600))
            .expect("time"),
    };
    let algorithm = AlgorithmIdentifierOwned {
        oid: ECDSA_WITH_SHA256,
        parameters: None,
    };
    let basic_constraints = BasicConstraints {
        ca: true,
        path_len_constraint: None,
    };
    let ku = KeyUsage(KeyUsages::KeyCertSign | KeyUsages::CRLSign);
    let tbs = TbsCertificate {
        version: Version::V3,
        serial_number: SerialNumber::from(1u32),
        signature: algorithm.clone(),
        issuer: subject.clone(),
        validity,
        subject,
        subject_public_key_info: SubjectPublicKeyInfoOwned::from_der(&spki_der(ca_key))
            .expect("CA SPKI parses"),
        issuer_unique_id: None,
        subject_unique_id: None,
        extensions: Some(vec![
            typed_ext(
                const_oid::db::rfc5280::ID_CE_BASIC_CONSTRAINTS,
                true,
                &basic_constraints,
            ),
            typed_ext(const_oid::db::rfc5280::ID_CE_KEY_USAGE, true, &ku),
        ]),
    };
    let tbs_der = tbs.to_der().expect("CA TBS encodes");
    let sig: DerSignature = ca_key.sign(&tbs_der);
    Certificate {
        tbs_certificate: tbs,
        signature_algorithm: algorithm,
        signature: BitString::from_bytes(sig.as_bytes()).expect("signature fits"),
    }
    .to_der()
    .expect("CA encodes")
}

/// Encode a typed pkix extension (`KeyUsage`, `SubjectAltName`, …) as a raw [`Extension`].
fn typed_ext<T: Encode>(oid: const_oid::ObjectIdentifier, critical: bool, value: &T) -> Extension {
    Extension {
        extn_id: oid,
        critical,
        extn_value: OctetString::new(value.to_der().expect("extension encodes"))
            .expect("extension wraps"),
    }
}

// ── RFC 6962 TLS structures (serialize-only mirrors of sigstore-verify's parse side) ──────────

#[derive(TlsSerializeBytes, TlsSize)]
struct CtPreCert {
    issuer_key_hash: [u8; 32],
    tbs_certificate: TlsByteVecU24,
}

#[derive(TlsSerializeBytes, TlsSize)]
#[repr(u16)]
enum CtSignedEntry {
    #[allow(unused)]
    #[tls_codec(discriminant = 0)]
    X509Entry(TlsByteVecU24),
    #[tls_codec(discriminant = 1)]
    PrecertEntry(CtPreCert),
}

#[derive(TlsSerializeBytes, TlsSize)]
struct CtSignedData {
    version: u8,
    signature_type: u8,
    timestamp: u64,
    signed_entry: CtSignedEntry,
    extensions: TlsByteVecU16,
}

/// The `SignedCertificateTimestampList` TLS payload: one embedded SCT
/// `{v1, log_id, timestamp, no extensions, ecdsa-sha256, signature}`.
fn encode_sct_list(ct_log_id: &[u8], timestamp_ms: u64, der_sig: &[u8]) -> Vec<u8> {
    let mut sct = Vec::new();
    sct.push(0u8); // version v1
    sct.extend_from_slice(ct_log_id); // 32-byte log id (sha256 of the CT key's SPKI)
    sct.extend_from_slice(&timestamp_ms.to_be_bytes());
    sct.extend_from_slice(&0u16.to_be_bytes()); // no extensions
    sct.extend_from_slice(&[0x04, 0x03]); // SignatureAndHashAlgorithm: sha256 + ecdsa
    sct.extend_from_slice(&(der_sig.len() as u16).to_be_bytes());
    sct.extend_from_slice(der_sig);

    let mut entry = (sct.len() as u16).to_be_bytes().to_vec();
    entry.extend_from_slice(&sct);
    let mut list = (entry.len() as u16).to_be_bytes().to_vec();
    list.extend_from_slice(&entry);
    list
}

// ── Small helpers ──────────────────────────────────────────────────────────────────────────────

fn spki_der(key: &SigningKey) -> Vec<u8> {
    key.verifying_key()
        .to_public_key_der()
        .expect("SPKI encodes")
        .into_vec()
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

/// RFC 6962 leaf hash: `sha256(0x00 || data)`.
fn rfc6962_leaf_hash(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update([0u8]);
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Minimal `application/x-www-form-urlencoded` value decoding (%XX + '+' → space) — enough for
/// the token-exchange form fields the mock OIDC provider parses.
fn urldecode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex_pair = &value[i + 1..i + 3];
                if let Ok(byte) = u8::from_str_radix(hex_pair, 16) {
                    out.push(byte);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after epoch")
        .as_secs() as i64
}

fn to_pem(tag: &str, der: &[u8]) -> String {
    let b64 = B64.encode(der);
    let lines: Vec<&str> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect();
    format!(
        "-----BEGIN {tag}-----\n{}\n-----END {tag}-----\n",
        lines.join("\n")
    )
}

mod pem {
    /// Extract the DER body of the first PEM block (enough for the SPKI PEM Fulcio receives).
    pub fn parse_pem_body(pem: &str) -> Result<Vec<u8>, String> {
        use base64::Engine as _;
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .map_err(|err| format!("bad PEM body: {err}"))
    }
}

/// A tiny keep-alive mock HTTP/1.1 server for the fixture handlers: serves every incoming
/// connection on a background thread with `handler(method, path, body)`. Returns the base URL.
/// Hermetic — binds 127.0.0.1 only.
pub fn spawn_mock(
    handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static,
) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                if header == "\r\n" || header == "\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 && reader.read_exact(&mut body).is_err() {
                continue;
            }
            let (status, json) = handler(&method, &path, &String::from_utf8_lossy(&body));
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                json.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyless;
    use crate::provenance::Attestation;
    use crate::registry::GitCoords;
    use sigstore_trust_root::TrustedRoot;

    const ISSUER: &str = "https://token.actions.githubusercontent.com";
    const IDENTITY: &str =
        "https://github.com/acme/imgfx/.github/workflows/release.yaml@refs/heads/main";

    /// The whole keyless loop, hermetic, under the DEFAULT verification policy: mock Fulcio
    /// mints a real cert (embedded SCT), mock Rekor mints a real inclusion proof + signed
    /// checkpoint, the publish seam assembles the bundle, and the verify seam accepts it —
    /// chain, SCT, inclusion, checkpoint, artifact binding, identity policy, all of it.
    #[test]
    fn publish_then_verify_round_trips_hermetically() {
        let sigstore = std::sync::Arc::new(TestSigstore::new(ISSUER, IDENTITY));
        let fulcio = {
            let s = sigstore.clone();
            spawn_mock(move |m, p, b| s.handle_fulcio(m, p, b))
        };
        let rekor = {
            let s = sigstore.clone();
            spawn_mock(move |m, p, b| s.handle_rekor(m, p, b))
        };

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
        let statement = keyless::publish_statement(&attestation, &coords);
        let token = {
            let response: serde_json::Value =
                serde_json::from_str(&sigstore.github_token_response()).unwrap();
            keyless::AmbientIdentity::from_jwt(response["value"].as_str().unwrap()).unwrap()
        };

        let bundle =
            keyless::publish_bundle_at(statement.as_bytes(), token, &fulcio, &rekor).unwrap();

        // Verify against the fixture trust root — the full default policy, no skips.
        let root = TrustedRoot::from_json(&sigstore.trusted_root_json()).unwrap();
        let digest = keyless::attested_digest(&attestation);
        let pin = keyless::IdentityPolicy {
            issuer: ISSUER.to_string(),
            identity: IDENTITY.to_string(),
        };
        let verified =
            keyless::verify_bundle_with_root(&bundle, &digest, Some(&pin), &root).unwrap();
        assert_eq!(verified.identity, IDENTITY);
        assert_eq!(verified.issuer, ISSUER);
        assert!(verified.integrated_time.is_some());

        // The wrong identity pin still fails against a genuinely-verifying bundle.
        let wrong = keyless::IdentityPolicy {
            issuer: ISSUER.to_string(),
            identity: "https://github.com/evil/repo/.github/workflows/x.yaml@refs/heads/main"
                .to_string(),
        };
        let err =
            keyless::verify_bundle_with_root(&bundle, &digest, Some(&wrong), &root).unwrap_err();
        assert!(err.message().contains("identity mismatch"), "{err}");

        // And the production trust root rejects it (the fixture CA is NOT sigstore.dev).
        let err = keyless::verify_bundle(&bundle, &digest, None).unwrap_err();
        assert!(
            err.message().contains("keyless verification failed"),
            "{err}"
        );
    }

    /// A programmatic [`AuthCallback`]: "the user" fetches the auth URL itself (hitting the
    /// mock provider's login page) and pastes back the code it returns — the whole OOB flow
    /// with no stdin and no browser, PKCE enforced by the mock.
    struct ScriptedLogin {
        auth_url: std::sync::Mutex<Option<String>>,
    }

    impl sigstore_oidc::templates::HtmlTemplates for ScriptedLogin {
        fn success_html(&self) -> &str {
            "ok"
        }
        fn error_html(&self, error: &str) -> String {
            error.to_string()
        }
    }

    impl sigstore_oidc::oauth::AuthCallback for ScriptedLogin {
        fn auth_url_ready(&self, url: &str, _mode: sigstore_oidc::oauth::AuthMode) {
            *self.auth_url.lock().unwrap() = Some(url.to_string());
        }

        fn prompt_for_code(&self) -> std::io::Result<String> {
            // Visit the login page like a browser would; the mock returns the code as JSON.
            let url = self
                .auth_url
                .lock()
                .unwrap()
                .clone()
                .expect("auth URL announced");
            let body = http_get(&url);
            let page: serde_json::Value = serde_json::from_str(&body).expect("login page JSON");
            Ok(page["code"].as_str().expect("code on page").to_string())
        }

        fn waiting_for_redirect(&self) {}
        fn auth_complete(&self) {}
    }

    /// Plain-std HTTP GET against a 127.0.0.1 mock — returns the response body.
    fn http_get(url: &str) -> String {
        use std::io::{Read, Write};
        let rest = url.strip_prefix("http://").expect("http mock URL");
        let (host, path) = rest.split_once('/').expect("URL has a path");
        let mut stream = std::net::TcpStream::connect(host).expect("connect mock");
        write!(
            stream,
            "GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        )
        .expect("send request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .expect("response has a body")
    }

    /// The interactive login end-to-end, hermetic: OAuth authorization-code flow with the
    /// mock provider ENFORCING PKCE, an email identity (rfc822 SAN in the minted cert), then the
    /// same Fulcio/Rekor publish and default-policy verification as the CI path.
    #[test]
    fn interactive_login_publish_then_verify_round_trips() {
        let issuer_placeholder = "set-below";
        let email = "maintainer@example.test";
        // The OIDC mock needs to exist before we know its URL; the issuer inside the fake JWT
        // is informational for this test (Fulcio stamps the fixture's configured issuer).
        let sigstore = std::sync::Arc::new(TestSigstore::new(issuer_placeholder, email));
        let oidc = {
            let s = sigstore.clone();
            spawn_mock(move |m, p, b| s.handle_oidc(m, p, b))
        };
        // Rebuild the fixture with the real issuer URL so JWT, cert extension and pin agree.
        let sigstore = std::sync::Arc::new(TestSigstore::new(&oidc, email));
        let oidc = {
            let s = sigstore.clone();
            spawn_mock(move |m, p, b| s.handle_oidc(m, p, b))
        };
        let fulcio = {
            let s = sigstore.clone();
            spawn_mock(move |m, p, b| s.handle_fulcio(m, p, b))
        };
        let rekor = {
            let s = sigstore.clone();
            spawn_mock(move |m, p, b| s.handle_rekor(m, p, b))
        };

        // "Sign in": the scripted user completes the OOB OAuth flow against the mock provider.
        let login = ScriptedLogin {
            auth_url: std::sync::Mutex::new(None),
        };
        let identity =
            keyless::interactive_identity_at(Some(&oidc), login, true).expect("login succeeds");
        assert_eq!(identity.identity(), email);

        // Publish + verify exactly as the CI path does.
        let version = semver::Version::new(2, 0, 0);
        let attestation = Attestation {
            name: "acme/imgfx",
            version: &version,
            sha: "b4e8d7c6",
        };
        let coords = GitCoords {
            url: "https://github.com/acme/imgfx".to_string(),
            tag: "v2.0.0".to_string(),
            sha: "b4e8d7c6".to_string(),
        };
        let statement = keyless::publish_statement(&attestation, &coords);
        let bundle = keyless::publish_bundle_at(statement.as_bytes(), identity, &fulcio, &rekor)
            .expect("keyless signing succeeds");

        let root = TrustedRoot::from_json(&sigstore.trusted_root_json()).unwrap();
        let digest = keyless::attested_digest(&attestation);
        let pin = keyless::IdentityPolicy {
            issuer: sigstore.issuer.clone(),
            identity: email.to_string(),
        };
        let verified =
            keyless::verify_bundle_with_root(&bundle, &digest, Some(&pin), &root).unwrap();
        assert_eq!(verified.identity, email);

        // A wrong verifier is refused by the provider: PKCE is genuinely enforced.
        let (status, body) = sigstore.handle_oidc(
            "POST",
            "/token",
            "client_id=sigstore&code=bogus-challenge.state&code_verifier=wrong&grant_type=authorization_code&redirect_uri=x",
        );
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("PKCE"), "{body}");
    }
}
