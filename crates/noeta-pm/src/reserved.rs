//! Reserved namespace protection — the single source of truth for which
//! `company` scopes are not open for anyone to occupy, and why.
//!
//! A package identity is `company/package` ([`crate::manifest::PackageName`]); the `company` segment
//! is the **scope**, the unit of ownership in the registry. Two tiers of scope are protected, for two
//! different threats:
//!
//! - **Built-in** (`std`, `noeta`, `core`) — owned by the toolchain itself. The stdlib and the
//!   compiler's own modules live *inside the binary*, never in a registry. So these are resolved by
//!   the built-in provider and **never fetched from any registry** — the resolver refuses a registry
//!   dependency under a built-in scope outright ([`Reserved::Builtin`]). That is the supply-chain
//!   invariant: a third-party (or compromised) registry serving a malicious `std/*` can never shadow
//!   core, because the client won't ask a registry for `std/*` in the first place. `noeta add`
//!   refuses them for the same reason, and the registry refuses to publish them (belt and suspenders).
//!
//! - **First-party** (`para`) — a *published* first-party namespace that legitimately lives in the
//!   registry, so it must stay resolvable. Its protection is at the **claim** boundary: only the
//!   designated first-party identity may register/publish under it, so no one can squat `para/*`.
//!   The resolver treats it like any other registry package (trust is pinned via `ScopeTrust`); only
//!   scope registration consults [`Reserved::FirstParty`].
//!
//! Parsing stays neutral on purpose ([`crate::manifest::PackageName::parse`] accepts any identifier
//! pair): reservation is an *authority* decision enforced at the resolve / add / publish / claim
//! boundaries, not a lexical one — the built-in provider and the first party themselves refer to these
//! names, so the name itself is never illegal, only occupying it by the wrong party is.

/// How a scope is reserved, if at all — the classification [`classify`] returns for a `company` scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserved {
    /// A toolchain-owned scope (`std`/`noeta`/`core`): satisfied by the built-in provider, never
    /// fetched from a registry. Refused as a registry dependency and unpublishable.
    Builtin,
    /// A first-party published scope (`para`): resolvable from the registry, but only claimable by
    /// the first-party identity. Not refused at resolve time.
    FirstParty,
}

/// Toolchain-owned scopes — the stdlib built-in provider (`std`) and the compiler's/CLI's own
/// namespaces. Never a registry package. Kept lowercase; [`classify`] compares case-sensitively
/// because `company` segments are case-sensitive identifiers.
const BUILTIN_SCOPES: &[&str] = &["std", "noeta", "core"];

/// First-party published scopes — reserved against open registration but served from the registry
/// like any other package.
const FIRST_PARTY_SCOPES: &[&str] = &["para"];

/// Classify a `company` scope: `Some(Reserved::Builtin | ::FirstParty)` when the name is reserved,
/// `None` when it is free for anyone to own.
pub fn classify(scope: &str) -> Option<Reserved> {
    if BUILTIN_SCOPES.contains(&scope) {
        Some(Reserved::Builtin)
    } else if FIRST_PARTY_SCOPES.contains(&scope) {
        Some(Reserved::FirstParty)
    } else {
        None
    }
}

/// Whether `scope` is a built-in (toolchain-owned) scope — the ones the resolver refuses to fetch
/// from any registry, and that `noeta add` refuses to add.
pub fn is_builtin(scope: &str) -> bool {
    matches!(classify(scope), Some(Reserved::Builtin))
}

/// The built-in (toolchain-owned) scopes themselves — for callers that need the *set* rather than a
/// membership test (the publish namespace lint reports a package extension claiming any of these as
/// its namespace root).
pub fn builtin_scopes() -> &'static [&'static str] {
    BUILTIN_SCOPES
}

/// The error a resolver/`noeta add` raises when asked to fetch a built-in scope from a registry —
/// naming the offending `identity` and its reserved `scope`, and pointing at the reason (it is not a
/// registry package but an attempt to shadow core code). Callers prefix their own context (which
/// dependency key) when raising it.
pub fn builtin_registry_refusal(scope: &str, identity: &str) -> String {
    format!(
        "`{identity}` is under the reserved `{scope}` namespace, which is built into the Noeta \
         toolchain and is never fetched from a registry. A package served under `{scope}/…` is a \
         supply-chain attack attempting to shadow core code — the resolver refuses it. (Built-in \
         scopes: {list}.)",
        list = BUILTIN_SCOPES.join(", ")
    )
}

/// The error raised when a **local tree** declares a built-in scope as its own identity — a
/// `path`/`git` dependency (or the root project) whose manifest says `name = "std/fs"`.
///
/// Distinct from [`builtin_registry_refusal`] because the situation is not the same one. That names
/// a *registry* serving reserved code, which is an attack with no legitimate reading; this names a
/// tree on disk, which is more often a mistake — someone naming their own filesystem helper `std/fs`
/// — so it says what the name claims and what to write instead, rather than accusing.
///
/// It is refused all the same, and for a reason the wording carries: a built-in scope is where core
/// code lives, so a package claiming one is claiming to *be* core code. Where the tree came from
/// does not change what the name says.
pub fn builtin_identity_refusal(scope: &str, identity: &str) -> String {
    format!(
        "`{identity}` declares itself under the reserved `{scope}` namespace, which is built into \
         the Noeta toolchain. A package cannot claim a built-in scope as its identity — that name \
         means core code, wherever the package came from. Rename it under your own scope \
         (`yourco/{package}`). (Built-in scopes: {list}.)",
        package = identity.split_once('/').map_or(identity, |(_, pkg)| pkg),
        list = BUILTIN_SCOPES.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_scopes_classify_as_builtin() {
        for s in ["std", "noeta", "core"] {
            assert_eq!(classify(s), Some(Reserved::Builtin), "{s}");
            assert!(is_builtin(s), "{s}");
        }
    }

    #[test]
    fn first_party_scopes_classify_but_are_not_builtin() {
        assert_eq!(classify("para"), Some(Reserved::FirstParty));
        assert!(!is_builtin("para"), "para must stay registry-resolvable");
    }

    #[test]
    fn ordinary_scopes_are_free() {
        for s in ["acme", "guzzle", "niklas", "stdlib", "corely", "paranoid"] {
            assert_eq!(classify(s), None, "{s} should be unreserved");
            assert!(!is_builtin(s));
        }
    }

    #[test]
    fn classification_is_case_sensitive() {
        // `company` segments are case-sensitive identifiers, so `Std` is a distinct, free scope.
        assert_eq!(classify("Std"), None);
        assert_eq!(classify("STD"), None);
    }

    #[test]
    fn the_refusal_message_names_the_scope_and_the_threat() {
        let msg = builtin_registry_refusal("std", "std/extra");
        assert!(msg.contains("std/extra"));
        assert!(msg.contains("supply-chain"));
        assert!(msg.contains("built into"));
    }
}
