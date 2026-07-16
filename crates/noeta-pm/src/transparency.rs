//! Client-side transparency-log verification (namespace-protection #1) — the verify half of the
//! registry's RFC 6962 Merkle log (`noeta-registry/src/merkle.ts`). A client uses this to check,
//! without trusting the registry, that a resolved release is **included** in the log at a signed
//! checkpoint, and that the log is a **consistent** (append-only) extension of a checkpoint it pinned
//! earlier — so a registry compromised after first use can't rewrite history or equivocate.
//!
//! Behind the `provenance` feature (it needs Ed25519 + SHA-256), like the rest of the trust story.
//! The canonical record and checkpoint byte formats MUST match the server's exactly.

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

/// A 32-byte SHA-256 hash — a Merkle node or leaf.
pub type Hash = [u8; 32];

/// RFC 6962 leaf hash: `SHA-256(0x00 || data)`.
pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([0x00]);
    h.update(data);
    h.finalize().into()
}

/// RFC 6962 interior hash: `SHA-256(0x01 || left || right)`.
fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([0x01]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The canonical log record for a release — the exact bytes whose leaf the log stored, reproduced here
/// so a client can recompute the leaf and check it against the served record. MUST match the server's
/// `logRecord`. `provenance` is `key:{sig}`, `keyless:{sha256hex(bundle)}`, or `unsigned`; `license`
/// is the declared SPDX expression, or "" when the release declared none.
///
/// Record fields are only ever **appended** (`license` came after the original six), and verification
/// parses length-tolerantly: a record missing trailing fields predates them and still verifies
/// against what it does bind.
pub fn log_record(
    name: &str,
    version: &str,
    url: &str,
    tag: &str,
    sha: &str,
    provenance: &str,
    license: &str,
) -> String {
    format!(
        "noeta-transparency-log-v1\n{name}\n{version}\n{url}\n{tag}\n{sha}\n{provenance}\n{license}\n"
    )
}

/// Verify that `leaf` at index `m` in a tree of `size` with `root` is proven by `proof`.
pub fn verify_inclusion(leaf: Hash, m: usize, size: usize, proof: &[Hash], root: &Hash) -> bool {
    if m >= size {
        return false;
    }
    root_from_inclusion(leaf, m, size, proof).is_some_and(|r| r == *root)
}

fn root_from_inclusion(leaf: Hash, m: usize, n: usize, proof: &[Hash]) -> Option<Hash> {
    if n <= 1 {
        return proof.is_empty().then_some(leaf);
    }
    let (sibling, rest) = proof.split_last()?;
    let k = largest_pow2_below(n);
    if m < k {
        let left = root_from_inclusion(leaf, m, k, rest)?;
        Some(node_hash(&left, sibling))
    } else {
        let right = root_from_inclusion(leaf, m - k, n - k, rest)?;
        Some(node_hash(sibling, &right))
    }
}

/// Verify a consistency `proof` that a tree of size `m`/root `root_m` is a prefix of one of size `n`/
/// root `root_n` (the append-only guarantee between two checkpoints).
pub fn verify_consistency(
    m: usize,
    n: usize,
    proof: &[Hash],
    root_m: &Hash,
    root_n: &Hash,
) -> bool {
    if m > n {
        return false;
    }
    if m == n {
        return proof.is_empty() && root_m == root_n;
    }
    if m == 0 {
        return true;
    }
    match reconstruct_consistency(proof, m, n, true, root_m) {
        Some((lo, hi)) => lo == *root_m && hi == *root_n,
        None => false,
    }
}

/// Reconstruct `(MTH(0:m), MTH(0:n))` from a consistency proof, mirroring the server's `subproof`.
/// `b` marks that the m-tree root at this level is the elided pinned `root_m` (the power-of-two seed).
fn reconstruct_consistency(
    proof: &[Hash],
    m: usize,
    n: usize,
    b: bool,
    root_m: &Hash,
) -> Option<(Hash, Hash)> {
    if m == n {
        if b {
            return Some((*root_m, *root_m));
        }
        let h = proof.last()?;
        return Some((*h, *h));
    }
    let (sibling, rest) = proof.split_last()?;
    let k = largest_pow2_below(n);
    if m <= k {
        let (lo, hi) = reconstruct_consistency(rest, m, k, b, root_m)?;
        Some((lo, node_hash(&hi, sibling)))
    } else {
        let (lo, hi) = reconstruct_consistency(rest, m - k, n - k, false, root_m)?;
        Some((node_hash(sibling, &lo), node_hash(sibling, &hi)))
    }
}

/// Verify a checkpoint's Ed25519 signature over `noeta-log-checkpoint-v1\n{size}\n{root_hex}\n` against
/// the log's pinned public key (all hex). `Ok(false)` = a well-formed but non-verifying signature;
/// `Err` = malformed key/signature/root input.
pub fn verify_checkpoint(
    public_hex: &str,
    size: u64,
    root_hex: &str,
    signature_hex: &str,
) -> Result<bool, String> {
    let pk: [u8; 32] = hex_to_array(public_hex).ok_or("log public key is not 32 hex bytes")?;
    let key = VerifyingKey::from_bytes(&pk).map_err(|err| format!("bad log public key: {err}"))?;
    let sig: [u8; 64] =
        hex_to_array(signature_hex).ok_or("checkpoint signature is not 64 hex bytes")?;
    let signature = Signature::from_bytes(&sig);
    let msg = format!("noeta-log-checkpoint-v1\n{size}\n{root_hex}\n");
    Ok(key.verify_strict(msg.as_bytes(), &signature).is_ok())
}

/// Parse a hex string into a fixed-size byte array (`None` on wrong length or a non-hex digit).
pub fn hex_to_array<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The largest power of two strictly less than `n` (n ≥ 2) — RFC 6962's split point `k`.
fn largest_pow2_below(n: usize) -> usize {
    let mut k = 1;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // --- proof generation (server-side; here only to drive the verifier over many shapes) ----------

    fn merkle_root(leaves: &[Hash]) -> Hash {
        match leaves.len() {
            0 => Sha256::digest([]).into(),
            1 => leaves[0],
            n => {
                let k = largest_pow2_below(n);
                node_hash(&merkle_root(&leaves[..k]), &merkle_root(&leaves[k..]))
            }
        }
    }

    fn inclusion_proof(leaves: &[Hash], m: usize) -> Vec<Hash> {
        let n = leaves.len();
        if n <= 1 {
            return Vec::new();
        }
        let k = largest_pow2_below(n);
        if m < k {
            let mut p = inclusion_proof(&leaves[..k], m);
            p.push(merkle_root(&leaves[k..]));
            p
        } else {
            let mut p = inclusion_proof(&leaves[k..], m - k);
            p.push(merkle_root(&leaves[..k]));
            p
        }
    }

    fn consistency_proof(leaves: &[Hash], m: usize) -> Vec<Hash> {
        subproof(m, leaves, true)
    }
    fn subproof(m: usize, leaves: &[Hash], b: bool) -> Vec<Hash> {
        let n = leaves.len();
        if m == n {
            return if b {
                Vec::new()
            } else {
                vec![merkle_root(leaves)]
            };
        }
        let k = largest_pow2_below(n);
        if m <= k {
            let mut p = subproof(m, &leaves[..k], b);
            p.push(merkle_root(&leaves[k..]));
            p
        } else {
            let mut p = subproof(m - k, &leaves[k..], false);
            p.push(merkle_root(&leaves[..k]));
            p
        }
    }

    fn leaves(n: usize) -> Vec<Hash> {
        (0..n)
            .map(|i| leaf_hash(format!("entry-{i}").as_bytes()))
            .collect()
    }

    const SIZES: &[usize] = &[1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 23];

    #[test]
    fn inclusion_verifies_for_every_leaf_and_size() {
        for &n in SIZES {
            let ls = leaves(n);
            let root = merkle_root(&ls);
            for m in 0..n {
                let proof = inclusion_proof(&ls, m);
                assert!(verify_inclusion(ls[m], m, n, &proof, &root), "n={n} m={m}");
                // A different leaf under the same proof/root must fail.
                let other = (m + 1) % n;
                if other != m {
                    assert!(!verify_inclusion(ls[other], m, n, &proof, &root));
                }
            }
        }
    }

    #[test]
    fn consistency_verifies_for_every_prefix() {
        for &n in SIZES {
            let ls = leaves(n);
            let root_n = merkle_root(&ls);
            for m in 1..=n {
                let root_m = merkle_root(&ls[..m]);
                let proof = consistency_proof(&ls, m);
                assert!(
                    verify_consistency(m, n, &proof, &root_m, &root_n),
                    "n={n} m={m}"
                );
            }
        }
    }

    #[test]
    fn a_rewritten_log_fails_consistency_and_a_tamper_fails_inclusion() {
        let ls = leaves(12);
        let root_n = merkle_root(&ls);
        let root_m = merkle_root(&ls[..5]);
        let proof = consistency_proof(&ls, 5);
        assert!(verify_consistency(5, 12, &proof, &root_m, &root_n));
        // A rewritten root_to (leaf 2 changed) is not consistent with the honest prefix root.
        let mut rewritten = ls.clone();
        rewritten[2] = leaf_hash(b"tampered");
        let bad_root = merkle_root(&rewritten);
        let bad_proof = consistency_proof(&rewritten, 5);
        assert!(!verify_consistency(5, 12, &bad_proof, &root_m, &bad_root));
        // A flipped inclusion-proof byte fails.
        let mut incl = inclusion_proof(&ls, 3);
        incl[0][0] ^= 0xff;
        assert!(!verify_inclusion(ls[3], 3, 12, &incl, &root_n));
    }

    #[test]
    fn checkpoint_signature_round_trips() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let public_hex: String = sk
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let size = 42u64;
        let root_hex = "ab".repeat(32);
        let msg = format!("noeta-log-checkpoint-v1\n{size}\n{root_hex}\n");
        let sig_hex: String = sk
            .sign(msg.as_bytes())
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        assert!(verify_checkpoint(&public_hex, size, &root_hex, &sig_hex).unwrap());
        // A tampered size does not verify.
        assert!(!verify_checkpoint(&public_hex, size + 1, &root_hex, &sig_hex).unwrap());
        // Malformed inputs are errors, not panics.
        assert!(verify_checkpoint("zz", size, &root_hex, &sig_hex).is_err());
    }
}
