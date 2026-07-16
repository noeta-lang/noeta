//! The pm layer's **typed error** (architectural-audit fix: audit-5 #7 / cross-cutting #3).
//!
//! Every public `noeta-pm` API returns [`PmError`] instead of a bare `String`, so a consumer can
//! *branch on kind* — the IDE shows a trust refusal prominently but quietly degrades on a network
//! failure; a CLI verb can distinguish "not a project" from "the registry is down". The
//! **messages are unchanged**: each variant carries the exact human-readable string this crate
//! always produced, and [`Display`](std::fmt::Display) renders it verbatim — so the CLI's stderr
//! contract (and every e2e expectation pinned on it) is byte-identical. Interior code keeps
//! formatting strings; the classification happens where the error is *made* (the one place that
//! knows whether a failure was IO, the network, or a violated trust policy).
//!
//! This is deliberately a hand-rolled enum with `Display` (the workspace convention — no
//! `anyhow`/`thiserror` anywhere), and deliberately *one* crate-wide enum rather than per-module
//! ones: the interesting consumers (`resolve_graph`, the `Index` trait) aggregate every failure
//! domain, so a single classification axis is what they need.

/// A classified `noeta-pm` failure. The variant is the **stable kind** consumers branch on; the
/// payload is the human-readable message (rendered verbatim by `Display`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PmError {
    /// No `noeta.toml` exists at or above the queried directory — the path is not inside a
    /// package/project. Distinct from [`PmError::Manifest`] so "not a project" (often fine —
    /// a bare script) is never confused with "your manifest is broken".
    NoManifest(String),
    /// The manifest (or a package name / registry-source / cargo-manifest value inside one)
    /// exists but fails to parse or validate.
    Manifest(String),
    /// Filesystem IO failed — an unreadable file, an unwritable directory, a missing store.
    Io(String),
    /// A network or registry-protocol failure: an HTTP request, a git transport, a corrupt or
    /// refusing index. Typically transient/environmental — the kind an editor degrades quietly on.
    Network(String),
    /// The version solver found no compatible selection (a PubGrub conflict report), or the walk
    /// hit the one-version-per-identity invariant.
    Conflict(String),
    /// Authentication/authorization failed or is missing: a publish token, an OIDC token, a
    /// device-flow login.
    Auth(String),
    /// A signature/provenance/transparency/trust-policy violation: a bad signature, a trust-root
    /// downgrade, an unlogged release, an untrusted native package. The kind an editor must
    /// surface prominently — it is never routine.
    Trust(String),
    /// The lockfile contradicts reality: a stored tree's content hash drifted from the pin.
    Lock(String),
    /// A declared native crate is unusable (its `Cargo.toml` is missing where the manifest
    /// points) — the resolve-time half of a native-build failure.
    NativeBuild(String),
}

impl PmError {
    /// The human-readable message — exactly what `Display` renders (and what the pre-typed API
    /// returned as its `Err(String)`).
    pub fn message(&self) -> &str {
        match self {
            PmError::NoManifest(m)
            | PmError::Manifest(m)
            | PmError::Io(m)
            | PmError::Network(m)
            | PmError::Conflict(m)
            | PmError::Auth(m)
            | PmError::Trust(m)
            | PmError::Lock(m)
            | PmError::NativeBuild(m) => m,
        }
    }

    /// Transform the message while **keeping the kind** — the context-wrapping idiom
    /// (`err.map_msg(|m| format!("dependency `{key}`: {m}"))`), replacing the old
    /// `map_err(|err| format!("dependency `{key}`: {err}"))` without laundering a `Trust`
    /// failure into an unclassified string.
    #[must_use]
    pub fn map_msg(self, f: impl FnOnce(String) -> String) -> PmError {
        match self {
            PmError::NoManifest(m) => PmError::NoManifest(f(m)),
            PmError::Manifest(m) => PmError::Manifest(f(m)),
            PmError::Io(m) => PmError::Io(f(m)),
            PmError::Network(m) => PmError::Network(f(m)),
            PmError::Conflict(m) => PmError::Conflict(f(m)),
            PmError::Auth(m) => PmError::Auth(f(m)),
            PmError::Trust(m) => PmError::Trust(f(m)),
            PmError::Lock(m) => PmError::Lock(f(m)),
            PmError::NativeBuild(m) => PmError::NativeBuild(f(m)),
        }
    }
}

impl std::fmt::Display for PmError {
    /// The message, verbatim — no kind prefix, so every consumer that prints (`noeta: {err}`)
    /// emits byte-identical text to the pre-typed API.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PmError {}

/// A boundary escape hatch: leaf tools that genuinely want a string (`dap`/`prof`, a `?` in a
/// `Result<_, String>` function) convert without spelling out `.to_string()` at every site.
impl From<PmError> for String {
    fn from(err: PmError) -> String {
        match err {
            PmError::NoManifest(m)
            | PmError::Manifest(m)
            | PmError::Io(m)
            | PmError::Network(m)
            | PmError::Conflict(m)
            | PmError::Auth(m)
            | PmError::Trust(m)
            | PmError::Lock(m)
            | PmError::NativeBuild(m) => m,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_renders_the_message_verbatim() {
        let err = PmError::Trust("the registry's public key changed".to_string());
        assert_eq!(err.to_string(), "the registry's public key changed");
        assert_eq!(err.message(), "the registry's public key changed");
        // `format!` interpolation — the wrapping idiom consumers use — sees the bare message.
        assert_eq!(
            format!("noeta: {err}"),
            "noeta: the registry's public key changed"
        );
    }

    #[test]
    fn map_msg_keeps_the_kind() {
        let err = PmError::Network("registry returned 502".to_string())
            .map_msg(|m| format!("dependency `http`: {m}"));
        assert_eq!(
            err,
            PmError::Network("dependency `http`: registry returned 502".to_string())
        );
    }

    #[test]
    fn string_conversion_is_the_message() {
        let s: String = PmError::Io("cannot read `x`".to_string()).into();
        assert_eq!(s, "cannot read `x`");
    }
}
