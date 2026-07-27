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
//! lean [`noeta_ext_abi`] crate (P-NATIVE) so a third-party extension does not drag core's batteries
//! (crypto/uuid/json). `noeta-stdlib` re-exports it (`pub use noeta_ext_abi::*`) and layers the
//! concrete `std` modules on top: the `core`/`std` relationship. Every existing `noeta_stdlib::`
//! path keeps resolving.

// The ABI — the [`registry`] contract, the [`Host`] capability seam, the extern-value contract,
// the neutral marshalling, `MapKey`, the async executor seam, and the Ring 1 primitives — lives in
// `noeta-ext-abi` and is re-exported here (the `core`/`std` relation), so every existing
// `noeta_stdlib::` path resolves unchanged.
pub use noeta_ext_abi::*;

pub mod bulk;
pub mod cell;
pub mod cookie;
pub mod crypto;
/// The `std.datetime` calendar/timezone surface (Ring 3), gated behind the default-on
/// `ring-datetime` feature so a footprint-tailored build can shed jiff and the tzdb.
#[cfg(feature = "ring-datetime")]
pub mod datetime;
pub mod env;
pub mod fs;
pub mod handle;
pub mod host;
pub mod http_client;
pub mod id;
pub mod io;
pub mod iter;
pub mod json;
pub mod liveview;
pub mod log;
pub mod map_key;
pub mod math;
pub mod metrics;
pub mod net;
pub mod quat;
pub mod random;
pub mod reactive;
pub mod reductions;
/// The `std.regex` engine surface (Ring 3), gated behind the default-on `ring-regex` feature so a
/// footprint-tailored build can shed the engine and its Unicode tables.
#[cfg(feature = "ring-regex")]
pub mod regex;
pub mod registry;
/// The `Scalar` element trait — one source of truth for per-element-type numeric behaviour, consumed
/// by the reduction / element-wise / vector kernels. Exported as `scalar::Scalar` (not re-exported at
/// the crate root, where `Scalar` already names the boxed-runtime-value enum from `noeta-ext-abi`).
pub mod scalar;
pub mod serve;
pub mod session;
pub mod task;
pub mod template;
pub mod tiers;
pub mod tracing;
pub mod url;
pub mod vec3;
pub mod vec_kernels;

// The stdlib-only surface (the ABI items above arrive via the `noeta_ext_abi::*` glob).
pub use bulk::{
    ElemBinOp, ElemMap, clamp_num_packed, clamp_num_scalars, is_bulk_method, length_mismatch,
    map_num_packed, map_num_scalars, scale_num_packed, scale_num_scalars, zip_num_packed,
    zip_num_scalars,
};
pub use handle::{FileHandle, FileMode, Flush};
pub use host::{CounterIds, DeterministicClock, DeterministicEntropy, SandboxHost, SeededRng};
pub use iter::IterMethod;
pub use reductions::{
    BoolReduce, NumReduce, RedBool, RedNum, checked_sum_packed, checked_sum_scalars,
    reduce_bool_packed, reduce_bool_scalars, reduce_num_packed, reduce_num_scalars,
};
#[cfg(feature = "ring-regex")]
pub use regex::RegexExtension;
pub use registry::{CoreExtension, CryptoExtension, HttpExtension, IdExtension, VecExtension};
