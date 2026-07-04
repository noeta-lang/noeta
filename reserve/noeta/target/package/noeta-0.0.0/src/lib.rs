//! # Noeta
//!
//! **Noeta** is a programming language (source extension `.noe`).
//!
//! This `0.0.0` release is a **name-reservation placeholder**. The crate is
//! published only to hold the `noeta` name while the language is under active
//! development. It exposes no functionality yet.
//!
//! When Noeta reaches its first tagged release, this crate will become the
//! embeddable library entry point — a stable façade over the interpreter,
//! intended for hosting Noeta as a scripting language inside other Rust
//! programs (e.g. game engines), in the spirit of `rlua`, `rhai`, or
//! `deno_core`.
//!
//! Follow development at <https://noeta.dev>.

/// Placeholder marker for the reserved `noeta` crate name.
///
/// This exists solely so the crate has public API surface during the
/// reservation period; it will be removed at the first real release.
pub const RESERVED: &str = "Noeta — name reserved. See https://noeta.dev";
