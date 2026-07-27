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
//! `rt_ver` mismatch with a clear error rather than risk decoding a stale layout.
//!
//! ## Obfuscation (P-AOT L1.4)
//!
//! The default payload is **obfuscated, not plaintext**: the postcard module is deflate-compressed
//! (a size win that also defeats `strings`/`grep`) and then XOR-scrambled with a fixed-seed
//! keystream, so `noeta dump`, a hex editor, and automated tooling all fail on the shipped file
//! (`FLAG_COMPRESSED` marks it). This is **obfuscation, honestly labeled — not security**: the
//! transform is fully reversible from this open-source runtime, and the module is recoverable from
//! process memory at run time. It raises the bar against casual inspection, nothing more. Access
//! control / encrypt-at-rest is deliberately **not** provided here — that is application *policy*,
//! the developer's to build on the crypto/network primitives the language ships (`plans/aot`).
//! `FLAG_ENCRYPTED` (bit 1) stays a reserved header bit for forward-compat; a reader rejects any
//! bundle that sets it.
//!
//! ## Stapled executables (P-AOT L2)
//!
//! A `.noeb` can also be **appended to a copy of the runtime binary** to make a single
//! self-contained executable (`noeta build --exe`). The layout is the runtime image, then the
//! bundle, then a fixed 16-byte trailer `[bundle_len: u64 LE | "NOEBEXE\0"]`. On startup the
//! `noeta` binary reads only that trailer (a cheap seek to end, not the whole image) and, if the
//! sentinel is present, runs the embedded bundle instead of the toolchain CLI. See [`staple`] /
//! [`extract_stapled`] / [`stapled_len`].

use noeta_bytecode::Module;

/// The four magic bytes every `.noeb` starts with.
pub const MAGIC: &[u8; 4] = b"NOEB";

