//! The `para/p2p` package's native entry crate.
//!
//! The composed toolchain (package-manager Phase 3) aggregates each native dependency's
//! `NOETA_EXTENSIONS` slice and installs the union into the runtime registry. This crate is the
//! thin entry point declared by the package manifest's `native = "native"` key; the surface itself
//! (`para.crdt` / `para.p2p` / `para.synced`) lives in the `noeta-para-p2p` library crate.
pub use noeta_para_p2p::NOETA_EXTENSIONS;
