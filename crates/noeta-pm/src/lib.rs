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

/// Language **editions** — re-exported from the leaf `noeta-edition` crate so `noeta_pm::edition`
/// keeps resolving (the resolution-side arc introduced the type here; the compiler arc relocated it
/// to a crate the front-end can also depend on).
pub use noeta_edition as edition;
pub mod composed;
pub mod error;
pub mod graph;
pub mod lock;
pub mod manifest;
pub mod registry;
pub mod reserved;
pub mod sources;

/// The crate's typed error, re-exported at the root — `noeta_pm::PmError` is the name consumers
/// match on (audit-5 #7 / cross-cutting #3).
pub use error::PmError;

/// GitHub OAuth device flow for the laptop scope-claim path (namespace-protection #1). Behind
/// `registry-http` (it needs the HTTP client), like the rest of the hosted-registry client.
#[cfg(feature = "registry-http")]
pub mod github;

/// A git forge (GitHub org) used as a registry — resolve packages from repos + tags instead of the
/// hosted index (private-registries arc). Implements the `registry::Index` trait.
pub mod git_forge;

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

/// CVSS v3.x base-score computation (advisory-intake residual b) — re-derives an imported advisory's
/// base score from the CVSS vector the feed echoes, so `noeta audit` shows the band *and* the score.
/// Pure math; gated with `advisory` since only the audit display consumes it.
#[cfg(all(feature = "registry-http", feature = "provenance"))]
pub mod cvss;

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
/// Optional git credential injection for private-repo access (private-registries arc).
mod git_auth;
mod resolve;
mod store;

/// Hermetic, per-process fixture directories for this crate's unit tests — the one place a test
/// temp path is built, so no two processes ever share one. Test builds only.
#[cfg(test)]
mod test_temp;

/// The git **authorship** helpers backing the committer signal (`noeta update`/`add`) — re-exported so
/// front-ends reach them without the rest of the git-fetch internals (which keep `Store` private).
pub use git::{Authorship, authorship, commit_web_url, repo_web_url};

/// Resolve a git `url`@`tag` to its current commit SHA (package-manager Phase 4, S2) — the one git
/// operation `noeta publish` needs, to pin the SHA into the registry index at publish time.
pub use git::resolve_tag_sha;

/// The content hash of a source tree — the same hash the resolver pins into the lockfile and the
/// compose key folds in. Re-exported for the one out-of-graph consumer (`noeta publish`'s API-docs
/// build hands the composer a crate dir directly, with no resolved graph to copy the hash from).
pub use store::hash_tree;

/// Minimal TOML basic-string quoting for values this workspace *emits* (lockfile entries, local
/// index records, composed-shim manifests, `noeta add` dependency lines): escapes backslashes and
/// double quotes; the values never contain control characters. One implementation — four crates
/// hand-rolled identical copies before.
pub fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('\"', "\\\""))
}
