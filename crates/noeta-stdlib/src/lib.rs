//! The layered standard library (M1.10).
//!
//! Ring 1 is the always-present core surface bound to the language's primitive types.
//! Where a Ring 1 operation is expressible over data that is represented *identically*
//! in both runtimes, its semantics live here once and both backends call into it — so
//! the differential oracle (`TreeWalkBackend` ≡ `VmBackend`) holds by construction, not
//! merely by test. Strings are the first such surface: both the M0 tree-walker
//! (`Value::Str(String)`) and the M1 VM (`Payload::Str(String)`) store them as a Rust
//! `String`, so every string method's behavior, arity, and argument typing is defined
//! here and each backend is reduced to thin value↔primitive glue.
//!
//! Collection methods (list/map) manipulate backend-specific value representations and
//! so cannot live here wholesale; they are implemented per backend with the differential
//! as the guard. Determinism is a hard requirement throughout (no wall clock, no
//! hash-order, seeded PRNG) — see `plans/m1/slice-10-stdlib.md`.
//!
//! Ring 2 native modules (`json`, `math`, `fs`, …) are imported with `use std.{name}` and
//! dispatched as `name.func(args)` through the native-extension [`registry`]: each module declares
//! its functions and one shared `dispatch`, and both backends route every call through it (so the
//! differential holds by construction). Each backend only binds the module value and marshals
//! arguments/results across the neutral [`registry::NativeValue`]/[`registry::NativeOut`] seam.
//!
//! The native-extension **ABI** — the [`registry`] contract, the [`Host`] capability seam, the
//! neutral marshalling ([`NativeValue`]/[`NativeOut`]), and the Ring 1 primitives — lives in the
//! lean [`noeta_native`] crate (P-NATIVE) so a third-party extension does not drag core's batteries
//! (crypto/uuid/json). `noeta-stdlib` re-exports it (`pub use noeta_native::*`) and layers the
//! concrete `std` modules on top: the `core`/`std` relationship. Every existing `noeta_stdlib::`
//! path keeps resolving.

// The ABI — the [`registry`] contract, the [`Host`] capability seam, the extern-value contract,
// the neutral marshalling, `MapKey`, the async executor seam, and the Ring 1 primitives — lives in
// `noeta-native` and is re-exported here (the `core`/`std` relation), so every existing
// `noeta_stdlib::` path resolves unchanged.
pub use noeta_native::*;

pub mod crypto;
pub mod env;
pub mod fs;
pub mod handle;
pub mod host;
pub mod id;
pub mod iter;
pub mod json;
pub mod map_key;
pub mod math;
pub mod net;
pub mod quat;
pub mod random;
pub mod registry;
pub mod vec3;

// The stdlib-only surface (the ABI items above arrive via the `noeta_native::*` glob).
pub use handle::{FileHandle, FileMode, Flush};
pub use host::SandboxHost;
pub use iter::IterMethod;
pub use registry::StdExtension;
