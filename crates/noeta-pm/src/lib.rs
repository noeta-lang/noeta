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
pub mod reserved;

/// GitHub OAuth device flow for the laptop scope-claim path (namespace-protection #1). Behind
/// `registry-http` (it needs the HTTP client), like the rest of the hosted-registry client.
#[cfg(feature = "registry-http")]
pub mod github;

/// Package provenance — Ed25519-signed attestations binding a release to its commit (Phase 4, #2).
/// Behind the `provenance` feature (CLI-only; the LSP and offline consumers don't pull the crypto).
#[cfg(feature = "provenance")]
pub mod provenance;

/// Client-side transparency-log verification (namespace-protection #1): inclusion + consistency proofs
/// and signed-checkpoint verification over the registry's RFC 6962 Merkle log. Behind `provenance`
/// (it needs Ed25519 + SHA-256).
#[cfg(feature = "provenance")]
pub mod transparency;

/// Client-side security-advisory verification (namespace-protection #1, advisory feed): fetch the
/// registry's signed advisory database, verify each entry against a pinned key, and match it against
/// resolved versions. Needs serde (the feed) + Ed25519/SHA-256 (the signatures).
#[cfg(all(feature = "registry-http", feature = "provenance"))]
pub mod advisory;

/// Keyless provenance — Sigstore bundles verified offline against the public sigstore.dev trust
/// root (Phase 5). Behind the `keyless` feature (CLI-only), for the same reason as `provenance`.
#[cfg(feature = "keyless")]
pub mod keyless;

/// Hermetic Sigstore test fixtures — a real in-process CA/CT/Rekor for minting bundles that
/// verify under the default policy (Phase 5, K4). Test builds only (`keyless-test-fixtures`).
#[cfg(feature = "keyless-test-fixtures")]
pub mod keyless_fixtures;

// Internal to the crate: the git fetch, the content-addressed store, and the pure resolver are
// implementation details the public modules above compose (the CLI never names them directly).
mod git;
mod resolve;
mod store;

/// The git **authorship** helpers backing the committer signal (`noeta update`/`add`) — re-exported so
/// front-ends reach them without the rest of the git-fetch internals (which keep `Store` private).
pub use git::{Authorship, authorship, commit_web_url, repo_web_url};

/// Resolve a git `url`@`tag` to its current commit SHA (package-manager Phase 4, S2) — the one git
/// operation `noeta publish` needs, to pin the SHA into the registry index at publish time.
pub use git::resolve_tag_sha;
