//! The native-extension ABI (P-NATIVE): the contract a crate implements to register native
//! modules and first-class types into the language, plus the dep-free primitives both backends
//! and the front-end share.
//!
//! Split out of `noeta-stdlib` so the contract does not drag core's batteries (crypto/UUID/JSON):
//! a third-party extension — and internal mid-end crates like `noeta-ir` — depend on this lean
//! crate, while `noeta-stdlib` re-exports it (`pub use noeta_native::*`) and adds the concrete
//! `std` modules on top (the `core`/`std` relationship). See `plans/native-abi/README.md`.
