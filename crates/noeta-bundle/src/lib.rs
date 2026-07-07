//! The `.noeb` bundle container (P-AOT L1.1): a versioned envelope around a serialized
//! [`noeta_bytecode::Module`], so a compiled program can be shipped and run **without its `.noe`
//! source**. This crate owns the artifact *format* — magic, versioning, and (later) the
//! obfuscation/encryption transforms — and is deliberately isolated from the core mid-end crates so
//! those never pull the container's future compression/crypto dependencies.
//!
//! ## Format
//!
//! ```text
//! ┌────────┬─────────┬───────┬────────┬──────────────┬──────────────────────┐
//! │ "NOEB" │ fmt_ver │ flags │ rt_len │ rt_ver bytes │ payload …            │
//! │ 4 B    │ u8      │ u8    │ u8     │ rt_len B     │ postcard(Module) …   │
//! └────────┴─────────┴───────┴────────┴──────────────┴──────────────────────┘
//! ```
//!
//! `fmt_ver` versions the *container* layout (this crate); `rt_ver` records the **runtime version**
//! that built the artifact. Payload compatibility is not self-describing (postcard is not), so v1's
//! policy is that **artifacts are pinned to the runtime that built them**: [`read`] rejects a
//! `rt_ver` mismatch with a clear error rather than risk decoding a stale layout. `flags` reserves
//! bit 0 (compressed) and bit 1 (encrypted) for L1.4/L1.5; both are 0 today and any set bit in a v1
//! reader is an "unsupported transform" error.

use noeta_bytecode::Module;

/// The four magic bytes every `.noeb` starts with.
pub const MAGIC: &[u8; 4] = b"NOEB";

/// The container format version this crate reads and writes.
pub const FORMAT_VERSION: u8 = 1;

/// The runtime version stamped into and checked against artifacts — the building crate's
/// package version. Any release that changes the serialized [`Module`] layout bumps this, so a
/// mismatch is the signal to rebuild the bundle.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `flags` bit 0: the payload is compressed (P-AOT L1.4). Not yet emitted or accepted.
pub const FLAG_COMPRESSED: u8 = 1 << 0;
/// `flags` bit 1: the payload is encrypted (P-AOT L1.5). Not yet emitted or accepted.
pub const FLAG_ENCRYPTED: u8 = 1 << 1;

/// Why a byte slice is not a loadable `.noeb`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// The blob is shorter than a valid header, or truncated mid-header.
    Truncated,
    /// The first four bytes are not [`MAGIC`] — not a `.noeb` at all.
    BadMagic,
    /// The container format version is newer/older than this reader supports.
    UnsupportedFormat { found: u8, supported: u8 },
    /// The artifact was built by a different runtime version (v1 pins artifacts to their builder).
    VersionMismatch { built: String, current: String },
    /// A `flags` transform (compression/encryption) this reader does not implement is set.
    UnsupportedTransform { flags: u8 },
    /// The payload did not deserialize into a `Module` (corrupt or malformed).
    Decode,
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::Truncated => write!(f, "not a valid .noeb: truncated header"),
            BundleError::BadMagic => write!(f, "not a .noeb bundle (bad magic)"),
            BundleError::UnsupportedFormat { found, supported } => write!(
                f,
                "unsupported .noeb format version {found} (this runtime reads {supported})"
            ),
            BundleError::VersionMismatch { built, current } => write!(
                f,
                "bundle was built by noeta {built}, but this runtime is {current} — rebuild the bundle"
            ),
            BundleError::UnsupportedTransform { flags } => write!(
                f,
                "bundle uses an unsupported transform (flags {flags:#04b}); this runtime cannot decode it"
            ),
            BundleError::Decode => write!(f, "corrupt .noeb payload (could not decode the module)"),
        }
    }
}

impl std::error::Error for BundleError {}