/// The container format version this crate reads and writes.
///
/// Bumped to 2 when `Op::Call`/`Op::CallGlobal` gained their supplied-mask field: that changes the
/// serialized [`Module`] layout, and `RUNTIME_VERSION` alone does not catch it during development,
/// where the package version stays put across such a change. Without the bump a `.noeb` written by
/// an earlier build passes the version gate and is then postcard-decoded against the new layout —
/// a silent misread. The gate turns that into an explicit `UnsupportedFormat`.
///
/// Bumped to 3 when `reflect::ParamSig` gained its `optional` flag. `Module::reflection` is part of
/// the postcard payload, and postcard is **not** self-describing: a struct is its fields back to
/// back with no names or tags, so an extra `bool` shifts every byte after it. A version-2 bundle
/// decoded by a version-3 reader would read the next parameter's name length as the flag and
/// desynchronise from there — a corrupt manifest, not a clean error. Same reasoning as the bump
/// before it: any change to the serialized shape, however additive it looks in Rust, is a format
/// break on the wire.
///
/// Bumped to 4 when `Op::Invoke` gained the free-function form and its `recv` register became an
/// `Option<Reg>`. Same non-self-describing-encoding reasoning: an `Option` writes a discriminant
/// byte ahead of the register, so a version-3 reader would take that byte *as* the receiver
/// register and desynchronise for the rest of the chunk. `Module::code` is part of the payload, so
/// an op-layout change is a format break exactly as a manifest change is.
///
/// Parameter attributes, by contrast, did **not** bump it, and that is the same rule read the other
/// way round: they added no field to any serialized struct. A parameter's `#[...]` attributes ride
/// as ordinary rows in `reflection.manifest`, a `Vec<AttributeRecord>` whose element shape is
/// untouched — a longer vector, not a different layout, and postcard length-prefixes vectors. Nor
/// can an earlier artifact be *stale* the way a layout change makes one: a bundle written before
/// that slice came from a compiler that could not parse an attribute in a parameter list, so its
/// source cannot have contained one, and the rows it lacks are rows its program never had.
///
/// Bumped to 5 by the packed-widths arc, which added variants to three serialized enums in the
/// postcard payload — all non-self-describing, so a new variant shifts every discriminant after it.
/// `NarrowTarget` (embedded in `Op::As`/`Op::TypeTest`, part of `Module::code`) gained an `F32` head
/// for reified `f32` narrowing; `PackedFieldDef` (in `Module::packed_schemas`) gained `F64` and
/// `IntN { bits, signed }`; and `reflect::TypeRepr` (baked into `Op` narrow targets and
/// construction-site tags) gained the matching `F64` and `IntN { signed, bits }`. A version-4 bundle
/// decoded by a version-5 reader would map the old discriminants onto the wrong variants — a `Bool`
/// head read as `F32`, a `Struct` field read as an `IntN` — and desynchronise the chunk. Same rule
/// as the bumps before it: any change to a serialized enum's variant set is a wire break.
///
/// Bumped to 6 by the packed-widths **bare-scalar** arc: `PackedSchemaDef::shape` (in
/// `Module::packed_schemas`) changed from `u32` to `Option<u32>` so a bare-scalar `List<i32>`/`List<f32>`
/// element can carry *no* shape (it materializes to a bare `int`/`f32`, not an object). postcard prefixes
/// an `Option` with a present/absent discriminant byte, so the field's encoding — and every byte after it
/// in the schema table — shifts; a version-5 reader would misread the leading `u32` as an `Option` tag.
///
/// Bumped to 7 by the debugger top-level-locals arc: `Module` gained a trailing `global_bindings:
/// Vec<GlobalId>` (the top-level value-binding slots the debugger shows on the `main` frame). It is
/// empty in a release bundle, but postcard appends the (zero-length) sequence unconditionally, so
/// the payload grows by one byte and a version-6 reader would run off the end of the previous field.
///
/// Bumped to 8 by the struct-reflection arc: `reflect::TypeInfo` (in `Module::reflection`) gained two
/// trailing `Vec`s parallel to `fields` — `field_types: Vec<TypeRepr>` and `field_optional:
/// Vec<bool>` — so the type-level `field_specs_of` query can report each field's precise declared type
/// and optionality. Same non-self-describing-encoding reasoning as the bumps before it: postcard
/// writes the two sequences back to back after the existing fields with no tag, so a version-7 reader
/// decoding a version-8 payload would read their length prefixes as the next `TypeInfo`'s bytes and
/// desynchronise the manifest. (`field_specs_of` / `construct` add new `Op` variants too, but those
/// only appear in bundles this reader also produced.)
///
/// Bumped to 9 by the precise-trait-narrowing arc: `reflect::ReflectionInfo` (in
/// `Module::reflection`) gained a trailing `trait_impls: Vec<TraitImplRecord>` — the membership
/// table the now-precise `x is dyn Trait` / `x.as<dyn Trait>()` and the new `traits_of(value)`
/// read — and `NarrowTarget` (in `Module::code`) gained a `DynTrait(String)` variant. Postcard is
/// not self-describing, so the appended sequence and the new enum variant are both wire breaks by
/// the same reasoning as every bump above.
pub const FORMAT_VERSION: u8 = 9;

/// The runtime version stamped into and checked against artifacts — the building crate's
/// package version. Any release that changes the serialized [`Module`] layout bumps this, so a
/// mismatch is the signal to rebuild the bundle.
pub const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `flags` bit 0: the payload is obfuscated (deflate-compressed + scrambled, P-AOT L1.4). Set on
/// every bundle [`write`] emits.
pub const FLAG_COMPRESSED: u8 = 1 << 0;
/// `flags` bit 1: reserved for a future encrypted payload. Access control / encrypt-at-rest is
/// intentionally out of scope for the build tool (application policy — see the module docs), so no
/// writer sets this and a reader rejects any bundle that does.
pub const FLAG_ENCRYPTED: u8 = 1 << 1;

/// The fixed seed for the obfuscation keystream (P-AOT L1.4). Not a secret — obfuscation only; it
/// lives in this open-source runtime. Chosen arbitrarily (no significance to the value).
const SCRAMBLE_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

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

