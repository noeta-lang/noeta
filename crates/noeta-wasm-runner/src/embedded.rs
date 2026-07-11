//! The stapled-bundle slot (P-WASM W1.2) — the runner side of `noeta build --wasm`.
//!
//! A single-artifact build injects the program's `.noeb` into this binary's wasm data section
//! (`noeta_bundle::staple_wasm`): it appends the bundle as a new active data segment placed at
//! the old data end (bumping the memory minimum to cover it — Rust's wasm allocator acquires
//! fresh pages via `memory.grow`, so the heap starts above it and never overlaps), then finds
//! [`SLOT_MAGIC`] inside the existing data and patches the two `u32`s after it to the bundle's
//! address and length. At startup the runner reads the slot back; a zero length means "not
//! stapled" and the two-file argv path runs instead.
//!
//! The reads are `volatile` — the static's compile-time value IS zero, and a plain read of an
//! immutable static would be constant-folded to that zero, blinding the runner to the patch.
//! This is the only `unsafe` in the crate (see the Cargo.toml lints note): it cannot run under
//! miri (the patched bytes exist only in a rewritten wasm binary), so the wasm differential
//! oracle gates it instead, the same posture as the JIT's native code.

/// The patchable slot: the shared [`noeta_bundle::WASM_SLOT_MAGIC`] marker, then the bundle's
/// linear-memory address and length as little-endian `u32`s (wasm32 pointers). `repr(C)` so the
/// field offsets are exactly the byte offsets the patcher writes (16 and 20). The magic is used
/// only as this initializer — the runner never compares it at runtime — so the emitted binary
/// contains exactly one copy for the patcher to find.
#[repr(C)]
struct BundleSlot {
    magic: [u8; 16],
    ptr: u32,
    len: u32,
}

/// The slot instance the patcher rewrites. `#[used]` keeps it in the emitted data section even
/// though the unpatched binary only ever reads zeros from it.
#[used]
static BUNDLE_SLOT: BundleSlot = BundleSlot {
    magic: noeta_bundle::WASM_SLOT_MAGIC,
    ptr: 0,
    len: 0,
};

/// The stapled bundle, if this binary has been patched — `None` in a plain (two-file) runner.
pub fn bundle() -> Option<&'static [u8]> {
    // SAFETY: volatile forces real memory reads of the (possibly patched) static — see the
    // module docs. When `len` is non-zero the patcher has guaranteed `[ptr, ptr+len)` is an
    // initialized, immutable active data segment inside the instance's minimum memory, disjoint
    // from every Rust allocation (it sits between the old data end and the bumped memory
    // minimum; the allocator grows beyond it). The address never came from a Rust allocation,
    // so it is materialized with exposed provenance, like the NaN-box codec's pointers.
    #[allow(unsafe_code)]
    unsafe {
        let len = std::ptr::read_volatile(&raw const BUNDLE_SLOT.len);
        if len == 0 {
            return None;
        }
        let ptr = std::ptr::read_volatile(&raw const BUNDLE_SLOT.ptr);
        Some(std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<u8>(ptr as usize),
            len as usize,
        ))
    }
}