/// Serialize `module` into a `.noeb` bundle: the versioned header followed by the encoded module.
/// L1.1 emits an untransformed payload (`flags == 0`); L1.4/L1.5 will set the flag bits.
pub fn write(module: &Module) -> Vec<u8> {
    let rt = RUNTIME_VERSION.as_bytes();
    let payload = module.encode();
    let mut out = Vec::with_capacity(4 + 3 + rt.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.push(0); // flags: no transform yet
    out.push(rt.len() as u8);
    out.extend_from_slice(rt);
    out.extend_from_slice(&payload);
    out
}

/// Parse and validate a `.noeb` bundle back into a [`Module`], or explain why it cannot be loaded.
pub fn read(bytes: &[u8]) -> Result<Module, BundleError> {
    // magic(4) + fmt_ver(1) + flags(1) + rt_len(1) = 7-byte minimum header.
    if bytes.len() < 7 {
        return Err(BundleError::Truncated);
    }
    if &bytes[0..4] != MAGIC {
        return Err(BundleError::BadMagic);
    }
    let fmt_ver = bytes[4];
    if fmt_ver != FORMAT_VERSION {
        return Err(BundleError::UnsupportedFormat {
            found: fmt_ver,
            supported: FORMAT_VERSION,
        });
    }
    let flags = bytes[5];
    if flags & (FLAG_COMPRESSED | FLAG_ENCRYPTED) != 0 {
        return Err(BundleError::UnsupportedTransform { flags });
    }
    let rt_len = bytes[6] as usize;
    let rt_end = 7 + rt_len;
    if bytes.len() < rt_end {
        return Err(BundleError::Truncated);
    }
    let built = std::str::from_utf8(&bytes[7..rt_end]).map_err(|_| BundleError::Truncated)?;
    if built != RUNTIME_VERSION {
        return Err(BundleError::VersionMismatch {
            built: built.to_string(),
            current: RUNTIME_VERSION.to_string(),
        });
    }
    Module::decode(&bytes[rt_end..]).map_err(|_| BundleError::Decode)
}

/// Whether `bytes` begins with the `.noeb` magic — a cheap sniff for the CLI to decide between
/// "run this bundle" and "compile this source file", without a full parse.
pub fn is_bundle(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_bytecode::Module;

    /// A minimal but non-empty module round-trips through the container.
    fn tiny_module() -> Module {
        // The compiler is not a dependency here; an empty module exercises the header + payload
        // path. Corpus-wide module coverage lives in the conformance bundle oracle (L1.0/L1.3).
        Module::default()
    }

    #[test]
    fn write_then_read_recovers_the_module() {
        let m = tiny_module();
        let blob = write(&m);
        assert!(is_bundle(&blob));
        let back = read(&blob).expect("valid bundle");
        assert_eq!(back.encode(), m.encode());
    }

    /// `Module` has no `PartialEq` (its ops carry none), so tests assert on the error side only.
    fn err(bytes: &[u8]) -> BundleError {
        read(bytes).expect_err("expected a rejection")
    }

    #[test]
    fn bad_magic_is_rejected() {
        assert_eq!(err(b"not a bundle at all"), BundleError::BadMagic);
    }

    #[test]
    fn truncated_is_rejected() {
        assert_eq!(err(b"NOE"), BundleError::Truncated);
        // Valid magic + fmt but a rt_len claiming more bytes than present.
        let mut blob = write(&tiny_module());
        blob.truncate(5);
        assert_eq!(err(&blob), BundleError::Truncated);
    }

    #[test]
    fn version_mismatch_is_reported() {
        let mut blob = write(&tiny_module());
        // Corrupt the stored runtime-version bytes (rt starts at offset 7).
        blob[7] = blob[7].wrapping_add(1);
        assert!(matches!(err(&blob), BundleError::VersionMismatch { .. }));
    }

    #[test]
    fn a_set_transform_flag_is_unsupported_in_v1() {
        let mut blob = write(&tiny_module());
        blob[5] = FLAG_COMPRESSED;
        assert_eq!(
            err(&blob),
            BundleError::UnsupportedTransform {
                flags: FLAG_COMPRESSED
            }
        );
    }
}