/// Serialize `module` into an obfuscated `.noeb` bundle: the versioned header followed by the
/// deflate-compressed, scrambled module payload (`FLAG_COMPRESSED`). See the module docs for what
/// obfuscation does and does not protect.
pub fn write(module: &Module) -> Vec<u8> {
    let rt = RUNTIME_VERSION.as_bytes();
    let payload = obfuscate(&module.encode());
    let mut out = Vec::with_capacity(4 + 3 + rt.len() + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.push(FLAG_COMPRESSED);
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
    // Encryption (L1.5) is not implemented here; any other unknown bit is a future transform.
    if flags & !FLAG_COMPRESSED != 0 {
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
    let payload = &bytes[rt_end..];
    let encoded = if flags & FLAG_COMPRESSED != 0 {
        deobfuscate(payload)?
    } else {
        payload.to_vec()
    };
    Module::decode(&encoded).map_err(|_| BundleError::Decode)
}

/// Deflate-compress then scramble a raw module payload (P-AOT L1.4).
fn obfuscate(encoded: &[u8]) -> Vec<u8> {
    let mut compressed = miniz_oxide::deflate::compress_to_vec(encoded, 8);
    scramble(&mut compressed);
    compressed
}

/// Reverse [`obfuscate`]: de-scramble then inflate. `Err` on a corrupt/foreign payload.
fn deobfuscate(payload: &[u8]) -> Result<Vec<u8>, BundleError> {
    let mut buf = payload.to_vec();
    scramble(&mut buf); // XOR is its own inverse
    miniz_oxide::inflate::decompress_to_vec(&buf).map_err(|_| BundleError::Decode)
}

/// XOR `buf` in place with a SplitMix64 keystream seeded from [`SCRAMBLE_SEED`] — its own inverse.
/// A byte-level scramble so the shipped payload is not literally "just deflate, inflate it";
/// obfuscation only (the seed is public), not encryption.
fn scramble(buf: &mut [u8]) {
    let mut state = SCRAMBLE_SEED;
    let mut keystream = 0u64;
    for (i, byte) in buf.iter_mut().enumerate() {
        if i % 8 == 0 {
            // SplitMix64: advance and mix a fresh 64-bit keystream word every 8 bytes.
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            keystream = z ^ (z >> 31);
        }
        *byte ^= (keystream >> ((i % 8) * 8)) as u8;
    }
}

/// Whether `bytes` begins with the `.noeb` magic — a cheap sniff for the CLI to decide between
/// "run this bundle" and "compile this source file", without a full parse.
pub fn is_bundle(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[0..4] == MAGIC
}

/// The sentinel closing a stapled-executable trailer (P-AOT L2). Placed at the very end of a
/// `noeta build --exe` artifact so startup can detect an embedded bundle by reading the tail alone.
pub const EXE_MAGIC: &[u8; 8] = b"NOEBEXE\0";

/// The fixed trailer size a stapled executable ends with: an 8-byte little-endian bundle length
/// followed by [`EXE_MAGIC`].
pub const TRAILER_LEN: usize = 16;

/// Append `bundle` (the bytes from [`write`]) to a copy of the runtime image `runtime`, producing a
/// self-contained executable (P-AOT L2). The bundle sits between the untouched runtime image and a
/// locating trailer, so the OS still sees a valid executable while startup can recover the bundle.
pub fn staple(runtime: &[u8], bundle: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(runtime.len() + bundle.len() + TRAILER_LEN);
    out.extend_from_slice(runtime);
    out.extend_from_slice(bundle);
    out.extend_from_slice(&(bundle.len() as u64).to_le_bytes());
    out.extend_from_slice(EXE_MAGIC);
    out
}

/// If `trailer` is exactly the final [`TRAILER_LEN`] bytes of a stapled executable, return the
/// embedded bundle's length in bytes. `None` for the plain runtime binary (no sentinel) — the
/// startup fast path that avoids reading the whole image just to learn there is no bundle.
pub fn stapled_len(trailer: &[u8]) -> Option<usize> {
    if trailer.len() != TRAILER_LEN || &trailer[8..] != EXE_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(trailer[0..8].try_into().ok()?);
    Some(len as usize)
}

/// Recover the embedded bundle from a whole stapled-executable `image`, or `None` if it carries no
/// trailer (a plain runtime binary). A convenience over [`stapled_len`] for callers that already
/// hold the full bytes (tests); the CLI seeks the tail instead of reading the whole binary.
pub fn extract_stapled(image: &[u8]) -> Option<&[u8]> {
    if image.len() < TRAILER_LEN {
        return None;
    }
    let (body, trailer) = image.split_at(image.len() - TRAILER_LEN);
    let bundle_len = stapled_len(trailer)?;
    body.len()
        .checked_sub(bundle_len)
        .map(|start| &body[start..])
}

// Wasm stapling (P-WASM W1.2) — the `noeta build --wasm` analogue of [`staple`]: instead of a
// tail trailer (a wasm guest cannot read its own binary), the bundle is injected into the wasm
// runner's data section and a compiled-in slot is patched to point at it. The patcher is a
// dependency-free section-level rewrite, so it compiles everywhere this crate does (the runner
// included — dead code there, stripped by the linker).
mod wasm;
pub use wasm::{WasmStapleError, staple_wasm};

/// The 16-byte marker the wasm runner's bundle slot starts with (P-WASM W1.2) — the
/// patcher↔runner contract. Slot layout: `magic, ptr: u32 LE, len: u32 LE`; the runner keeps
/// exactly one copy in its data section (the slot initializer), and `staple_wasm` refuses to
/// patch zero or several occurrences.
pub const WASM_SLOT_MAGIC: [u8; 16] = *b"NOETA_BUNDLE_SLT";

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
    fn the_encryption_flag_is_unsupported_in_v1() {
        // L1.4 supports FLAG_COMPRESSED; the keyed-encryption bit (L1.5) is still rejected.
        let mut blob = write(&tiny_module());
        blob[5] |= FLAG_ENCRYPTED;
        assert!(matches!(
            err(&blob),
            BundleError::UnsupportedTransform { .. }
        ));
    }

    #[test]
    fn a_stapled_bundle_round_trips_out_of_a_runtime_image() {
        // A fake "runtime image" with a real bundle stapled on: the trailer locates the bundle and
        // `extract_stapled` recovers exactly the bytes `write` produced.
        let runtime = b"pretend this is a big ELF binary".as_slice();
        let bundle = write(&tiny_module());
        let image = staple(runtime, &bundle);
        assert_eq!(
            &image[..runtime.len()],
            runtime,
            "runtime image is untouched"
        );
        let recovered = extract_stapled(&image).expect("trailer present");
        assert_eq!(recovered, bundle.as_slice());
        // And the recovered bundle loads back into a module.
        let back = read(recovered).expect("embedded bundle is valid");
        assert_eq!(back.encode(), tiny_module().encode());
    }

    #[test]
    fn a_plain_runtime_image_has_no_stapled_bundle() {
        // No trailer sentinel ⇒ the startup fast path reports "no bundle" from the tail alone.
        let runtime = b"pretend this is a big ELF binary with no bundle".as_slice();
        assert!(extract_stapled(runtime).is_none());
        assert!(stapled_len(&runtime[runtime.len() - TRAILER_LEN..]).is_none());
        // Too short to even hold a trailer.
        assert!(extract_stapled(b"tiny").is_none());
    }

    #[test]
    fn the_payload_is_obfuscated_not_plaintext_bytecode() {
        let m = tiny_module();
        let blob = write(&m);
        // The compression flag is set…
        assert_eq!(blob[5] & FLAG_COMPRESSED, FLAG_COMPRESSED);
        // …and the on-disk payload is not the raw postcard encoding (a transform was applied).
        let rt_len = blob[6] as usize;
        let payload = &blob[7 + rt_len..];
        assert_ne!(
            payload,
            m.encode().as_slice(),
            "payload must be transformed"
        );
    }
}
