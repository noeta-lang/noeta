//! The Noeta **package manager** (package-manager milestone, Phase 2).
//!
//! Everything a build needs to turn a `noeta.toml`'s `[dependencies]` into the re-rooted source
//! packages the loader links: manifest parsing, the transitive graph walk, git fetch + a
//! content-addressed store, the `noeta.lock` reproducible pin, the registry index, and the pure
//! PubGrub resolver. This lives in a **library** (not the `noeta-cli` binary) so every front-end —
//! the CLI, the `noeta-lsp` language server, and the `noeta-db` salsa graph — resolves dependencies
//! through the same code, and cross-package `use` resolves identically whether you run, check, or
//! edit.
//!
//! The determinism boundary holds: fetching/resolving are real host IO done *before* compilation,
//! producing on-disk source trees + `noeta.lock`; the loader/compiler then run deterministically over
//! those, outside the differential oracle.

pub mod graph;
pub mod lock;
pub mod manifest;
pub mod registry;

// Internal to the crate: the git fetch, the content-addressed store, and the pure resolver are
// implementation details the public modules above compose (the CLI never names them directly).
mod git;
mod resolve;
mod store;
